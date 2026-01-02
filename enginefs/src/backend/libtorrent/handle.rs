//! Torrent handle implementation for libtorrent backend

use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::backend::{
    BackendFileInfo, EngineStats, FileStreamTrait, TorrentHandle as TorrentHandleTrait,
    metadata::MetadataInspector,
};
use libtorrent_sys::LibtorrentSession;

use super::helpers::{default_stats, make_engine_stats};
use super::stream::LibtorrentFileStream;

/// Handle to a torrent managed by libtorrent
#[derive(Clone)]
pub struct LibtorrentTorrentHandle {
    pub(crate) session: Arc<RwLock<LibtorrentSession>>,
    pub(crate) info_hash: String,
    pub(crate) save_path: PathBuf,
    pub(crate) config: crate::backend::BackendConfig,
    pub(crate) stream_counter: Arc<std::sync::atomic::AtomicUsize>,
    /// In-memory piece cache for fast streaming
    pub(crate) piece_cache: Arc<crate::piece_cache::PieceCacheManager>,
    /// Registry of wakers waiting for pieces to finish downloading
    pub(crate) piece_waiter: Arc<crate::piece_waiter::PieceWaiterRegistry>,
}

#[async_trait::async_trait]
impl TorrentHandleTrait for LibtorrentTorrentHandle {
    fn info_hash(&self) -> String {
        self.info_hash.clone()
    }

    fn name(&self) -> Option<String> {
        // We need to query the session to get the name
        // This is a sync operation wrapped in a blocking task
        let session = self.session.blocking_read();
        match session.find_torrent(&self.info_hash) {
            Ok(handle) => {
                let name = handle.name();
                if name.is_empty() { None } else { Some(name) }
            }
            Err(_) => None,
        }
    }

    async fn stats(&self) -> EngineStats {
        let session = self.session.read().await;

        let handle = match session.find_torrent(&self.info_hash) {
            Ok(h) => h,
            Err(_) => return default_stats(&self.info_hash),
        };

        let status = handle.status();
        let mut stats = make_engine_stats(&status);
        let piece_length = handle.piece_length() as u64;

        // Populate files from the handle
        let files = handle.files();
        let mut current_offset = 0u64;

        stats.files = files
            .iter()
            .map(|f| {
                let file_offset = current_offset;
                current_offset += f.size as u64;

                // Calculate downloaded based on pieces we have (more accurate for streaming)
                // file_progress() returns 0 for files with priority 0 or when streaming
                let downloaded = if f.downloaded > 0 {
                    f.downloaded as u64
                } else if piece_length > 0 {
                    // Count pieces we have in this file's range
                    let mut piece_bytes = 0u64;
                    for piece in f.first_piece..=f.last_piece {
                        if handle.have_piece(piece) {
                            piece_bytes += piece_length;
                        }
                    }
                    // Cap at file size (last piece may be partial)
                    piece_bytes.min(f.size as u64)
                } else {
                    0
                };

                crate::backend::StatsFile {
                    name: f.path.to_string(),
                    path: f.path.to_string(),
                    length: f.size as u64,
                    offset: file_offset,
                    downloaded,
                    // Use C++ calculated progress which comes from file_progress()
                    progress: f.progress as f64,
                }
            })
            .collect();

        stats
    }

    async fn add_trackers(&self, trackers: Vec<String>) -> Result<()> {
        let session = self.session.read().await;
        let mut handle = session
            .find_torrent(&self.info_hash)
            .map_err(|e| anyhow!("Torrent not found: {}", e))?;

        // Add trackers with tier based on position (faster trackers first get lower tier = higher priority)
        for (idx, tracker) in trackers.iter().enumerate() {
            handle.add_tracker(tracker, idx as i32);
        }
        Ok(())
    }

