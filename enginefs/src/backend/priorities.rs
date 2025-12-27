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

    // 1. DYNAMIC WINDOW SIZING
    // -----------------------------------------------------------------------

    // Base "Urgent" Window: The absolute minimum we need to keep playback going.
    // Default: 15 pieces (~15MB-60MB depending on piece size).
    // If bitrate is known, ensure at least 15 seconds.
    let mut urgent_base_pieces = 15;
    if let Some(br) = bitrate {
        let pieces_for_15s = ((br * 15) / piece_length) as i32;
        urgent_base_pieces = urgent_base_pieces.max(pieces_for_15s);
    }

    // Proactive "Lookahead" Window:
    // If we are downloading significantly faster than the bitrate, expadn the window
    // to build a larger safety buffer.
    let mut proactive_bonus_pieces = 0;
    if let Some(br) = bitrate {
        if download_speed > (br * 15 / 10) {
            // 1.5x bitrate
            // We have excess bandwidth. Expand window up to 60s ahead.
            let pieces_for_45s = ((br * 45) / piece_length) as i32;
            proactive_bonus_pieces = pieces_for_45s;
        }
    } else {
        // Fallback if bitrate unknown: if speed > 5MB/s, add 20 pieces (~20MB)
        if download_speed > 5 * 1024 * 1024 {
            proactive_bonus_pieces = 20;
        }
    }

    // Parameters for window sizing
    let (mut urgent_window, buffer_window, max_buffer_pieces) = if config.enabled {
        let max_pieces = (config.size / piece_length) as i32;

        // Urgent = Base + Proactive
        // But clamped to valid cache limits
        let mut urgent = urgent_base_pieces + proactive_bonus_pieces;

        // Cap urgent window to avoid insane values on huge cache
        urgent = urgent.min(max_pieces).min(300); // Absolute max 300 pieces urgent

        // Buffer: Fill the rest of the cache space, up to a limit
        let remaining_for_buffer = max_pieces.saturating_sub(urgent);
        let buffer = 15.min(remaining_for_buffer);
        (urgent, buffer, max_pieces)
    } else {
        // Cache Disabled: Streaming Mode
        // Use calculated values but clamp to a safe limit for strict streaming (e.g. 50 pieces max)
        let urgent = (urgent_base_pieces + proactive_bonus_pieces).min(50);
        (urgent, 0, urgent)
    };

    let start_piece = current_piece;

    // 2. DYNAMIC HEAD/DEADLINE CALCULATION
    // -----------------------------------------------------------------------

    // Calculate "Head Window": The extremely critical immediate future.
    // We want this to be roughly 5-10 seconds of playback.
    let target_buffer_seconds = 5;
    let minimum_buffer_bytes = 5 * 1024 * 1024u64; // Minimum 5MB
    let maximum_buffer_bytes = 50 * 1024 * 1024u64; // Maximum 50MB

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
    head_window = head_window.clamp(5, 250);

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

    // Dynamic Deadline Steps
    // If usage is high (low buffer/health) or speed is low, we might need stricter deadlines.
    // For now, we use a simple heuristic:
    // If we have high speed, we can afford slightly looser deadlines to allow
    // multiple peers to compete naturally?
    // ACTUALLY: High speed usually comes from aggressive requesting.
    // Let's stick to a robust default but allow specific overrides.

    // We'll just use the standard step but ensure head is strict.
    // The "Staircase" ensures order.

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
            // Normal Streaming
            if distance < 5 {
                // CRITICAL HEAD: Very Strict Staircase
                // 0, 50, 100, 150, 200
                distance * 50
            } else if distance < head_window {
                // HEAD WINDOW: Linear staircase
                250 + ((distance - 5) * 50)
            } else {
                // BODY/TAIL
                // If we are in the "proactive bonus" area (distance > urgent_base),
                // we can be very relaxed.
                let is_proactive_area = distance > urgent_base_pieces;

                if is_proactive_area {
                    // Relaxed deadlines (10s+) for lookahead
                    10000 + (distance * 50)
                } else {
                    // Standard body
                    5000 + (distance * 20)
                }
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
        // Speed 10MB/s. Bitrate None.
        // Proactive bonus: 10MB > 5MB -> +20 pieces.
        // Urgent Base: 15.
        // Total Urgent: 35.
        // Cache enabled=false -> buffer=0.
        // Total = 35.
        let priorities =
            calculate_priorities(100, 1000, piece_len, &config, 1, 10 * 1024 * 1024, None);

        assert_eq!(priorities.len(), 35);

        // Verify deadlines - now more aggressive (0, 50, 100...)
        assert_eq!(priorities[0].deadline, 0); // distance 0 * 50
        assert_eq!(priorities[1].deadline, 50); // distance 1 * 50
    }

    #[test]
    fn test_cache_enabled_normal_mode() {
        let config = EngineCacheConfig {
            size: 1024 * 1024 * 1024, // 1GB
            enabled: true,
        };
        let piece_len = 1024 * 1024; // 1MB
        // Speed 10MB/s -> target head window 5s = 50MB -> 50 pieces.
        // Proactive bonus: 20 pieces.
        // Urgent Base: 15 + 20 = 35.
        // Urgent = max(35, head(50) + 15) = 65 pieces.
        // Buffer window = 15 pieces.
        // Total = 80 pieces.
        let priorities =
            calculate_priorities(0, 1000, piece_len, &config, 1, 10 * 1024 * 1024, None);

        assert_eq!(priorities.len(), 80);

        // Head - Strict Staircase
        assert_eq!(priorities[0].deadline, 0);
        assert_eq!(priorities[1].deadline, 50);
        assert_eq!(priorities[2].deadline, 100);
    }

    #[test]
    fn test_cache_enabled_small_cache_clamping() {
        let piece_len = 10 * 1024 * 1024; // 10MB pieces
        let config = EngineCacheConfig {
            size: 200 * 1024 * 1024, // 200MB cache limit
            enabled: true,
        };

        // Max pieces = 200 / 10 = 20 pieces.
        // Speed 10MB.
        let priorities =
            calculate_priorities(0, 1000, piece_len, &config, 1, 10 * 1024 * 1024, None);

        // Should be fully clamped to 20 items (cache max)
        assert_eq!(priorities.len(), 20);

        // Verify clamp effect
        assert_eq!(priorities[0].deadline, 0);
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
        // Download speed 20MB/s (2x bitrate).
        // Proactive bonus: Speed > 1.5*Br? Yes.
        // Bonus = 45s @ 10MB/s = 450MB = 450 pieces.

        // Urgent Base = 15s @ 10MB = 150 pieces.
        // Urgent Total Calc = 150 + 450 = 600 pieces.
        // CLAMP: max urgent is 300. So Urgent = 300.

        // Head Window: 5s @ 10MB = 50 pieces. (Target head bytes uses bitrate if avail).
        // Urgent = max(300, 50+15) = 300.

        // Buffer = 15.
        // Total = 315.

        let priorities =
            calculate_priorities(0, 1000, piece_len, &config, 1, 20 * 1024 * 1024, bitrate);

        assert_eq!(priorities.len(), 315);
    }
}
