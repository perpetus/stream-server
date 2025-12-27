use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EngineCacheConfig {
    pub size: u64,
    pub enabled: bool,
}

impl Default for EngineCacheConfig {
    fn default() -> Self {
        Self {
            size: 10 * 1024 * 1024 * 1024, // 10 GB
            enabled: true,
        }
    }
}

pub struct PriorityItem {
    pub piece_idx: i32,
    pub deadline: i32, // ms
}

/// Calculates piece priorities based on current position and cache configuration.
///
/// # Arguments
/// * `current_piece` - The piece index corresponding to the current playback position.
/// * `total_pieces` - Total number of pieces in the torrent.
/// * `piece_length` - Size of one piece in bytes.
/// * `config` - Cache configuration (size and enabled status).
/// * `priority` - Priority level (0-255). 0 or 1 is normal, >1 is high priority.
/// * `download_speed` - Current download speed in bytes/sec (used for dynamic buffering).
/// * `bitrate` - Optional average bitrate of the content in bytes/sec.
///
/// # Returns
/// A vector of `PriorityItem` containing (piece_idx, deadline).
/// Deadlines are in milliseconds. 0 means no deadline (normal priority).
pub fn calculate_priorities(
    current_piece: i32,
    total_pieces: i32,
    piece_length: u64,
    config: &EngineCacheConfig,
    priority: u8,
    download_speed: u64,
    bitrate: Option<u64>,
) -> Vec<PriorityItem> {
    let mut priorities = Vec::new();

    // Safety check
    if piece_length == 0 || current_piece < 0 {
        return priorities;
    }

    // Parameters for window sizing
    let (mut urgent_window, buffer_window, max_buffer_pieces) = if config.enabled {
        let max_pieces = (config.size / piece_length) as i32;

        // Base urgent: 15 pieces (~15MB) - Reduced from 60 to prevent probe flooding
        // If bitrate is known, ensure at least 10 seconds of content is "urgent"
        let mut urgent = 15;
        if let Some(br) = bitrate {
            let pieces_for_10s = ((br * 10) / piece_length) as i32;
            urgent = urgent.max(pieces_for_10s);
        }

        // Buffer: Up to 15 extra pieces
        let remaining_for_buffer = max_pieces.saturating_sub(urgent);
        let buffer = 15.min(remaining_for_buffer);
        (urgent, buffer, max_pieces)
    } else {
        // Cache Disabled: Streaming Mode
        // Minimal window to keep playback going without filling disk
        // Urgent: 15 pieces (~15-60MB). Slightly increased for safety.
        (15, 0, 15)
    };

    let start_piece = current_piece;

    // Dynamic Head Window Calculation based on download speed and bitrate
    // Target: Buffer enough data for N seconds of playback startup
    // Dynamic Head Window Calculation based on download speed and bitrate
    // Target: Buffer enough data for N seconds of playback startup
    // REDUCED: 50MB is too much for immediate start. Use 5MB min, 50MB max.
    let target_buffer_seconds = 5;
    let minimum_buffer_bytes = 5 * 1024 * 1024u64; // Minimum 5MB (approx 5 pieces)
    let maximum_buffer_bytes = 50 * 1024 * 1024u64; // Maximum 50MB to avoid over-buffering

    // If bitrate is known, we use it to calculate required bytes for 10s.
    // Otherwise we use a heuristic based on download speed.
    let target_head_bytes = if let Some(br) = bitrate {
        (br * target_buffer_seconds)
            .max(minimum_buffer_bytes)
            .min(maximum_buffer_bytes)
    } else {
        download_speed
            .saturating_mul(target_buffer_seconds)
            .max(minimum_buffer_bytes)
            .min(maximum_buffer_bytes)
    };

    let mut head_window = (target_head_bytes / piece_length) as i32;
    if head_window < 5 {
        head_window = 5;
    }
    if head_window > 250 {
        head_window = 250;
    } // Increased cap for 4K

    // Urgent window must be at least head window + some body
    urgent_window = urgent_window.max(head_window + 15);

    let total_window = urgent_window + buffer_window;
    if total_window == 0 {
        return priorities;
    }

    // Calculate end index (inclusive) based on length
    let end_piece = (start_piece + total_window - 1).min(total_pieces - 1);

    // Safety clamp for cache size
    let allowed_end_piece = if max_buffer_pieces > 0 {
        (start_piece + max_buffer_pieces - 1).min(total_pieces - 1)
    } else {
        start_piece - 1
    };

    let effective_end_piece = end_piece.min(allowed_end_piece);

    for p in start_piece..=effective_end_piece {
        let distance = p - start_piece;

        let deadline = if priority >= 250 {
            // Internal Probes / Metadata parsing (Absolute priority)
            50
        } else if priority >= 100 {
            // Seeking (High priority, tighter deadlines)
            10 + (distance * 10)
        } else if priority == 0 {
            // Background pre-caching (Lazy deadlines)
            20000 + (distance * 200)
        } else {
            // Normal Streaming (Regular priority)
            // Normal Streaming (Regular priority)
            if distance < 5 {
                // CRITICAL HEAD: Strict Staircase (0, 50, 100, 150, 200 ms)
                // This forces sequential download of the first few chunks.
                distance * 50
            } else if distance < head_window {
                // HEAD WINDOW: Linear staircase (250, 300, 350...)
                // We must maintain strict ordering to prevent "rarest first" from sniping later pieces.
                250 + ((distance - 5) * 50)
            } else {
                // BODY/TAIL: Relaxed but still ordered
                // 10000+, but kept ordered
                5000 + (distance * 10)
            }
        };

        priorities.push(PriorityItem {
            piece_idx: p,
            deadline,
        });
    }

    priorities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_disabled_streaming_mode() {
        let config = EngineCacheConfig {
            size: 0,
            enabled: false,
        };
        let piece_len = 1024 * 1024; // 1MB
        let priorities =
            calculate_priorities(100, 1000, piece_len, &config, 1, 10 * 1024 * 1024, None);

        // Minimal window without cache is now 15 (urgent).
        assert_eq!(priorities.len(), 15);

        // Verify deadlines - now more aggressive
        assert_eq!(priorities[0].deadline, 100);
        assert_eq!(priorities[1].deadline, 150);
    }

    #[test]
    fn test_cache_enabled_normal_mode() {
        let config = EngineCacheConfig {
            size: 1024 * 1024 * 1024, // 1GB
            enabled: true,
        };
        let piece_len = 1024 * 1024; // 1MB
        // Speed 10MB/s -> target 100MB head window -> 100 pieces
        // Urgent window = 100 + 15 = 115 pieces
        // Buffer window = 100 pieces
        // Total = 215 pieces
        let priorities =
            calculate_priorities(0, 1000, piece_len, &config, 1, 10 * 1024 * 1024, None);

        assert_eq!(priorities.len(), 215);

        // Head - Tighter deadlines now
        assert_eq!(priorities[0].deadline, 100);
        assert_eq!(priorities[1].deadline, 150);
        assert_eq!(priorities[2].deadline, 200);
    }

    #[test]
    fn test_cache_enabled_small_cache_clamping() {
        let piece_len = 10 * 1024 * 1024; // 10MB pieces
        let config = EngineCacheConfig {
            size: 200 * 1024 * 1024, // 200MB cache limit
            enabled: true,
        };

        // Max pieces = 200 / 10 = 20 pieces.
        let priorities =
            calculate_priorities(0, 1000, piece_len, &config, 1, 10 * 1024 * 1024, None);

        // Should be clamped to 20 items
        assert_eq!(priorities.len(), 20);

        // Verify clamp effect
        assert_eq!(priorities[0].deadline, 100);
        assert_eq!(priorities[19].piece_idx, 19);
    }

    #[test]
    fn test_bitrate_aware_window() {
        let piece_len = 1024 * 1024; // 1MB
        let config = EngineCacheConfig {
            size: 2 * 1024 * 1024 * 1024, // 2GB
            enabled: true,
        };

        // High bitrate content (10MB/s = 80Mbps)
        let bitrate = Some(10 * 1024 * 1024);
        let priorities =
            calculate_priorities(0, 1000, piece_len, &config, 1, 5 * 1024 * 1024, bitrate);

        // head_window = (10MB * 10s) / 1MB = 100 pieces.
        // urgent_window = max(60, (10MB * 30s)/1MB) = 300 pieces.
        // efficient_urgent = max(300, 100 + 15) = 300 pieces.
        // buffer_window = 100 pieces.
        // Total = 400 pieces.
        assert_eq!(priorities.len(), 400);
    }
}
