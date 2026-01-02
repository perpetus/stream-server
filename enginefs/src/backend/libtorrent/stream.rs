//! File stream implementation for libtorrent backend

use std::path::PathBuf;
use std::sync::Arc;

use super::helpers::read_piece_from_disk;
use crate::backend::priorities::{EngineCacheConfig, calculate_priorities};

/// Type of seek operation - determines priority behavior
/// Used for DETERMINISTIC seek detection instead of heuristic piece jumps
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SeekType {
    /// Normal sequential reading (no seek)
    Sequential,
    /// Initial playback (offset=0 on first request)
    InitialPlayback,
    /// User scrubbing to a new position
    UserScrub,
    /// Container metadata read (moov, Cues at end of file)
    ContainerMetadata,
}

pub(crate) struct LibtorrentFileStream {
    pub(crate) file: Option<tokio::fs::File>, // Made optional - file may not exist yet
    pub(crate) file_path: PathBuf,            // Path to file for lazy opening
    pub(crate) handle: libtorrent_sys::LibtorrentHandle,
    pub(crate) first_piece: i32,
    pub(crate) last_piece: i32,
    pub(crate) piece_length: u64,
    pub(crate) file_offset: u64,
    // For multi-file torrents:
    // We used to calculate file-relative offsets here, but now we enforce FULL pieces in cache.
    // The offset logic has been simplified to: (file_offset + pos) % piece_length
    pub(crate) current_pos: u64,
    pub(crate) is_complete: bool,
    pub(crate) last_priorities_piece: i32, // Track last piece we set priorities for
    pub(crate) cache_config: EngineCacheConfig,
    pub(crate) priority: u8,
    pub(crate) bitrate: Option<u64>,
    pub(crate) download_speed_ema: f64,
    pub(crate) stream_id: usize,
    /// In-memory piece cache for fast streaming
    pub(crate) piece_cache: Arc<crate::piece_cache::PieceCacheManager>,
    /// Info hash for cache lookups
    pub(crate) info_hash: String,
    /// Currently cached piece data for fast serving
    /// Tuple: (piece_idx, data, file_relative_start)
    pub(crate) cached_piece_data: Option<(i32, Arc<Vec<u8>>, u64)>,
    /// Last piece we triggered prefetch for (to avoid repeated requests)
    pub(crate) last_prefetch_piece: i32,
    /// Track pieces we've requested via read_piece() API to avoid duplicate requests
    pub(crate) requested_piece_via_api: std::collections::HashSet<i32>,
    /// Memory-mapped file for zero-copy reads on completed files
    pub(crate) mmap: Option<memmap2::Mmap>,
    /// Registry of wakers waiting for pieces to finish downloading
    pub(crate) piece_waiter: Arc<crate::piece_waiter::PieceWaiterRegistry>,
    /// Current seek type for DETERMINISTIC priority handling
    pub(crate) seek_type: SeekType,
    /// File size for container metadata detection
    pub(crate) file_size: u64,
}

