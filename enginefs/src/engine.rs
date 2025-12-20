use crate::backend::{EngineStats, PeerStat, SubtitleTrack, TorrentHandle};
use crate::cache::DataCache;
use anyhow::Context;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncSeekExt;
use tokio::sync::Mutex;

use crate::files::FileHandle;
use regex::Regex;

#[derive(serde::Deserialize, serde::Serialize)]
pub struct SeriesInfo {
    pub season: Option<usize>,
    pub episode: Option<usize>,
}

pub struct Engine<H: TorrentHandle> {
    pub info_hash: String,
    pub handle: H,
    pub last_accessed: AtomicI64,
    pub active_streams: Arc<AtomicUsize>,
    pub probe_cache: Mutex<HashMap<usize, crate::hls::ProbeResult>>,
    pub data_cache: DataCache,
}

impl<H: TorrentHandle> Engine<H> {
    pub fn new_with_handle(handle: H, info_hash: &str) -> Self {
        let now_unix_timestamp = Instant::now().elapsed().as_secs() as i64;

        Self {
            info_hash: info_hash.to_string(),
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
        let _re = Regex::new(regex_str).ok()?;
        // This is tricky now as find_file_by_regex was librqbit specific.
        // For now, we'll assume we can list files.
        None
    }

    pub async fn guess_file_index(
        &self,
        series_info: Option<&crate::engine::SeriesInfo>,
    ) -> Option<usize> {
        let files = self.handle.get_files().await;

        let media_extensions = [
            ".mkv", ".avi", ".mp4", ".wmv", ".mov", ".mpg", ".ts", ".webm", ".flac", ".mp3",
            ".wav", ".wma", ".aac", ".ogg",
        ];

        let mut media_files = Vec::new();

        for (idx, file) in files.iter().enumerate() {
            let filename = file.name.to_lowercase();
            if media_extensions.iter().any(|ext| filename.ends_with(ext)) {
                media_files.push((idx, file.length, filename));
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
        let mut stats = self.handle.stats().await;

        let guessed_file_idx = self.guess_file_index(None).await.unwrap_or(0);

        // Get stream info for the guessed file
        if guessed_file_idx < stats.files.len() {
            stats.stream_name = stats.files[guessed_file_idx].name.clone();
            stats.stream_len = stats.files[guessed_file_idx].length;
        }

        stats
    }

    pub async fn get_file(self: &Arc<Self>, file_idx: usize) -> Option<FileHandle<H>> {
        self.last_accessed
            .store(Instant::now().elapsed().as_secs() as i64, Ordering::SeqCst);

        let files = self.handle.get_files().await;
        if file_idx >= files.len() {
            return None;
        }

        let length = files[file_idx].length;
        let name = files[file_idx].name.clone();

        let reader = self.handle.get_file_reader(file_idx).await.ok()?;

        self.active_streams.fetch_add(1, Ordering::SeqCst);

        // Use raw reader directly for better performance
        // The libtorrent backend reads from local files, caching adds overhead
        Some(FileHandle::new(length, name, reader, self.clone()))
    }

    pub async fn get_opensub_hash(self: &Arc<Self>, file_idx: usize) -> anyhow::Result<String> {
        let files = self.handle.get_files().await;
        if file_idx >= files.len() {
            return Err(anyhow::anyhow!("File not found"));
        }
        let file_len = files[file_idx].length;

        let file_opt = self.get_file(file_idx).await;
        let mut file = file_opt.context("failed to get file handle")?;

        let chunk_size = 65536u64;
        let head_size = std::cmp::min(file_len, chunk_size);
        let tail_size = std::cmp::min(file_len, chunk_size);

        let mut head = vec![0u8; head_size as usize];
        use tokio::io::AsyncReadExt;
        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.read_exact(&mut head).await?;

        let mut tail = vec![0u8; tail_size as usize];
        let start_pos = file_len.saturating_sub(chunk_size);
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
        let files = self.handle.get_files().await;
        for (idx, file) in files.iter().enumerate() {
            let filename = file.name.clone();
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
                            size: file.length,
                        });
                    }
                }
            }
        }
        tracks
    }

    pub async fn get_peer_stats(&self) -> Vec<PeerStat> {
        // Placeholder for now, handle stats() returns EngineStats which has peers count but not per-peer stats yet
        Vec::new()
    }
}
