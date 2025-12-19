use crate::cache::{CachedStream, DataCache};
use anyhow::{Context, Result};
use librqbit::{ManagedTorrent, Session};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::files::FileHandle;
use regex::Regex;

#[derive(serde::Deserialize, serde::Serialize)]
pub struct SeriesInfo {
    pub season: Option<usize>,
    pub episode: Option<usize>,
}

pub struct Engine {
    pub info_hash: String,
    pub session: Arc<Session>,
    pub handle: Arc<ManagedTorrent>,
    pub last_accessed: AtomicI64,
    pub active_streams: Arc<AtomicUsize>,
    pub probe_cache: Mutex<HashMap<usize, crate::hls::ProbeResult>>,
    pub data_cache: DataCache,
}

impl Engine {
    pub async fn new(
        session: Arc<Session>,
        add_torrent: librqbit::AddTorrent<'static>,
        info_hash: &str,
    ) -> Result<Self> {
        let response = session
            .add_torrent(
                add_torrent,
                Some(librqbit::AddTorrentOptions {
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .context("Failed to add torrent")?;

        let handle = match response {
            librqbit::AddTorrentResponse::Added(_, handle) => handle,
            librqbit::AddTorrentResponse::AlreadyManaged(_, handle) => handle,
            _ => return Err(anyhow::anyhow!("Unexpected response from add_torrent")),
        };

        // Get current time for last_accessed
        let now_unix_timestamp = Instant::now().elapsed().as_secs() as i64;

        Ok(Self {
            info_hash: info_hash.to_string(),
            session,
            handle,
            last_accessed: AtomicI64::new(now_unix_timestamp),
            active_streams: Arc::new(AtomicUsize::new(0)),
            probe_cache: Mutex::new(HashMap::new()),
            data_cache: moka::future::Cache::builder()
                .weigher(|_key, value: &Arc<Vec<u8>>| value.len() as u32)
                .max_capacity(64 * 1024 * 1024) // 64MB cache per engine
                .build(),
        })
    }

    pub fn new_from_handle(
        session: Arc<Session>,
        handle: Arc<librqbit::ManagedTorrent>,
        info_hash: &str,
    ) -> Self {
        // Get current time for last_accessed
        let now_unix_timestamp = Instant::now().elapsed().as_secs() as i64;

        Self {
            info_hash: info_hash.to_string(),
            session,
            handle,
            last_accessed: AtomicI64::new(now_unix_timestamp),
            active_streams: Arc::new(AtomicUsize::new(0)),
            probe_cache: Mutex::new(HashMap::new()),
            data_cache: moka::future::Cache::builder()
                .weigher(|_key, value: &Arc<Vec<u8>>| value.len() as u32)
                .max_capacity(64 * 1024 * 1024) // 64MB cache per engine
                .build(),
        }
    }

    pub async fn get_probe_result(
        &self,
        file_idx: usize,
        input_mri: &str,
    ) -> anyhow::Result<crate::hls::ProbeResult> {
        let mut cache = self.probe_cache.lock().await;
        if let Some(res) = cache.get(&file_idx) {
            return Ok(res.clone());
        }

        // If not cached, probe execution
        let res = crate::hls::HlsEngine::probe_video(input_mri).await?;
        cache.insert(file_idx, res.clone());
        Ok(res)
    }

    pub fn find_file_by_regex(&self, regex_str: &str) -> Option<usize> {
        let re = Regex::new(regex_str).ok()?;
        let metadata_arc = self.handle.metadata.load_full()?;

        let pos = metadata_arc
            .info
            .iter_file_details()
            .ok()?
            .position(|file| re.is_match(&file.filename.to_string().unwrap_or_default()));
        pos
    }

    pub fn guess_file_index(
        &self,
        series_info: Option<&crate::engine::SeriesInfo>,
    ) -> Option<usize> {
        let metadata_arc = self.handle.metadata.load_full()?;

        let media_extensions = [
            ".mkv", ".avi", ".mp4", ".wmv", ".mov", ".mpg", ".ts", ".webm", ".flac", ".mp3",
            ".wav", ".wma", ".aac", ".ogg",
        ];

        let mut media_files = Vec::new();

        if let Ok(files) = metadata_arc.info.iter_file_details() {
            for (idx, file) in files.enumerate() {
                let filename = file.filename.to_string().unwrap_or_default().to_lowercase();
                if media_extensions.iter().any(|ext| filename.ends_with(ext)) {
                    media_files.push((idx, file.len, filename));
                }
            }
        }

        if media_files.is_empty() {
            return None;
        }

        if let Some(series) = series_info {
            let re_sxe = Regex::new(r"[sS](\d+)[eE](\d+)").unwrap();
            let re_xx = Regex::new(r"(\d+)x(\d+)").unwrap();

            let mut candidates = Vec::new();
            for (idx, length, filename) in &media_files {
                let mut found_s = None;
                let mut found_e = None;

                if let Some(caps) = re_sxe.captures(filename) {
                    found_s = caps.get(1).and_then(|m| m.as_str().parse::<usize>().ok());
                    found_e = caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok());
                } else if let Some(caps) = re_xx.captures(filename) {
                    found_s = caps.get(1).and_then(|m| m.as_str().parse::<usize>().ok());
                    found_e = caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok());
                }

                let s_match = series.season.is_none() || found_s == series.season;
                let e_match = series.episode.is_none() || found_e == series.episode;

                if s_match && e_match && (found_s.is_some() || found_e.is_some()) {
                    candidates.push((*idx, *length));
                }
            }
            if !candidates.is_empty() {
                return candidates
                    .into_iter()
                    .max_by_key(|&(_, len)| len)
                    .map(|(idx, _)| idx);
            }
        }

        // Fallback to largest media file
        media_files
            .into_iter()
            .max_by_key(|&(_, len, _)| len)
            .map(|(idx, _, _)| idx)
    }

    pub async fn get_statistics(&self) -> EngineStats {
        self.last_accessed
            .store(Instant::now().elapsed().as_secs() as i64, Ordering::SeqCst);
        let stats = self.handle.stats();

        let (download_speed, upload_speed) = if let Some(ref live) = stats.live {
            // Log live stats occasionally or on error?
            // println!("[EngineFS] Stats live: peers={}, down={}, up={}", live.snapshot.peer_stats.live, live.download_speed.mbps, live.upload_speed.mbps);
            (
                live.download_speed.mbps * 1_048_576.0,
                live.upload_speed.mbps * 1_048_576.0,
            )
        } else {
            if let Some(e) = stats.error {
                warn!(error = ?e, "Stats Error: Torrent in error state");
            } else if stats.finished {
                // debug!("Stats: Torrent finished");
            } else {
                debug!("Stats: No live stats available (initializing?)");
            }
            (0.0, 0.0)
        };

        // Fetch real peer count from rqbit stats
        let peers = if let Some(ref live) = stats.live {
            live.snapshot.peer_stats.live as u64
        } else {
            0
        };

        let (downloaded, uploaded) = if let Some(ref live) = stats.live {
            (live.snapshot.fetched_bytes, live.snapshot.uploaded_bytes)
        } else {
            (0, 0)
        };

        let (name, total_size) = if let Some(m) = self.handle.metadata.load_full() {
            let total: u64 = m
                .info
                .iter_file_details()
                .ok()
                .into_iter()
                .flatten()
                .map(|f| f.len)
                .sum();
            (
                m.info
                    .name
                    .as_ref()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "Unknown".to_string()),
                total,
            )
        } else {
            ("Unknown".to_string(), 0)
        };

        let guessed_file_idx = self.guess_file_index(None).unwrap_or(0);

        let mut files = Vec::new();
        let mut offset = 0u64;
        if let Some(m) = self.handle.metadata.load_full() {
            if let Ok(iter) = m.info.iter_file_details() {
                for f in iter {
                    let filename = f.filename.to_string().unwrap_or_default();
                    files.push(StatsFile {
                        name: filename.clone(),
                        path: filename,
                        length: f.len,
                        offset,
                    });
                    offset += f.len;
                }
            }
        }

        // Get stream info for the guessed file
        let (stream_name, stream_len) = if guessed_file_idx < files.len() {
            (
                files[guessed_file_idx].name.clone(),
                files[guessed_file_idx].length,
            )
        } else {
            (name.clone(), total_size)
        };

        // Calculate stream progress
        let stream_progress = if total_size > 0 {
            (downloaded as f64 / total_size as f64).min(1.0)
        } else {
            0.0
        };

        EngineStats {
            name: name.clone(),
            info_hash: self.info_hash.clone(),
            files,
            sources: vec![],
            opts: StatsOptions {
                connections: None,
                dht: true,
                growler: Growler {
                    flood: 0,
                    pulse: None,
                },
                handshake_timeout: None,
                path: std::env::temp_dir()
                    .join("rqbit-downloads")
                    .to_string_lossy()
                    .to_string(),
                peer_search: PeerSearch {
                    max: 100,
                    min: 10,
                    sources: vec![format!("dht:{}", self.info_hash)],
                },
                swarm_cap: SwarmCap {
                    max_speed: None,
                    min_peers: None,
                },
                timeout: None,
                tracker: true,
                r#virtual: false,
            },
            download_speed,
            upload_speed,
            downloaded,
            uploaded,
            unchoked: peers,
            peers,
            queued: 0,
            unique: peers,
            connection_tries: 0,
            peer_search_running: true,
            stream_len,
            stream_name,
            stream_progress,
            swarm_connections: peers,
            swarm_paused: false,
            swarm_size: peers,
        }
    }

