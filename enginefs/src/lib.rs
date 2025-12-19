use crate::engine::Engine;
use anyhow::{Context, Result};
use librqbit::Session;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};

pub mod cache;
pub mod engine;
pub mod files;
pub mod hls;
pub mod trackers;

const ENGINE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

// Single start time for the process - used for consistent time tracking
static START_TIME: OnceLock<Instant> = OnceLock::new();

fn elapsed_secs() -> i64 {
    START_TIME.get_or_init(Instant::now).elapsed().as_secs() as i64
}

// Best trackers from ngosang/trackers/best.txt
// Best trackers from ngosang/trackers/best.txt
// Kept as fallback constant
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

pub struct EngineFS {
    session: Arc<Session>,
    engines: Arc<RwLock<HashMap<String, Arc<Engine>>>>,
    tracker_manager: Arc<crate::trackers::TrackerManager>,
    cache_dir: std::path::PathBuf,
}

impl EngineFS {
    pub async fn new() -> Result<Self> {
        // Use local directory for downloads to avoid filling up /tmp (tmpfs)
        let download_dir = std::env::current_dir()?.join("rqbit-downloads");
        tokio::fs::create_dir_all(&download_dir).await?;
        debug!(path = ?download_dir, "Storing downloads");

        let session_opts = librqbit::SessionOptions {
            listen_port_range: Some(42000..42010),
            enable_upnp_port_forwarding: true,
            persistence: Some(librqbit::SessionPersistenceConfig::Json {
                folder: Some(download_dir.clone()),
            }),
            peer_opts: Some(librqbit::PeerConnectionOptions {
                connect_timeout: Some(Duration::from_secs(5)), // Increased from 2s
                read_write_timeout: Some(Duration::from_secs(30)), // Increased from 10s
                ..Default::default()
            }),
            ..Default::default()
        };

        let session = Session::new_with_opts(download_dir.clone(), session_opts).await?;
        let engines = session.with_torrents(|iter| {
            let mut map: HashMap<String, Arc<Engine>> = HashMap::new();
            for (_id, handle) in iter {
                let info_hash = handle.info_hash().as_string();
                let engine = Arc::new(Engine::new_from_handle(
                    session.clone(),
                    handle.clone(),
                    &info_hash,
                ));
                trace!(info_hash = %info_hash, "Restored torrent");
                map.insert(info_hash, engine);
            }
            map
        });
        let engines = Arc::new(RwLock::new(engines));

        // Restore from .cache directory
        let cache_dir = download_dir.join(".cache");
        if let Ok(mut entries) = tokio::fs::read_dir(&cache_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "torrent") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let info_hash = stem.to_string();
                        // Check if already managed
                        let exists = {
                            let read = engines.read().await;
                            read.contains_key(&info_hash)
                        };

                        if !exists {
                            trace!(info_hash = %info_hash, "Restoring from cache");
                            if let Ok(bytes) = tokio::fs::read(&path).await {
                                let bytes = bytes::Bytes::from(bytes);
                                let add_torrent = librqbit::AddTorrent::from_bytes(bytes);
                                match session.add_torrent(add_torrent, None).await {
                                    Ok(response) => {
                                        // Add to engines map
                                        if let librqbit::AddTorrentResponse::Added(_, handle) =
                                            response
                                        {
                                            let engine = Arc::new(Engine::new_from_handle(
                                                session.clone(),
                                                handle,
                                                &info_hash,
                                            ));
                                            let mut write = engines.write().await;
                                            write.insert(info_hash, engine);
                                        } else if let librqbit::AddTorrentResponse::AlreadyManaged(
                                            _,
                                            handle,
                                        ) = response
                                        {
                                            let engine = Arc::new(Engine::new_from_handle(
                                                session.clone(),
                                                handle,
                                                &info_hash,
                                            ));
                                            let mut write = engines.write().await;
                                            write.insert(info_hash, engine);
                                        }
                                    }
                                    Err(e) => warn!(
                                        error = %e,
                                        "Failed to add torrent from cache"
                                    ),
                                }
                            }
                        }
                    }
                }
            }
        }

        let tracker_manager = Arc::new(crate::trackers::TrackerManager::new());

        let efs = Self {
            session,
            engines: engines.clone(),
            tracker_manager,
            cache_dir: download_dir.join(".cache"),
        };

        // Create cache dir
        tokio::fs::create_dir_all(&efs.cache_dir).await?;

        // Background task to remove inactive engines
        let engines_clone = engines.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let mut to_remove: Vec<String> = Vec::new();
                let now = elapsed_secs();

                {
                    let engines_read: tokio::sync::RwLockReadGuard<HashMap<String, Arc<Engine>>> =
                        engines_clone.read().await;
                    for (hash, engine) in engines_read.iter() {
                        let active = engine
                            .active_streams
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let last_accessed = engine
                            .last_accessed
                            .load(std::sync::atomic::Ordering::SeqCst);

                        if (now - last_accessed) as u64 > ENGINE_TIMEOUT.as_secs() && active == 0 {
                            to_remove.push(hash.clone());
                        }
                    }
                }

                if !to_remove.is_empty() {
                    let mut engines_write: tokio::sync::RwLockWriteGuard<
                        HashMap<String, Arc<Engine>>,
                    > = engines_clone.write().await;
                    for hash in to_remove {
                        println!("Auto-removing inactive engine: {}", hash);
                        engines_write.remove(&hash);
                    }
                }
            }
        });

        Ok(efs)
    }

    pub async fn add_torrent(
        &self,
        add_torrent: librqbit::AddTorrent<'static>,
        extra_trackers: Option<Vec<String>>,
    ) -> Result<Arc<Engine>> {
        let mut trackers: Vec<String> = DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect();

        // Merge dynamic trackers
        let dynamic_trackers = self.tracker_manager.get_trackers().await;
        println!(
            "[EngineFS] Dynamic trackers available: {}",
            dynamic_trackers.len()
        );

        if !dynamic_trackers.is_empty() {
            trackers.extend(dynamic_trackers);
            // Deduplicate
            trackers.sort();
            trackers.dedup();
        }
        println!(
            "[EngineFS] Total trackers for new torrent: {}",
            trackers.len()
        );
        if let Some(extra) = extra_trackers {
            trackers.extend(extra);
        }

        let mut add_torrent = add_torrent;
        let mut from_cache = false;

        // Try to check if we have a cached .torrent file for this magnet link
        if let librqbit::AddTorrent::Url(ref url) = add_torrent {
            if url.starts_with("magnet:") {
                // Extract info hash from magnet link manually to fallback safely
                let re = regex::Regex::new(r"xt=urn:btih:([a-zA-Z0-9]+)").unwrap();
                if let Some(caps) = re.captures(url) {
                    let info_hash = caps[1].to_string().to_lowercase();
                    // Convert base32 to hex if needed? librqbit usually uses hex internally.
                    // But cache filename should be consistent.
                    // If it is 32 chars, it is base32. If 40, hex.
                    // Standardize to hex for consistency?
                    // For now, let's assume hex if 40, if 32 try to decode?
                    // actually add_torrent will figure it out.
                    // We just need a consistent filename. regex match will be 'hash'.

                    let cache_path = self.cache_dir.join(format!("{}.torrent", info_hash));
                    if cache_path.exists() {
                        if let Ok(bytes) = tokio::fs::read(&cache_path).await {
                            debug!(
                                info_hash = %info_hash,
                                "Found cached metadata, loading..."
                            );
                            // Need to convert Vec<u8> to Bytes
                            add_torrent =
                                librqbit::AddTorrent::from_bytes(bytes::Bytes::from(bytes));
                            from_cache = true;
                        }
                    }
                }
            }
        }

        let response = self
            .session
            .add_torrent(
                add_torrent,
                Some(librqbit::AddTorrentOptions {
                    overwrite: true,
                    trackers: Some(trackers),
                    peer_opts: Some(librqbit::PeerConnectionOptions {
                        connect_timeout: Some(Duration::from_secs(5)), // Increased from 2s
                        read_write_timeout: Some(Duration::from_secs(30)), // Increased from 10s
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            )
            .await
            .context("Failed to add torrent")?;

        let (_id, handle) = match response {
            librqbit::AddTorrentResponse::Added(id, handle)
            | librqbit::AddTorrentResponse::AlreadyManaged(id, handle) => (id, handle),
            _ => return Err(anyhow::anyhow!("Unexpected response from add_torrent")),
        };

        // If we didn't load from cache, spawn a task to save the metadata when it becomes available
        if !from_cache {
            let info_hash_clone = handle.info_hash().as_string();
            let handle_clone = handle.clone();
            let cache_dir_clone = self.cache_dir.clone();
            tokio::spawn(async move {
                // Poll for metadata
                loop {
                    if let Some(meta) = handle_clone.metadata.load_full() {
                        #[derive(serde::Serialize)]
                        struct TorrentFile<'a, T: serde::Serialize> {
                            info: &'a T,
                        }
                        let tf = TorrentFile { info: &meta.info };
                        match serde_bencode::to_bytes(&tf) {
                            Ok(bytes) => {
                                let cache_path =
                                    cache_dir_clone.join(format!("{}.torrent", info_hash_clone));
                                if let Err(e) = tokio::fs::write(&cache_path, bytes).await {
                                    warn!(error = %e, "Failed to write cache file");
                                } else {
                                    debug!(info_hash = %info_hash_clone, "Cached metadata");
                                }
                            }
                            Err(e) => warn!(error = %e, "Failed to serialize metadata"),
                        }
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }

        // Get the actual info hash from the handle (id was just the torrent ID)
        // Id<N> has as_string() that returns hex-encoded hash
        let actual_info_hash = handle.info_hash().as_string();
        debug!(
            info_hash = %actual_info_hash,
            "add_torrent: storing engine"
        );

        let mut engines = self.engines.write().await;
        // Use actual_info_hash for lookup and insertion
        if let Some(engine) = engines.get(&actual_info_hash) {
            debug!(
                info_hash = %actual_info_hash,
                "Engine already exists"
            );
            let now = elapsed_secs();
            engine
                .last_accessed
                .store(now, std::sync::atomic::Ordering::SeqCst);
            return Ok(engine.clone());
        }

        debug!(
            info_hash = %actual_info_hash,
            "Creating new engine"
        );
        let engine = Arc::new(Engine {
            info_hash: actual_info_hash.clone(),
            session: self.session.clone(),
            handle,
            last_accessed: std::sync::atomic::AtomicI64::new(elapsed_secs()),
            active_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            probe_cache: tokio::sync::Mutex::new(HashMap::new()),
            data_cache: moka::future::Cache::builder()
                .weigher(|_key, value: &Arc<Vec<u8>>| value.len() as u32)
                .max_capacity(64 * 1024 * 1024) // 64MB cache per engine
                .build(),
        });

        engines.insert(actual_info_hash, engine.clone());
        Ok(engine)
    }

    pub async fn get_engine(&self, info_hash: &str) -> Option<Arc<Engine>> {
        let engines = self.engines.read().await;
        let lookup_hash = info_hash.to_lowercase();
        let result = engines.get(&lookup_hash).cloned();
        let found = result.is_some();
        trace!(
            info_hash = %lookup_hash,
            found = found,
            "get_engine: looking for hash"
        );
        result
    }

    /// Get an existing engine or create one from the info_hash using a magnet link.
    /// This allows HLS endpoints to lazily create engines on first access.
    pub async fn get_or_add_engine(&self, info_hash: &str) -> anyhow::Result<Arc<Engine>> {
        // First check if engine exists
        if let Some(engine) = self.get_engine(info_hash).await {
            return Ok(engine);
        }

        // Engine doesn't exist, create it from a magnet link
        debug!(
            info_hash = %info_hash,
            "get_or_add_engine: creating engine"
        );
        let magnet_url = format!("magnet:?xt=urn:btih:{}", info_hash);
        let add_torrent = librqbit::AddTorrent::from_url(magnet_url);

        self.add_torrent(add_torrent, None).await
    }

    pub async fn remove_engine(&self, info_hash: &str) {
        let mut engines = self.engines.write().await;
        engines.remove(info_hash);
    }

    pub async fn get_all_statistics(&self) -> HashMap<String, crate::engine::EngineStats> {
        let engines = self.engines.read().await;
        let mut stats_map = HashMap::new();
        for (hash, engine) in engines.iter() {
            stats_map.insert(hash.clone(), engine.get_statistics().await);
        }
        stats_map
    }

    pub async fn list_engines(&self) -> Vec<String> {
        let engines = self.engines.read().await;
        engines.keys().cloned().collect()
    }
}
