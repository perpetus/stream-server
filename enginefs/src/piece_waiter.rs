//! Piece waiter registry for notification-based streaming
//!
//! Instead of polling have_piece(), streams register their waker here.
//! Once piece bytes are cached and readable, we wake all waiters for that piece.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::task::Waker;

type PieceKey = (String, i32);

/// Registry of wakers waiting for specific pieces
pub struct PieceWaiterRegistry {
    /// Maps (info_hash, piece_idx) -> list of waiting wakers
    waiters: RwLock<HashMap<PieceKey, HashMap<usize, Waker>>>,
}

impl PieceWaiterRegistry {
    pub fn new() -> Self {
        Self {
            waiters: RwLock::new(HashMap::new()),
        }
    }

    /// Register a waker to be notified when a piece finishes downloading
    /// Synchronous - safe to call from poll_read
    pub fn register(&self, info_hash: &str, piece: i32, stream_id: usize, waker: Waker) {
        let key = (info_hash.to_lowercase(), piece);
        self.waiters
            .write()
            .entry(key)
            .or_default()
            .insert(stream_id, waker);
    }

    /// Called once piece data is readable from cache - wake all waiters for this piece
    pub fn notify_piece_finished(&self, info_hash: &str, piece: i32) {
        let key = (info_hash.to_lowercase(), piece);
        if let Some(waker_list) = self.waiters.write().remove(&key) {
            let count = waker_list.len();
            for (_, waker) in waker_list {
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

    /// Remove all waiters registered by a stream.
    pub fn unregister_stream(&self, stream_id: usize) {
        let mut waiters = self.waiters.write();
        waiters.retain(|_, stream_waiters| {
            stream_waiters.remove(&stream_id);
            !stream_waiters.is_empty()
        });
    }

    pub fn stats(&self) -> PieceWaiterStats {
        let waiters = self.waiters.read();
        let keys = waiters.len() as u64;
        let wakers = waiters.values().map(|v| v.len() as u64).sum();
        PieceWaiterStats { keys, wakers }
    }
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct PieceWaiterStats {
    pub keys: u64,
    pub wakers: u64,
}

impl Default for PieceWaiterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::PieceWaiterRegistry;
    use futures::task::noop_waker;

    #[test]
    fn register_replaces_waker_for_same_stream() {
        let registry = PieceWaiterRegistry::new();
        let waker = noop_waker();

        registry.register("ABC", 7, 1, waker.clone());
        registry.register("abc", 7, 1, waker.clone());

        let stats = registry.stats();
        assert_eq!(stats.keys, 1);
        assert_eq!(stats.wakers, 1);
    }

    #[test]
    fn unregister_stream_removes_stale_waiters() {
        let registry = PieceWaiterRegistry::new();
        let waker = noop_waker();

        registry.register("abc", 7, 1, waker.clone());
        registry.register("abc", 7, 2, waker.clone());
        registry.register("abc", 8, 1, waker);

        registry.unregister_stream(1);

        let stats = registry.stats();
        assert_eq!(stats.keys, 1);
        assert_eq!(stats.wakers, 1);
    }
}