    pub async fn get_file(self: &Arc<Self>, file_idx: usize) -> Option<FileHandle> {
        self.last_accessed
            .store(Instant::now().elapsed().as_secs() as i64, Ordering::SeqCst);

        // Wait for metadata with retries
        let metadata = loop {
            if let Some(m) = self.handle.metadata.load_full() {
                break m;
            }
            debug!("Waiting for torrent metadata...");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        };

        let info = &metadata.info;
        let mut files_iter = info.iter_file_details().ok()?;
        let file_details = files_iter.nth(file_idx)?;

        let length = file_details.len;
        let name = file_details.filename.to_string().ok()?;

        // Retry streaming with backoff to handle initializing state
        let mut attempts = 0;
        let max_attempts = 20; // 10 seconds max wait
        let stream = loop {
            match self.handle.clone().stream(file_idx) {
                Ok(s) => break s,
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("initializing") && attempts < max_attempts {
                        attempts += 1;
                        debug!(attempt = attempts, "Torrent initializing, waiting...");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                    warn!(error = %e, "Error streaming file");
                    return None;
                }
            }
        };

        self.active_streams.fetch_add(1, Ordering::SeqCst);

        // Wrap stream with CachedStream
        let cached_stream =
            CachedStream::new(Box::new(stream), self.data_cache.clone(), file_idx, length);

        Some(FileHandle::new(
            length,
            name,
            Box::new(cached_stream),
            self.clone(),
        ))
    }