    async fn get_file_reader(
        &self,
        file_idx: usize,
        start_offset: u64,
        priority: u8,
        bitrate: Option<u64>,
    ) -> Result<Box<dyn FileStreamTrait>> {
        tracing::debug!("get_file_reader: starting for file {}", file_idx);
        let session = self.session.read().await;
        let mut handle = session
            .find_torrent(&self.info_hash)
            .map_err(|e| anyhow!("Torrent not found: {}", e))?;

        let files = handle.files();
        let file_info = files
            .get(file_idx)
            .ok_or_else(|| anyhow!("File index {} out of range", file_idx))?;

        let first_piece = file_info.first_piece;
        let last_piece = file_info.last_piece;
        let piece_length = handle.piece_length() as u64;
        let global_file_offset = file_info.offset as u64;

        // Check if file is already complete by checking pieces in its range
        let mut is_complete = true;
        for p in first_piece..=last_piece {
            if !handle.have_piece(p) {
                is_complete = false;
                break;
            }
        }

        tracing::debug!(
            "get_file_reader: file {} is_complete={}",
            file_idx,
            is_complete
        );

        // CRITICAL: Resume torrent if paused!
        // Torrent may have been paused after being marked "finished" when all files had priority 0
        let status = handle.status();
        if status.is_paused {
            tracing::info!("get_file_reader: Resuming paused torrent for streaming");
            handle.resume();
        }

        // Set file priorities: Requested file = 4, Others = 0 (Skip)
        // This ensures all bandwidth goes to the stream
        let all_files = handle.files();
        for (idx, f) in all_files.iter().enumerate() {
            if idx == file_idx {
                tracing::debug!(
                    "get_file_reader: Setting PRIORITY 4 (NORMAL) for file idx={} name={}",
                    idx,
                    f.path
                );
                handle.set_file_priority(idx as i32, 4);
            } else {
                tracing::debug!(
                    "get_file_reader: Setting PRIORITY 0 (SKIP) for file idx={} name={}",
                    idx,
                    f.path
                );
                handle.set_file_priority(idx as i32, 0);
            }
        }

        let actual_start_piece: i32;

        // PRIORITY 255 = Internal reader (e.g., metadata inspection)
        // These should NOT modify piece priorities as they would conflict with playback
        let skip_prioritization = priority == 255 || is_complete;

        // File size is needed for both prioritization and seek type detection
        let file_size = file_info.size as u64;

        // =========================================================================
        // SEEK TYPE DETECTION with Priority Bands
        // =========================================================================
        // | Band     | Deadline   | Use Case                                     |
        // |----------|------------|----------------------------------------------|
        // | URGENT   | 0-200ms    | Initial playback (first request, offset=0)  |
        // | CRITICAL | 300-500ms  | User-initiated seeks (scrubbing)            |
        // | NORMAL   | 1000-1200ms| Prefetch/buffer expansion                   |
        // | DEFERRED | 2000-3000ms| Container metadata (moov/Cues at end)       |
        // =========================================================================

        #[derive(Debug, Clone, Copy)]
        enum SeekType {
            InitialPlayback,   // offset=0, first request
            ContainerMetadata, // near end of file, seeking for moov/Cues
            UserScrub,         // user is seeking mid-video
        }

        let seek_type = {
            if start_offset == 0 {
                SeekType::InitialPlayback
            } else {
                // Near end of file = container metadata (last 10MB or last 5%)
                let end_threshold = file_size
                    .saturating_sub(10 * 1024 * 1024)
                    .max(file_size * 95 / 100);
                if start_offset >= end_threshold {
                    SeekType::ContainerMetadata
                } else {
                    SeekType::UserScrub
                }
            }
        };

        if !skip_prioritization {
            // Calculate actual start piece
            actual_start_piece = ((global_file_offset + start_offset) / piece_length) as i32;

            // CRITICAL FIX: Only clear deadlines for non-container-metadata requests
            // Container metadata requests should ADD priorities, not replace them
            // This prevents wiping out head piece priorities (piece 0-7) which are
            // essential for playback to start
            if !matches!(seek_type, SeekType::ContainerMetadata) {
                handle.clear_piece_deadlines();
            }

            // Get download speed for dynamic adjustment (0 if unknown)
            let download_speed = handle.status().download_rate as u64; // bytes/sec

            // Dynamic deadline multiplier based on download speed
            // Faster downloads = tighter deadlines, slower = more slack
            let speed_factor = if download_speed > 5_000_000 {
                0.5 // Fast (>5MB/s): halve deadlines
            } else if download_speed > 1_000_000 {
                1.0 // Normal (1-5MB/s): standard deadlines
            } else if download_speed > 100_000 {
                1.5 // Slow (100KB-1MB/s): 1.5x deadlines
            } else {
                2.0 // Very slow (<100KB/s): double deadlines
            };

            // Base deadlines and window sizes per seek type
            // AGGRESSIVE FOCUS: Keep window sizes small to concentrate bandwidth
            let (base_deadline, window_size, label) = match seek_type {
                SeekType::InitialPlayback => {
                    // URGENT: deadline=0 on just 5 pieces max for immediate playback
                    (0, 5, "URGENT")
                }
                SeekType::UserScrub => {
                    // CRITICAL: 300ms for user seeks, small window
                    (300, 5, "CRITICAL")
                }
                SeekType::ContainerMetadata => {
                    // Container metadata - lower priority, small window
                    (1000, 3, "CONTAINER-INDEX")
                }
            };

            // Apply dynamic speed factor to base deadline (except URGENT which stays at 0)
            let adjusted_deadline = if base_deadline == 0 {
                0
            } else {
                (base_deadline as f64 * speed_factor) as i32
            };

            tracing::info!(
                "get_file_reader: {} - {} pieces from piece {} (deadlines {}ms+, speed={:.1}MB/s)",
                label,
                window_size,
                actual_start_piece,
                adjusted_deadline,
                download_speed as f64 / 1_000_000.0
            );

            // Set piece PRIORITY and DEADLINE with staircase pattern
            // CRITICAL: Both are needed! Priority 7 = highest, deadline = time constraint
            for i in 0..window_size {
                let p = actual_start_piece + i;
                if p <= last_piece {
                    handle.set_piece_priority(p, 7); // Highest priority - ESSENTIAL for download
                    let deadline = adjusted_deadline + i * 10;
                    handle.set_piece_deadline(p, deadline);
                }
            }

            // For initial playback, also prioritize container index at end
            // Use DEFERRED deadline so it doesn't compete with playback
            if matches!(seek_type, SeekType::InitialPlayback) {
                if last_piece > actual_start_piece + window_size {
                    tracing::debug!(
                        "get_file_reader: Also prioritizing last 2 pieces for container index (DEFERRED)"
                    );
                    handle.set_piece_deadline(last_piece, 2000);
                    if last_piece > 0 {
                        handle.set_piece_deadline(last_piece - 1, 2000);
                    }
                }
            }

            // HEAD PIECE PROTECTION: Only for InitialPlayback and ContainerMetadata
            // - InitialPlayback: Already prioritizes head pieces, ensures staircase order
            // - ContainerMetadata: Seeking to end for moov/Cues, but still needs head for playback
            // - UserScrub: Does NOT need head pieces - user is playing from a different position
            //   The player already has container info cached from previous requests
            if matches!(
                seek_type,
                SeekType::InitialPlayback | SeekType::ContainerMetadata
            ) {
                for i in 0..8 {
                    let p = first_piece + i;
                    if p <= last_piece && !handle.have_piece(p) {
                        handle.set_piece_priority(p, 7); // ESSENTIAL - without this pieces won't download
                        // Use staircase: 0, 10, 20... ms to maintain order
                        // For ContainerMetadata: always set URGENT deadlines
                        // For InitialPlayback: only set if not already at head (avoids double-setting)
                        if matches!(seek_type, SeekType::ContainerMetadata)
                            || actual_start_piece != first_piece
                        {
                            handle.set_piece_deadline(p, (i as i32) * 10);
                        }
                    }
                }
            }
        } else {
            // Skip prioritization for internal readers or complete files
            actual_start_piece = ((global_file_offset + start_offset) / piece_length) as i32;
            tracing::debug!(
                "get_file_reader: Skipping prioritization (priority={}, is_complete={})",
                priority,
                is_complete
            );
        }

        // Get file path (file may not exist yet for incomplete torrents)
        let file_path = PathBuf::from(&file_info.absolute_path);

        // Try to open file if it exists - don't block waiting for it
        // poll_read will handle lazy opening
        let opened_file = if file_path.exists() {
            tracing::debug!("get_file_reader: opening file {:?}", file_path);
            tokio::fs::File::open(&file_path).await.ok()
        } else {
            tracing::debug!(
                "get_file_reader: file does not exist yet, will open lazily: {:?}",
                file_path
            );
            None
        };

        tracing::debug!(
            "get_file_reader: Calculated global offset for file {}: {}",
            file_idx,
            global_file_offset
        );

        let stream_id = self
            .stream_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Create mmap for completed files for zero-copy reads
        let mmap = if is_complete && file_path.exists() {
            std::fs::File::open(&file_path)
                .and_then(|f| unsafe { memmap2::Mmap::map(&f) })
                .map(|m| {
                    tracing::info!(
                        "get_file_reader: Using mmap for completed file (size={})",
                        m.len()
                    );
                    m
                })
                .ok()
        } else {
            None
        };

        // Map local SeekType to stream's SeekType for deterministic handling
        let initial_seek_type = match seek_type {
            SeekType::InitialPlayback => super::stream::SeekType::InitialPlayback,
            SeekType::ContainerMetadata => super::stream::SeekType::ContainerMetadata,
            SeekType::UserScrub => super::stream::SeekType::UserScrub,
        };

        Ok(Box::new(LibtorrentFileStream {
            file: opened_file,
            file_path,
            handle: handle.clone(),
            first_piece,
            last_piece,
            piece_length,
            file_offset: global_file_offset, // Use the true global offset
            current_pos: 0,
            is_complete, // Use actual completion status
            last_priorities_piece: if !is_complete { actual_start_piece } else { -1 },
            cache_config: self.config.cache.clone(),
            priority,
            bitrate,
            download_speed_ema: 0.0,
            stream_id,
            piece_cache: self.piece_cache.clone(),
            info_hash: self.info_hash.clone(),
            cached_piece_data: None,
            last_prefetch_piece: -1,
            requested_piece_via_api: std::collections::HashSet::new(),
            mmap,
            piece_waiter: self.piece_waiter.clone(),
            seek_type: initial_seek_type,
            file_size,
        }))
    }

