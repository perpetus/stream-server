use crate::backend::{EngineStats, PeerStat, SubtitleTrack, TorrentHandle};
use crate::cache::DataCache;
use anyhow::Context;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
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
        fallback_url: &str,
    ) -> anyhow::Result<crate::hls::ProbeResult> {
        tracing::info!(
            "[HLS STREAMING] Preparing file {} for HLS playback",
            file_idx
        );

        let cache = self.probe_cache.lock().await;
        if let Some(res) = cache.get(&file_idx) {
            tracing::debug!(
                "[HLS STREAMING] Using cached probe result for file {}",
                file_idx
            );
            return Ok(res.clone());
        }
        drop(cache); // Release lock before potentially slow operations

        // CRITICAL FIX: Prepare the file for streaming BEFORE probing
        // This ensures the target file is prioritized and initial pieces are downloaded
        // Without this, multi-file torrents fail because all files start at priority 0
        self.handle.prepare_file_for_streaming(file_idx).await?;

        // CRITICAL: Prefer local file path over HTTP URL to avoid thread starvation
        // When probing via HTTP loopback (e.g. http://127.0.0.1/stream/...),
        // ffmpeg waits for HTTP data, but the server can't serve data because
        // it's blocked waiting for the probe to finish. This creates a deadlock.
        let probe_path = if let Some(local_path) = self.handle.get_file_path(file_idx).await {
            tracing::info!("Probing via local file path: {}", local_path);
            local_path
        } else {
            tracing::info!("Probing via HTTP URL (fallback): {}", fallback_url);
            fallback_url.to_string()
        };

        let res = crate::hls::HlsEngine::probe_video(&probe_path).await?;

        // Re-acquire cache lock to store result
        let mut cache = self.probe_cache.lock().await;
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
            let file = &stats.files[guessed_file_idx];
            stats.stream_name = file.name.clone();
            stats.stream_len = file.length;

            // Note: stats.stream_progress is already populated by the backend
            // using total_wanted_done / total_wanted, which is accurate for
            // multi-file torrents where only the streaming file is "wanted".

            tracing::debug!(
                "get_statistics: file_idx={} file.progress={:.2}% total_done={} stream_progress={:.2}%",
                guessed_file_idx,
                file.progress * 100.0,
                stats.downloaded,
                stats.stream_progress * 100.0
            );
        } else {
            tracing::debug!(
                "get_statistics: guessed_file_idx {} >= stats.files.len() {}",
                guessed_file_idx,
                stats.files.len()
            );
        }

        stats
    }

    pub async fn get_file(
        self: &Arc<Self>,
        file_idx: usize,
        start_offset: u64,
        priority: u8,
    ) -> Option<FileHandle<H>> {
        tracing::info!(
            "[DIRECT STREAMING] Preparing file {} for direct playback (offset={})",
            file_idx,
            start_offset
        );

        self.last_accessed
            .store(Instant::now().elapsed().as_secs() as i64, Ordering::SeqCst);

        let files = self.handle.get_files().await;
        if file_idx >= files.len() {
            return None;
        }

        let length = files[file_idx].length;
        let name = files[file_idx].name.clone();

        // Try to recover bitrate from probe cache if available
        let bitrate = {
            let cache = self.probe_cache.lock().await;
            cache.get(&file_idx).and_then(|res| {
                // Find video stream and get its bitrate
                res.streams
                    .iter()
                    .filter(|s| s.codec_type == "video")
                    .find_map(|s| s.bitrate)
            })
        };

        if let Some(br) = bitrate {
            tracing::debug!(
                "get_file: using bitrate {} B/s from probe cache for file {}",
                br / 8,
                file_idx
            );
        }

        // CRITICAL: Prepare file for streaming BEFORE getting the reader.
        // This ensures the file has priority and initial pieces are downloaded.
        // Without this, direct stream requests fail because data isn't available.
        if let Err(e) = self.handle.prepare_file_for_streaming(file_idx).await {
            tracing::warn!("get_file: prepare_file_for_streaming failed: {}", e);
            // Continue anyway - the file reader will block on pieces as needed
        }

        let reader = self
            .handle
            .get_file_reader(file_idx, start_offset, priority, bitrate.map(|b| b / 8))
            .await
            .ok()?;

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

        let file_opt = self.get_file(file_idx, 0, 0).await;
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
        tracing::info!(
            "[SUBTITLES] find_subtitle_tracks called for info_hash={}",
            self.info_hash
        );
        let mut tracks = Vec::new();
        let files = self.handle.get_files().await;

        // 1. Find external subtitle files in the torrent
        for (idx, file) in files.iter().enumerate() {
            let filename = file.name.clone();
            let path = std::path::PathBuf::from(&filename);
            if let Some(ext) = path.extension() {
                if let Some(ext_str) = ext.to_str() {
                    let ext_lower = ext_str.to_lowercase();
                    if ["srt", "vtt", "sub", "idx", "txt", "ssa", "ass"]
                        .contains(&ext_lower.as_str())
                    {
                        tracing::info!("[SUBTITLES] Found external subtitle: {}", filename);
                        tracks.push(SubtitleTrack {
                            id: idx,
                            name: filename,
                            size: file.length,
                        });
                    }
                }
            }
        }

        // 2. Check for embedded subtitles in the main video file
        // Find the largest video file (likely the main content)
        let video_extensions = ["mkv", "mp4", "avi", "webm", "mov"];
        let video_file = files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let path = std::path::PathBuf::from(&f.name);
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| video_extensions.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false)
            })
            .max_by_key(|(_, f)| f.length);

        if let Some((file_idx, file)) = video_file {
            tracing::info!(
                "[SUBTITLES] Main video file found: {} (idx={})",
                file.name,
                file_idx
            );

            // Get local file path and probe for embedded subtitles
            if let Some(file_path) = self.handle.get_file_path(file_idx).await {
                // CRITICAL FIX: Prepare the file for streaming BEFORE probing
                // This ensures the main video file is prioritized and header pieces are downloaded
                if let Err(e) = self.handle.prepare_file_for_streaming(file_idx).await {
                    tracing::warn!("[SUBTITLES] prepare_file_for_streaming failed: {}", e);
                    // Continue anyway, probe might fail but worth a try
                }

                tracing::info!("[SUBTITLES] Probing file at path: {}", file_path);
                match probe_embedded_subtitles(&file_path).await {
                    Ok(embedded) => {
                        tracing::info!("[SUBTITLES] Probed {} embedded tracks", embedded.len());
                        for (stream_idx, lang, title) in embedded {
                            // Create a special ID format for embedded subs: 1000 + stream_idx
                            let id = 1000 + stream_idx;
                            let name = if let Some(t) = title {
                                format!("{} ({})", t, lang.unwrap_or_else(|| "und".to_string()))
                            } else {
                                format!(
                                    "Track {} ({})",
                                    stream_idx,
                                    lang.unwrap_or_else(|| "und".to_string())
                                )
                            };
                            tracks.push(SubtitleTrack {
                                id,
                                name,
                                size: 0, // Embedded subs don't have a file size
                            });
                        }
                    }
                    Err(e) => {
                        tracing::error!("[SUBTITLES] Probe failed: {}", e);
                    }
                }
            } else {
                tracing::warn!(
                    "[SUBTITLES] Could not get file path for probing idx={}",
                    file_idx
                );
            }
        } else {
            tracing::warn!("[SUBTITLES] No main video file found to probe");
        }

        tracks
    }

    pub async fn get_peer_stats(&self) -> Vec<PeerStat> {
        // Placeholder for now, handle stats() returns EngineStats which has peers count but not per-peer stats yet
        Vec::new()
    }

    /// Extract embedded subtitle track to VTT format
    pub async fn extract_embedded_subtitle(
        &self,
        file_idx: usize,
        track_id: usize,
    ) -> anyhow::Result<String> {
        // Special ID format: 1000 + stream_index
        if track_id < 1000 {
            return Err(anyhow::anyhow!("Invalid embedded track ID"));
        }
        let stream_index = track_id - 1000;

        let file_path = self
            .handle
            .get_file_path(file_idx)
            .await
            .ok_or_else(|| anyhow::anyhow!("File not found locally"))?;

        // Prepare file for streaming first to ensure we have enough data
        self.handle.prepare_file_for_streaming(file_idx).await?;

        // Use ffmpeg to extract and convert to VTT
        // ffmpeg -i input.mkv -map 0:s:INDEX -c:s webvtt -f webvtt -
        let mut cmd = tokio::process::Command::new("ffmpeg");

        // Add hardware acceleration flags if available (might speed up decoding even for subs)
        cmd.arg("-y"); // Overwrite output (though we output to stdout)
        cmd.arg("-i").arg(&file_path);

        // Map the specific subtitle stream
        // Note: stream_index from probe is absolute index among subtitles
        // So we use -map 0:s:INDEX
        cmd.args(["-map", &format!("0:s:{}", stream_index)]);

        // Convert to WebVTT
        cmd.args(["-c:s", "webvtt"]);
        cmd.args(["-f", "webvtt"]);

        // Output to stdout
        cmd.arg("-");

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("FFmpeg extraction failed: {}", stderr));
        }

        let content =
            String::from_utf8(output.stdout).context("Invalid UTF-8 in extracted subtitles")?;

        Ok(content)
    }
}

