//! File stream implementation for libtorrent backend

use std::path::PathBuf;
use std::sync::Arc;

use super::helpers::read_piece_from_disk;
use crate::backend::priorities::{EngineCacheConfig, calculate_priorities};

pub(crate) struct LibtorrentFileStream {
    pub(crate) file: Option<tokio::fs::File>, // Made optional - file may not exist yet
    pub(crate) file_path: PathBuf,            // Path to file for lazy opening
    pub(crate) handle: libtorrent_sys::LibtorrentHandle,
    pub(crate) first_piece: i32,
    pub(crate) last_piece: i32,
    pub(crate) piece_length: u64,
    pub(crate) file_offset: u64,
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
    pub(crate) cached_piece_data: Option<(i32, Arc<Vec<u8>>)>, // (piece_idx, data)
    /// Last piece we triggered prefetch for (to avoid repeated requests)
    pub(crate) last_prefetch_piece: i32,
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

        if self.last_priorities_piece != -1 {
            let old_start = self.last_priorities_piece;
            // We clear a reasonably large window around the old position to ensure no dangling high priorities.
            // If the jump is small, we just clear the old window.
            // If the jump is large, we should probably clear the whole file's deadlines to be safe,
            // but only if we are the only stream. However, libtorrent handles multiple deadlines,
            // so clearing pieces we know we don't need anymore is always safe.

            let old_window_end = old_start + 250; // Increased to match new max window

            for p in old_start..=old_window_end {
                if p >= self.first_piece && p <= self.last_piece {
                    // Clear if it's outside the NEW window
                    // New window is roughly [current_piece, current_piece + 250]
                    let in_new_window = p >= current_piece && p <= current_piece + 250;
                    if !in_new_window {
                        self.handle.set_piece_deadline(p, 0);
                    }
                }
            }
        } else {
            // This is likely a SEEK or INITIAL start.
            // If we don't know the last window, but we want to be clean,
            // we could clear the entire file range.
            // However, to avoid breaking concurrent streams, we only clear if it's a "large" file
            // and we want to ensure any previous streaming by this same Handle is cleaned up.
            for p in self.first_piece..=self.last_piece {
                self.handle.set_piece_deadline(p, 0);
            }
        }

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
        let total_pieces = status.num_pieces;
        let current_speed = status.download_rate as f64;

        // Alpha of 0.2 means 20% weight to new sample, ~5 samples to converge
        if self.download_speed_ema == 0.0 {
            self.download_speed_ema = current_speed;
        } else {
            self.download_speed_ema = (self.download_speed_ema * 0.8) + (current_speed * 0.2);
        }

