//! Piece waiter registry for notification-based streaming
//!
//! Instead of polling have_piece(), streams register their waker here.
//! When piece_finished_alert fires, we wake all waiters for that piece.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::task::Waker;

type PieceKey = (String, i32);

/// Registry of wakers waiting for specific pieces
pub struct PieceWaiterRegistry {
    /// Maps (info_hash, piece_idx) -> list of waiting wakers
    waiters: RwLock<HashMap<PieceKey, Vec<Waker>>>,
}

impl PieceWaiterRegistry {
    pub fn new() -> Self {
        Self {
            waiters: RwLock::new(HashMap::new()),
        }
    }

    /// Register a waker to be notified when a piece finishes downloading
    /// Synchronous - safe to call from poll_read
    pub fn register(&self, info_hash: &str, piece: i32, waker: Waker) {
        let key = (info_hash.to_lowercase(), piece);
        self.waiters.write().entry(key).or_default().push(waker);
    }

    /// Called when piece_finished_alert fires - wake all waiters for this piece
    pub fn notify_piece_finished(&self, info_hash: &str, piece: i32) {
        let key = (info_hash.to_lowercase(), piece);
        if let Some(waker_list) = self.waiters.write().remove(&key) {
            let count = waker_list.len();
            for waker in waker_list {
                waker.wake();
            }
            if count > 0 {
                tracing::trace!(
                    "PieceWaiterRegistry: Woke {} waiters for piece {} of {}",
                    count,
                    piece,
                    info_hash
                );
            }
        }
    }

    /// Clear all waiters for a torrent (called when torrent is removed)
    #[allow(dead_code)]
    pub fn clear_torrent(&self, info_hash: &str) {
        let info_hash_lower = info_hash.to_lowercase();
        self.waiters
            .write()
            .retain(|(hash, _), _| hash != &info_hash_lower);
    }
}

impl Default for PieceWaiterRegistry {
    fn default() -> Self {
        Self::new()
    }
}