/// Helper to probe for embedded subtitles using ffprobe
async fn probe_embedded_subtitles(
    path: &str,
) -> anyhow::Result<Vec<(usize, Option<String>, Option<String>)>> {
    tracing::info!("[SUBTITLES] Executing ffprobe on: {}", path);
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "s",
            path,
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!("[SUBTITLES] ffprobe command failed: {}", stderr);
        return Err(anyhow::anyhow!("ffprobe failed"));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut tracks = Vec::new();

    if let Some(streams) = json.get("streams").and_then(|s| s.as_array()) {
        for (idx, stream) in streams.iter().enumerate() {
            // Get language and title tags
            let tags = stream.get("tags");
            let lang = tags
                .and_then(|t| t.get("language"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let title = tags
                .and_then(|t| t.get("title"))
                .and_then(|v| v.as_str())
                .map(String::from);

            // Check codec - only support text-based subs for now (ass, srt, webvtt)
            // PGS/DVD bitmap subs require OCR which ffmpeg can't do easily to text
            let codec = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            tracing::debug!("[SUBTITLES] Found stream idx={} codec={}", idx, codec);

            if ["ass", "ssa", "subrip", "webvtt", "mov_text", "text"].contains(&codec) {
                tracks.push((idx, lang, title));
            }
        }
    }

    Ok(tracks)
}