    async fn get_files(&self) -> Vec<BackendFileInfo> {
        // First check if metadata is already available (fast path)
        {
            let session = self.session.read().await;
            if let Ok(handle) = session.find_torrent(&self.info_hash) {
                if handle.status().has_metadata {
                    tracing::debug!("get_files: metadata already available (fast path)");
                    return handle
                        .files()
                        .iter()
                        .map(|f| BackendFileInfo {
                            name: f.path.to_string(),
                            length: f.size as u64,
                        })
                        .collect();
                }
            }
        }

        tracing::debug!("get_files: waiting for metadata...");

        // Wait for metadata if not yet available with ADAPTIVE POLLING (up to 30 seconds)
        let metadata_start = std::time::Instant::now();
        let mut poll_interval_ms = 10u64;
        loop {
            if metadata_start.elapsed().as_secs() >= 30 {
                tracing::warn!("get_files: Timeout waiting for metadata after 30s");
                return vec![];
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(poll_interval_ms)).await;
            // Adaptive backoff: 10 -> 20 -> 40 -> 80 -> 100 (max)
            poll_interval_ms = (poll_interval_ms * 2).min(100);

            let session = self.session.read().await;
            if let Ok(handle) = session.find_torrent(&self.info_hash) {
                if handle.status().has_metadata {
                    tracing::info!(
                        "get_files: Metadata acquired in {:?}",
                        metadata_start.elapsed()
                    );
                    return handle
                        .files()
                        .iter()
                        .map(|f| BackendFileInfo {
                            name: f.path.to_string(),
                            length: f.size as u64,
                        })
                        .collect();
                }
            }
        }
    }

