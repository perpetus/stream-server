//! libtorrent-rasterbar backend implementation
//!
//! Uses the libtorrent-sys crate to provide a high-performance native torrent backend.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::backend::{
    BackendFileInfo, EngineStats, FileStreamTrait, Growler, PeerSearch, Source, StatsOptions,
    SwarmCap, TorrentBackend, TorrentHandle as TorrentHandleTrait, TorrentSource,
};

use libtorrent_sys::{LibtorrentSession, SessionSettings, TorrentStatus};

/// libtorrent backend implementation
pub struct LibtorrentBackend {
    session: Arc<RwLock<LibtorrentSession>>,
    save_path: PathBuf,
}

impl LibtorrentBackend {
    /// Create a new libtorrent backend
    pub fn new(save_path: PathBuf) -> Result<Self> {
        let settings = SessionSettings {
            listen_interfaces: "0.0.0.0:42000-42010,[::]:42000-42010".to_string(),
            user_agent: "stream-server/1.0".to_string(),
            enable_dht: true,
            enable_lsd: true,
            enable_upnp: true,
            enable_natpmp: true,
            // Increased limits for better throughput
            max_connections: 500,
            max_connections_per_torrent: 200,
            download_rate_limit: 0,
            upload_rate_limit: 0,
            active_downloads: 20,
            active_seeds: 10,
            active_limit: 30,
            anonymous_mode: false,
            proxy_host: String::new(),
            proxy_port: 0,
            proxy_type: 0,
        };

        let session = LibtorrentSession::new(settings)
            .map_err(|e| anyhow!("Failed to create libtorrent session: {}", e))?;

        std::fs::create_dir_all(&save_path)?;

        Ok(Self {
            session: Arc::new(RwLock::new(session)),
            save_path,
        })
    }

