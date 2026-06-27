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
pub mod metadata_pins;
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
const HLS_PLAYBACK_LEASE_TTL: Duration = Duration::from_secs(300);

static START_TIME: OnceLock<Instant> = OnceLock::new();

pub fn now_secs() -> u64 {
    START_TIME.get_or_init(Instant::now).elapsed().as_secs()
}

fn hls_playback_lease_ttl_secs() -> u64 {
    HLS_PLAYBACK_LEASE_TTL.as_secs()
}

fn playback_lease_is_active(lease: &PlaybackLease, now: u64) -> bool {
    lease.expires_at_secs > now
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
    /// HLS playback is made of short segment reads. A lease keeps the file wanted
    /// while the player is buffered and no response body is currently open.
    active_playback_leases: Arc<RwLock<HashMap<(String, usize), PlaybackLease>>>,
    /// Optional disk cache for persisting completed files
    disk_cache: Option<Arc<disk_cache::DiskCacheManager>>,
    /// When false, torrents are paused once their download completes.
    seeding_enabled: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct PlaybackLease {
    last_seen_secs: u64,
    expires_at_secs: u64,
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
pub struct ActivePlaybackLeaseSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
    pub last_seen_secs: u64,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamActivitySnapshot {
    pub uptime_secs: u64,
    pub engine_count: usize,
    pub engine_active_streams: usize,
    pub active_streams: HashMap<String, usize>,
    pub active_file_streams: Vec<ActiveFileStreamSnapshot>,
    pub active_file: Option<ActiveFileSnapshot>,
    pub active_playback_leases: Vec<ActivePlaybackLeaseSnapshot>,
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
            active_playback_leases: Arc::new(RwLock::new(HashMap::new())),
            disk_cache: None,
            seeding_enabled: Arc::new(AtomicBool::new(true)),
        };

        let engines_clone = engines.clone();
        let backend_clone = efs.backend.clone();
        let active_streams_clone = efs.active_streams.clone();
        let active_file_streams_clone = efs.active_file_streams.clone();
        let active_file_clone = efs.active_file.clone();
        let active_playback_leases_clone = efs.active_playback_leases.clone();
        let seeding_flag = efs.seeding_enabled.clone();
        tokio::spawn(async move {
            loop {
                // Run fairly frequently so seeding stops promptly after the
                // user disables it; torrent removal is still gated by the much
                // longer inactivity timeout below, so this only changes how
                // quickly the seeding-disabled pause reacts.
                tokio::time::sleep(Duration::from_secs(15)).await;
                let mut to_remove = Vec::new();
                let now = now_secs();

                let expired_leases = {
                    let mut leases = active_playback_leases_clone.write().await;
                    let expired = leases
                        .iter()
                        .filter(|(_, lease)| !playback_lease_is_active(lease, now))
                        .map(|(key, _)| key.clone())
                        .collect::<Vec<_>>();
                    for key in &expired {
                        leases.remove(key);
                    }
                    expired
                };
                for (info_hash, file_idx) in expired_leases {
                    tracing::info!(
                        info_hash = %info_hash,
                        file_idx,
                        ttl_secs = hls_playback_lease_ttl_secs(),
                        "HLS playback lease expired"
                    );

                    let still_active = {
                        let streams = active_file_streams_clone.read().await;
                        streams
                            .get(&(info_hash.clone(), file_idx))
                            .copied()
                            .unwrap_or(0)
                            > 0
                    };
                    if still_active {
                        continue;
                    }

                    {
                        let mut active = active_file_clone.write().await;
                        if let Some((ref h, idx)) = *active {
                            if h == &info_hash && idx == file_idx {
                                *active = None;
                            }
                        }
                    }

                    let engine = {
                        let engines = engines_clone.read().await;
                        engines.get(&info_hash).cloned()
                    };
                    if let Some(engine) = engine {
                        if let Err(err) = engine.handle.clear_file_streaming(file_idx).await {
                            tracing::warn!(
                                info_hash = %info_hash,
                                file_idx,
                                error = %err,
                                "Failed to clear file priorities after HLS playback lease expired"
                            );
                        } else {
                            tracing::info!(
                                info_hash = %info_hash,
                                file_idx,
                                "Cleared file priorities after HLS playback lease expired"
                            );
                        }
                    }
                }

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
                        let active_playback_lease_count = {
                            let leases = active_playback_leases_clone.read().await;
                            leases
                                .iter()
                                .filter(|((stream_hash, _), lease)| {
                                    stream_hash == hash && playback_lease_is_active(lease, now)
                                })
                                .count()
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
                        } else if active_playback_lease_count > 0 {
                            Some("active_playback_leases")
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
                                active_playback_lease_count,
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
                                active_playback_lease_count,
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

                // Stop seeding on finished torrents when the user has seeding
                // disabled -- WITHOUT pausing. A full pause disconnects every
                // peer, and after a long idle the swarm can't be re-acquired
                // (tracker min-announce-intervals + decayed DHT), which leaves
                // the next episode stuck at zero peers. Instead we clamp the
                // upload rate to a trickle while keeping the torrent connected,
                // so a newly-requested file downloads instantly from the peers
                // that were never dropped. on_stream_start() restores unlimited
                // upload the moment any stream begins.
                if !seeding_flag.load(Ordering::Relaxed) {
                    let read = engines_clone.read().await;
                    for (hash, engine) in read.iter() {
                        let stats = engine.handle.stats().await;

                        // `is_finished` is torrent-wide ("all wanted pieces
                        // downloaded"), not per-file. on_stream_start() bumps the
                        // new file's priority moments after the stream begins, so
                        // in a brief window a brand-new stream on an undownloaded
                        // file can coexist with is_finished still reading true.
                        // Only throttle when no active file is still downloading,
                        // so an in-progress download never loses its tit-for-tat
                        // upload (and thus its download speed).
                        let active_file_indices: Vec<usize> = {
                            let mut indices = Vec::new();
                            let streams = active_file_streams_clone.read().await;
                            indices.extend(
                                streams
                                    .iter()
                                    .filter(|((stream_hash, _), count)| {
                                        stream_hash == hash && **count > 0
                                    })
                                    .map(|((_, file_idx), _)| *file_idx),
                            );
                            drop(streams);

                            let leases = active_playback_leases_clone.read().await;
                            indices.extend(
                                leases
                                    .iter()
                                    .filter(|((stream_hash, _), lease)| {
                                        stream_hash == hash && playback_lease_is_active(lease, now)
                                    })
                                    .map(|((_, file_idx), _)| *file_idx),
                            );
                            indices.sort_unstable();
                            indices.dedup();
                            indices
                        };
                        let active_file_incomplete = active_file_indices.iter().any(|idx| {
                            stats
                                .files
                                .get(*idx)
                                .map(|f| f.progress < 1.0)
                                .unwrap_or(true)
                        });
                        if active_file_incomplete {
                            continue;
                        }

                        // Only throttle a torrent that has FINISHED downloading
                        // its wanted data and is therefore purely seeding. A
                        // torrent still downloading -- or a freshly added magnet
                        // without metadata yet -- must keep full upload so its
                        // tit-for-tat download speed is unaffected.
                        if !stats.has_metadata || !stats.is_finished {
                            continue;
                        }

                        // Only act on a real transition so we don't redundantly
                        // re-clamp / re-log every 15s.
                        if engine.upload_throttled.swap(true, Ordering::Relaxed) {
                            continue;
                        }

                        tracing::info!(
                            info_hash = %hash,
                            upload_speed = stats.upload_speed,
                            "Throttling seeding on idle finished torrent (seeding disabled)"
                        );
                        if let Err(e) = engine.handle.set_upload_throttled(true).await {
                            tracing::warn!(
                                info_hash = %hash,
                                error = %e,
                                "Failed to throttle idle torrent upload"
                            );
                            engine.upload_throttled.store(false, Ordering::Relaxed);
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
        let now = now_secs();
        let active_playback_leases = self
            .active_playback_leases
            .read()
            .await
            .iter()
            .filter(|(_, lease)| playback_lease_is_active(lease, now))
            .map(
                |((info_hash, file_idx), lease)| ActivePlaybackLeaseSnapshot {
                    info_hash: info_hash.clone(),
                    file_idx: *file_idx,
                    last_seen_secs: lease.last_seen_secs,
                    expires_in_secs: lease.expires_at_secs.saturating_sub(now),
                },
            )
            .collect();

        StreamActivitySnapshot {
            uptime_secs: now,
            engine_count,
            engine_active_streams,
            active_streams,
            active_file_streams,
            active_file,
            active_playback_leases,
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
        self.activate_file(&info_hash, file_idx, false, "stream")
            .await;

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

    async fn activate_file(
        &self,
        info_hash: &str,
        file_idx: usize,
        keep_file_downloading: bool,
        source: &'static str,
    ) {
        if let Some(engine) = self.get_engine(info_hash).await {
            engine.touch();

            // Restore full upload the moment any stream begins. While seeding is
            // off, a finished idle torrent has its upload clamped to a trickle;
            // a starting stream means the user is watching/about-to-download, so
            // we want full tit-for-tat speed back. Because the torrent was only
            // throttled (never paused), its peers are still connected, so the
            // new file downloads immediately -- no swarm re-acquisition needed.
            if engine.upload_throttled.swap(false, Ordering::Relaxed) {
                if let Err(err) = engine.handle.set_upload_throttled(false).await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        file_idx,
                        error = %err,
                        source,
                        "Failed to restore torrent upload for active stream"
                    );
                    engine.upload_throttled.store(true, Ordering::Relaxed);
                }
            }

            // Resume any torrent that is still paused. We no longer pause for
            // seeding (we throttle instead), but a torrent may have been paused
            // by an older build or by libtorrent state, and a stream cannot make
            // progress while paused.
            if let Err(err) = engine.handle.resume_torrent().await {
                tracing::warn!(
                    info_hash = %info_hash,
                    file_idx,
                    error = %err,
                    source,
                    "Failed to resume torrent for active stream"
                );
            }

            if keep_file_downloading {
                if let Err(err) = engine.handle.keep_file_downloading(file_idx).await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        file_idx,
                        error = %err,
                        source,
                        "Failed to keep HLS playback file downloading"
                    );
                }
            }
        }

        {
            let mut active = self.active_file.write().await;
            *active = Some((info_hash.to_string(), file_idx));
        }
    }

    /// Refresh an HLS playback lease. HLS segment reads are short-lived, so this
    /// keeps the requested file wanted while the player is buffered.
    pub async fn refresh_hls_playback(
        &self,
        info_hash: &str,
        file_idx: usize,
        source: &'static str,
    ) {
        let info_hash = info_hash.to_lowercase();
        let now = now_secs();
        {
            let mut leases = self.active_playback_leases.write().await;
            leases.insert(
                (info_hash.clone(), file_idx),
                PlaybackLease {
                    last_seen_secs: now,
                    expires_at_secs: now.saturating_add(hls_playback_lease_ttl_secs()),
                },
            );
        }

        self.activate_file(&info_hash, file_idx, true, source).await;

        tracing::debug!(
            info_hash = %info_hash,
            file_idx,
            source,
            ttl_secs = hls_playback_lease_ttl_secs(),
            "HLS playback lease refreshed"
        );
    }

    /// Refresh a lease only if playback is already known to be active. This is
    /// used by stats.json so a progress poll cannot create a new download.
    pub async fn refresh_existing_hls_playback(
        &self,
        info_hash: &str,
        file_idx: usize,
        source: &'static str,
    ) -> bool {
        let info_hash = info_hash.to_lowercase();
        let now = now_secs();
        let refreshed = {
            let mut leases = self.active_playback_leases.write().await;
            let key = (info_hash.clone(), file_idx);
            match leases.get_mut(&key) {
                Some(lease) if playback_lease_is_active(lease, now) => {
                    lease.last_seen_secs = now;
                    lease.expires_at_secs = now.saturating_add(hls_playback_lease_ttl_secs());
                    true
                }
                Some(_) => {
                    leases.remove(&key);
                    false
                }
                None => false,
            }
        };

        if refreshed {
            self.activate_file(&info_hash, file_idx, true, source).await;
            tracing::debug!(
                info_hash = %info_hash,
                file_idx,
                source,
                "Existing HLS playback lease refreshed"
            );
        }

        refreshed
    }

    /// End an HLS playback lease immediately when the client sends an explicit
    /// destroy signal. Silent page closes are handled by lease expiry.
    pub async fn end_hls_playback(&self, info_hash: &str, file_idx: usize, reason: &'static str) {
        let info_hash = info_hash.to_lowercase();
        let removed = {
            let mut leases = self.active_playback_leases.write().await;
            leases.remove(&(info_hash.clone(), file_idx)).is_some()
        };

        if removed {
            tracing::info!(
                info_hash = %info_hash,
                file_idx,
                reason,
                "HLS playback lease ended"
            );
            self.schedule_file_cleanup(info_hash.clone(), file_idx);
            self.schedule_torrent_pause(info_hash);
        }
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

    /// Promptly throttle seeding shortly after the last stream on a torrent
    /// ends (the periodic loop is the slower backstop). Never pauses -- pausing
    /// disconnects peers and breaks the next episode's download.
    fn schedule_torrent_pause(&self, info_hash: String) {
        let engines = self.engines.clone();
        let active_streams = self.active_streams.clone();
        let active_file_streams = self.active_file_streams.clone();
        let active_playback_leases = self.active_playback_leases.clone();
        let seeding_enabled = self.seeding_enabled.clone();

        tokio::spawn(async move {
            tokio::time::sleep(INACTIVE_TORRENT_PAUSE_GRACE).await;

            // Throttling only matters for the seeding-disabled policy. When
            // seeding is enabled we never clamp upload.
            if seeding_enabled.load(Ordering::Relaxed) {
                return;
            }

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
            let playback_active = {
                let now = now_secs();
                let leases = active_playback_leases.read().await;
                leases.iter().any(|((hash, _), lease)| {
                    hash == &info_hash && playback_lease_is_active(lease, now)
                })
            };
            if hash_active || file_active || playback_active {
                tracing::debug!(
                    info_hash = %info_hash,
                    hash_active,
                    file_active,
                    playback_active,
                    "Skipping seeding throttle because stream activity resumed"
                );
                return;
            }

            let engine = {
                let engines = engines.read().await;
                engines.get(&info_hash).cloned()
            };
            if let Some(engine) = engine {
                engine.touch();
                // Only throttle a FINISHED torrent (purely seeding). One still
                // downloading must keep full upload for tit-for-tat speed.
                if !engine.handle.is_finished().await {
                    return;
                }
                if engine.upload_throttled.swap(true, Ordering::Relaxed) {
                    return;
                }
                tracing::info!(
                    info_hash = %info_hash,
                    "Throttling seeding on inactive finished torrent (seeding disabled)"
                );
                if let Err(err) = engine.handle.set_upload_throttled(true).await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        error = %err,
                        "Failed to throttle inactive torrent upload after grace period"
                    );
                    engine.upload_throttled.store(false, Ordering::Relaxed);
                }
            }
        });
    }

    fn schedule_file_cleanup(&self, info_hash: String, file_idx: usize) {
        let engines = self.engines.clone();
        let active_file = self.active_file.clone();
        let active_file_streams = self.active_file_streams.clone();
        let active_playback_leases = self.active_playback_leases.clone();

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
            let playback_active = {
                let now = now_secs();
                let leases = active_playback_leases.read().await;
                leases
                    .get(&key)
                    .map(|lease| playback_lease_is_active(lease, now))
                    .unwrap_or(false)
            };
            if playback_active {
                tracing::info!(
                    info_hash = %info_hash,
                    file_idx,
                    "Skipping delayed cleanup because HLS playback lease is active"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackendFileInfo, EngineStats, FileStreamTrait, Growler, PeerSearch, PieceReadiness,
        StatsFile, StatsOptions, SwarmCap,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[derive(Default)]
    struct FakeCounters {
        keep_file_downloading: AtomicUsize,
        clear_file_streaming: AtomicUsize,
        resume_torrent: AtomicUsize,
    }

    #[derive(Clone)]
    struct FakeHandle {
        info_hash: String,
        counters: Arc<FakeCounters>,
    }

    struct FakeBackend {
        handle: FakeHandle,
    }

    #[async_trait::async_trait]
    impl TorrentBackend for FakeBackend {
        type Handle = FakeHandle;

        async fn add_torrent(
            &self,
            _source: TorrentSource,
            _trackers: Vec<String>,
        ) -> Result<Self::Handle> {
            Ok(self.handle.clone())
        }

        async fn get_torrent(&self, info_hash: &str) -> Option<Self::Handle> {
            (info_hash == self.handle.info_hash).then(|| self.handle.clone())
        }

        async fn remove_torrent(&self, _info_hash: &str) -> Result<()> {
            Ok(())
        }

        async fn list_torrents(&self) -> Vec<String> {
            vec![self.handle.info_hash.clone()]
        }

        async fn memory_diagnostics(&self) -> BackendMemoryDiagnostics {
            BackendMemoryDiagnostics::default()
        }
    }

    #[async_trait::async_trait]
    impl TorrentHandle for FakeHandle {
        fn info_hash(&self) -> String {
            self.info_hash.clone()
        }

        fn name(&self) -> Option<String> {
            Some("fake".to_string())
        }

        async fn stats(&self) -> EngineStats {
            EngineStats {
                name: "fake".to_string(),
                info_hash: self.info_hash.clone(),
                files: vec![StatsFile {
                    name: "video.mkv".to_string(),
                    path: "video.mkv".to_string(),
                    length: 100,
                    offset: 0,
                    downloaded: 50,
                    progress: 0.5,
                }],
                sources: vec![],
                opts: StatsOptions {
                    connections: None,
                    dht: true,
                    growler: Growler::default(),
                    handshake_timeout: None,
                    path: String::new(),
                    peer_search: PeerSearch::default(),
                    swarm_cap: SwarmCap::default(),
                    timeout: None,
                    tracker: true,
                    r#virtual: false,
                },
                download_speed: 0.0,
                upload_speed: 0.0,
                downloaded: 50,
                uploaded: 0,
                unchoked: 0,
                peers: 0,
                queued: 0,
                unique: 0,
                connection_tries: 0,
                peer_search_running: false,
                stream_len: 100,
                stream_name: "video.mkv".to_string(),
                stream_progress: 0.5,
                swarm_connections: 0,
                swarm_paused: false,
                swarm_size: 0,
                is_finished: false,
                has_metadata: true,
            }
        }

        async fn add_trackers(&self, _trackers: Vec<String>) -> Result<()> {
            Ok(())
        }

        async fn resume_torrent(&self) -> Result<()> {
            self.counters.resume_torrent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn keep_file_downloading(&self, _file_idx: usize) -> Result<()> {
            self.counters
                .keep_file_downloading
                .fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn get_file_reader(
            &self,
            _file_idx: usize,
            _start_offset: u64,
            _priority: u8,
            _bitrate: Option<u64>,
            _intent: crate::backend::priorities::PlaybackIntent,
        ) -> Result<Box<dyn FileStreamTrait>> {
            anyhow::bail!("not implemented")
        }

        async fn get_files(&self) -> Vec<BackendFileInfo> {
            vec![BackendFileInfo {
                name: "video.mkv".to_string(),
                length: 100,
            }]
        }

        async fn get_file_path(&self, _file_idx: usize) -> Option<String> {
            None
        }

        async fn prepare_file_for_streaming(&self, _file_idx: usize) -> Result<()> {
            Ok(())
        }

        async fn clear_file_streaming(&self, _file_idx: usize) -> Result<()> {
            self.counters
                .clear_file_streaming
                .fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait_for_piece_ready(
            &self,
            _file_idx: usize,
            _offset: u64,
            _timeout: Duration,
            _intent: crate::backend::priorities::PlaybackIntent,
        ) -> Result<PieceReadiness> {
            Ok(PieceReadiness {
                ready: true,
                piece: 0,
                ready_pieces: 1,
                target_pieces: 1,
                elapsed_ms: 0,
                peers: 0,
                download_rate: 0,
                reason: "fake".to_string(),
            })
        }
    }

    fn test_enginefs() -> (BackendEngineFS<FakeBackend>, Arc<FakeCounters>) {
        let counters = Arc::new(FakeCounters::default());
        let handle = FakeHandle {
            info_hash: TEST_HASH.to_string(),
            counters: counters.clone(),
        };
        let mut restored = HashMap::new();
        restored.insert(TEST_HASH.to_string(), handle.clone());
        let root = std::env::temp_dir().join("enginefs-hls-lease-tests");
        let enginefs = BackendEngineFS::new_with_backend(
            FakeBackend { handle },
            restored,
            root.join("cache"),
            root.join("downloads"),
        );
        (enginefs, counters)
    }

    #[tokio::test]
    async fn refresh_existing_hls_playback_does_not_create_lease() {
        let (enginefs, counters) = test_enginefs();

        let refreshed = enginefs
            .refresh_existing_hls_playback(TEST_HASH, 0, "stats-json")
            .await;

        assert!(!refreshed);
        assert_eq!(counters.keep_file_downloading.load(Ordering::SeqCst), 0);
        assert!(
            enginefs
                .stream_activity_snapshot()
                .await
                .active_playback_leases
                .is_empty()
        );
    }

    #[tokio::test]
    async fn refresh_hls_playback_creates_lease_and_keeps_file_wanted() {
        let (enginefs, counters) = test_enginefs();

        enginefs.refresh_hls_playback(TEST_HASH, 0, "test").await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_playback_leases.len(), 1);
        assert_eq!(counters.keep_file_downloading.load(Ordering::SeqCst), 1);
        assert_eq!(counters.resume_torrent.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot.active_file.map(|active| active.file_idx), Some(0));
    }

    #[tokio::test]
    async fn active_hls_lease_prevents_delayed_cleanup() {
        let (enginefs, counters) = test_enginefs();

        enginefs.refresh_hls_playback(TEST_HASH, 0, "test").await;
        enginefs.schedule_file_cleanup(TEST_HASH.to_string(), 0);
        tokio::time::sleep(Duration::from_secs(6)).await;

        assert_eq!(counters.clear_file_streaming.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn expired_hls_lease_allows_delayed_cleanup() {
        let (enginefs, counters) = test_enginefs();

        enginefs.refresh_hls_playback(TEST_HASH, 0, "test").await;
        {
            let mut leases = enginefs.active_playback_leases.write().await;
            leases
                .get_mut(&(TEST_HASH.to_string(), 0))
                .unwrap()
                .expires_at_secs = now_secs();
        }
        enginefs.schedule_file_cleanup(TEST_HASH.to_string(), 0);
        tokio::time::sleep(Duration::from_secs(6)).await;

        assert_eq!(counters.clear_file_streaming.load(Ordering::SeqCst), 1);
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

        // When seeding is turned back on, lift the upload throttle from any
        // torrent the seeding-disabled policy had clamped, so it can seed at
        // full speed again. (Turning seeding off is handled lazily by the
        // periodic loop / schedule_torrent_pause.)
        if enabled {
            let engines = self.engines.clone();
            tokio::spawn(async move {
                let read = engines.read().await;
                for engine in read.values() {
                    if engine.upload_throttled.swap(false, Ordering::Relaxed) {
                        if let Err(err) = engine.handle.set_upload_throttled(false).await {
                            tracing::warn!(
                                info_hash = %engine.info_hash,
                                error = %err,
                                "Failed to restore torrent upload after re-enabling seeding"
                            );
                            engine.upload_throttled.store(true, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    }

    pub fn seeding_enabled(&self) -> bool {
        self.seeding_enabled.load(Ordering::Relaxed)
    }

    /// Mark the torrent as active without pausing other active torrents.
    pub async fn focus_torrent(&self, target_info_hash: &str) {
        self.backend.set_streaming_mode(true).await;
        // focus_torrent resumes the torrent and reannounces to the swarm. Only
        // do that when the torrent still needs the swarm (not finished) or when
        // seeding is enabled. A finished torrent with seeding disabled is served
        // from disk and must stay paused so it is not re-seeded.
        let needs_swarm = match self.get_engine(&target_info_hash.to_lowercase()).await {
            Some(engine) => !engine.handle.is_finished().await,
            None => false,
        };
        if needs_swarm || self.seeding_enabled.load(Ordering::Relaxed) {
            self.backend.focus_torrent(target_info_hash).await;
        }
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