impl LibtorrentFileStream {
    fn set_priorities(&mut self, pos: u64) {
        // Skip if already complete
        if self.is_complete {
            return;
        }

        if self.piece_length == 0 {
            return;
        }

        // Correct calculation: file_offset is now the TRUE global byte offset of the file start.
        // pos is relative to file start.
        // So (file_offset + pos) is the global byte offset in the torrent.
        let current_piece = ((self.file_offset + pos) / self.piece_length) as i32;

        // Efficient cache check: if we are on the same piece, do nothing
        if current_piece == self.last_priorities_piece {
            return;
        }

        // DETERMINISTIC SEEK HANDLING: Use tracked SeekType instead of piece-jump heuristics
        match self.seek_type {
            SeekType::Sequential | SeekType::InitialPlayback => {
                // Sequential read or initial playback - just extend window, no cleanup
                tracing::trace!(
                    "set_priorities: {:?} at piece {} - extending window",
                    self.seek_type,
                    current_piece
                );
            }
            SeekType::ContainerMetadata => {
                // Container metadata read - ADD priorities, but preserve head pieces
                // This is critical: don't wipe out piece 0-7 priorities when reading moov/Cues
                tracing::debug!(
                    "set_priorities: ContainerMetadata at piece {} - preserving head priorities",
                    current_piece
                );
                // Don't clear deadlines - this is the key fix for container metadata!
            }
            SeekType::UserScrub => {
                // User scrub - full reset for new playback position
                tracing::debug!(
                    "set_priorities: UserScrub to piece {} - resetting all priorities",
                    current_piece
                );
                self.handle.clear_piece_deadlines();
            }
        }

        // After handling the seek, reset to sequential for subsequent reads
        self.seek_type = SeekType::Sequential;

        self.last_priorities_piece = current_piece;

        // Check if complete (all pieces downloaded)
        let mut all_downloaded = true;
        for p in self.first_piece..=self.last_piece {
            if !self.handle.have_piece(p) {
                all_downloaded = false;
                break;
            }
        }
        if all_downloaded {
            self.is_complete = true;
            return;
        }

        // Use centralized priorities calculation
        // Calculate dynamic EMA for download speed to avoid priority oscillations
        let status = self.handle.status();
        let _total_pieces = status.num_pieces; // Unused, kept for potential future use
        let current_speed = status.download_rate as f64;

        // Alpha of 0.2 means 20% weight to new sample, ~5 samples to converge
        if self.download_speed_ema == 0.0 {
            self.download_speed_ema = current_speed;
        } else {
            self.download_speed_ema = (self.download_speed_ema * 0.8) + (current_speed * 0.2);
        }

        let priorities = calculate_priorities(
            current_piece,
            self.last_piece + 1, // Use file's piece range, not torrent total
            self.piece_length,
            &self.cache_config,
            self.priority,
            self.download_speed_ema as u64,
            self.bitrate,
        );

        // Apply fair-sharing jitter (Shuffle Mirroring)
        // Adding a small unique offset to each stream's deadlines ensures that
        // when multiple streams are active, their "earliest" pieces are interleaved.
        let jitter = (self.stream_id % 10) as i32 * 5; // Up to 50ms jitter

        for item in priorities {
            if item.piece_idx >= self.first_piece
                && item.piece_idx <= self.last_piece
                && !self.handle.have_piece(item.piece_idx)
            {
                let shared_deadline = if item.deadline == 0 {
                    0
                } else if item.deadline >= 100000 {
                    // Don't jitter very long background deadlines
                    item.deadline
                } else {
                    item.deadline + jitter
                };
                self.handle
                    .set_piece_deadline(item.piece_idx, shared_deadline);
            }
        }
    }
}