    /// Create with custom settings
    pub fn with_settings(save_path: PathBuf, settings: SessionSettings) -> Result<Self> {
        let session = LibtorrentSession::new(settings)
            .map_err(|e| anyhow!("Failed to create libtorrent session: {}", e))?;

        std::fs::create_dir_all(&save_path)?;

        Ok(Self {
            session: Arc::new(RwLock::new(session)),
            save_path,
        })
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
            TorrentSource::Url(url) => session
                .add_magnet(&url, &save_path)
                .map_err(|e| anyhow!("Failed to add magnet: {}", e))?,
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
                };
                session
                    .add_torrent(&params)
                    .map_err(|e| anyhow!("Failed to add torrent: {}", e))?
            }
        };

        // Add trackers with tier based on position
        for (idx, tracker) in trackers.iter().enumerate() {
            handle.add_tracker(tracker, idx as i32);
        }

        // Disable sequential download for streaming - we manage it manually via deadlines
        // handle.set_sequential_download(true);

        Ok(LibtorrentTorrentHandle {
            session: self.session.clone(),
            info_hash: handle.info_hash(),
        })
    }

    async fn get_torrent(&self, info_hash: &str) -> Option<Self::Handle> {
        let session = self.session.read().await;
        match session.find_torrent(info_hash) {
            Ok(_) => Some(LibtorrentTorrentHandle {
                session: self.session.clone(),
                info_hash: info_hash.to_string(),
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
                if name.is_empty() {
                    None
                } else {
                    Some(name)
                }
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

        // Populate files from the handle
        let files = handle.files();
        let mut current_offset = 0u64;
        stats.files = files
            .iter()
            .map(|f| {
                let file_offset = current_offset;
                current_offset += f.size as u64;
                crate::backend::StatsFile {
                    name: f.path.to_string(),
                    path: f.path.to_string(),
                    length: f.size as u64,
                    offset: file_offset,
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

    async fn get_file_reader(&self, file_idx: usize, start_offset: u64) -> Result<Box<dyn FileStreamTrait>> {
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

        // Check if file is already complete
        let is_complete = handle.status().is_finished;
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
                    "get_file_reader: Setting PRIORITY 7 (MAX) for file idx={} name={}",
                    idx,
                    f.path
                );
                handle.set_file_priority(idx as i32, 7);
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
            // Aggressively prioritize the pieces starting from the requested offset
            // We use a deadline of 10ms to force immediate download
            let start_piece_offset = ((start_offset as f64) / (piece_length as f64)).floor() as i32;
            actual_start_piece = first_piece + start_piece_offset;

            tracing::debug!(
                "get_file_reader: Prioritizing start window from offset {} (piece {})",
                start_offset,
                actual_start_piece
            );

            for i in 0..20 {
                let p = actual_start_piece + i;
                if p <= last_piece {
                    handle.set_piece_deadline(p, 1);
                }
            }
        }

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

        let file = tokio::fs::File::open(&file_path).await?;

        Ok(Box::new(LibtorrentFileStream {
            file,
            handle: handle.clone(),
            first_piece,
            last_piece,
            piece_length,
            file_offset: (first_piece as u64) * piece_length,
            current_pos: 0,
            is_complete,               // Use actual completion status
            last_priorities_piece: if !is_complete { actual_start_piece } else { -1 },
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

        let current_piece =
            self.first_piece + ((self.file_offset + pos) / self.piece_length) as i32;

        // Efficient cache check: if we are on the same piece, do nothing
        if current_piece == self.last_priorities_piece {
            return;
        }

        // AGGRESSIVE DE-PRIORITIZATION of old window
        // If we moved to a new piece (seek or playback), cancel urgency for the old window.
        // This is the manual, safe alternative to `clear_piece_deadlines()`.
        if self.last_priorities_piece != -1 {
             let old_start = self.last_priorities_piece;
             let old_end = old_start + 20; // Assuming previous window was ~20 pieces
             
             for p in old_start..=old_end {
                 // Only reset if it's not in the NEW window (overlap protection)
                 // And check bounds
                 if p >= self.first_piece && p <= self.last_piece {
                     if p < current_piece || p > current_piece + 60 {
                         // Reset deadline to 0 (no deadline/normal priority)
                         // This tells libtorrent: "We don't need this ASAP anymore"
                         self.handle.set_piece_deadline(p, 0); 
                     }
                 }
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

        // Prioritize the immediate window (20 pieces ~ 10-40MB) ASAP
        // Increased from 10 to 20 for faster fill of TCP window
        // Reduced deadline from 10ms to 1ms for ultra-responsiveness
        for i in 0..20 {
            let p = current_piece + i;
            if p <= self.last_piece && !self.handle.have_piece(p) {
                self.handle.set_piece_deadline(p, 1);
            }
        }

        // Prioritize the next 50 pieces (buffer)
        for i in 20..70 {
            let p = current_piece + i;
            if p <= self.last_piece && !self.handle.have_piece(p) {
                // Gradient: 100ms... 
                self.handle.set_piece_deadline(p, 100 * (i - 19));
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
            let piece = self.first_piece + ((self.file_offset + pos) / self.piece_length) as i32;
            if !self.handle.have_piece(piece) {
                // Throttle log logs
                if pos % (1024 * 1024) == 0 {
                    // log roughly every MB or when stuck
                    tracing::debug!("poll_read: blocking for piece {} (pos={})", piece, pos);
                }

                // Ensure we prioritize this piece (redundant with set_priorities but safe)
                // Deadline 0 is usually interpreted as "no deadline" (infinite) by libtorrent
                // We want ASAP, so we use a small value like 1ms (was 10ms)
                self.handle.set_piece_deadline(piece, 1);

                // Schedule a wakeup to check again
                // Using 10ms for highly responsive polling during buffering
                let waker = cx.waker().clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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
                     self.first_piece + ((self.file_offset + pos) / self.piece_length) as i32
                } else {
                    -1
                };

                // Optimization: Only clear and reset priorities if we moved to a new piece window
                if piece_idx != self.last_priorities_piece {
                    // Do NOT clear old piece deadlines globally, as this breaks concurrent streams (e.g. browser acting with multiple connections).
                    // Libtorrent handles multiple deadlines fine.
                    // self.handle.clear_piece_deadlines(); 
                    self.last_priorities_piece = -1; // Force immediate priority update via set_priorities
                    self.set_priorities(pos); 
                } else {
                     tracing::debug!("poll_complete: Seek within the same priority window (piece {}), skipping update", piece_idx);
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
        stream_progress: if status.total_size > 0 {
            (status.total_done as f64) / (status.total_size as f64)
        } else {
            0.0
        },
        swarm_connections: status.num_peers as u64,
        swarm_paused: status.is_paused,
        swarm_size: (status.num_complete + status.num_incomplete) as u64,
    }
}