        let priorities = calculate_priorities(
            current_piece,
            total_pieces,
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
            if item.piece_idx <= self.last_piece && !self.handle.have_piece(item.piece_idx) {
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

        // Calculate offset within the piece (global piece offset)
        let global_piece_start = piece as u64 * self.piece_length;

        // For multi-file torrents: cached data may be a partial piece
        // The cached data starts at max(file_offset, global_piece_start) - global_piece_start
        // But wait, read_piece_from_disk returns data starting at the file beginning for pieces
        // that start before the file. So we need to calculate the offset into the cached data.
        //
        // If piece starts BEFORE file: cached data starts at offset 0 (file beginning)
        // If piece starts AFTER/AT file: cached data starts at the piece-relative offset
        let piece_starts_before_file = global_piece_start < self.file_offset;

        // offset_in_cached_data: where in the cached piece data do we read from
        let offset_in_cached_data = if piece_starts_before_file {
            // Piece spans from previous file into this one
            // The cached data represents the portion in this file (starts at offset 0 in cached data)
            // Our position in this portion is: (file_offset + pos) - max(file_offset, piece_start)
            pos as usize // pos is already file-relative
        } else {
            // Piece starts at or after the file start
            // Cached data represents the whole piece (or portion until next file)
            ((self.file_offset + pos) % self.piece_length) as usize
        };

        // MEMORY-FIRST READING: Check if we have this piece in our local cache
        if piece >= 0 {
            // Check if we already have the right piece cached locally
            let have_cached = match &self.cached_piece_data {
                Some((cached_piece, _)) => *cached_piece == piece,
                None => false,
            };

            if have_cached {
                // Serve from local cache - FASTEST PATH
                if let Some((_, data)) = &self.cached_piece_data {
                    let available = data.len().saturating_sub(offset_in_cached_data);
                    let to_read = buf.remaining().min(available);

                    if to_read > 0 {
                        buf.put_slice(
                            &data[offset_in_cached_data..offset_in_cached_data + to_read],
                        );
                        self.current_pos += to_read as u64;

                        if pos % (1024 * 1024) == 0 || pos < 4096 {
                            tracing::debug!(
                                "poll_read: Served {} bytes from MEMORY cache (piece {})",
                                to_read,
                                piece
                            );
                        }
                    }
                    return std::task::Poll::Ready(Ok(()));
                }
            }

            // Try to get from moka cache (check synchronously)
            let cache = self.piece_cache.clone();
            let info_hash = self.info_hash.clone();
            if let Some(piece_data) =
                futures::executor::block_on(cache.get_piece(&info_hash, piece))
            {
                // Cache hit! Store locally for fast repeated access
                self.cached_piece_data = Some((piece, piece_data.clone()));

                let available = piece_data.len().saturating_sub(offset_in_cached_data);
                let to_read = buf.remaining().min(available);

                if to_read > 0 {
                    buf.put_slice(
                        &piece_data[offset_in_cached_data..offset_in_cached_data + to_read],
                    );
                    self.current_pos += to_read as u64;

                    tracing::debug!(
                        "poll_read: Served {} bytes from MOKA cache (piece {})",
                        to_read,
                        piece
                    );
                }

                // === READ-AHEAD PREFETCH ===
                // Pre-fetch next 3 pieces into moka cache for fast access
                if piece != self.last_prefetch_piece {
                    self.last_prefetch_piece = piece;
                    let prefetch_cache = cache.clone();
                    let prefetch_info_hash = info_hash.clone();
                    let prefetch_handle = self.handle.clone();
                    let file_path = self.file_path.clone();
                    let piece_len = self.piece_length;
                    let file_offset = self.file_offset;
                    let last_piece = self.last_piece;

                    // Spawn background prefetch task
                    tokio::spawn(async move {
                        const PREFETCH_COUNT: i32 = 3;
                        for i in 1..=PREFETCH_COUNT {
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
                            if let Ok(data) =
                                read_piece_from_disk(&file_path, next_piece, piece_len, file_offset)
                                    .await
                            {
                                prefetch_cache
                                    .put_piece(&prefetch_info_hash, next_piece, data)
                                    .await;
                                tracing::debug!(
                                    "Read-ahead: prefetched piece {} into cache",
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
            if pos % (1024 * 1024) == 0 {
                tracing::debug!("poll_read: waiting for piece {} (pos={})", piece, pos);
            }

            // ADAPTIVE DELAY: Poll faster when download speed is high
            // At 10MB/s, a 4MB piece takes ~400ms - poll every 15-50ms
            let delay_ms = if self.download_speed_ema > 5_000_000.0 {
                15 // Fast download: check frequently (>5MB/s)
            } else if self.download_speed_ema > 1_000_000.0 {
                25 // Medium: moderate polling (1-5MB/s)
            } else {
                50 // Slow/starting: reduce CPU usage
            };

            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
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
        // If file is open, delegate to it
        if let Some(ref mut file) = self.file {
            std::pin::Pin::new(file).start_seek(position)
        } else {
            // File not open yet - just calculate the new position
            // We'll handle actual seeking when file opens
            let new_pos = match position {
                std::io::SeekFrom::Start(pos) => pos,
                std::io::SeekFrom::Current(delta) => (self.current_pos as i64 + delta) as u64,
                std::io::SeekFrom::End(_) => {
                    // Can't seek from end without file size
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Cannot seek from end without file",
                    ));
                }
            };
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

                    // Optimization: Only clear and reset priorities if we moved to a new piece window
                    if piece_idx != self.last_priorities_piece {
                        self.handle.clear_piece_deadlines();
                        self.last_priorities_piece = -1;
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
