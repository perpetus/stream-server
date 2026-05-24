//! Torrent handle implementation for libtorrent backend

use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::backend::{
    BackendFileInfo, EngineStats, FileStreamTrait, PieceReadiness,
    TorrentHandle as TorrentHandleTrait,
    metadata::MetadataInspector,
    priorities::{
        MAX_STARTUP_PIECES, MemoryPressure, PlaybackIntent, PlaybackPriorityPolicy,
        PriorityContext, disk_backed_sequential_download,
    },
};
use libtorrent_sys::LibtorrentSession;

use super::LibtorrentStorageMode;
use super::disk_stream::LibtorrentDiskFileStream;
use super::helpers::{default_stats, make_engine_stats};
use super::stream::LibtorrentFileStream;

/// Handle to a torrent managed by libtorrent
#[derive(Clone)]
pub struct LibtorrentTorrentHandle {
    pub(crate) session: Arc<RwLock<LibtorrentSession>>,
    pub(crate) info_hash: String,
    pub(crate) save_path: PathBuf,
    pub(crate) config: crate::backend::BackendConfig,
    pub(crate) storage_mode: LibtorrentStorageMode,
    pub(crate) stream_counter: Arc<std::sync::atomic::AtomicUsize>,
    /// In-memory piece cache for fast streaming
    pub(crate) piece_cache: Arc<crate::piece_cache::PieceCacheManager>,
    /// Registry of wakers waiting for pieces to finish downloading
    pub(crate) piece_waiter: Arc<crate::piece_waiter::PieceWaiterRegistry>,
    /// Torrent/file metadata inspections already scheduled for this backend lifetime
    pub(crate) metadata_inspections:
        Arc<tokio::sync::Mutex<std::collections::HashSet<(String, usize)>>>,
    /// Torrents whose file priorities have been initialized once metadata is known.
    pub(crate) file_priority_initializations:
        Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
}

impl LibtorrentTorrentHandle {
    fn label_memory_storage(&self) {
        if matches!(self.storage_mode, LibtorrentStorageMode::MemoryOnly) {
            libtorrent_sys::memory_label_last_unlabeled_storage(&self.info_hash);
        }
    }