    pub async fn get_opensub_hash(self: &Arc<Self>, file_idx: usize) -> anyhow::Result<String> {
        // Use metadata directly
        let metadata = self.handle.metadata.load_full().context("no metadata")?;
        let details = metadata
            .info
            .iter_file_details()
            .ok()
            .context("file details error")?
            .nth(file_idx)
            .context("file not found")?;
        let file_len = details.len as u64;

        let file_opt = self.get_file(file_idx).await;
        let mut file = file_opt.context("failed to get file handle")?;

        let chunk_size = 65536u64;
        let head_size = std::cmp::min(file_len, chunk_size);
        let tail_size = std::cmp::min(file_len, chunk_size);

        let mut head = vec![0u8; head_size as usize];
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.read_exact(&mut head).await?;

        let mut tail = vec![0u8; tail_size as usize];
        let start_pos = if file_len > chunk_size {
            file_len - chunk_size
        } else {
            0
        };
        file.seek(std::io::SeekFrom::Start(start_pos)).await?;
        file.read_exact(&mut tail).await?;

        let mut hash = file_len;

        for chunk in head.chunks(8) {
            let mut buf = [0u8; 8];
            let len = chunk.len();
            buf[..len].copy_from_slice(chunk);
            hash = hash.wrapping_add(u64::from_le_bytes(buf));
        }

        for chunk in tail.chunks(8) {
            let mut buf = [0u8; 8];
            let len = chunk.len();
            buf[..len].copy_from_slice(chunk);
            hash = hash.wrapping_add(u64::from_le_bytes(buf));
        }

        Ok(format!("{:016x}", hash))
    }

    pub async fn find_subtitle_tracks(&self) -> Vec<SubtitleTrack> {
        let mut tracks = Vec::new();
        if let Some(metadata) = self.handle.metadata.load_full() {
            if let Ok(details) = metadata.info.iter_file_details() {
                for (idx, file) in details.enumerate() {
                    let filename = file.filename.to_string().unwrap_or_default();
                    let path = std::path::PathBuf::from(&filename);
                    if let Some(ext) = path.extension() {
                        if let Some(ext_str) = ext.to_str() {
                            let ext_lower = ext_str.to_lowercase();
                            if ["srt", "vtt", "sub", "idx", "txt", "ssa", "ass"]
                                .contains(&ext_lower.as_str())
                            {
                                tracks.push(SubtitleTrack {
                                    id: idx,
                                    name: filename,
                                    size: file.len as u64,
                                });
                            }
                        }
                    }
                }
            }
        }
        tracks
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleTrack {
    pub id: usize,
    pub name: String,
    pub size: u64,
}

// Stremio-compatible stats structures
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsFile {
    pub name: String,
    pub path: String,
    pub length: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Growler {
    pub flood: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pulse: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSearch {
    pub max: u64,
    pub min: u64,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmCap {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_peers: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections: Option<u64>,
    pub dht: bool,
    pub growler: Growler,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_timeout: Option<u64>,
    pub path: String,
    pub peer_search: PeerSearch,
    pub swarm_cap: SwarmCap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    pub tracker: bool,
    pub r#virtual: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub last_started: String,
    pub num_found: u64,
    pub num_found_uniq: u64,
    pub num_requests: u64,
    pub url: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStats {
    pub name: String,
    pub info_hash: String,
    pub files: Vec<StatsFile>,
    pub sources: Vec<Source>,
    pub opts: StatsOptions,
    pub download_speed: f64,
    pub upload_speed: f64,
    pub downloaded: u64,
    pub uploaded: u64,
    pub unchoked: u64,
    pub peers: u64,
    pub queued: u64,
    pub unique: u64,
    pub connection_tries: u64,
    pub peer_search_running: bool,
    pub stream_len: u64,
    pub stream_name: String,
    pub stream_progress: f64,
    pub swarm_connections: u64,
    pub swarm_paused: bool,
    pub swarm_size: u64,
}
