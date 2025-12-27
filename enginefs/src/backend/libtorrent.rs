//! libtorrent-rasterbar backend implementation
//!
//! Uses the libtorrent-sys crate to provide a high-performance native torrent backend.

use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::backend::{
    BackendFileInfo, EngineStats, FileStreamTrait, Growler, PeerSearch, Source, StatsOptions,
    SwarmCap, TorrentBackend, TorrentHandle as TorrentHandleTrait, TorrentSource,
    metadata::MetadataInspector,
    priorities::{EngineCacheConfig, calculate_priorities},
};
use crate::tracker_prober::TrackerProber;

use libtorrent_sys::{LibtorrentSession, SessionSettings, TorrentStatus};

/// Peer discovery and metadata caching constants
const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://open.stealth.si:80/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://www.torrent.eu.org:451/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://retracker.lanta-net.ru:2710/announce",
];

/// libtorrent backend implementation
pub struct LibtorrentBackend {
    session: Arc<RwLock<LibtorrentSession>>,
    save_path: PathBuf,
    metadata_path: PathBuf,
    config: crate::backend::BackendConfig,
    stream_counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl LibtorrentBackend {
    /// Create a new libtorrent backend
    pub fn new(save_path: PathBuf, config: crate::backend::BackendConfig) -> Result<Self> {
        let settings = SessionSettings {
            listen_interfaces: "0.0.0.0:42000-42010,[::]:42000-42010".to_string(),
            user_agent: "stream-server/1.0".to_string(),
            enable_dht: true,
            enable_lsd: true,
            enable_upnp: true,
            enable_natpmp: true,
            // Apply speed profile settings from config
            max_connections: config.speed_profile.bt_max_connections as i32,
            max_connections_per_torrent: (config.speed_profile.bt_max_connections / 2) as i32,
            download_rate_limit: config.speed_profile.bt_download_speed_hard_limit as i32,
            upload_rate_limit: 0,
            active_downloads: 30,
            active_seeds: 20,
            active_limit: 50,
            anonymous_mode: false,
            proxy_host: String::new(),
            proxy_port: 0,
            proxy_type: 0,
            announce_to_all_trackers: true,
            announce_to_all_tiers: true,
        };

        tracing::info!(
            "LibtorrentBackend: max_connections={}, download_limit={} B/s",
            settings.max_connections,
            settings.download_rate_limit
        );

        let session = LibtorrentSession::new(settings)
            .map_err(|e| anyhow!("Failed to create libtorrent session: {}", e))?;

        std::fs::create_dir_all(&save_path)?;

        let metadata_path = save_path.join(".metadata");
        let _ = std::fs::create_dir_all(&metadata_path);

        let backend = Self {
            session: Arc::new(RwLock::new(session)),
            save_path,
            metadata_path,
            config,
            stream_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        backend.start_monitor_task();
        Ok(backend)
    }

    /// Create with custom settings
    pub fn with_settings(
        save_path: PathBuf,
        settings: SessionSettings,
        config: crate::backend::BackendConfig,
    ) -> Result<Self> {
        let session = LibtorrentSession::new(settings)
            .map_err(|e| anyhow!("Failed to create libtorrent session: {}", e))?;

        std::fs::create_dir_all(&save_path)?;

        let metadata_path = save_path.join(".metadata");
        let _ = std::fs::create_dir_all(&metadata_path);

        let backend = Self {
            session: Arc::new(RwLock::new(session)),
            save_path,
            metadata_path,
            config,
            stream_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        backend.start_monitor_task();
        Ok(backend)
    }

    /// Update session settings dynamically (called when user changes settings)
    pub async fn update_session_settings(&self, profile: &crate::backend::TorrentSpeedProfile) {
        let mut session = self.session.write().await;

        // Update download rate limit (0 = unlimited)
        let download_limit = if profile.bt_download_speed_hard_limit > 0.0 {
            profile.bt_download_speed_hard_limit as i32
        } else {
            0 // Unlimited
        };

        // Apply new settings via full settings pack
        let new_settings = libtorrent_sys::SessionSettings {
            listen_interfaces: "0.0.0.0:6881,[::]:6881".to_string(),
            user_agent: "stream-server/1.0".to_string(),
            enable_dht: true,
            enable_lsd: true,
            enable_upnp: true,
            enable_natpmp: true,
            max_connections: profile.bt_max_connections as i32,
            max_connections_per_torrent: (profile.bt_max_connections / 2) as i32,
            download_rate_limit: download_limit,
            upload_rate_limit: 0,
            active_downloads: 30,
            active_seeds: 20,
            active_limit: 50,
            anonymous_mode: false,
            proxy_host: String::new(),
            proxy_port: 0,
            proxy_type: 0,
            announce_to_all_trackers: true,
            announce_to_all_tiers: true,
        };

        if let Err(e) = session.apply_settings(&new_settings) {
            tracing::error!("Failed to apply session settings: {}", e);
        } else {
            tracing::info!(
                "Updated libtorrent settings: max_connections={}, download_limit={} B/s",
                profile.bt_max_connections,
                download_limit
            );
        }
    }
    fn start_monitor_task(&self) {
        let session = self.session.clone();
        let metadata_path = self.metadata_path.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            let mut last_reannounce: std::collections::HashMap<String, std::time::Instant> =
                std::collections::HashMap::new();
            loop {
                interval.tick().await;

                // Get all handles first to avoid holding lock too long?
                // Actually we need the lock to find them.
                // We'll iterate.
                let handles: Vec<_> = {
                    let s = session.read().await;
                    s.get_torrents()
                        .iter()
                        .filter_map(|t| s.find_torrent(&t.info_hash).ok())
                        .collect()
                };

                for mut handle in handles {
                    let status = handle.status();

                    // --- Metadata Initialization Logic ---
                    // For magnets, files are not known until metadata is acquired.
                    // Once metadata is available, libtorrent defaults all files to priority 4.
                    // We catch this and set them to 0 (skip) so the user can then start streaming a specific file.
                    if status.has_metadata {
                        let priorities = handle.get_file_priorities();
                        // 4 is the libtorrent default for "normal" priority.
                        // If ALL files are at 4, it's highly likely they haven't been initialized by us yet.
                        if !priorities.is_empty() && priorities.iter().all(|&p| p == 4) {
                            tracing::info!(
                                "Monitor: Metadata acquired for '{}' ({} files). Resetting all priorities to 0 (skip).",
                                status.name,
                                priorities.len()
                            );
                            for (i, _) in priorities.iter().enumerate() {
                                handle.set_file_priority(i as i32, 0);
                            }
                        }

                        // Instant Loading Part 3: Save Metadata to Cache
                        let info_hash = handle.info_hash();
                        let cache_file =
                            metadata_path.join(format!("{}.torrent", info_hash.to_lowercase()));
                        if !cache_file.exists() {
                            let metadata = handle.get_metadata();
                            if !metadata.is_empty() {
                                if let Ok(_) = std::fs::write(&cache_file, metadata) {
                                    tracing::info!(
                                        "Instant Loading: Saved metadata for {} to cache.",
                                        info_hash
                                    );
                                }
                            }
                        }
                    }

                    // --- PeerSearch Logic ---
                    {
                        let mut force = false;
                        let min_peers = config.peer_search.min as i32;
                        let num_peers = status.num_peers as i32;

                        // Rule 1: Low peer count or slow speed
                        let slow_threshold = 1024 * 1024; // 1MB/s
                        if num_peers < min_peers
                            || (status.download_rate < slow_threshold
                                && num_peers < config.peer_search.max as i32)
                        {
                            force = true;
                        }

                        // Rule 2: Periodic Re-announce
                        let now = std::time::Instant::now();
                        let last_announce =
                            last_reannounce.entry(handle.info_hash()).or_insert(now);

                        // Metadata Burst: If we don't have metadata, re-announce every 15s instead of 60s
                        let interval = if !status.has_metadata {
                            std::time::Duration::from_secs(15)
                        } else {
                            std::time::Duration::from_secs(60)
                        };

                        if now.duration_since(*last_announce) > interval {
                            force = true;
                            *last_announce = now;
                        }

                        if force {
                            let _ = handle.force_reannounce();
                            let _ = handle.force_dht_announce();
                        }
                    }

                    // --- SwarmCap Logic ---
                    if let Some(max_speed) = config.swarm_cap.max_speed {
                        if (status.download_rate as f64) > max_speed {
                            // Limit reached. Pause this torrent?
                            // handle.auto_managed(true); // Let libtorrent manage?
                            // Or explicit pause.
                            // if !status.is_paused { handle.pause(); }
                        }
                    }

                    // --- Growler Logic ---
                    let total_downloaded = status.total_downloaded as u64;
                    if total_downloaded > config.growler.flood {
                        if let Some(pulse) = config.growler.pulse {
                            handle.set_download_limit(pulse as i32);
                        }
                    } else {
                        handle.set_download_limit(-1);
                    }
                }
            }
        });
    }

    /// Pause all torrents except the specified one to focus bandwidth on streaming
    pub async fn focus_torrent(&self, target_info_hash: &str) {
        let session = self.session.read().await;
        let torrents = session.get_torrents();

        for status in torrents {
            if status.info_hash.to_lowercase() != target_info_hash.to_lowercase() {
                if let Ok(mut handle) = session.find_torrent(&status.info_hash) {
                    if !status.is_paused {
                        tracing::info!("Pausing torrent {} to focus on stream", status.info_hash);
                        handle.pause();
                    }
                }
            }
        }
    }

    /// Resume all paused torrents (called when streaming ends)
    pub async fn resume_all_torrents(&self) {
        let session = self.session.read().await;
        let torrents = session.get_torrents();

        for status in torrents {
            if status.is_paused {
                if let Ok(mut handle) = session.find_torrent(&status.info_hash) {
                    tracing::info!("Resuming torrent {}", status.info_hash);
                    handle.resume();
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl TorrentBackend for LibtorrentBackend {
    type Handle = LibtorrentTorrentHandle;

    async fn add_torrent(
        &self,
        source: TorrentSource,
        trackers: Vec<String>,
    ) -> Result<Self::Handle> {
        let mut session = self.session.write().await;
        let save_path = self.save_path.to_string_lossy().to_string();

        let mut handle = match source {
            TorrentSource::Url(url) => {
                // Instant Loading Part 1: Check Metadata Cache
                if let Ok(params) = libtorrent_sys::parse_magnet(&url) {
                    let info_hash = params.info_hash.to_lowercase();
                    let cache_file = self.metadata_path.join(format!("{}.torrent", info_hash));

                    if cache_file.exists() {
                        if let Ok(cached_data) = std::fs::read(&cache_file) {
                            tracing::info!(
                                "Instant Loading: Found cached metadata for {}. Skipping magnet resolution.",
                                info_hash
                            );
                            let mut p = params.clone();
                            p.torrent_data = cached_data;
                            p.save_path = save_path;
                            // Inject known trackers immediately
                            for &t in DEFAULT_TRACKERS {
                                if !p.trackers.contains(&t.to_string()) {
                                    p.trackers.push(t.to_string());
                                }
                            }
                            session
                                .add_torrent(&p)
                                .map_err(|e| anyhow!("Failed to add torrent from cache: {}", e))?
                        } else {
                            session
                                .add_magnet(&url, &save_path)
                                .map_err(|e| anyhow!("Failed to add magnet: {}", e))?
                        }
                    } else {
                        session
                            .add_magnet(&url, &save_path)
                            .map_err(|e| anyhow!("Failed to add magnet: {}", e))?
                    }
                } else {
                    session
                        .add_magnet(&url, &save_path)
                        .map_err(|e| anyhow!("Failed to add magnet: {}", e))?
                }
            }
            TorrentSource::Bytes(data) => {
                let params = libtorrent_sys::AddTorrentParams {
                    magnet_uri: String::new(),
                    torrent_data: data,
                    save_path,
                    name: String::new(),
                    trackers: trackers.clone(),
                    paused: false,
                    auto_managed: true,
                    upload_limit: 0,
                    download_limit: 0,
                    sequential_download: false,
                    info_hash: String::new(),
                    info_hash_v2: String::new(),
                };
                session
                    .add_torrent(&params)
                    .map_err(|e| anyhow!("Failed to add torrent: {}", e))?
            }
        };

        // Instant Loading Part 2: Tracker Injection & Force Reannounce
        let mut final_trackers: Vec<String> = trackers.clone();
        for &t in DEFAULT_TRACKERS {
            if !final_trackers.iter().any(|existing| existing == t) {
                final_trackers.push(t.to_string());
            }
        }

        for tracker in &final_trackers {
            handle.add_tracker(tracker, 0);
        }

        // Force immediate discovery
        handle.force_reannounce();
        handle.force_dht_announce();

        // Background: Rank trackers and re-apply
        let mut rank_handle = handle.clone();
        tokio::spawn(async move {
            let ranked = TrackerProber::rank_trackers(final_trackers).await;
            if rank_handle.is_valid() {
                rank_handle.replace_trackers(&ranked);
                tracing::debug!(
                    "Trackers ranked and updated for {}",
                    rank_handle.info_hash()
                );
            }
        });

        // Add trackers with tier based on position
        // Disable sequential download for streaming - we manage it manually via deadlines
        // handle.set_sequential_download(true);

        // DEBUG MONITOR: Log stats every 5 seconds to diagnose slow speeds
        // let monitor_handle = handle.clone();
        // tokio::spawn(async move {
        //      let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        //      loop {
        //          interval.tick().await;
        //          let status = monitor_handle.status();
        //          tracing::info!(
        //              "Monitor: Peers={} Seeds={} Down={:.2} MB/s Up={:.2} MB/s State={:?}",
        //              status.num_peers,
        //              status.num_seeds,
        //              (status.download_rate as f64) / 1024.0 / 1024.0,
        //              (status.upload_rate as f64) / 1024.0 / 1024.0,
        //              status.state
        //          );
        //      }
        // });

        // Add trackers with tier based on position
        for (idx, tracker) in trackers.iter().enumerate() {
            handle.add_tracker(tracker, idx as i32);
        }

        // CRITICAL: Set ALL files to priority 0 (skip) immediately
        // This prevents downloading all 366 episodes when user only wants 1
        // The get_file_reader() will set priority 7 for the specific file being streamed
        let files = handle.files();
        tracing::info!(
            "add_torrent: Setting all {} files to priority 0 (skip) to prevent unwanted downloads",
            files.len()
        );
        for (idx, _f) in files.iter().enumerate() {
            handle.set_file_priority(idx as i32, 0);
        }

        Ok(LibtorrentTorrentHandle {
            session: self.session.clone(),
            info_hash: handle.info_hash(),
            save_path: self.save_path.clone(),
            config: self.config.clone(),
            stream_counter: self.stream_counter.clone(),
        })
    }

    async fn get_torrent(&self, info_hash: &str) -> Option<Self::Handle> {
        let session = self.session.read().await;
        match session.find_torrent(info_hash) {
            Ok(_) => Some(LibtorrentTorrentHandle {
                session: self.session.clone(),
                info_hash: info_hash.to_string(),
                save_path: self.save_path.clone(),
                config: self.config.clone(),
                stream_counter: self.stream_counter.clone(),
            }),
            Err(_) => None,
        }
    }

    async fn remove_torrent(&self, info_hash: &str) -> Result<()> {
        let mut session = self.session.write().await;
        let handle = session
            .find_torrent(info_hash)
            .map_err(|e| anyhow!("Torrent not found: {}", e))?;
        session
            .remove_torrent(&handle, false)
            .map_err(|e| anyhow!("Failed to remove torrent: {}", e))?;
        Ok(())
    }

    async fn list_torrents(&self) -> Vec<String> {
        let session = self.session.read().await;
        session
            .get_torrents()
            .iter()
            .map(|t| t.info_hash.to_string())
            .collect()
    }
}

/// Handle to a torrent managed by libtorrent
#[derive(Clone)]
pub struct LibtorrentTorrentHandle {
    session: Arc<RwLock<LibtorrentSession>>,
    info_hash: String,
    save_path: PathBuf,
    config: crate::backend::BackendConfig,
    stream_counter: Arc<std::sync::atomic::AtomicUsize>,
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

        // Set file priorities: Requested file = 7 (Top), Others = 0 (Skip)
        // This ensures all bandwidth goes to the stream
        let all_files = handle.files();
        for (idx, f) in all_files.iter().enumerate() {
            if idx == file_idx {
                tracing::debug!(
                    "get_file_reader: Setting PRIORITY 1 (LOW) for file idx={} name={}",
                    idx,
                    f.path
                );
                handle.set_file_priority(idx as i32, 1);
            } else {
                tracing::debug!(
                    "get_file_reader: Setting PRIORITY 0 (SKIP) for file idx={} name={}",
                    idx,
                    f.path
                );
                handle.set_file_priority(idx as i32, 0);
            }
        }

        let mut actual_start_piece = -1;

        if !is_complete {
            // AGGRESSIVE INITIAL PRIORITIZATION for fast loading
            // First piece of the requested window
            actual_start_piece = ((global_file_offset + start_offset) / piece_length) as i32;

            // Phase 2: Seek Detection
            // If offset > 0, this is likely a seek request (player seeking for metadata or user scrubbing)
            let is_seek_request = start_offset > 0;

            // Calculate pieces for 3 seconds of playback (Minimal Startup)
            // Default 10 pieces (~10MB)
            let mut pieces_to_prioritize = 10;
            if let Some(br) = bitrate {
                let pieces_for_3s = ((br * 3) / piece_length) as i32;
                pieces_to_prioritize = pieces_to_prioritize.max(pieces_for_3s);
            }
            // Absolute Cap at 20 pieces (20MB) to ensure < 10% start on typical files
            pieces_to_prioritize = pieces_to_prioritize.min(20);

            tracing::info!(
                "get_file_reader: Calculated pieces_to_prioritize={} for FAST LOAD (Bitrate: {:?})",
                pieces_to_prioritize,
                bitrate
            );

            if is_seek_request {
                // Seek request: tight window but larger than before for video clusters
                // Video clusters can span multiple pieces, prioritize 15 pieces (~15MB)
                let seek_window = 15;
                tracing::info!(
                    "get_file_reader: SEEK DETECTED - Prioritizing {} pieces from piece {} with STAIRCASE deadlines",
                    seek_window,
                    actual_start_piece
                );
                for i in 0..seek_window {
                    let p = actual_start_piece + i;
                    if p <= last_piece {
                        handle.set_piece_deadline(p, i * 10); // Staircase: 0, 10, 20...
                    }
                }
            } else {
                // Normal start: prioritize more pieces with gradual deadlines
                tracing::info!(
                    "get_file_reader: FAST LOAD - Prioritizing {} pieces from offset {} (piece {})",
                    pieces_to_prioritize,
                    start_offset,
                    actual_start_piece
                );

                for i in 0..pieces_to_prioritize {
                    let p = actual_start_piece + i;
                    if p <= last_piece {
                        // Staircase: 0, 10, 20, 30...
                        handle.set_piece_deadline(p, i * 10);
                    }
                }
            }

            // IMPORTANT: Also prioritize the LAST piece of the file
            // Many video containers (MP4, MKV) store index/moov at the end
            // This allows faster initial parsing by video players
            if last_piece > actual_start_piece + pieces_to_prioritize {
                tracing::debug!(
                    "get_file_reader: Also prioritizing last piece {} for container index",
                    last_piece
                );
                handle.set_piece_deadline(last_piece, 0);
                // Also get the piece before last (moov can span 2 pieces)
                if last_piece > 0 {
                    handle.set_piece_deadline(last_piece - 1, 0);
                }
            }
        }

        // REAL SOLUTION: Wait for seek target piece before returning
        // REMOVED: We want to return potentially BEFORE the piece is available.
        // The LibtorrentFileStream::poll_read will handle the waiting/blocking if data is missing.
        // This allows the Axum handler to send HTTP headers immediately.
        /*
        if !is_complete && actual_start_piece >= 0 {
            let target_piece = actual_start_piece;
            // ... (removed blocking loop)
        }
        */

        // Open the file from disk
        let file_path = PathBuf::from(&file_info.absolute_path);

        // If complete, file should exist immediately; otherwise wait briefly
        if !file_path.exists() {
            if is_complete {
                tracing::warn!(
                    "get_file_reader: complete but file doesn't exist: {:?}",
                    file_path
                );
            }
            let max_attempts = if is_complete { 50 } else { 500 }; // 1s for complete, 10s otherwise
            let mut attempts = 0;
            while !file_path.exists() && attempts < max_attempts {
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
                attempts += 1;
            }
        }

        if !file_path.exists() {
            return Err(anyhow!("File does not exist yet: {:?}", file_path));
        }

        tracing::debug!("get_file_reader: opening file {:?}", file_path);
        let opened_file = tokio::fs::File::open(&file_path).await?;

        tracing::debug!(
            "get_file_reader: Calculated global offset for file {}: {}",
            file_idx,
            global_file_offset
        );

        let stream_id = self
            .stream_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        Ok(Box::new(LibtorrentFileStream {
            file: opened_file,
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

        // Wait for metadata if not yet available (up to 30 seconds)
        for _ in 0..600 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let session = self.session.read().await;
            if let Ok(handle) = session.find_torrent(&self.info_hash) {
                if handle.status().has_metadata {
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
        vec![]
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
        tracing::info!(
            "prepare_file_for_streaming: Preparing file {} for streaming",
            file_idx
        );

        // Phase 1: Wait for metadata
        let timeout_start = std::time::Instant::now();
        loop {
            let session = self.session.read().await;
            if let Ok(handle) = session.find_torrent(&self.info_hash) {
                if handle.status().has_metadata {
                    break;
                }
            }
            drop(session);

            if timeout_start.elapsed().as_secs() >= 30 {
                return Err(anyhow::anyhow!(
                    "Timeout waiting for torrent metadata (30s)"
                ));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
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

            // Set file priorities: target file = 7 (max), all others = 0 (skip)
            for (idx, _) in files.iter().enumerate() {
                if idx == file_idx {
                    handle.set_file_priority(idx as i32, 1); // Low priority - let piece deadlines drive urgency
                } else {
                    handle.set_file_priority(idx as i32, 0);
                }
            }

            // REAL SOLUTION: Enable sequential download for streaming
            // This ensures pieces are downloaded in order, which is what players expect
            // handle.set_sequential_download(true);

            // Phase 1: Minimal initial prioritization
            // STRATEGY CHANGE: Use sequential_download for the bulk of the file.
            // Only strictly prioritize the very first few pieces to ensure immediate start.
            // Setting deadlines for 1000+ pieces causes overhead and speed regressions.
            let total_pieces = (last_piece - first_piece + 1) as i32;

            // Only prioritize the first 5 pieces for immediate start
            // STAIRCASE PRIORITIZATION: 0, 10, 20... forces libtorrent to pick Piece 0 first.
            for i in 0..total_pieces.min(10) {
                let p = first_piece + i;
                handle.set_piece_deadline(p, i * 10);
            }

            // ALWAYS prioritize last pieces for Cues/moov (overwrite their deadline)
            for p in (last_piece - 10).max(first_piece)..=last_piece {
                handle.set_piece_deadline(p, 0); // Immediate
            }

            // REAL SOLUTION: Enable sequential download for streaming
            // This ensures pieces are downloaded in order, which is what players expect
            // forcing sequential download helps prevent "rarest first" from flooding the connection
            handle.set_sequential_download(true);

            tracing::info!(
                "prepare_file_for_streaming: Prioritized first 5 and last 10 pieces (Sequential mode ON)",
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
        // This ensures we return from prepare_file_for_streaming immediately so playback can start
        let this = self.clone();
        tokio::spawn(async move {
            tracing::info!(
                "Background Metadata Inspection: Starting for file {}",
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

        // Phase 3: Wait for BOTH first piece AND last piece (for container metadata)
        // REMOVED: Blocking here prevents the player from receiving headers and starting playback.
        // The player will block on the actual read of the specific bytes it needs.
        // We just verified the file exists in the torrent structure.

        tracing::info!("prepare_file_for_streaming: Ready for playback (non-blocking)");
        return Ok(());
    }
}

struct LibtorrentFileStream {
    file: tokio::fs::File,
    handle: libtorrent_sys::LibtorrentHandle,
    first_piece: i32,
    last_piece: i32,
    piece_length: u64,
    file_offset: u64,
    current_pos: u64,
    is_complete: bool,
    last_priorities_piece: i32, // Track last piece we set priorities for
    cache_config: EngineCacheConfig,
    priority: u8,
    bitrate: Option<u64>,
    download_speed_ema: f64,
    stream_id: usize,
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

        // tracing::info!(
        //     "set_priorities: Pos={} (GlobalOffset={}), Piece={}, Window={}-{}",
        //     pos,
        //     self.file_offset,
        //     current_piece,
        //     current_piece,
        //     current_piece + 30 // Approx urgent window
        // );

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

        // Critical Fix: Block read if piece is not downloaded yet.
        // Reading undownloaded parts from disk returns sparse zeros, which confuses parsers (ffmpeg).
        if self.piece_length > 0 {
            // Correct calculation: file_offset is TRUE global offset
            let piece = ((self.file_offset + pos) / self.piece_length) as i32;
            if !self.handle.have_piece(piece) {
                // Throttle log logs
                if pos % (1024 * 1024) == 0 {
                    // log roughly every MB or when stuck
                    tracing::debug!("poll_read: blocking for piece {} (pos={})", piece, pos);
                }

                // REMOVED: Do NOT reset deadline here. It resets the timer in libtorrent.
                // set_priorities() already set the deadline for this piece.
                // Letting libtorrent manage the existing deadline is correct.

                // Schedule a wakeup to check again
                // Using 50ms for slightly less aggressive polling
                let waker = cx.waker().clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    waker.wake();
                });
                return std::task::Poll::Pending;
            } else {
                if pos % (1024 * 1024) == 0 || pos < 4096 {
                    tracing::debug!(
                        "poll_read: have piece {}, proceeding to read. Pos={}",
                        piece,
                        pos
                    );
                }
            }
        }

        let rem_before = buf.remaining();
        match std::pin::Pin::new(&mut self.file).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let read = rem_before - buf.remaining();
                if read > 0 {
                    if pos % (1024 * 1024) == 0 || pos < 4096 {
                        tracing::debug!(
                            "poll_read: Read {} bytes. New pos={}",
                            read,
                            self.current_pos + read as u64
                        );
                    }
                } else {
                    tracing::debug!("poll_read: Read 0 bytes (EOF?). Pos={}", self.current_pos);
                }
                self.current_pos += read as u64;
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Pending => {
                // tracing::debug!("poll_read: Underlying file read pending");
                std::task::Poll::Pending
            }
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
        std::pin::Pin::new(&mut self.file).start_seek(position)
    }

    fn poll_complete(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        match std::pin::Pin::new(&mut self.file).poll_complete(cx) {
            std::task::Poll::Ready(Ok(pos)) => {
                self.current_pos = pos;

                let piece_idx = if self.piece_length > 0 {
                    // Correct calculation: file_offset is TRUE global offset
                    ((self.file_offset + pos) / self.piece_length) as i32
                } else {
                    -1
                };

                // Optimization: Only clear and reset priorities if we moved to a new piece window
                if piece_idx != self.last_priorities_piece {
                    // Critical for seek: Clear OLD deadlines so we don't keep downloading the old position.
                    // Concurrent streams will re-assert their priorities in their next poll_read().
                    self.handle.clear_piece_deadlines();
                    self.last_priorities_piece = -1; // Force immediate priority update via set_priorities
                    self.set_priorities(pos);
                } else {
                    tracing::debug!(
                        "poll_complete: Seek within the same priority window (piece {}), skipping update",
                        piece_idx
                    );
                }
                std::task::Poll::Ready(Ok(pos))
            }
            other => other,
        }
    }
}

fn default_stats(info_hash: &str) -> EngineStats {
    EngineStats {
        name: "Unknown".to_string(),
        info_hash: info_hash.to_string(),
        files: vec![],
        sources: vec![],
        opts: StatsOptions {
            connections: None,
            dht: true,
            growler: Growler {
                flood: 0,
                pulse: None,
            },
            handshake_timeout: None,
            path: String::new(),
            peer_search: PeerSearch {
                max: 0,
                min: 0,
                sources: vec![],
            },
            swarm_cap: SwarmCap {
                max_speed: None,
                min_peers: None,
            },
            timeout: None,
            tracker: true,
            r#virtual: false,
        },
        download_speed: 0.0,
        upload_speed: 0.0,
        downloaded: 0,
        uploaded: 0,
        unchoked: 0,
        peers: 0,
        queued: 0,
        unique: 0,
        connection_tries: 0,
        peer_search_running: false,
        stream_len: 0,
        stream_name: String::new(),
        stream_progress: 0.0,
        swarm_connections: 0,
        swarm_paused: false,
        swarm_size: 0,
    }
}

fn make_engine_stats(status: &TorrentStatus) -> EngineStats {
    EngineStats {
        name: status.name.to_string(),
        info_hash: status.info_hash.to_string(),
        files: vec![], // Would need to query files separately
        sources: vec![Source {
            last_started: String::new(),
            num_found: status.num_peers as u64,
            num_found_uniq: status.num_peers as u64,
            num_requests: 0,
            url: status.current_tracker.to_string(),
        }],
        opts: StatsOptions {
            connections: Some(status.num_peers as u64),
            dht: true,
            growler: Growler {
                flood: 0,
                pulse: None,
            },
            handshake_timeout: None,
            path: status.save_path.to_string(),
            peer_search: PeerSearch {
                max: 200,
                min: 0,
                sources: vec![],
            },
            swarm_cap: SwarmCap {
                max_speed: None,
                min_peers: None,
            },
            timeout: None,
            tracker: true,
            r#virtual: false,
        },
        download_speed: status.download_rate as f64,
        upload_speed: status.upload_rate as f64,
        downloaded: status.total_downloaded as u64,
        uploaded: status.total_uploaded as u64,
        unchoked: 0,
        peers: status.num_peers as u64,
        queued: 0,
        unique: status.num_peers as u64,
        connection_tries: 0,
        peer_search_running: !status.is_finished,
        stream_len: status.total_size as u64,
        stream_name: status.name.to_string(),
        stream_progress: if status.total_wanted > 0 {
            (status.total_wanted_done as f64) / (status.total_wanted as f64)
        } else if status.total_size > 0 {
            (status.total_done as f64) / (status.total_size as f64)
        } else {
            0.0
        },
        swarm_connections: status.num_peers as u64,
        swarm_paused: status.is_paused,
        swarm_size: (status.num_complete + status.num_incomplete) as u64,
    }
}
