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
    "udp://tracker.coppersurfer.tk:6969/announce",
    "udp://tracker.leechers-paradise.org:6969/announce",
    "udp://p4p.arenabg.com:1337/announce",
    "udp://9.rarbg.me:2970/announce",
    "udp://9.rarbg.to:2710/announce",
    "udp://tracker.internetwarriors.net:1337/announce",
    "udp://tracker.cyberia.is:6969/announce",
    "udp://tracker.moeking.me:6969/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "udp://tracker.zer0day.to:1337/announce",
    "udp://tracker.leechers-paradise.org:6969/announce",
    "udp://coppersurfer.tk:6969/announce",
];

/// libtorrent backend implementation
pub struct LibtorrentBackend {
    session: Arc<RwLock<LibtorrentSession>>,
    save_path: PathBuf,
    metadata_path: PathBuf,
    config: crate::backend::BackendConfig,
    stream_counter: Arc<std::sync::atomic::AtomicUsize>,
    /// In-memory piece cache for fast streaming
    piece_cache: Arc<crate::piece_cache::PieceCacheManager>,
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
            active_downloads: 50, // Increased from 30
            active_seeds: 50,     // Increased from 20
            active_limit: 100,    // Increased from 50
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

        // Create piece cache using existing cache settings
        let piece_cache_config = crate::piece_cache::PieceCacheConfig::from_engine_config(
            &config.cache,
            save_path.join(".piece_cache"),
        );
        let piece_cache = Arc::new(crate::piece_cache::PieceCacheManager::new(
            piece_cache_config,
        ));

        let backend = Self {
            session: Arc::new(RwLock::new(session)),
            save_path,
            metadata_path,
            config,
            stream_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            piece_cache,
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

        // Create piece cache using existing cache settings
        let piece_cache_config = crate::piece_cache::PieceCacheConfig::from_engine_config(
            &config.cache,
            save_path.join(".piece_cache"),
        );
        let piece_cache = Arc::new(crate::piece_cache::PieceCacheManager::new(
            piece_cache_config,
        ));

        let backend = Self {
            session: Arc::new(RwLock::new(session)),
            save_path,
            metadata_path,
            config,
            stream_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            piece_cache,
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
            active_downloads: 50,
            active_seeds: 50,
            active_limit: 100,
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
        let piece_cache = self.piece_cache.clone();
        let save_path = self.save_path.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            let mut last_reannounce: std::collections::HashMap<String, std::time::Instant> =
                std::collections::HashMap::new();

            // Alert type constant for piece_finished_alert (libtorrent internal)
            const PIECE_FINISHED_ALERT_TYPE: i32 = 69;

            loop {
                interval.tick().await;

                // === PROACTIVE PIECE CACHING VIA ALERTS ===
                // Poll alerts and cache pieces when they finish downloading
                {
                    let mut s = session.write().await;
                    let alerts = s.pop_alerts();
                    for alert in alerts {
                        if alert.alert_type == PIECE_FINISHED_ALERT_TYPE && alert.piece_index >= 0 {
                            let info_hash = alert.info_hash.clone();
                            let piece_idx = alert.piece_index;
                            let cache = piece_cache.clone();
                            let sp = save_path.clone();

                            // Spawn background task to read piece from disk and cache it
                            tokio::spawn(async move {
                                // Find the piece data from disk
                                // This is a simplified approach - we read the piece from disk
                                // In a more optimized version, we'd get it directly from libtorrent's buffer
                                tracing::debug!(
                                    "Proactive caching: piece {} for torrent {}",
                                    piece_idx,
                                    info_hash
                                );

                                // For now, just mark that we know about this piece
                                // The full implementation would read the piece bytes here
                                // but that requires knowing piece length and file mapping
                                let _ = (cache, sp); // Silence unused warnings for now
                            });
                        }
                    }
                }

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

                        // Rule 1: Low peer count or slow speed (Aggressive)
                        // If we are slow (<2MB/s) and have fewer than max peers, try to find more.
                        let slow_threshold = 2 * 1024 * 1024; // Increased to 2MB/s
                        if num_peers < min_peers
                            || (status.download_rate < slow_threshold
                                && num_peers < config.peer_search.max as i32)
                        {
                            force = true;
                        }

                        // Rule 2: Periodic Re-announce (Aggressive)
                        let now = std::time::Instant::now();
                        let last_announce =
                            last_reannounce.entry(handle.info_hash()).or_insert(now);

                        // Metadata Burst: If we don't have metadata, re-announce every 10s (was 15s)
                        // If we have metadata but are slow, re-announce every 30s (was 60s)
                        let interval = if !status.has_metadata {
                            std::time::Duration::from_secs(10)
                        } else {
                            // If slow, being more aggressive
                            if status.download_rate < slow_threshold {
                                std::time::Duration::from_secs(30)
                            } else {
                                std::time::Duration::from_secs(60)
                            }
                        };

                        if now.duration_since(*last_announce) > interval {
                            force = true;
                            *last_announce = now;
                        }

                        if force {
                            // Don't spam logs too much, but this is an "aggressive" mode
                            // tracing::debug!("Monitor: Force re-announce for {}", handle.info_hash());
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
            piece_cache: self.piece_cache.clone(),
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
                piece_cache: self.piece_cache.clone(),
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
    /// In-memory piece cache for fast streaming
    piece_cache: Arc<crate::piece_cache::PieceCacheManager>,
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
    file: Option<tokio::fs::File>, // Made optional - file may not exist yet
    file_path: PathBuf,            // Path to file for lazy opening
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
    /// In-memory piece cache for fast streaming
    piece_cache: Arc<crate::piece_cache::PieceCacheManager>,
    /// Info hash for cache lookups
    info_hash: String,
    /// Currently cached piece data for fast serving
    cached_piece_data: Option<(i32, Arc<Vec<u8>>)>, // (piece_idx, data)
    /// Last piece we triggered prefetch for (to avoid repeated requests)
    last_prefetch_piece: i32,
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

            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

/// Read a piece from disk for prefetch caching
/// Correctly handles multi-file torrents by accounting for file_offset
async fn read_piece_from_disk(
    file_path: &PathBuf,
    piece_idx: i32,
    piece_length: u64,
    file_offset: u64,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = tokio::fs::File::open(file_path).await?;

    // Get file size to calculate valid read range
    let file_metadata = file.metadata().await?;
    let file_size = file_metadata.len();

    // For multi-file torrents:
    // - piece_idx * piece_length gives global torrent byte offset
    // - file_offset is where this file starts in the torrent
    // - We need the offset WITHIN this file
    let piece_global_start = piece_idx as u64 * piece_length;
    let piece_global_end = piece_global_start + piece_length;

    // Calculate file's range in global torrent space
    let file_global_start = file_offset;
    let file_global_end = file_offset + file_size;

    // Check if piece overlaps with this file
    if piece_global_end <= file_global_start || piece_global_start >= file_global_end {
        // Piece doesn't overlap with this file
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Piece does not overlap with this file",
        ));
    }

    // Calculate the overlap range
    let overlap_start = piece_global_start.max(file_global_start);
    let overlap_end = piece_global_end.min(file_global_end);
    let overlap_size = overlap_end - overlap_start;

    // Convert to file-relative offset
    let file_relative_offset = overlap_start - file_global_start;

    // Seek to the correct position in the file
    file.seek(std::io::SeekFrom::Start(file_relative_offset))
        .await?;

    // Read the overlapping portion
    let mut data = vec![0u8; overlap_size as usize];
    let bytes_read = file.read(&mut data).await?;
    data.truncate(bytes_read);

    Ok(data)
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
