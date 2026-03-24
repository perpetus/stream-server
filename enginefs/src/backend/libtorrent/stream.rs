//! File stream implementation for libtorrent backend

use std::sync::Arc;
use std::time::Instant;

use crate::backend::priorities::{EngineCacheConfig, calculate_priorities, container_metadata_start};

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
    pub(crate) handle: libtorrent_sys::LibtorrentHandle,
    pub(crate) first_piece: i32,
    pub(crate) last_piece: i32,
    pub(crate) piece_length: u64,
    pub(crate) file_offset: u64,
    pub(crate) current_pos: u64,
    pub(crate) is_complete: bool,
    pub(crate) last_priorities_piece: i32,
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
    pub(crate) requested_piece_via_api: std::collections::HashMap<i32, Instant>,
    /// Registry of wakers waiting for pieces to finish downloading
    pub(crate) piece_waiter: Arc<crate::piece_waiter::PieceWaiterRegistry>,
    /// Current seek type for DETERMINISTIC priority handling
    pub(crate) seek_type: SeekType,
    /// File size for container metadata detection
    pub(crate) file_size: u64,
    /// Stream creation time for startup instrumentation
    pub(crate) created_at: Instant,
    /// Whether we already logged the first successful read
    pub(crate) first_read_logged: bool,
    /// Whether we already logged the first startup wait
    pub(crate) first_wait_logged: bool,
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

        // Calculate which piece we need
        let piece = if self.piece_length > 0 {
            ((self.file_offset + pos) / self.piece_length) as i32
        } else {
            -1
        };

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
                    let offset_in_cached = ((self.file_offset + pos) % self.piece_length) as usize;
                    let available = data.len().saturating_sub(offset_in_cached);
                    let to_read = buf.remaining().min(available);

                    if to_read > 0 {
                        buf.put_slice(&data[offset_in_cached..offset_in_cached + to_read]);
                        self.current_pos += to_read as u64;
                        if !self.first_read_logged {
                            self.first_read_logged = true;
                            tracing::info!(
                                "startup: first direct-stream bytes ready after {:?} (piece={}, source=local-cache)",
                                self.created_at.elapsed(),
                                piece
                            );
                        }

                        if pos % (1024 * 1024) == 0 || pos < 4096 {
                            tracing::debug!(
                                "poll_read: Served {} bytes from MEMORY cache (piece {}, offset_in_cached={})",
                                to_read,
                                piece,
                                offset_in_cached
                            );
                        }
                    }
                    self.requested_piece_via_api.remove(&piece);
                    return std::task::Poll::Ready(Ok(()));
                }
            }

            // Try to get from moka cache (check synchronously)
            if let Some(piece_data) =
                futures::executor::block_on(self.piece_cache.get_piece(&self.info_hash, piece))
            {
                self.requested_piece_via_api.remove(&piece);
                let offset_in_cached = ((self.file_offset + pos) % self.piece_length) as usize;
                self.cached_piece_data = Some((piece, piece_data.clone(), 0));

                let available = piece_data.len().saturating_sub(offset_in_cached);
                let to_read = buf.remaining().min(available);

                if to_read > 0 {
                    buf.put_slice(&piece_data[offset_in_cached..offset_in_cached + to_read]);
                    self.current_pos += to_read as u64;
                    if !self.first_read_logged {
                        self.first_read_logged = true;
                        tracing::info!(
                            "startup: first direct-stream bytes ready after {:?} (piece={}, source=piece-cache)",
                            self.created_at.elapsed(),
                            piece
                        );
                    }

                    tracing::debug!(
                        "poll_read: Served {} bytes from MOKA cache (piece {}, offset_in_cached={})",
                        to_read,
                        piece,
                        offset_in_cached
                    );
                }

                // === READ-AHEAD PREFETCH (memory-only) ===
                if piece != self.last_prefetch_piece {
                    self.last_prefetch_piece = piece;
                    let prefetch_cache = self.piece_cache.clone();
                    let prefetch_info_hash = self.info_hash.clone();
                    let prefetch_handle = self.handle.clone();
                    let last_piece = self.last_piece;

                    // ADAPTIVE PREFETCH COUNT
                    let prefetch_count: i32 = if self.download_speed_ema > 10_000_000.0 {
                        8
                    } else if self.download_speed_ema > 5_000_000.0 {
                        5
                    } else if self.download_speed_ema > 1_000_000.0 {
                        3
                    } else {
                        2
                    };

                    // Spawn background prefetch task (memory-only: read directly from storage)
                    tokio::spawn(async move {
                        for i in 1..=prefetch_count {
                            let next_piece = piece + i;
                            if next_piece > last_piece {
                                break;
                            }
                            if prefetch_cache
                                .has_piece(&prefetch_info_hash, next_piece)
                                .await
                            {
                                continue;
                            }
                            if !prefetch_handle.have_piece(next_piece) {
                                continue;
                            }
                            // Read directly from memory storage (no libtorrent read_piece)
                            let data = libtorrent_sys::memory_read_piece_direct(next_piece);
                            if !data.is_empty() {
                                prefetch_cache.put_piece(&prefetch_info_hash, next_piece, data).await;
                                tracing::debug!(
                                    "Read-ahead: cached piece {} directly from memory",
                                    next_piece
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
            // NOTIFICATION-BASED WAITING
            self.piece_waiter
                .register(&self.info_hash, piece, cx.waker().clone());

            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                waker.wake();
            });

            if pos % (1024 * 1024) == 0 {
                let status = self.handle.status();
                tracing::info!(
                    "poll_read: WAITING for piece {} (pos={}, peers={}, speed={:.1}MB/s, paused={}, finished={})",
                    piece,
                    pos,
                    status.num_peers,
                    status.download_rate as f64 / 1_000_000.0,
                    status.is_paused,
                    status.is_finished
                );
            }
            if pos == 0 && !self.first_wait_logged {
                self.first_wait_logged = true;
                let status = self.handle.status();
                tracing::info!(
                    "startup: waiting for first playable piece {} after {:?} (peers={}, paused={}, finished={})",
                    piece,
                    self.created_at.elapsed(),
                    status.num_peers,
                    status.is_paused,
                    status.is_finished
                );
            }

            return std::task::Poll::Pending;
        }

        // Piece is downloaded but not in cache — read directly from memory storage
        if piece >= 0 && !self.requested_piece_via_api.contains_key(&piece) {
            let piece_data = libtorrent_sys::memory_read_piece_direct(piece);
            if !piece_data.is_empty() {
                // Got data directly! Cache it and serve immediately on next poll.
                let info_hash = self.info_hash.clone();
                let cache = self.piece_cache.clone();
                let waiter = self.piece_waiter.clone();
                self.requested_piece_via_api.insert(piece, Instant::now());
                tracing::info!(
                    "poll_read: Direct read piece {} from memory storage ({} bytes)",
                    piece,
                    piece_data.len()
                );
                tokio::spawn(async move {
                    cache.put_piece(&info_hash, piece, piece_data).await;
                    waiter.notify_piece_finished(&info_hash, piece);
                });
            } else {
                tracing::debug!(
                    "poll_read: piece {} downloaded but not yet in memory storage",
                    piece,
                );
                self.piece_waiter
                    .register(&self.info_hash, piece, cx.waker().clone());
            }

            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                waker.wake();
            });
            return std::task::Poll::Pending;
        }

        // Piece was requested, waiting for cache to be populated
        if piece >= 0 && self.requested_piece_via_api.contains_key(&piece) {
            let should_rerequest = self
                .requested_piece_via_api
                .get(&piece)
                .map(|requested_at| requested_at.elapsed() > std::time::Duration::from_millis(250))
                .unwrap_or(false);
            if should_rerequest {
                tracing::warn!(
                    "poll_read: piece {} still missing from cache after 250ms, re-reading from memory",
                    piece
                );
                let piece_data = libtorrent_sys::memory_read_piece_direct(piece);
                if !piece_data.is_empty() {
                    let info_hash = self.info_hash.clone();
                    let cache = self.piece_cache.clone();
                    let waiter = self.piece_waiter.clone();
                    tokio::spawn(async move {
                        cache.put_piece(&info_hash, piece, piece_data).await;
                        waiter.notify_piece_finished(&info_hash, piece);
                    });
                }
                self.requested_piece_via_api.insert(piece, Instant::now());
            }
            self.piece_waiter
                .register(&self.info_hash, piece, cx.waker().clone());
            tracing::trace!(
                "poll_read: MEMORY-ONLY waiting for piece {} in cache (have_piece={})",
                piece,
                self.handle.have_piece(piece)
            );
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                waker.wake();
            });
            return std::task::Poll::Pending;
        }

        // Should not reach here
        tracing::error!("poll_read: Unexpected state - piece={}, pos={}", piece, pos);
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Memory-only streaming: unexpected state in poll_read",
        )))
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
            std::io::SeekFrom::End(delta) => (self.file_size as i64 + delta).max(0) as u64,
        };

        // DETERMINISTIC: Detect seek type based on target position
        let end_threshold = container_metadata_start(self.file_size);

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

        // Memory-only mode: just update position, no file handle to seek
        self.current_pos = new_pos;
        // Invalidate local cached piece data since position changed
        self.cached_piece_data = None;
        Ok(())
    }

    fn poll_complete(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        // Memory-only mode: position is already set in start_seek
        let pos = self.current_pos;

        let piece_idx = if self.piece_length > 0 {
            ((self.file_offset + pos) / self.piece_length) as i32
        } else {
            -1
        };

        if piece_idx != self.last_priorities_piece {
            self.set_priorities(pos);
        }

        std::task::Poll::Ready(Ok(pos))
    }
}
