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
/// // * `piece_length` - Size of one piece in bytes.
/// * `config` - Cache configuration (size and enabled status).
/// 
/// # Returns
/// A vector of `PriorityItem` containing (piece_idx, deadline). 
/// Deadlines are in milliseconds. 0 means no deadline (normal priority).
/// Calculates piece priorities based on current position and cache configuration.
/// 
/// # Arguments
/// * `current_piece` - The piece index corresponding to the current playback position.
/// * `total_pieces` - Total number of pieces in the torrent.
/// // * `piece_length` - Size of one piece in bytes.
/// * `config` - Cache configuration (size and enabled status).
/// * `priority` - Priority level (0-255). 0 or 1 is normal, >1 is high priority.
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
) -> Vec<PriorityItem> {
    let mut priorities = Vec::new();

    // Safety check
    if piece_length == 0 || current_piece < 0 {
        return priorities;
    }

    // Parameters for window sizing
    let (urgent_window, buffer_window, max_buffer_pieces) = if config.enabled {
        let max_pieces = (config.size / piece_length) as i32;
        // Urgent: 30 pieces (~30-100MB depending on piece size). Enough for 30s-1m of video.
        let urgent = 30;
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
    let total_window = urgent_window + buffer_window;
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
            // 1ms for the first few pieces, then 10ms
            if distance < 5 { 1 } else { 10 }
        } else {
            // Normal Priority
            if distance < 5 {
                // HEAD (Urgent): Strict Gradient
                // Forces sequential download for instant seeking
                // 0->1ms, 1->5ms, 2->10ms, 3->15ms, 4->20ms
                if distance == 0 { 1 } else { distance * 5 }
            } else if distance < urgent_window {
                // BODY (Urgent): Fast (10ms)
                // Keeps the pipe full
                10
            } else {
                // TAIL (Buffer): Background (Stepped)
                // Only applies if buffering is enabled
                // 50ms base + gradient to allow aggregation
                50 + (distance - urgent_window) * 10
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
        let priorities = calculate_priorities(100, 1000, piece_len, &config, 1);

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
        let priorities = calculate_priorities(0, 1000, piece_len, &config, 1);

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
        
        let priorities = calculate_priorities(0, 1000, piece_len, &config, 1);
        
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
