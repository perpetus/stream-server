//! libtorrent-rasterbar backend implementation
//!
//! Uses the libtorrent-sys crate to provide a high-performance native torrent backend.

use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::backend::{TorrentBackend, TorrentSource};
use crate::tracker_prober::TrackerProber;

use libtorrent_sys::{LibtorrentSession, SessionSettings};

mod constants;
mod handle;
mod helpers;
mod stream;

pub use handle::LibtorrentTorrentHandle;
// pub(crate) use stream::LibtorrentFileStream;
// Explicitly re-export read_piece_from_disk for legacy/testing if needed, or just use internally
// Actually mostly internal.

use constants::DEFAULT_TRACKERS;

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
        // === FAST ALERT PUMP ===
        // Process alerts every 100ms for minimal latency on piece caching
        // This is CRITICAL for streaming - reduces cache latency from 2s to ~100ms
        let alert_session = self.session.clone();
        let alert_piece_cache = self.piece_cache.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

            // Alert type constants
            const PIECE_FINISHED_ALERT_TYPE: i32 = 69;
            const READ_PIECE_ALERT_TYPE: i32 = 45;

            loop {
                interval.tick().await;

                let mut s = alert_session.write().await;
                let alerts = s.pop_alerts();

                for alert in alerts {
                    // Handle piece_finished_alert: Request piece data from libtorrent
                    if alert.alert_type == PIECE_FINISHED_ALERT_TYPE && alert.piece_index >= 0 {
                        if let Ok(mut handle) = s.find_torrent(&alert.info_hash) {
                            let _ = handle.read_piece(alert.piece_index);
                            tracing::trace!(
                                "Fast-alert: Requested piece {} for {}",
                                alert.piece_index,
                                alert.info_hash
                            );
                        }
                    }

                    // Handle read_piece_alert: Cache piece data in memory immediately
                    if alert.alert_type == READ_PIECE_ALERT_TYPE && !alert.piece_data.is_empty() {
                        let info_hash = alert.info_hash.clone();
                        let piece_idx = alert.piece_index;
                        let piece_data = alert.piece_data.clone();
                        let cache = alert_piece_cache.clone();

                        // Cache synchronously for fastest path (data is already in memory)
                        tokio::spawn(async move {
                            cache.put_piece(&info_hash, piece_idx, piece_data).await;
                            tracing::debug!(
                                "Fast-alert: Cached piece {} for {}",
                                piece_idx,
                                info_hash
                            );
                        });
                    }
                }
            }
        });

        // === SLOW MONITOR ===
        // Handle stats, metadata, peer search, etc. every 2 seconds
        let session = self.session.clone();
        let metadata_path = self.metadata_path.clone();
        let config = self.config.clone();
        let _save_path = self.save_path.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            let mut last_reannounce: std::collections::HashMap<String, std::time::Instant> =
                std::collections::HashMap::new();

            loop {
                interval.tick().await;

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
                    if status.has_metadata {
                        let priorities = handle.get_file_priorities();
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

                        let slow_threshold = 2 * 1024 * 1024;
                        if num_peers < min_peers
                            || (status.download_rate < slow_threshold
                                && num_peers < config.peer_search.max as i32)
                        {
                            force = true;
                        }

                        let now = std::time::Instant::now();
                        let last_announce =
                            last_reannounce.entry(handle.info_hash()).or_insert(now);

                        let reannounce_interval = if !status.has_metadata {
                            std::time::Duration::from_secs(10)
                        } else if status.download_rate < slow_threshold {
                            std::time::Duration::from_secs(30)
                        } else {
                            std::time::Duration::from_secs(60)
                        };

                        if now.duration_since(*last_announce) > reannounce_interval {
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
                            // Limit handling placeholder
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
