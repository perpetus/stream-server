use crate::engine::Engine;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::debug;

pub mod backend;
pub mod cache;
pub mod disk_cache;
pub mod engine;
pub mod files;
pub mod hls;
pub mod hwaccel;
pub mod metadata_cache;
pub mod piece_cache;
pub mod piece_waiter;
pub mod subtitles;
pub mod tracker_prober;
pub mod trackers;

// Re-export TrackerStorage for use by server crate
pub use trackers::TrackerStorage;

#[cfg(all(feature = "librqbit", not(feature = "libtorrent")))]
use crate::backend::librqbit::LibrqbitBackend;
#[cfg(feature = "libtorrent")]
use crate::backend::libtorrent::LibtorrentBackend;

use crate::backend::{BackendMemoryDiagnostics, TorrentBackend, TorrentHandle, TorrentSource};

const INACTIVE_TORRENT_REMOVE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
const INACTIVE_TORRENT_PAUSE_GRACE: Duration = Duration::from_secs(15);

static START_TIME: OnceLock<Instant> = OnceLock::new();

pub fn now_secs() -> u64 {
    START_TIME.get_or_init(Instant::now).elapsed().as_secs()
}

const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://9.rarbg.com:2810/announce",
    "udp://tracker.openbittorrent.com:80/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "udp://opentracker.i2p.rocks:6969/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://tracker.moeking.me:6969/announce",
    "udp://ipv4.tracker.harry.lu:80/announce",
];

