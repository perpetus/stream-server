//! Metadata cache for video file Cues/index information
//!
//! Caches duration and keyframe positions extracted from MKV Cues
//! to enable instant playback on subsequent requests.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Cached metadata for a video file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFileMetadata {
    /// Video duration in milliseconds (if known)
    pub duration_ms: Option<u64>,
    /// Byte offset of Cues/index in the file
    pub cues_offset: Option<u64>,
    /// Keyframe byte offsets for seek prioritization
    pub keyframe_offsets: Vec<u64>,
    /// File size (for validation)
    pub file_size: u64,
}

impl CachedFileMetadata {
    pub fn new(file_size: u64) -> Self {
        Self {
            duration_ms: None,
            cues_offset: None,
            keyframe_offsets: Vec::new(),
            file_size,
        }
    }
}

/// Key for the metadata cache: (info_hash, file_index)
pub type MetadataCacheKey = (String, usize);

/// Thread-safe metadata cache using std RwLock + HashMap
/// Simple implementation without external dependencies
pub struct MetadataCache {
    cache: RwLock<HashMap<MetadataCacheKey, Arc<CachedFileMetadata>>>,
}

impl MetadataCache {
    /// Create a new metadata cache
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Get cached metadata for a file
    pub fn get(&self, info_hash: &str, file_index: usize) -> Option<Arc<CachedFileMetadata>> {
        let key = (info_hash.to_string(), file_index);
        let cache = self.cache.read().ok()?;
        cache.get(&key).cloned()
    }

    /// Store metadata for a file
    pub fn insert(&self, info_hash: &str, file_index: usize, metadata: CachedFileMetadata) {
        let key = (info_hash.to_string(), file_index);
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(key, Arc::new(metadata));
        }
    }

    /// Check if metadata exists for a file
    pub fn contains(&self, info_hash: &str, file_index: usize) -> bool {
        let key = (info_hash.to_string(), file_index);
        self.cache
            .read()
            .map(|c| c.contains_key(&key))
            .unwrap_or(false)
    }

    /// Update just the keyframe offsets for a file
    pub fn update_keyframes(&self, info_hash: &str, file_index: usize, keyframes: Vec<u64>) {
        let key = (info_hash.to_string(), file_index);
        if let Ok(mut cache) = self.cache.write() {
            if let Some(existing) = cache.get(&key) {
                let mut updated = (**existing).clone();
                updated.keyframe_offsets = keyframes;
                cache.insert(key, Arc::new(updated));
            }
        }
    }

    /// Update Cues offset for a file
    pub fn update_cues_offset(&self, info_hash: &str, file_index: usize, offset: u64) {
        let key = (info_hash.to_string(), file_index);
        if let Ok(mut cache) = self.cache.write() {
            if let Some(existing) = cache.get(&key) {
                let mut updated = (**existing).clone();
                updated.cues_offset = Some(offset);
                cache.insert(key, Arc::new(updated));
            } else {
                // Create new entry with just cues offset
                let mut metadata = CachedFileMetadata::new(0);
                metadata.cues_offset = Some(offset);
                cache.insert(key, Arc::new(metadata));
            }
        }
    }
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}

// Global singleton for the metadata cache using OnceLock
static METADATA_CACHE: std::sync::OnceLock<MetadataCache> = std::sync::OnceLock::new();

/// Get the global metadata cache instance
pub fn get_metadata_cache() -> &'static MetadataCache {
    METADATA_CACHE.get_or_init(MetadataCache::new)
}
