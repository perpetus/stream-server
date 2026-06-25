//! Per-torrent registry of "metadata-critical" pieces (MKV Cues / MP4 moov).
//!
//! Container seek indexes frequently live at the *end* of the file, far from
//! the playback head. A player cannot start until it has read them, so when a
//! tail-seek (`ContainerMetadata`) request — or the background metadata
//! inspector — locates those pieces, they are pinned here. Concurrent head
//! streams read this registry and rank the pinned pieces ABOVE their own
//! read-ahead (re-asserting priority 7 + an immediate deadline after the
//! `set_file_priority` that would otherwise reset them), so the rare tail wins
//! the few peers that hold it and playback can start quickly.
//!
//! Pins are dropped once their pieces verify; see `unpin_pieces`.

use parking_lot::RwLock;
use std::collections::HashMap;

/// Registry of metadata-critical (Cues/moov) piece indices per torrent.
pub struct MetadataPinRegistry {
    /// info_hash (lowercase) -> pinned piece indices still needing the swarm.
    pins: RwLock<HashMap<String, Vec<i32>>>,
}

impl MetadataPinRegistry {
    pub fn new() -> Self {
        Self {
            pins: RwLock::new(HashMap::new()),
        }
    }

    /// Pin pieces as metadata-critical for a torrent, merging with any existing
    /// pins. Duplicates are ignored; an empty input never creates an entry.
    pub fn pin_pieces(&self, info_hash: &str, pieces: impl IntoIterator<Item = i32>) {
        let pieces: Vec<i32> = pieces.into_iter().collect();
        if pieces.is_empty() {
            return;
        }
        let key = info_hash.to_lowercase();
        let mut map = self.pins.write();
        let entry = map.entry(key).or_default();
        for p in pieces {
            if !entry.contains(&p) {
                entry.push(p);
            }
        }
    }

    /// Currently pinned pieces for a torrent (empty if none).
    pub fn pinned(&self, info_hash: &str) -> Vec<i32> {
        let key = info_hash.to_lowercase();
        self.pins.read().get(&key).cloned().unwrap_or_default()
    }

    /// Remove pieces that have verified; drop the torrent entry when nothing
    /// remains pinned.
    pub fn unpin_pieces(&self, info_hash: &str, pieces: &[i32]) {
        if pieces.is_empty() {
            return;
        }
        let key = info_hash.to_lowercase();
        let mut map = self.pins.write();
        if let Some(entry) = map.get_mut(&key) {
            entry.retain(|p| !pieces.contains(p));
            if entry.is_empty() {
                map.remove(&key);
            }
        }
    }

    /// Drop all pins for a torrent (e.g. on removal).
    #[allow(dead_code)]
    pub fn clear_torrent(&self, info_hash: &str) {
        let key = info_hash.to_lowercase();
        self.pins.write().remove(&key);
    }
}

impl Default for MetadataPinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::MetadataPinRegistry;

    #[test]
    fn pin_and_unpin_round_trip() {
        let reg = MetadataPinRegistry::new();
        reg.pin_pieces("ABC", [5994, 5995, 5994]); // duplicate ignored
        let mut pinned = reg.pinned("abc"); // case-insensitive
        pinned.sort_unstable();
        assert_eq!(pinned, vec![5994, 5995]);

        reg.unpin_pieces("abc", &[5994]);
        assert_eq!(reg.pinned("abc"), vec![5995]);

        // Unpinning the last piece drops the entry entirely.
        reg.unpin_pieces("abc", &[5995]);
        assert!(reg.pinned("abc").is_empty());
    }
}