pub struct BackendEngineFS<B: TorrentBackend> {
    pub backend: Arc<B>,
    engines: Arc<RwLock<HashMap<String, Arc<Engine<B::Handle>>>>>,
    tracker_manager: Arc<crate::trackers::TrackerManager>,
    pub cache_dir: std::path::PathBuf,
    pub download_dir: std::path::PathBuf,
    /// Track active streams per info_hash for legacy compatibility
    active_streams: Arc<RwLock<HashMap<String, usize>>>,
    /// Track active requests per specific streamed file so cleanup does not race probe retries.
    active_file_streams: Arc<RwLock<HashMap<(String, usize), usize>>>,
    /// Tracks the most recently active streamed file for legacy diagnostics.
    /// Active scheduling is driven by active_file_streams so several torrents can stream at once.
    active_file: Arc<RwLock<Option<(String, usize)>>>,
    /// Optional disk cache for persisting completed files
    disk_cache: Option<Arc<disk_cache::DiskCacheManager>>,
    /// When false, torrents are paused once their download completes.
    seeding_enabled: Arc<AtomicBool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActiveFileStreamSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActiveFileSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamActivitySnapshot {
    pub uptime_secs: u64,
    pub engine_count: usize,
    pub engine_active_streams: usize,
    pub active_streams: HashMap<String, usize>,
    pub active_file_streams: Vec<ActiveFileStreamSnapshot>,
    pub active_file: Option<ActiveFileSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineDiagnosticsSnapshot {
    pub uptime_secs: u64,
    pub streams: StreamActivitySnapshot,
    pub memory: BackendMemoryDiagnostics,
}

#[cfg(all(feature = "librqbit", not(feature = "libtorrent")))]
pub type EngineFS = BackendEngineFS<LibrqbitBackend>;

#[cfg(feature = "libtorrent")]
pub type EngineFS = BackendEngineFS<LibtorrentBackend>;

impl<B: TorrentBackend + 'static> BackendEngineFS<B> {
    pub fn new_with_backend(
        backend: B,
        restored_handles: HashMap<String, B::Handle>,
        cache_dir: std::path::PathBuf,
        download_dir: std::path::PathBuf,
    ) -> Self {
        Self::new_with_backend_and_storage(backend, restored_handles, cache_dir, download_dir, None)
    }

    pub fn new_with_backend_and_storage(
        backend: B,
        restored_handles: HashMap<String, B::Handle>,
        cache_dir: std::path::PathBuf,
        download_dir: std::path::PathBuf,
        tracker_storage: Option<Arc<dyn crate::trackers::TrackerStorage>>,
    ) -> Self {
        let mut engines_map = HashMap::new();
        for (hash, handle) in restored_handles {
            engines_map.insert(
                hash.clone(),
                Arc::new(Engine::new_with_handle(handle, &hash)),
            );
        }

        let engines = Arc::new(RwLock::new(engines_map));

        // Create tracker manager with or without storage
        let tracker_manager = match tracker_storage {
            Some(storage) => Arc::new(crate::trackers::TrackerManager::new_with_storage(storage)),
            None => Arc::new(crate::trackers::TrackerManager::new()),
        };

        let efs = Self {
            backend: Arc::new(backend),
            engines: engines.clone(),
            tracker_manager,
            cache_dir,
            download_dir: download_dir.clone(),
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            active_file_streams: Arc::new(RwLock::new(HashMap::new())),
            active_file: Arc::new(RwLock::new(None)),
            disk_cache: None,
            seeding_enabled: Arc::new(AtomicBool::new(true)),
        };

        let engines_clone = engines.clone();
        let backend_clone = efs.backend.clone();
        let active_streams_clone = efs.active_streams.clone();
        let active_file_streams_clone = efs.active_file_streams.clone();
        let active_file_clone = efs.active_file.clone();
        let seeding_flag = efs.seeding_enabled.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let mut to_remove = Vec::new();
                let now = now_secs();

                {
                    let read = engines_clone.read().await;
                    for (hash, engine) in read.iter() {
                        let engine_active_streams = engine
                            .active_streams
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let last = engine
                            .last_accessed
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let age_secs = now.saturating_sub(last);
                        if age_secs <= INACTIVE_TORRENT_REMOVE_TIMEOUT.as_secs() {
                            continue;
                        }

                        let active_stream_count = {
                            let streams = active_streams_clone.read().await;
                            streams.get(hash).copied().unwrap_or(0)
                        };
                        let active_file_stream_count = {
                            let streams = active_file_streams_clone.read().await;
                            streams
                                .iter()
                                .filter(|((stream_hash, _), _)| stream_hash == hash)
                                .map(|(_, count)| *count)
                                .sum::<usize>()
                        };
                        let active_file_matches = {
                            let active = active_file_clone.read().await;
                            active
                                .as_ref()
                                .map(|(stream_hash, _)| stream_hash == hash)
                                .unwrap_or(false)
                        };

                        let skip_reason = if engine_active_streams > 0 {
                            Some("engine_active_streams")
                        } else if active_stream_count > 0 {
                            Some("active_streams")
                        } else if active_file_stream_count > 0 {
                            Some("active_file_streams")
                        } else if active_file_matches {
                            Some("active_file")
                        } else {
                            None
                        };

                        if let Some(skip_reason) = skip_reason {
                            tracing::debug!(
                                info_hash = %hash,
                                age_secs,
                                engine_active_streams,
                                active_stream_count,
                                active_file_stream_count,
                                removed = false,
                                skip_reason,
                                "Skipping inactive-engine cleanup"
                            );
                        } else {
                            tracing::debug!(
                                info_hash = %hash,
                                age_secs,
                                engine_active_streams,
                                active_stream_count,
                                active_file_stream_count,
                                removed = true,
                                "Scheduling inactive-engine cleanup"
                            );
                            to_remove.push(hash.clone());
                        }
                    }
                }

                if !to_remove.is_empty() {
                    let mut write = engines_clone.write().await;
                    for hash in &to_remove {
                        debug!(info_hash = %hash, "Auto-removing inactive engine");
                        write.remove(hash);
                    }
                    drop(write);

                    // Actually stop the torrents in the backend session
                    for hash in to_remove {
                        if let Err(e) = backend_clone.remove_torrent(&hash).await {
                            tracing::warn!(
                                info_hash = %hash,
                                error = %e,
                                removed = false,
                                "Failed to remove inactive torrent from backend"
                            );
                        } else {
                            tracing::info!(
                                info_hash = %hash,
                                removed = true,
                                "Removed inactive torrent from backend"
                            );
                        }
                    }
                }

                // Pause finished torrents when seeding is disabled
                if !seeding_flag.load(Ordering::Relaxed) {
                    let read = engines_clone.read().await;
                    for (hash, engine) in read.iter() {
                        // Never pause while anything is still streaming from this
                        // torrent: pausing kicks all peers and the active stream
                        // immediately resumes, creating a pause/resume war that
                        // throttles playback.
                        let engine_active = engine
                            .active_streams
                            .load(std::sync::atomic::Ordering::SeqCst)
                            > 0;
                        let hash_active = {
                            let streams = active_streams_clone.read().await;
                            streams.get(hash).copied().unwrap_or(0) > 0
                        };
                        let file_active = {
                            let streams = active_file_streams_clone.read().await;
                            streams
                                .iter()
                                .any(|((stream_hash, _), count)| stream_hash == hash && *count > 0)
                        };
                        if engine_active || hash_active || file_active {
                            continue;
                        }

                        let stats = engine.handle.stats().await;
                        if stats.stream_progress >= 1.0 && !stats.swarm_paused {
                            tracing::info!(
                                info_hash = %hash,
                                "Pausing finished torrent (seeding disabled)"
                            );
                            if let Err(e) = engine.handle.pause_torrent().await {
                                tracing::warn!(
                                    info_hash = %hash,
                                    error = %e,
                                    "Failed to pause finished torrent"
                                );
                            }
                        }
                    }
                }
            }
        });

        efs
    }

    pub async fn add_torrent(
        &self,
        source: TorrentSource,
        extra_trackers: Option<Vec<String>>,
    ) -> Result<Arc<Engine<B::Handle>>> {
        // Start with default trackers
        let mut trackers: Vec<String> = DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect();

        // Add cached trackers from tracker manager (already ranked by RTT)
        let cached_trackers = self.tracker_manager.get_trackers().await;
        trackers.extend(cached_trackers);

        // Add any extra trackers provided
        if let Some(extra) = extra_trackers {
            trackers.extend(extra);
        }
        trackers.sort();
        trackers.dedup();

        debug!(count = trackers.len(), "Adding torrent with trackers");

        let handle = self.backend.add_torrent(source, trackers).await?;
        let info_hash = handle.info_hash();

        let mut engines = self.engines.write().await;
        if let Some(engine) = engines.get(&info_hash) {
            engine.touch();
            return Ok(engine.clone());
        }

        let engine = Arc::new(Engine::new_with_handle(handle, &info_hash));
        engines.insert(info_hash.clone(), engine.clone());
        Ok(engine)
    }

    pub async fn get_engine(&self, info_hash: &str) -> Option<Arc<Engine<B::Handle>>> {
        let engines = self.engines.read().await;
        let engine = engines.get(&info_hash.to_lowercase()).cloned();
        if let Some(engine) = &engine {
            engine.touch();
        }
        engine
    }

    pub async fn get_or_add_engine(&self, info_hash: &str) -> Result<Arc<Engine<B::Handle>>> {
        if let Some(engine) = self.get_engine(info_hash).await {
            return Ok(engine);
        }
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        self.add_torrent(TorrentSource::Url(magnet), None).await
    }

    pub async fn remove_engine(&self, info_hash: &str) {
        let mut engines = self.engines.write().await;
        engines.remove(&info_hash.to_lowercase());
    }

    pub async fn get_all_statistics(&self) -> HashMap<String, crate::backend::EngineStats> {
        let engines = self.engines.read().await;
        let mut stats = HashMap::new();
        for (hash, engine) in engines.iter() {
            stats.insert(hash.clone(), engine.get_statistics().await);
        }
        stats
    }

    pub async fn list_engines(&self) -> Vec<String> {
        let engines = self.engines.read().await;
        engines.keys().cloned().collect()
    }

    pub async fn stream_activity_snapshot(&self) -> StreamActivitySnapshot {
        let engines = self.engines.read().await;
        let engine_count = engines.len();
        let engine_active_streams = engines
            .values()
            .map(|engine| {
                engine
                    .active_streams
                    .load(std::sync::atomic::Ordering::SeqCst)
            })
            .sum();
        drop(engines);

        let active_streams = self.active_streams.read().await.clone();
        let active_file_streams = self
            .active_file_streams
            .read()
            .await
            .iter()
            .map(|((info_hash, file_idx), count)| ActiveFileStreamSnapshot {
                info_hash: info_hash.clone(),
                file_idx: *file_idx,
                count: *count,
            })
            .collect();
        let active_file = self
            .active_file
            .read()
            .await
            .as_ref()
            .map(|(info_hash, file_idx)| ActiveFileSnapshot {
                info_hash: info_hash.clone(),
                file_idx: *file_idx,
            });

        StreamActivitySnapshot {
            uptime_secs: now_secs(),
            engine_count,
            engine_active_streams,
            active_streams,
            active_file_streams,
            active_file,
        }
    }

    pub async fn diagnostics_snapshot(&self) -> EngineDiagnosticsSnapshot {
        let streams = self.stream_activity_snapshot().await;
        let memory = self.backend.memory_diagnostics().await;

        EngineDiagnosticsSnapshot {
            uptime_secs: now_secs(),
            streams,
            memory,
        }
    }

    /// Called when a stream starts for a torrent file.
    /// Several torrent files may be active at once; cleanup is per file stream.
    pub async fn on_stream_start(&self, info_hash: &str, file_idx: usize) {
        let info_hash = info_hash.to_lowercase();
        if let Some(engine) = self.get_engine(&info_hash).await {
            engine.touch();
            if let Err(err) = engine.handle.resume_torrent().await {
                tracing::warn!(
                    info_hash = %info_hash,
                    file_idx,
                    error = %err,
                    "Failed to resume torrent for active stream"
                );
            }
        }

        {
            let mut active = self.active_file.write().await;
            *active = Some((info_hash.clone(), file_idx));
        }

        // Also update legacy active_streams counter
        {
            let mut streams = self.active_streams.write().await;
            let count = streams.entry(info_hash.clone()).or_insert(0);
            *count += 1;
        }
        {
            let mut streams = self.active_file_streams.write().await;
            let count = streams.entry((info_hash.clone(), file_idx)).or_insert(0);
            *count += 1;
        }

        tracing::info!(
            "Stream started for {} file_idx={} (shared mode)",
            info_hash,
            file_idx
        );
    }

    /// Called when a stream ends for a torrent file
    pub async fn on_stream_end(&self, info_hash: &str, file_idx: usize) {
        let info_hash = info_hash.to_lowercase();

        let hash_streams_remaining = {
            let mut streams = self.active_streams.write().await;
            if let Some(count) = streams.get_mut(&info_hash) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    streams.remove(&info_hash);
                    0
                } else {
                    *count
                }
            } else {
                0
            }
        };

        let file_streams_remaining = {
            let mut streams = self.active_file_streams.write().await;
            let key = (info_hash.clone(), file_idx);
            if let Some(count) = streams.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    streams.remove(&key);
                    0
                } else {
                    *count
                }
            } else {
                0
            }
        };

        if let Some(engine) = self.get_engine(&info_hash).await {
            // Reset idle age so removal happens after the stream becomes
            // inactive, not after the stream originally started.
            engine.touch();
        }

        if hash_streams_remaining == 0 && file_streams_remaining == 0 {
            self.schedule_torrent_pause(info_hash.clone());
        }

        if file_streams_remaining == 0 {
            self.schedule_file_cleanup(info_hash.clone(), file_idx);
        }

        let remaining = self.active_streams.read().await.values().sum::<usize>();
        tracing::info!(
            "Stream ended for {} file_idx={}, total active streams: {}, file streams remaining: {}",
            info_hash,
            file_idx,
            remaining,
            file_streams_remaining
        );
    }

    fn schedule_torrent_pause(&self, info_hash: String) {
        let engines = self.engines.clone();
        let active_streams = self.active_streams.clone();
        let active_file_streams = self.active_file_streams.clone();

        tokio::spawn(async move {
            tokio::time::sleep(INACTIVE_TORRENT_PAUSE_GRACE).await;

            let hash_active = {
                let streams = active_streams.read().await;
                streams.get(&info_hash).copied().unwrap_or(0) > 0
            };
            let file_active = {
                let streams = active_file_streams.read().await;
                streams
                    .iter()
                    .any(|((hash, _), count)| hash == &info_hash && *count > 0)
            };
            if hash_active || file_active {
                tracing::debug!(
                    info_hash = %info_hash,
                    hash_active,
                    file_active,
                    "Skipping torrent pause because stream activity resumed"
                );
                return;
            }

            let engine = {
                let engines = engines.read().await;
                engines.get(&info_hash).cloned()
            };
            if let Some(engine) = engine {
                engine.touch();
                if let Err(err) = engine.handle.pause_torrent().await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        error = %err,
                        "Failed to pause inactive torrent after grace period"
                    );
                }
            }
        });
    }

    fn schedule_file_cleanup(&self, info_hash: String, file_idx: usize) {
        let engines = self.engines.clone();
        let active_file = self.active_file.clone();
        let active_file_streams = self.active_file_streams.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;

            let key = (info_hash.clone(), file_idx);
            let still_active = {
                let streams = active_file_streams.read().await;
                streams.get(&key).copied().unwrap_or(0) > 0
            };
            if still_active {
                tracing::debug!(
                    "Skipping delayed cleanup for {} idx={} because a new stream started",
                    info_hash,
                    file_idx
                );
                return;
            }

            {
                let mut active = active_file.write().await;
                if let Some((ref h, idx)) = *active {
                    if h == &info_hash && idx == file_idx {
                        tracing::info!(
                            "Delayed cleanup: clearing active file for {} file_idx={}",
                            info_hash,
                            file_idx
                        );
                        *active = None;
                    }
                }
            }

            let engine = {
                let engines = engines.read().await;
                engines.get(&info_hash).cloned()
            };
            if let Some(engine) = engine {
                if let Err(e) = engine.handle.clear_file_streaming(file_idx).await {
                    tracing::warn!(
                        "Failed to clear file priorities for {} idx={}: {}",
                        info_hash,
                        file_idx,
                        e
                    );
                } else {
                    tracing::info!(
                        "Delayed cleanup: cleared file priorities for {} idx={}",
                        info_hash,
                        file_idx
                    );
                }
            }
        });
    }

    /// Get a reference to the backend for direct access
    pub fn get_backend(&self) -> &Arc<B> {
        &self.backend
    }
}