impl tokio::io::AsyncRead for LibtorrentFileStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let pos = self.current_pos;
        self.set_priorities(pos);

        // MMAP FAST PATH: If we have mmap, serve directly from memory (zero-copy)
        if let Some(ref mmap) = self.mmap {
            let offset = pos as usize;
            let available = mmap.len().saturating_sub(offset);
            let to_read = buf.remaining().min(available);

            if to_read > 0 {
                buf.put_slice(&mmap[offset..offset + to_read]);
                self.current_pos += to_read as u64;

                if pos == 0 || pos % (10 * 1024 * 1024) == 0 {
                    tracing::debug!(
                        "poll_read: Served {} bytes via MMAP (zero-copy, pos={})",
                        to_read,
                        pos
                    );
                }
            }
            return std::task::Poll::Ready(Ok(()));
        }

        // Calculate which piece we need
        let piece = if self.piece_length > 0 {
            ((self.file_offset + pos) / self.piece_length) as i32
        } else {
            -1
        };

        // For multi-file torrents:
        // We used to calculate file-relative offsets here, but now we enforce FULL pieces in cache.
        // The offset logic has been simplified to: (file_offset + pos) % piece_length

        // MEMORY-FIRST READING: Check if we have this piece in our local cache
        if piece >= 0 {
            // Check if we already have the right piece cached locally
            let have_cached = match &self.cached_piece_data {
                Some((cached_piece, _, _)) => *cached_piece == piece,
                None => false,
            };

            if have_cached {
                // Serve from local cache - FASTEST PATH
                if let Some((_, data, _)) = &self.cached_piece_data {
                    // Calculate offset into cached data: Global pos % piece_length
                    // We assume cached data is always FULL piece (aligned to global piece start)
                    let offset_in_cached = ((self.file_offset + pos) % self.piece_length) as usize;
                    let available = data.len().saturating_sub(offset_in_cached);
                    let to_read = buf.remaining().min(available);

                    if to_read > 0 {
                        buf.put_slice(&data[offset_in_cached..offset_in_cached + to_read]);
                        self.current_pos += to_read as u64;

                        if pos % (1024 * 1024) == 0 || pos < 4096 {
                            tracing::debug!(
                                "poll_read: Served {} bytes from MEMORY cache (piece {}, offset_in_cached={})",
                                to_read,
                                piece,
                                offset_in_cached
                            );
                        }
                    }
                    return std::task::Poll::Ready(Ok(()));
                }
            }

            // Try to get from moka cache (check synchronously)
            // OPTIMIZATION: Use reference directly, avoid clone for lookup
            if let Some(piece_data) =
                futures::executor::block_on(self.piece_cache.get_piece(&self.info_hash, piece))
            {
                // CRITICAL FIX: Ensure correct offset for Multi-File Torrents
                // We now ensure (in helpers.rs) that only FULL pieces are cached.
                // This means the cache data ALWAYS starts at piece_global_start.
                //
                // Global pos = file_offset + pos
                // Offset in piece = Global pos % piece_length
                let offset_in_cached = ((self.file_offset + pos) % self.piece_length) as usize;

                // Store locally. We use 0 for file_relative_start because we calculate
                // offset dynamically now, but we keep the field to satisfy the type.
                self.cached_piece_data = Some((piece, piece_data.clone(), 0));

                let available = piece_data.len().saturating_sub(offset_in_cached);
                let to_read = buf.remaining().min(available);

                if to_read > 0 {
                    buf.put_slice(&piece_data[offset_in_cached..offset_in_cached + to_read]);
                    self.current_pos += to_read as u64;

                    tracing::debug!(
                        "poll_read: Served {} bytes from MOKA cache (piece {}, offset_in_cached={})",
                        to_read,
                        piece,
                        offset_in_cached
                    );
                }

                // === READ-AHEAD PREFETCH ===
                // ADAPTIVE: Prefetch count based on download speed
                // Fast downloads = more prefetch, slow = less to avoid wasting cache
                if piece != self.last_prefetch_piece {
                    self.last_prefetch_piece = piece;
                    // Clone only what's needed for the async prefetch task
                    let prefetch_cache = self.piece_cache.clone();
                    let prefetch_info_hash = self.info_hash.clone();
                    let prefetch_handle = self.handle.clone();
                    let file_path = self.file_path.clone();
                    let piece_len = self.piece_length;
                    let file_offset = self.file_offset;
                    let last_piece = self.last_piece;

                    // ADAPTIVE PREFETCH COUNT:
                    // >10 MB/s = 8 pieces, >5 MB/s = 5 pieces, >1 MB/s = 3 pieces, else 2
                    let prefetch_count: i32 = if self.download_speed_ema > 10_000_000.0 {
                        8 // Very fast: prefetch 8 pieces (~32MB at 4MB pieces)
                    } else if self.download_speed_ema > 5_000_000.0 {
                        5 // Fast: prefetch 5 pieces
                    } else if self.download_speed_ema > 1_000_000.0 {
                        3 // Medium: prefetch 3 pieces
                    } else {
                        2 // Slow/starting: minimal prefetch
                    };

                    // Spawn background prefetch task
                    tokio::spawn(async move {
                        for i in 1..=prefetch_count {
                            let next_piece = piece + i;
                            if next_piece > last_piece {
                                break;
                            }

                            // Check if already in cache
                            if prefetch_cache
                                .has_piece(&prefetch_info_hash, next_piece)
                                .await
                            {
                                continue;
                            }

                            // Check if piece is downloaded
                            if !prefetch_handle.have_piece(next_piece) {
                                continue;
                            }

                            // Read piece from disk into cache
                            if let Ok(cached_data) =
                                read_piece_from_disk(&file_path, next_piece, piece_len, file_offset)
                                    .await
                            {
                                prefetch_cache
                                    .put_piece(&prefetch_info_hash, next_piece, cached_data.data)
                                    .await;
                                tracing::debug!(
                                    "Read-ahead: prefetched piece {} into cache (file_rel_start={})",
                                    next_piece,
                                    cached_data.file_relative_start
                                );
                            }
                        }
                    });
                }

                return std::task::Poll::Ready(Ok(()));
            }
        }

        // Not in cache - check if piece is available in libtorrent
        if piece >= 0 && !self.handle.have_piece(piece) {
            // NOTIFICATION-BASED WAITING: Register waker to be notified when piece finishes
            // This is more efficient than polling - we get woken immediately when
            // piece_finished_alert fires instead of polling every 5ms
            self.piece_waiter
                .register(&self.info_hash, piece, cx.waker().clone());

            // FALLBACK TIMEOUT: Also schedule a timeout wake in case notification is missed
            // (e.g., race condition where piece finishes between have_piece and register)
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                waker.wake();
            });

            // Log download status periodically to help debug stalled downloads
            if pos % (1024 * 1024) == 0 {
                let status = self.handle.status();
                tracing::debug!(
                    "poll_read: WAITING for piece {} (pos={}, peers={}, speed={:.1}MB/s)",
                    piece,
                    pos,
                    status.num_peers,
                    status.download_rate as f64 / 1_000_000.0
                );
            }

            return std::task::Poll::Pending;
        }

        // MEMORY-FIRST OPTIMIZATION: Piece is available on disk but not in our cache
        // Request it via read_piece() API - libtorrent may have it in ITS memory cache
        // This is faster than disk I/O on our side
        if piece >= 0 && !self.requested_piece_via_api.contains(&piece) {
            let _ = self.handle.read_piece(piece);
            self.requested_piece_via_api.insert(piece);
            tracing::trace!(
                "poll_read: Requested piece {} via read_piece API (memory-first)",
                piece
            );

            // Wait briefly for the alert handler to populate our cache
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                waker.wake();
            });
            return std::task::Poll::Pending;
        }

        // Ensure file is open (lazy opening)
        if self.file.is_none() {
            if self.file_path.exists() {
                match std::fs::File::open(&self.file_path) {
                    Ok(std_file) => {
                        self.file = Some(tokio::fs::File::from_std(std_file));
                        tracing::debug!("poll_read: lazily opened file {:?}", self.file_path);
                    }
                    Err(e) => {
                        tracing::debug!("poll_read: failed to open file: {}", e);
                    }
                }
            }
        }

        // If file is still not open, wait for it
        let file = match self.file.as_mut() {
            Some(f) => f,
            None => {
                tracing::debug!("poll_read: file not yet available, waiting...");
                let waker = cx.waker().clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    waker.wake();
                });
                return std::task::Poll::Pending;
            }
        };

        // Read from disk
        let rem_before = buf.remaining();
        match std::pin::Pin::new(file).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let read = rem_before - buf.remaining();
                if read > 0 {
                    self.current_pos += read as u64;

                    if pos % (1024 * 1024) == 0 || pos < 4096 {
                        tracing::debug!(
                            "poll_read: Read {} bytes from DISK. Pos={}",
                            read,
                            self.current_pos
                        );
                    }
                }
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Err(e)) => {
                tracing::error!("poll_read: Error reading file: {}", e);
                std::task::Poll::Ready(Err(e))
            }
        }
    }
}

