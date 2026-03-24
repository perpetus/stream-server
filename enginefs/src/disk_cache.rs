//! Conditional disk caching for completed torrent files
//!
//! After streaming ends, completed files are persisted to disk only if
//! their size is within the cache limit. This keeps memory-first streaming
//! performance while providing disk caching for reasonable-sized files.

use std::path::PathBuf;

use crate::piece_cache::PieceCacheManager;
use tracing::{debug, info, warn};

/// Manages conditional disk caching for completed torrent files.
///
/// Files are only written to disk if their total size fits within `max_cache_bytes`.
pub struct DiskCacheManager {
    /// Root directory for cached files
    cache_dir: PathBuf,
    /// Maximum total cache size in bytes (from settings.cache_size)
    max_cache_bytes: u64,
}

impl DiskCacheManager {
    pub fn new(cache_dir: PathBuf, max_cache_bytes: u64) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        info!(
            "DiskCacheManager: cache_dir={:?}, max_cache_bytes={}",
            cache_dir, max_cache_bytes
        );
        Self {
            cache_dir,
            max_cache_bytes,
        }
    }

    /// Get the disk-cached path for a file, if it was previously cached
    pub fn get_cached_path(&self, info_hash: &str, file_name: &str) -> Option<PathBuf> {
        let path = self.cache_dir.join(info_hash).join(file_name);
        if path.exists() { Some(path) } else { None }
    }

    /// Attempt to persist a completed file from the in-memory piece cache to disk.
    ///
    /// Returns `Some(path)` if successfully cached, `None` if skipped (too large or incomplete).
    pub async fn maybe_persist_file(
        &self,
        info_hash: &str,
        file_name: &str,
        file_size: u64,
        file_offset: u64,
        piece_length: u64,
        first_piece: i32,
        last_piece: i32,
        piece_cache: &PieceCacheManager,
    ) -> Option<PathBuf> {
        // Skip if cache is disabled (max_cache_bytes == 0)
        if self.max_cache_bytes == 0 {
            debug!("DiskCache: Skipping persist (cache disabled)");
            return None;
        }

        // Skip if file is too large for the cache
        if file_size > self.max_cache_bytes {
            info!(
                "DiskCache: Skipping persist for {} ({} bytes > {} max)",
                file_name, file_size, self.max_cache_bytes
            );
            return None;
        }

        // Check current cache usage to avoid exceeding limit
        let current_usage = self.calculate_usage().await;
        if current_usage + file_size > self.max_cache_bytes {
            info!(
                "DiskCache: Skipping persist (current={}+file={} > max={})",
                current_usage, file_size, self.max_cache_bytes
            );
            return None;
        }

        // Reassemble the file from pieces in the moka cache
        let output_dir = self.cache_dir.join(info_hash);
        if let Err(e) = tokio::fs::create_dir_all(&output_dir).await {
            warn!("DiskCache: Failed to create dir {:?}: {}", output_dir, e);
            return None;
        }

        let output_path = output_dir.join(file_name);

        // Collect all pieces for this file
        let mut file_data = Vec::with_capacity(file_size as usize);
        let mut bytes_written = 0u64;

        for piece_idx in first_piece..=last_piece {
            let piece_data = match piece_cache.get_piece(info_hash, piece_idx).await {
                Some(data) => data,
                None => {
                    warn!(
                        "DiskCache: Missing piece {} for {} — file incomplete, aborting persist",
                        piece_idx, info_hash
                    );
                    return None;
                }
            };

            // For the first piece, skip bytes before file_offset
            let piece_start = piece_idx as u64 * piece_length;
            let skip = if piece_start < file_offset {
                (file_offset - piece_start) as usize
            } else {
                0
            };

            // For the last piece, don't write beyond file end
            let remaining = (file_size - bytes_written) as usize;
            let usable = piece_data.len().saturating_sub(skip).min(remaining);

            if usable > 0 {
                file_data.extend_from_slice(&piece_data[skip..skip + usable]);
                bytes_written += usable as u64;
            }
        }

        if bytes_written != file_size {
            warn!(
                "DiskCache: Size mismatch: wrote {} bytes, expected {} for {}",
                bytes_written, file_size, file_name
            );
            return None;
        }

        // Write to disk
        match tokio::fs::write(&output_path, &file_data).await {
            Ok(_) => {
                info!(
                    "DiskCache: Persisted {} ({} bytes) to {:?}",
                    file_name, file_size, output_path
                );
                Some(output_path)
            }
            Err(e) => {
                warn!("DiskCache: Failed to write {:?}: {}", output_path, e);
                None
            }
        }
    }

    /// Calculate total bytes used by the disk cache
    async fn calculate_usage(&self) -> u64 {
        let mut total = 0u64;
        if let Ok(mut entries) = tokio::fs::read_dir(&self.cache_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Ok(meta) = entry.metadata().await {
                    if meta.is_dir() {
                        // Walk subdirectory
                        if let Ok(mut sub_entries) = tokio::fs::read_dir(entry.path()).await {
                            while let Ok(Some(sub_entry)) = sub_entries.next_entry().await {
                                if let Ok(sub_meta) = sub_entry.metadata().await {
                                    if sub_meta.is_file() {
                                        total += sub_meta.len();
                                    }
                                }
                            }
                        }
                    } else if meta.is_file() {
                        total += meta.len();
                    }
                }
            }
        }
        total
    }
}