#[cfg(all(feature = "librqbit", not(feature = "libtorrent")))]
impl BackendEngineFS<LibrqbitBackend> {
    pub async fn new(
        root_dir: std::path::PathBuf,
        _cache_config: EngineCacheConfig,
    ) -> Result<Self> {
        let download_dir = root_dir.join("rqbit-downloads");
        let (backend, restored) = LibrqbitBackend::new(download_dir.clone()).await?;
        Ok(Self::new_with_backend(
            backend,
            restored,
            root_dir.join("cache"),
            download_dir,
        ))
    }
}

#[cfg(feature = "libtorrent")]
impl BackendEngineFS<LibtorrentBackend> {
    pub async fn new(
        root_dir: std::path::PathBuf,
        config: crate::backend::BackendConfig,
    ) -> Result<Self> {
        Self::new_with_storage(root_dir, config, None).await
    }

    pub async fn new_with_storage(
        root_dir: std::path::PathBuf,
        config: crate::backend::BackendConfig,
        tracker_storage: Option<Arc<dyn crate::trackers::TrackerStorage>>,
    ) -> Result<Self> {
        let download_dir = root_dir.join("libtorrent-downloads");
        let cache_size = config.cache.size;
        let backend = LibtorrentBackend::new(download_dir.clone(), config)?;

        let mut efs = Self::new_with_backend_and_storage(
            backend,
            HashMap::new(),
            download_dir.clone(),
            download_dir,
            tracker_storage,
        );

        // Set up disk cache for conditional file persistence
        let disk_cache_dir = root_dir.join("disk-cache");
        efs.disk_cache = Some(Arc::new(disk_cache::DiskCacheManager::new(
            disk_cache_dir,
            cache_size,
        )));

        Ok(efs)
    }