    async fn initialize_file_priorities_if_needed(
        &self,
        handle: &mut libtorrent_sys::LibtorrentHandle,
    ) {
        let should_initialize = {
            let mut initialized = self.file_priority_initializations.lock().await;
            initialized.insert(self.info_hash.clone())
        };
        if !should_initialize {
            return;
        }

        let files = handle.files();
        for (idx, file) in files.iter().enumerate() {
            handle.set_file_priority(idx as i32, 0);
            tracing::debug!(
                info_hash = %self.info_hash,
                file_idx = idx,
                file_path = %file.path,
                "file priority initialized to skip"
            );
        }
        tracing::info!(
            info_hash = %self.info_hash,
            file_count = files.len(),
            "torrent file priorities initialized after metadata"
        );
    }
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
        self.label_memory_storage();

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
        intent: PlaybackIntent,
    ) -> Result<Box<dyn FileStreamTrait>> {
        tracing::debug!("get_file_reader: starting for file {}", file_idx);
        let session = self.session.read().await;
        let mut handle = session
            .find_torrent(&self.info_hash)
            .map_err(|e| anyhow!("Torrent not found: {}", e))?;
        self.label_memory_storage();

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
            // Force reannounce to quickly re-acquire peers after pause
            handle.force_reannounce();
            handle.force_dht_announce();
        }
        if priority != 255 {
            // Real playback/download readers should make their file wanted, but
            // must not clear other wanted files. Several HTTP requests or
            // torrents can be active at the same time.
            tracing::debug!(
                "get_file_reader: Setting PRIORITY 7 (HIGHEST) for file idx={} name={}",
                file_idx,
                file_info.path
            );
            handle.set_file_priority(file_idx as i32, 7);
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
            } else if matches!(intent, PlaybackIntent::ContainerMetadata) {
                SeekType::ContainerMetadata
            } else {
                SeekType::UserScrub
            }
        };

        let playback_intent = if priority == 255 {
            PlaybackIntent::InternalProbe
        } else {
            match intent {
                PlaybackIntent::DownloadFull | PlaybackIntent::DownloadRange => intent,
                PlaybackIntent::ContainerMetadata => PlaybackIntent::ContainerMetadata,
                PlaybackIntent::Background => PlaybackIntent::Background,
                PlaybackIntent::InternalProbe => PlaybackIntent::InternalProbe,
                PlaybackIntent::HlsInitial
                | PlaybackIntent::HlsSequential
                | PlaybackIntent::HlsSeek => {
                    if start_offset == 0 {
                        PlaybackIntent::HlsInitial
                    } else {
                        PlaybackIntent::HlsSeek
                    }
                }
                _ => {
                    if start_offset == 0 {
                        PlaybackIntent::DirectInitial
                    } else {
                        PlaybackIntent::DirectSeek
                    }
                }
            }
        };

        if priority != 255 {
            let sequential_download = disk_backed_sequential_download(playback_intent);
            handle.set_sequential_download(sequential_download);
            tracing::info!(
                intent = ?playback_intent,
                start_offset,
                sequential_download,
                storage_mode = ?self.storage_mode,
                "streaming sequential mode configured"
            );
        }

        if !skip_prioritization {
            // Calculate actual start piece
            actual_start_piece = ((global_file_offset + start_offset) / piece_length) as i32;

            if matches!(playback_intent, PlaybackIntent::DownloadFull)
                || (matches!(self.storage_mode, LibtorrentStorageMode::MemoryOnly)
                    && matches!(
                        playback_intent,
                        PlaybackIntent::DirectInitial
                            | PlaybackIntent::DirectSeek
                            | PlaybackIntent::DirectSequential
                            | PlaybackIntent::HlsInitial
                            | PlaybackIntent::HlsSeek
                            | PlaybackIntent::HlsSequential
                    ))
            {
                handle.clear_piece_deadlines();
                tracing::info!(
                    intent = ?playback_intent,
                    current_piece = actual_start_piece,
                    "streaming deadlines reset for playback target"
                );
            }

            let status = handle.status();
            let download_speed = status.download_rate as u64;
            let peers = status.num_peers as u64;
            let native_memory = libtorrent_sys::memory_storage_stats();
            let memory_pressure = if self.config.cache.size > 0
                && native_memory.total_bytes >= self.config.cache.size.saturating_mul(80) / 100
            {
                MemoryPressure::High
            } else {
                MemoryPressure::Normal
            };

            let decision = PlaybackPriorityPolicy::decide(PriorityContext {
                intent: playback_intent,
                current_piece: actual_start_piece,
                first_piece,
                last_piece,
                piece_length,
                file_size,
                bitrate_bytes_per_sec: bitrate,
                download_rate_bytes_per_sec: download_speed,
                peers,
                cache_size_bytes: self.config.cache.size,
                memory_pressure,
                consecutive_waits: 0,
                first_byte_sent: false,
            });
            let applied_window_size = decision.target_window_pieces;

            tracing::info!(
                intent = ?playback_intent,
                current_piece = actual_start_piece,
                hot_window = decision.hot_window_pieces,
                warm_window = decision.warm_window_pieces,
                immediate_pieces = decision.immediate_pieces,
                peers,
                speed_mb_s = download_speed as f64 / 1_000_000.0,
                memory_pressure = ?memory_pressure,
                reason = %decision.reason,
                "priority_decision get_file_reader"
            );
            tracing::info!(
                "get_file_reader: {:?} - {} pieces from piece {} (speed={:.1}MB/s)",
                playback_intent,
                decision.target_window_pieces,
                actual_start_piece,
                download_speed as f64 / 1_000_000.0
            );

            // Set piece PRIORITY and DEADLINE with staircase pattern
            // CRITICAL: Both are needed! Priority 7 = highest, deadline = time constraint
            for assignment in decision.assignments {
                let p = assignment.piece_idx;
                if p <= last_piece {
                    handle.set_piece_priority(p, assignment.piece_priority);
                    handle.set_piece_deadline(p, assignment.deadline);
                }
            }

            if matches!(self.storage_mode, LibtorrentStorageMode::DiskBacked) {
                tracing::info!(
                    info_hash = %self.info_hash,
                    file_idx,
                    intent = ?playback_intent,
                    current_piece = actual_start_piece,
                    sequential_download = disk_backed_sequential_download(playback_intent),
                    hot_window = decision.hot_window_pieces,
                    warm_window = decision.warm_window_pieces,
                    immediate_pieces = decision.immediate_pieces,
                    reason = %decision.reason,
                    "disk-backed priority configured"
                );
            }

            // PRE-REQUEST first piece via read_piece API if already downloaded
            // If the piece is already downloaded, read_piece() will load it from
            // memory storage into the cache. If NOT downloaded, skip - the
            // piece_finished_alert handler will call read_piece() when it's ready.
            if matches!(self.storage_mode, LibtorrentStorageMode::MemoryOnly)
                && handle.have_piece(actual_start_piece)
            {
                let _ = handle.read_piece(actual_start_piece);
            }

            // For initial playback, tail metadata should only start after the head window
            // is already in flight so the first frame is not delayed by end-of-file work.
            if matches!(self.storage_mode, LibtorrentStorageMode::MemoryOnly)
                && matches!(seek_type, SeekType::InitialPlayback)
            {
                if last_piece > actual_start_piece + applied_window_size {
                    tracing::debug!(
                        "get_file_reader: Deferring tail metadata deadlines until after startup"
                    );
                    handle.set_piece_deadline(last_piece, 1_200);
                    if last_piece > 0 {
                        handle.set_piece_deadline(last_piece - 1, 1_250);
                    }
                }
            }

            // HEAD PIECE PROTECTION: Only for InitialPlayback
            // - InitialPlayback: Already prioritizes head pieces, ensures staircase order
            // - UserScrub: Does NOT need head pieces - user is playing from a different position
            //   The player already has container info cached from previous requests
            if matches!(seek_type, SeekType::InitialPlayback) {
                for i in 0..MAX_STARTUP_PIECES {
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

        let stream_id = self
            .stream_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if matches!(self.storage_mode, LibtorrentStorageMode::DiskBacked) {
            return Ok(Box::new(LibtorrentDiskFileStream::new(
                handle.clone(),
                self.info_hash.clone(),
                self.save_path.join(&file_info.path),
                file_info.path.clone(),
                first_piece,
                last_piece,
                piece_length,
                global_file_offset,
                file_size,
                file_idx,
                stream_id,
                playback_intent,
                self.piece_waiter.clone(),
            )));
        }

        // Memory-only mode: no disk files to open or mmap

        // Map local SeekType to stream's SeekType for deterministic handling
        let initial_seek_type = match seek_type {
            SeekType::InitialPlayback => super::stream::SeekType::InitialPlayback,
            SeekType::ContainerMetadata => super::stream::SeekType::ContainerMetadata,
            SeekType::UserScrub => super::stream::SeekType::UserScrub,
        };

        Ok(Box::new(LibtorrentFileStream {
            handle: handle.clone(),
            first_piece,
            last_piece,
            piece_length,
            file_offset: global_file_offset,
            current_pos: 0,
            is_complete,
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
            last_served_replan_piece: -1,
            requested_piece_via_api: std::collections::HashMap::new(),
            piece_waiter: self.piece_waiter.clone(),
            seek_type: initial_seek_type,
            playback_intent,
            file_size,
            created_at: std::time::Instant::now(),
            first_read_logged: false,
            first_wait_logged: false,
            last_wait_log: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(5))
                .unwrap_or_else(std::time::Instant::now),
            last_retry_wake: std::time::Instant::now(),
            last_blocking_piece: -1,
            last_blocking_priority: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now),
            consecutive_waits: 0,
        }))
    }

    async fn get_files(&self) -> Vec<BackendFileInfo> {
        // First check if metadata is already available (fast path)
        {
            let session = self.session.read().await;
            if let Ok(mut handle) = session.find_torrent(&self.info_hash) {
                self.label_memory_storage();
                if handle.status().has_metadata {
                    self.initialize_file_priorities_if_needed(&mut handle).await;
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
            if let Ok(mut handle) = session.find_torrent(&self.info_hash) {
                self.label_memory_storage();
                if handle.status().has_metadata {
                    self.initialize_file_priorities_if_needed(&mut handle).await;
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
                if full_path.is_file() {
                    return Some(full_path.to_string_lossy().to_string());
                }
                tracing::debug!(
                    "get_file_path: No on-disk file available for {} (memory-only mode)",
                    full_path.display()
                );
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
                self.label_memory_storage();
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
        let (
            first_piece,
            last_piece,
            piece_length,
            file_offset,
            file_length,
            name,
            needs_end_metadata,
        ) = {
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

            // Do not clear piece deadlines here. Direct players can open several
            // overlapping ranges for the same file; clearing deadlines for each
            // range can starve the active playback piece. File switches and
            // delayed cleanup still clear deadlines explicitly.

            // Make the target file wanted without clearing other active files.
            // Files are initialized as skipped when metadata arrives and are
            // cleared by delayed cleanup after their own streams end.
            handle.set_file_priority(file_idx as i32, 7);

            // REMOVED: First-8-pieces prioritization was causing seeks to fail
            // because it set highest priority on pieces 0-7 even when seeking to piece 118.
            // Prioritization now happens ONLY in get_file_reader() which knows the actual offset.
            //
            // SAFETY NET: Set low-urgency deadlines on the first few pieces as fallback.
            // These will be overridden by get_file_reader() with proper URGENT (0ms) deadlines.
            // But if there's any delay before get_file_reader() runs, this ensures SOMETHING
            // starts downloading immediately after file switch (fixes 95%+ download issue).
            for i in 0..MAX_STARTUP_PIECES {
                let p = first_piece + i;
                if p <= last_piece && !handle.have_piece(p) {
                    // Safety net: very low urgency to give metadata inspector a head start
                    handle.set_piece_deadline(p, 3000 + i * 25);
                }
            }
            let _total_pieces = (last_piece - first_piece + 1) as i32;

            // PRE-WARM CACHE: If first 8 pieces are already complete, request them via read_piece
            // This populates moka cache immediately for zero-latency first read
            // Only prewarm if they're already available (don't wait for download)
            let prewarm_count = MAX_STARTUP_PIECES;
            let is_prewarm_complete =
                (0..prewarm_count).all(|i| handle.have_piece(first_piece + i));
            if is_prewarm_complete {
                tracing::info!(
                    "prepare_file_for_streaming: File head complete, pre-warming {} pieces",
                    prewarm_count
                );
                let cache = self.piece_cache.clone();
                let info_hash = self.info_hash.clone();
                let waiter = self.piece_waiter.clone();
                let fp = first_piece;
                tokio::spawn(async move {
                    for i in 0..prewarm_count {
                        let piece_data =
                            libtorrent_sys::memory_read_piece_direct(&info_hash, fp + i);
                        if !piece_data.is_empty() {
                            cache.put_piece(&info_hash, fp + i, piece_data).await;
                            waiter.notify_piece_finished(&info_hash, fp + i);
                        }
                    }
                    tracing::info!(
                        "prepare_file_for_streaming: Pre-warmed {} pieces",
                        prewarm_count
                    );
                });
            }

            let name_lower = name.to_lowercase();
            let needs_end_metadata = name_lower.ends_with(".mkv")
                || name_lower.ends_with(".mp4")
                || name_lower.ends_with(".webm")
                || name_lower.ends_with(".mov");

            // DO NOT prioritize other pieces here - let get_file_reader() handle it
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
                needs_end_metadata,
            )
        }; // session lock released here

        let should_inspect_metadata = if needs_end_metadata {
            let key = (self.info_hash.clone(), file_idx);
            let mut inspections = self.metadata_inspections.lock().await;
            inspections.insert(key)
        } else {
            false
        };

        if should_inspect_metadata {
            // Perform reactive metadata inspection in the background after startup gets the first shot.
            let this = self.clone();
            tokio::spawn(async move {
                let wait_start = std::time::Instant::now();
                let mut head_ready = false;
                while wait_start.elapsed() < std::time::Duration::from_secs(15) {
                    let ready = {
                        let session = this.session.read().await;
                        session
                            .find_torrent(&this.info_hash)
                            .map(|handle| handle.have_piece(first_piece))
                            .unwrap_or(false)
                    };

                    if ready {
                        head_ready = true;
                        break;
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }

                if !head_ready {
                    tracing::info!(
                        info_hash = %this.info_hash,
                        file_idx,
                        first_piece,
                        waited_ms = wait_start.elapsed().as_millis() as u64,
                        "Background Metadata Inspection: skipped until head piece is ready"
                    );
                    return;
                }

                tracing::info!(
                    info_hash = %this.info_hash,
                    file_idx,
                    first_piece,
                    waited_ms = wait_start.elapsed().as_millis() as u64,
                    "Background Metadata Inspection: Starting after head piece became ready"
                );

                if needs_end_metadata && last_piece > first_piece {
                    let session = this.session.read().await;
                    if let Ok(mut handle) = session.find_torrent(&this.info_hash) {
                        for i in 0..2 {
                            let p = last_piece - i;
                            if p >= first_piece && !handle.have_piece(p) {
                                handle.set_piece_deadline(p, 150 + (i * 50) as i32);
                            }
                        }
                        tracing::debug!(
                            "Background Metadata Inspection: Primed tail pieces after startup window"
                        );
                    }
                    drop(session);
                }

                // This will find 'moov' atoms (MP4) or index areas (MKV) and prioritize them.
                if let Ok(mut reader) = this
                    .get_file_reader(file_idx, 0, 255, None, PlaybackIntent::InternalProbe)
                    .await
                {
                    let critical_ranges = match tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        MetadataInspector::find_critical_ranges(
                            &mut reader,
                            file_length as u64,
                            &name,
                        ),
                    )
                    .await
                    {
                        Ok(ranges) => ranges,
                        Err(_) => {
                            tracing::warn!(
                                info_hash = %this.info_hash,
                                file_idx,
                                "Background Metadata Inspection: timed out"
                            );
                            Vec::new()
                        }
                    };

                    if !critical_ranges.is_empty() {
                        let session = this.session.read().await;
                        if let Ok(mut handle) = session.find_torrent(&this.info_hash) {
                            for (offset, len) in &critical_ranges {
                                let start_piece =
                                    ((file_offset as u64 + offset) / piece_length as u64) as i32;
                                let end_piece =
                                    ((file_offset as u64 + offset + len.saturating_sub(1))
                                        / piece_length as u64)
                                        as i32;

                                for p in start_piece..=end_piece {
                                    if p >= first_piece && p <= last_piece {
                                        // Player needs Cues/moov to start; this runs once per file.
                                        handle.set_piece_deadline(p, 150);
                                    }
                                }
                            }
                            tracing::info!(
                                "Background Metadata Inspection: Prioritized {} critical metadata ranges (Deadline 200ms)",
                                critical_ranges.len()
                            );
                        }
                    }
                }

                // NOTE: Keyframe inspection removed - it was blocking for 15+ seconds.
            });
        } else if needs_end_metadata {
            tracing::debug!(
                "Background Metadata Inspection: already scheduled for file {}",
                file_idx
            );
        }

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

        // Set file priority to 0 (skip) - no more pieces will be downloaded for this file.
        // Do not clear global piece deadlines or sequential mode here because
        // another file in the same torrent may still have an active stream.
        handle.set_file_priority(file_idx as i32, 0);

        tracing::info!(
            "clear_file_streaming: Cleared streaming state for file {} in {}",
            file_idx,
            self.info_hash
        );

        Ok(())
    }

    async fn wait_for_piece_ready(
        &self,
        file_idx: usize,
        offset: u64,
        timeout: std::time::Duration,
        intent: PlaybackIntent,
    ) -> anyhow::Result<PieceReadiness> {
        let start = std::time::Instant::now();
        let mut last_peers = 0u64;
        let mut last_rate = 0u64;
        let mut last_readiness_log = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(5))
            .unwrap_or_else(std::time::Instant::now);

        let (piece, first_piece, last_piece, piece_length, file_size) = {
            let session = self.session.read().await;
            let mut handle = session
                .find_torrent(&self.info_hash)
                .map_err(|e| anyhow!("Torrent not found: {}", e))?;
            self.label_memory_storage();
            let files = handle.files();
            let file_info = files
                .get(file_idx)
                .ok_or_else(|| anyhow!("File index {} out of range", file_idx))?;
            let piece_length = handle.piece_length() as u64;
            if piece_length == 0 {
                return Ok(PieceReadiness {
                    ready: false,
                    piece: -1,
                    ready_pieces: 0,
                    target_pieces: 0,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    peers: 0,
                    download_rate: 0,
                    reason: "zero-piece-length".to_string(),
                });
            }
            let piece = ((file_info.offset as u64 + offset) / piece_length) as i32;
            if piece < file_info.first_piece || piece > file_info.last_piece {
                return Ok(PieceReadiness {
                    ready: false,
                    piece,
                    ready_pieces: 0,
                    target_pieces: 1,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    peers: 0,
                    download_rate: 0,
                    reason: "piece-out-of-file-range".to_string(),
                });
            }
            let status = handle.status();
            let native_memory = libtorrent_sys::memory_storage_stats();
            let memory_pressure = if self.config.cache.size > 0
                && native_memory.total_bytes >= self.config.cache.size.saturating_mul(80) / 100
            {
                MemoryPressure::High
            } else {
                MemoryPressure::Normal
            };
            let decision = PlaybackPriorityPolicy::decide(PriorityContext {
                intent,
                current_piece: piece,
                first_piece: file_info.first_piece,
                last_piece: file_info.last_piece,
                piece_length,
                file_size: file_info.size as u64,
                bitrate_bytes_per_sec: None,
                download_rate_bytes_per_sec: status.download_rate as u64,
                peers: status.num_peers as u64,
                cache_size_bytes: self.config.cache.size,
                memory_pressure,
                consecutive_waits: 0,
                first_byte_sent: false,
            });
            for assignment in &decision.assignments {
                if !handle.have_piece(assignment.piece_idx) {
                    handle.set_piece_priority(assignment.piece_idx, assignment.piece_priority);
                    handle.set_piece_deadline(assignment.piece_idx, assignment.deadline);
                }
            }
            tracing::info!(
                intent = ?intent,
                piece,
                hot_window = decision.hot_window_pieces,
                warm_window = decision.warm_window_pieces,
                peers = status.num_peers,
                download_rate = status.download_rate,
                reason = %decision.reason,
                "priority_seek_readiness_begin"
            );
            (
                piece,
                file_info.first_piece,
                file_info.last_piece,
                piece_length,
                file_info.size as u64,
            )
        };

        let target_pieces = 1u32.min((last_piece.saturating_sub(piece) + 1).max(1) as u32);
        let mut best_ready_pieces = 0u32;

        while start.elapsed() < timeout {
            let (peers, rate, paused, finished) = {
                let session = self.session.read().await;
                let handle = session
                    .find_torrent(&self.info_hash)
                    .map_err(|e| anyhow!("Torrent not found: {}", e))?;
                self.label_memory_storage();
                let status = handle.status();
                (
                    status.num_peers as u64,
                    status.download_rate as u64,
                    status.is_paused,
                    status.is_finished,
                )
            };
            last_peers = peers;
            last_rate = rate;

            let mut ready_pieces = 0u32;
            for ready_piece in piece..=last_piece.min(piece + target_pieces as i32 - 1) {
                let have_piece = {
                    let session = self.session.read().await;
                    let handle = session
                        .find_torrent(&self.info_hash)
                        .map_err(|e| anyhow!("Torrent not found: {}", e))?;
                    self.label_memory_storage();
                    handle.have_piece(ready_piece)
                };

                if matches!(self.storage_mode, LibtorrentStorageMode::DiskBacked) {
                    if have_piece {
                        ready_pieces += 1;
                        continue;
                    }
                    break;
                }

                if self
                    .piece_cache
                    .has_piece(&self.info_hash, ready_piece)
                    .await
                {
                    ready_pieces += 1;
                    continue;
                }

                if have_piece {
                    let piece_data =
                        libtorrent_sys::memory_read_piece_direct(&self.info_hash, ready_piece);
                    if piece_data.is_empty() {
                        break;
                    }

                    self.piece_cache
                        .put_piece(&self.info_hash, ready_piece, piece_data)
                        .await;
                    self.piece_waiter
                        .notify_piece_finished(&self.info_hash, ready_piece);
                    ready_pieces += 1;
                } else {
                    break;
                }
            }
            best_ready_pieces = best_ready_pieces.max(ready_pieces);

            if ready_pieces >= target_pieces {
                return Ok(PieceReadiness {
                    ready: true,
                    piece,
                    ready_pieces,
                    target_pieces,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    peers,
                    download_rate: rate,
                    reason: if target_pieces > 1 {
                        "buffer-ready".to_string()
                    } else {
                        "piece-ready".to_string()
                    },
                });
            }

            if paused && !finished {
                let session = self.session.read().await;
                if let Ok(mut handle) = session.find_torrent(&self.info_hash) {
                    handle.resume();
                    handle.force_reannounce();
                    handle.force_dht_announce();
                }
            }

            if piece <= last_piece {
                let session = self.session.read().await;
                if let Ok(mut handle) = session.find_torrent(&self.info_hash) {
                    let status = handle.status();
                    let decision = PlaybackPriorityPolicy::decide(PriorityContext {
                        intent,
                        current_piece: piece,
                        first_piece,
                        last_piece,
                        piece_length,
                        file_size,
                        bitrate_bytes_per_sec: None,
                        download_rate_bytes_per_sec: status.download_rate as u64,
                        peers: status.num_peers as u64,
                        cache_size_bytes: self.config.cache.size,
                        memory_pressure: MemoryPressure::Normal,
                        consecutive_waits: 1,
                        first_byte_sent: false,
                    });
                    for assignment in &decision.assignments {
                        if !handle.have_piece(assignment.piece_idx) {
                            handle.set_piece_priority(
                                assignment.piece_idx,
                                assignment.piece_priority,
                            );
                            handle.set_piece_deadline(assignment.piece_idx, assignment.deadline);
                        }
                    }

                    if last_readiness_log.elapsed() >= std::time::Duration::from_secs(5) {
                        last_readiness_log = std::time::Instant::now();
                        let native_memory = libtorrent_sys::memory_storage_stats();
                        let priorities = handle.piece_priorities();
                        let availability = handle.piece_availability();
                        let target_priority = priorities.get(piece as usize).copied().unwrap_or(-1);
                        let target_availability =
                            availability.get(piece as usize).copied().unwrap_or(-1);
                        let immediate_end =
                            (piece + decision.immediate_pieces.saturating_sub(1)).min(last_piece);
                        let ready_immediate = if immediate_end >= piece {
                            (piece..=immediate_end)
                                .filter(|p| handle.have_piece(*p))
                                .count()
                        } else {
                            0
                        };
                        let verified_piece_count = (first_piece..=last_piece)
                            .filter(|p| handle.have_piece(*p))
                            .count();
                        let verified_bytes_estimate = (verified_piece_count as u64)
                            .saturating_mul(piece_length)
                            .min(file_size);
                        let request_offset_percent = if file_size > 0 {
                            (offset.min(file_size) as f64 / file_size as f64) * 100.0
                        } else {
                            0.0
                        };

                        tracing::info!(
                            intent = ?intent,
                            storage_mode = ?self.storage_mode,
                            piece,
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            peers = status.num_peers,
                            download_rate = status.download_rate,
                            paused = status.is_paused,
                            finished = status.is_finished,
                            target_priority,
                            target_availability,
                            immediate_pieces = decision.immediate_pieces,
                            ready_immediate,
                            verified_piece_count,
                            verified_bytes_estimate,
                            request_offset_percent,
                            native_storage_bytes = native_memory.total_bytes,
                            native_storage_pieces = native_memory.total_pieces,
                            reason = %decision.reason,
                            "direct stream readiness waiting"
                        );
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        Ok(PieceReadiness {
            ready: best_ready_pieces > 0,
            piece,
            ready_pieces: best_ready_pieces,
            target_pieces,
            elapsed_ms: start.elapsed().as_millis() as u64,
            peers: last_peers,
            download_rate: last_rate,
            reason: if best_ready_pieces > 0 {
                "partial-buffer-timeout".to_string()
            } else {
                "timeout".to_string()
            },
        })
    }
}
