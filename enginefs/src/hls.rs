use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::trace;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoStream {
    pub index: usize,
    pub codec_type: String, // "video" or "audio"
    pub codec_name: String, // "h264", "aac", etc.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bitrate: Option<u64>,
    pub fps: Option<f64>,
    pub lang: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub duration: f64, // seconds
    pub container: String,
    pub streams: Vec<VideoStream>,
}

#[derive(Clone)]
pub struct HlsEngine;

impl HlsEngine {
    pub async fn probe_video(file_path: &str) -> Result<ProbeResult> {
        // "Improve it without change in functionality":
        // We use ffmpeg -i as the JS did, but we use robust async process handling.

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-i").arg(file_path);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn ffmpeg")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;
        let mut reader = BufReader::new(stderr).lines();

        let mut duration = 0.0;
        let mut container = "unknown".to_string();
        let mut streams = Vec::new();

        // Regexes matching the original JS parsing logic but adapted for Rust
        let re_duration = Regex::new(r"Duration: (\d{2}):(\d{2}):(\d{2}(\.\d+)?)")?;
        let re_input = Regex::new(r"Input #0, ([^,]+),")?; // "Input #0, matroska,webm,"
        let re_stream = Regex::new(r"Stream #\d+:(\d+)(?:\(([^)]+)\))?: (Video|Audio): ([^,]+)")?;
        // Detailed stream parsing regexes
        let re_dim = Regex::new(r"(\d{3,4})x(\d{3,4})")?;
        let re_fps = Regex::new(r"(\d+(\.\d+)?) fps")?;
        let re_bitrate = Regex::new(r"(\d+) kb/s")?;

        while let Ok(Some(line)) = reader.next_line().await {
            let line = line.trim();

            if let Some(caps) = re_input.captures(line) {
                if let Some(formats) = caps.get(1) {
                    let fmts = formats.as_str().to_lowercase();
                    if fmts.contains("mp4") {
                        container = "mp4".to_string();
                    } else if fmts.contains("matroska") {
                        container = "matroska".to_string();
                    } else {
                        container = fmts.split(',').next().unwrap_or("unknown").to_string();
                    }
                }
            }

            if let Some(caps) = re_duration.captures(line) {
                let h: f64 = caps[1].parse().unwrap_or(0.0);
                let m: f64 = caps[2].parse().unwrap_or(0.0);
                let s: f64 = caps[3].parse().unwrap_or(0.0);
                duration = h * 3600.0 + m * 60.0 + s;
            }

            if let Some(caps) = re_stream.captures(line) {
                let index: usize = caps[1].parse().unwrap_or(0);
                let lang = caps.get(2).map(|m| m.as_str().to_string());
                let type_str = caps[3].to_lowercase();
                let codec_name = caps[4]
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_lowercase();

                let width = re_dim.captures(line).and_then(|c| c[1].parse().ok());
                let height = re_dim.captures(line).and_then(|c| c[2].parse().ok());
                let fps = re_fps.captures(line).and_then(|c| c[1].parse().ok());
                let bitrate = re_bitrate
                    .captures(line)
                    .and_then(|c| c[1].parse().ok().map(|kb: u64| kb * 1000));

                let is_default = line.contains("(default)");

                streams.push(VideoStream {
                    index,
                    codec_type: type_str.clone(),
                    codec_name: codec_name.clone(),
                    width,
                    height,
                    bitrate,
                    fps,
                    lang,
                    is_default,
                });
                trace!(
                    index,
                    type_str = %type_str,
                    codec = %codec_name,
                    "Probe found stream"
                );
            }
        }

        // We don't care about exit status being non-zero because ffmpeg -i without output always errors.
        let _ = child.wait().await;

