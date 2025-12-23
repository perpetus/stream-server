use crate::engine::Engine;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::debug;

pub mod backend;
pub mod cache;
pub mod engine;
pub mod files;
pub mod hls;
pub mod tracker_prober;
pub mod trackers;

#[cfg(all(feature = "librqbit", not(feature = "libtorrent")))]
use crate::backend::librqbit::LibrqbitBackend;
#[cfg(feature = "libtorrent")]
use crate::backend::libtorrent::LibtorrentBackend;

use crate::backend::{TorrentBackend, TorrentHandle, TorrentSource};

const ENGINE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

static START_TIME: OnceLock<Instant> = OnceLock::new();

fn elapsed_secs() -> i64 {
    START_TIME.get_or_init(Instant::now).elapsed().as_secs() as i64
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
        let mut engines_map = HashMap::new();
        for (hash, handle) in restored_handles {
            engines_map.insert(
                hash.clone(),
                Arc::new(Engine::new_with_handle(handle, &hash)),
            );
        }

        let engines = Arc::new(RwLock::new(engines_map));
        let tracker_manager = Arc::new(crate::trackers::TrackerManager::new());

        let efs = Self {
            backend: Arc::new(backend),
            engines: engines.clone(),
            tracker_manager,
            cache_dir,
            download_dir: download_dir.clone(),
        };

        let engines_clone = engines.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let mut to_remove = Vec::new();
                let now = elapsed_secs();

                {
                    let read = engines_clone.read().await;
                    for (hash, engine) in read.iter() {
                        let active = engine
                            .active_streams
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let last = engine
                            .last_accessed
                            .load(std::sync::atomic::Ordering::SeqCst);
                        if (now - last) as u64 > ENGINE_TIMEOUT.as_secs() && active == 0 {
                            to_remove.push(hash.clone());
                        }
                    }
                }

                if !to_remove.is_empty() {
                    let mut write = engines_clone.write().await;
                    for hash in to_remove {
                        debug!(info_hash = %hash, "Auto-removing inactive engine");
                        write.remove(&hash);
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
        let mut trackers: Vec<String> = DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect();
        if let Some(extra) = extra_trackers {
            trackers.extend(extra);
        }
        trackers.sort();
        trackers.dedup();

        let tracker_manager = self.tracker_manager.clone();

        // Handle magnets if source is URL
        if let TorrentSource::Url(ref url) = source {
            if url.starts_with("magnet:") {
                // We'll modify the URL to inject trackers if needed, but for now passing as is
                // or rebuilding it if we were modifying it.
                // The previous logic modified the magnet string.
            }
        }

        let handle = self.backend.add_torrent(source, trackers).await?;
        let info_hash = handle.info_hash();

        // Background Ranking
        let handle_clone = handle.clone();
        tokio::spawn(async move {
            let dynamic = tracker_manager.get_trackers().await;
            if !dynamic.is_empty() {
                let ranked = crate::tracker_prober::TrackerProber::rank_trackers(dynamic).await;
                if !ranked.is_empty() {
                    let _ = handle_clone.add_trackers(ranked).await;
                }
            }
        });

        let mut engines = self.engines.write().await;
        if let Some(engine) = engines.get(&info_hash) {
            engine
                .last_accessed
                .store(elapsed_secs(), std::sync::atomic::Ordering::SeqCst);
            return Ok(engine.clone());
        }

        let engine = Arc::new(Engine::new_with_handle(handle, &info_hash));
        engines.insert(info_hash.clone(), engine.clone());
        Ok(engine)
    }

    pub async fn get_engine(&self, info_hash: &str) -> Option<Arc<Engine<B::Handle>>> {
        let engines = self.engines.read().await;
        engines.get(&info_hash.to_lowercase()).cloned()
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
}

#[cfg(all(feature = "librqbit", not(feature = "libtorrent")))]
impl BackendEngineFS<LibrqbitBackend> {
    pub async fn new(root_dir: std::path::PathBuf, _cache_config: EngineCacheConfig) -> Result<Self> {
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
    pub async fn new(root_dir: std::path::PathBuf, config: crate::backend::BackendConfig) -> Result<Self> {
        let download_dir = root_dir.join("libtorrent-downloads");
        let backend = LibtorrentBackend::new(download_dir.clone(), config)?;
        // For libtorrent we don't restore handles automatically yet in this simple impl
        Ok(Self::new_with_backend(
            backend,
            HashMap::new(),
            download_dir.clone(),
            download_dir,
        ))
    }

    /// Update session settings dynamically (called when user changes torrent profile)
    pub async fn update_speed_profile(&self, profile: &crate::backend::TorrentSpeedProfile) {
        self.backend.update_session_settings(profile).await;
    }
}