    pub async fn new_disk_backed(
        root_dir: std::path::PathBuf,
        config: crate::backend::BackendConfig,
        tracker_storage: Option<Arc<dyn crate::trackers::TrackerStorage>>,
    ) -> Result<Self> {
        let download_dir = root_dir.join("torrent-cache");
        let backend = LibtorrentBackend::new_disk_backed(download_dir.clone(), config)?;

        Ok(Self::new_with_backend_and_storage(
            backend,
            HashMap::new(),
            download_dir.clone(),
            download_dir,
            tracker_storage,
        ))
    }

    /// Update session settings dynamically (called when user changes torrent profile)
    pub async fn update_speed_profile(&self, profile: &crate::backend::TorrentSpeedProfile) {
        self.backend.update_session_settings(profile).await;
    }

    pub fn set_seeding_enabled(&self, enabled: bool) {
        self.seeding_enabled.store(enabled, Ordering::Relaxed);
        tracing::info!(seeding_enabled = enabled, "Seeding policy updated");
    }

    pub fn seeding_enabled(&self) -> bool {
        self.seeding_enabled.load(Ordering::Relaxed)
    }

    /// Mark the torrent as active without pausing other active torrents.
    pub async fn focus_torrent(&self, target_info_hash: &str) {
        self.backend.set_streaming_mode(true).await;
        self.backend.focus_torrent(target_info_hash).await;
    }

    /// Resume all paused torrents (called when streaming ends)
    /// Also disables streaming mode (restores normal upload)
    pub async fn resume_all_torrents(&self) {
        self.backend.resume_all_torrents().await;
    }

    /// Pause all torrents (called when no active streams remain)
    pub async fn pause_all_torrents(&self) {
        self.backend.pause_all_torrents().await;
    }

    /// Enable or disable streaming mode (limits uploads during streaming)
    pub async fn set_streaming_mode(&self, enabled: bool) {
        self.backend.set_streaming_mode(enabled).await;
    }
}
