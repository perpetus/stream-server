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
) -> Vec<PriorityItem> {
    let mut priorities = Vec::new();

    // Safety check
    if piece_length == 0 || current_piece < 0 {
        return priorities;
    }

    // Parameters for window sizing
    let (urgent_window, buffer_window, max_buffer_pieces) = if config.enabled {
        let max_pieces = (config.size / piece_length) as i32;
        // Urgent: 60 pieces (~60-200MB depending on piece size). Increased for buffer health.
        let urgent = 60;
        // Buffer: Up to 100 extra pieces, but clamped by cache size
        // We ensure we don't use more than `max_pieces` total in the window
        let remaining_for_buffer = max_pieces.saturating_sub(urgent);
        let buffer = 100.min(remaining_for_buffer);
        (urgent, buffer, max_pieces)
    } else {
        // Cache Disabled: Streaming Mode
        // Minimal window to keep playback going without filling disk
        // Urgent: 10 pieces (~10-40MB)
        // Buffer: 0 pieces
        (10, 0, 10)
    };

    let start_piece = current_piece;
    
    // Dynamic Head Window Calculation based on download speed
    // Target: Buffer enough data for N seconds of playback startup
    // Formula: target_bytes = max(speed * target_seconds, minimum_bytes)
    let target_buffer_seconds = 10; // Target 10 seconds of buffer for smooth startup
    let minimum_buffer_bytes = 50 * 1024 * 1024u64; // Minimum 50MB regardless of speed
    let maximum_buffer_bytes = 200 * 1024 * 1024u64; // Maximum 200MB to avoid over-buffering
    
    let speed_based_bytes = download_speed.saturating_mul(target_buffer_seconds);
    let target_head_bytes = speed_based_bytes
        .max(minimum_buffer_bytes)
        .min(maximum_buffer_bytes);
    
    let mut head_window = (target_head_bytes / piece_length) as i32;
    if head_window < 5 { head_window = 5; } // Minimum safety defaults
    if head_window > 200 { head_window = 200; } // Cap to proper sanity limit
    
    // Urgent window must be at least head window + some body
    let effective_urgent = urgent_window.max(head_window + 10);
    
    let total_window = effective_urgent + buffer_window;
    if total_window == 0 {
        return priorities;
    }
    
    // Calculate end index (inclusive) based on length
    // If length is 10, we want indices 0..9. So start + 10 - 1.
    let end_piece = (start_piece + total_window - 1).min(total_pieces - 1);
    
    // Safety clamp for cache size
    // max_buffer_pieces is the total pieces allowed by cache
    let allowed_end_piece = if max_buffer_pieces > 0 {
        (start_piece + max_buffer_pieces - 1).min(total_pieces - 1)
    } else {
        start_piece - 1 // Should not happen given previous checks, but safe fallback
    };
    
    let effective_end_piece = end_piece.min(allowed_end_piece);

    // If somehow start > end (e.g. 0 length or clamping), range will be empty, which is correct
    for p in start_piece..=effective_end_piece {
        let distance = p - start_piece;
        
        let deadline = if priority > 1 {
            // High Priority: aggressively low deadlines
            // 200ms
            200
        } else {
            // Normal Priority
            if distance < 3 {
                // CRITICAL HEAD (First ~5MB): Absolute Priority
                // Forces strict sequential ordering for the very first pieces.
                // 100ms base. This must win against everything else.
                100 + (distance * 100)
            } else if distance < head_window {
                // HEAD WINDOW (dynamic based on speed): High but Non-Blocking Priority
                // We want this fast, but NOT at the expense of the critical head.
                // Base: 2000ms (2 seconds). This allows the first pieces to saturate the link.
                // Gradient: +20ms per piece to keep order.
                2000 + (distance * 20)
            } else if distance < effective_urgent {
                // BODY (Urgent): Background Fill
                // 5000ms (5 seconds). Keeps the pipe full but yields to head.
                5000
            } else {
                // TAIL (Buffer): Background Aggregation
                // 10s+
                10000 + (distance - urgent_window) * 100
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
        let priorities = calculate_priorities(100, 1000, piece_len, &config, 1, 10 * 1024 * 1024);

        // Window size 10 (urgent) + 0 (buffer) = 10.
        // Indices 100..109. Count 10.
        assert_eq!(priorities.len(), 10);
        
        // Verify deadlines
        assert_eq!(priorities[0].deadline, 1); // Immediate (Dist 0)
        assert_eq!(priorities[1].deadline, 5); // Gradient (Dist 1)
        assert_eq!(priorities[9].deadline, 10); // Fast (Dist 9, < 10)
    }

    #[test]
    fn test_cache_enabled_normal_mode() {
        let config = EngineCacheConfig {
            size: 1024 * 1024 * 1024, // 1GB
            enabled: true,
        };
        let piece_len = 1024 * 1024; // 1MB
        let priorities = calculate_priorities(0, 1000, piece_len, &config, 1, 10 * 1024 * 1024);

        // Window = 30 urgent + 100 buffer = 130 pieces total
        // 1GB cache >>> 130MB, so no clamping
        assert_eq!(priorities.len(), 130); 

        // Head - Strict Gradient
        assert_eq!(priorities[0].deadline, 1);
        assert_eq!(priorities[1].deadline, 5);
        assert_eq!(priorities[2].deadline, 10);
        
        // Body (at 10)
        assert_eq!(priorities[10].deadline, 10);
        // Buffer (at 40) -> distance 40. urgent=30. 50 + (40-30)*10 = 150ms
        assert_eq!(priorities[40].deadline, 150);
    }

    #[test]
    fn test_cache_enabled_small_cache_clamping() {
        let piece_len = 10 * 1024 * 1024; // 10MB pieces
        let config = EngineCacheConfig {
            size: 200 * 1024 * 1024, // 200MB cache limit
            enabled: true,
        };
        
        // Max pieces = 200 / 10 = 20 pieces.
        // prioritized logic: urgent=30, buffer=0. 
        // Max allow = 20.
        
        let priorities = calculate_priorities(0, 1000, piece_len, &config, 1, 10 * 1024 * 1024);
        
        // Should be clamped to 20 items
        assert_eq!(priorities.len(), 20);
        
        // Verify clamp effect
        // 0..4 = Gradient
        assert_eq!(priorities[0].deadline, 1);
        assert_eq!(priorities[1].deadline, 5);
        
        // 5..19 = 10ms
        // Total 20 items.
        assert_eq!(priorities[19].piece_idx, 19);
        assert_eq!(priorities[19].deadline, 10);
    }
}