impl tokio::io::AsyncSeek for LibtorrentFileStream {
    fn start_seek(
        mut self: std::pin::Pin<&mut Self>,
        position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        // Calculate target position
        let new_pos = match position {
            std::io::SeekFrom::Start(pos) => pos,
            std::io::SeekFrom::Current(delta) => (self.current_pos as i64 + delta).max(0) as u64,
            std::io::SeekFrom::End(delta) => {
                // Now we have file_size, we can handle End seeks
                (self.file_size as i64 + delta).max(0) as u64
            }
        };

        // DETERMINISTIC: Detect seek type based on target position
        let end_threshold = self
            .file_size
            .saturating_sub(10 * 1024 * 1024)
            .max(self.file_size * 95 / 100);

        self.seek_type = if new_pos >= end_threshold {
            SeekType::ContainerMetadata
        } else {
            SeekType::UserScrub
        };

        tracing::debug!(
            "start_seek: {} -> {} ({:?})",
            self.current_pos,
            new_pos,
            self.seek_type
        );

        // If file is open, delegate to it
        if let Some(ref mut file) = self.file {
            std::pin::Pin::new(file).start_seek(position)
        } else {
            // File not open yet - just update position
            self.current_pos = new_pos;
            Ok(())
        }
    }

    fn poll_complete(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        // If file is open, delegate to it
        if let Some(ref mut file) = self.file {
            match std::pin::Pin::new(file).poll_complete(cx) {
                std::task::Poll::Ready(Ok(pos)) => {
                    self.current_pos = pos;

                    let piece_idx = if self.piece_length > 0 {
                        ((self.file_offset + pos) / self.piece_length) as i32
                    } else {
                        -1
                    };

                    // Let set_priorities handle priority changes based on SeekType
                    // Don't clear deadlines here - set_priorities does it deterministically
                    if piece_idx != self.last_priorities_piece {
                        self.set_priorities(pos);
                    }
                    std::task::Poll::Ready(Ok(pos))
                }
                other => other,
            }
        } else {
            // File not open - return current position
            std::task::Poll::Ready(Ok(self.current_pos))
        }
    }
}