        Ok(ProbeResult {
            duration,
            container,
            streams,
        })
    }

    pub fn get_segments(duration: f64) -> Vec<(f64, f64)> {
        let segment_duration = 6.0;
        let count = (duration / segment_duration).ceil() as usize;
        let mut segments = Vec::new();
        for i in 0..count {
            let start = i as f64 * segment_duration;
            let dur = if start + segment_duration > duration {
                duration - start
            } else {
                segment_duration
            };
            segments.push((start, dur));
        }
        segments
    }

    pub fn get_master_playlist(
        probe: &ProbeResult,
        info_hash: &str,
        file_idx: usize,
        base_url: &str,
    ) -> String {
        let mut m3u = String::from("#EXTM3U\n#EXT-X-VERSION:4\n");

        // Collect audio streams from probe
        let audio_streams: Vec<&VideoStream> = probe
            .streams
            .iter()
            .filter(|s| s.codec_type == "audio")
            .collect();

        // Generate EXT-X-MEDIA entries for each audio track
        let audio_group_id = "audio";
        for (i, audio) in audio_streams.iter().enumerate() {
            let lang = audio.lang.as_deref().unwrap_or("und");
            let fallback_name = format!("Audio {}", i + 1);
            let name = audio.lang.as_deref().unwrap_or(&fallback_name);
            let is_default = if i == 0 || audio.is_default {
                "YES"
            } else {
                "NO"
            };
            let is_autoselect = is_default;

            // Each audio track gets its own stream playlist with audio index
            m3u.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",LANGUAGE=\"{}\",NAME=\"{}\",DEFAULT={},AUTOSELECT={},URI=\"{}/hlsv2/{}/{}/audio-{}.m3u8\"\n",
                audio_group_id, lang, name, is_default, is_autoselect, base_url, info_hash, file_idx, audio.index
            ));
        }

        // Video stream with audio group reference
        let bandwidth = 5_000_000;
        if !audio_streams.is_empty() {
            m3u.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"avc1.42E01E,mp4a.40.2\",AUDIO=\"{}\"\n",
                bandwidth, audio_group_id
            ));
        } else {
            m3u.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"avc1.42E01E,mp4a.40.2\"\n",
                bandwidth
            ));
        }
        m3u.push_str(&format!(
            "{}/hlsv2/{}/{}/stream-0.m3u8\n",
            base_url, info_hash, file_idx
        ));

        m3u
    }

    pub fn get_stream_playlist(
        probe: &ProbeResult,
        _stream_idx: usize,
        segment_base_url: &str,
    ) -> String {
        let segments = Self::get_segments(probe.duration);
        let target_duration = 6;

        let mut m3u = String::from("#EXTM3U\n#EXT-X-VERSION:4\n");
        m3u.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));
        m3u.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
        m3u.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

        for (i, (_start, dur)) in segments.iter().enumerate() {
            m3u.push_str(&format!("#EXTINF:{},\n", dur));
            m3u.push_str(&format!("{}{}.ts\n", segment_base_url, i));
        }

        m3u.push_str("#EXT-X-ENDLIST\n");
        m3u
    }

    pub async fn transcode_segment(
        input_path: &str,
        start: f64,
        duration: f64,
        container: Option<&str>,
        transcode_audio: bool,
        audio_stream_index: Option<usize>,
    ) -> Result<tokio::process::Child> {
        let mut cmd = Command::new("ffmpeg");

        // Input flags optimization
        cmd.args(&["-analyzeduration", "0", "-probesize", "1000000"]); // Reduce analysis time

        if let Some(cont) = container {
            cmd.arg("-f").arg(cont);
        }

        cmd.arg("-ss").arg(format!("{}", start));
        cmd.arg("-t").arg(format!("{}", duration));
        cmd.arg("-i").arg(input_path);

        // Stream mapping - select specific audio track if provided
        if let Some(audio_idx) = audio_stream_index {
            cmd.args(&["-map", "0:v:0"]); // Map first video stream
            cmd.args(&["-map", &format!("0:{}", audio_idx)]); // Map specific audio stream
        }

        // Video flags
        cmd.args(&[
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
        ]);

        // Audio flags
        if transcode_audio {
            // Transcode to AAC Stereo for browser compatibility (handles Dolby etc.)
            cmd.args(&["-c:a", "aac", "-ac", "2", "-strict", "experimental"]);
        } else {
            // Copy audio stream as-is (efficient for native players that support it)
            cmd.arg("-c:a").arg("copy");
        }

        // Output flags
        cmd.args(&[
            "-copyts",
            "-mpegts_copyts",
            "1",
            "-f",
            "mpegts",
            "-threads",
            "0",
            "pipe:1",
        ]);

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit()); // inherit stderr for easier debugging in server logs

        let child = cmd.spawn().context("Failed to spawn ffmpeg transcoder")?;
        Ok(child)
    }
}