    async fn get_file_path(&self, file_idx: usize) -> Option<String> {
        let session = self.session.read().await;
        if let Ok(handle) = session.find_torrent(&self.info_hash) {
            let files = handle.files();
            if let Some(file_info) = files.get(file_idx) {
                // Construct full path: save_path + file.path
                let full_path = self.save_path.join(&file_info.path);
                return Some(full_path.to_string_lossy().to_string());
            }
        }
        None
    }

    async fn prepare_file_for_streaming(&self, file_idx: usize) -> anyhow::Result<()> {
        let overall_start = std::time::Instant::now();
        tracing::info!(
            "prepare_file_for_streaming: Preparing file {} for streaming",
            file_idx
        );

        // Phase 1: Wait for metadata with ADAPTIVE POLLING
        // Start fast (10ms), increase to 100ms max - reduces latency when metadata arrives quickly
        let metadata_start = std::time::Instant::now();
        let mut poll_interval_ms = 10u64;
        loop {
            let session = self.session.read().await;
            if let Ok(handle) = session.find_torrent(&self.info_hash) {
                if handle.status().has_metadata {
                    tracing::info!(
                        "prepare_file_for_streaming: Metadata acquired in {:?}",
                        metadata_start.elapsed()
                    );
                    break;
                }
            }
            drop(session);

            if metadata_start.elapsed().as_secs() >= 30 {
                return Err(anyhow::anyhow!(
                    "Timeout waiting for torrent metadata (30s)"
                ));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(poll_interval_ms)).await;
            // Adaptive backoff: 10 -> 20 -> 40 -> 80 -> 100 (max)
            poll_interval_ms = (poll_interval_ms * 2).min(100);
        }

        // Phase 2: Set file priorities and reactive metadata inspection
        let (first_piece, last_piece, piece_length, file_offset, file_length, name) = {
            let session = self.session.read().await;
            let mut handle = session
                .find_torrent(&self.info_hash)
                .map_err(|e| anyhow::anyhow!("Torrent not found: {}", e))?;

            let files = handle.files();
            let file_info = files
                .get(file_idx)
                .ok_or_else(|| anyhow::anyhow!("File index {} out of range", file_idx))?;

            let first_piece = file_info.first_piece;
            let last_piece = file_info.last_piece;
            let piece_length = handle.piece_length();
            let file_offset = file_info.offset;
            let file_length = file_info.size; // Fixed: .size instead of .length
            let name = file_info.path.clone(); // Fixed: .path instead of .name

            tracing::info!(
                "prepare_file_for_streaming: File {} spans pieces {}-{} (piece_length={}, offset={})",
                file_idx,
                first_piece,
                last_piece,
                piece_length,
                file_offset
            );

            // MULTI-FILE FIX: Clear ALL piece deadlines before setting new ones
            // This prevents stale deadlines from previous file streams from
            // competing for bandwidth with the current stream
            handle.clear_piece_deadlines();

            // Set file priorities: target file = 1, all others = 0 (skip)
            for (idx, _) in files.iter().enumerate() {
                if idx == file_idx {
                    handle.set_file_priority(idx as i32, 4); // Normal priority - let piece deadlines drive urgency
                } else {
                    handle.set_file_priority(idx as i32, 0);
                }
            }

            // REMOVED: First-8-pieces prioritization was causing seeks to fail
            // because it set highest priority on pieces 0-7 even when seeking to piece 118.
            // Prioritization now happens ONLY in get_file_reader() which knows the actual offset.
            //
            // SAFETY NET: Set low-urgency deadlines on first 8 pieces as fallback.
            // These will be overridden by get_file_reader() with proper URGENT (0ms) deadlines.
            // But if there's any delay before get_file_reader() runs, this ensures SOMETHING
            // starts downloading immediately after file switch (fixes 95%+ download issue).
            for i in 0..8 {
                let p = first_piece + i;
                if p <= last_piece && !handle.have_piece(p) {
                    handle.set_piece_priority(p, 7); // Highest priority
                    handle.set_piece_deadline(p, 5000); // 5 second fallback deadline
                }
            }
            let _total_pieces = (last_piece - first_piece + 1) as i32;

            // PRE-WARM CACHE: If first 8 pieces are already complete, request them via read_piece
            // This populates moka cache immediately for zero-latency first read
            // Only prewarm if they're already available (don't wait for download)
            let prewarm_count = 8;
            let is_prewarm_complete =
                (0..prewarm_count).all(|i| handle.have_piece(first_piece + i));
            if is_prewarm_complete {
                tracing::info!(
                    "prepare_file_for_streaming: File head complete, pre-warming {} pieces",
                    prewarm_count
                );
                for i in 0..prewarm_count {
                    let _ = handle.read_piece(first_piece + i);
                }
            }

            // DO NOT prioritize pieces here - let get_file_reader() handle it
            // since it knows the actual seek offset

            tracing::info!(
                "prepare_file_for_streaming: Ready (prioritization deferred to get_file_reader)",
            );

            (
                first_piece,
                last_piece,
                piece_length,
                file_offset,
                file_length,
                name,
            )
        }; // session lock released here

        // Perform reactive metadata inspection in BACKGROUND
        // DEFERRED: Wait 500ms before starting to give initial playback pieces a head start
        // This ensures bandwidth is focused on pieces 0-2 before we start fetching moov/Cues
        let this = self.clone();
        tokio::spawn(async move {
            // CRITICAL: Delay metadata inspection to not compete with startup pieces
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            tracing::info!(
                "Background Metadata Inspection: Starting for file {} (deferred 500ms)",
                file_idx
            );

            // This will find 'moov' atoms (MP4) or index areas (MKV) and prioritize them.
            if let Ok(mut reader) = this.get_file_reader(file_idx, 0, 255, None).await {
                let critical_ranges =
                    MetadataInspector::find_critical_ranges(&mut reader, file_length as u64, &name)
                        .await;

                if !critical_ranges.is_empty() {
                    let session = this.session.read().await;
                    if let Ok(mut handle) = session.find_torrent(&this.info_hash) {
                        for (offset, len) in &critical_ranges {
                            let start_piece =
                                ((file_offset as u64 + offset) / piece_length as u64) as i32;
                            let end_piece = ((file_offset as u64 + offset + len.saturating_sub(1))
                                / piece_length as u64)
                                as i32;

                            for p in start_piece..=end_piece {
                                if p >= first_piece && p <= last_piece {
                                    // Downgrade metadata priority to 3000ms to let head pieces (0-200ms) win
                                    handle.set_piece_deadline(p, 3000);
                                }
                            }
                        }
                        tracing::info!(
                            "Background Metadata Inspection: Prioritized {} critical metadata ranges (Deadline 3000ms)",
                            critical_ranges.len()
                        );
                    }
                }
            }

            // Phase 2.5: Find and prioritize actual keyframe positions from container index
            // This reads Cues (MKV) or stss (MP4) to find where video keyframes are located
            if let Ok(mut reader2) = this.get_file_reader(file_idx, 0, 255, None).await {
                let keyframe_offsets = MetadataInspector::find_keyframe_offsets(
                    &mut reader2,
                    file_length as u64,
                    &name,
                )
                .await;

                if !keyframe_offsets.is_empty() {
                    let session = this.session.read().await;
                    if let Ok(mut handle) = session.find_torrent(&this.info_hash) {
                        let mut keyframe_pieces = Vec::new();

                        // Prioritize pieces containing the first 20 keyframes
                        for offset in keyframe_offsets.iter().take(20) {
                            let piece =
                                ((file_offset as u64 + offset) / piece_length as u64) as i32;
                            if piece >= first_piece
                                && piece <= last_piece
                                && !keyframe_pieces.contains(&piece)
                            {
                                // Downgrade keyframe priority to 3000ms
                                handle.set_piece_deadline(piece, 3000);
                                keyframe_pieces.push(piece);
                            }
                        }

                        tracing::info!(
                            "Background Metadata Inspection: Prioritized {} pieces containing keyframes (Deadline 3000ms)",
                            keyframe_pieces.len()
                        );
                    }
                }
            }
        });

        tracing::info!(
            "prepare_file_for_streaming: Ready for playback (non-blocking) - Total setup time: {:?}",
            overall_start.elapsed()
        );
        Ok(())
    }

    async fn clear_file_streaming(&self, file_idx: usize) -> anyhow::Result<()> {
        let session = self.session.read().await;
        let mut handle = session
            .find_torrent(&self.info_hash)
            .map_err(|e| anyhow!("Torrent not found: {}", e))?;

        // Set file priority to 0 (skip) - no more pieces will be downloaded for this file
        handle.set_file_priority(file_idx as i32, 0);

        // Clear all piece deadlines to stop prioritizing this file's pieces
        handle.clear_piece_deadlines();

        tracing::info!(
            "clear_file_streaming: Cleared streaming state for file {} in {}",
            file_idx,
            self.info_hash
        );

        Ok(())
    }
}
