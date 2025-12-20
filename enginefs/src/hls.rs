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
    pub profile: Option<String>,
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

                let raw_codec_desc = caps[4].to_string(); // e.g., "hevc (Main 10)"
                let codec_name = raw_codec_desc
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_lowercase();

                // Extract profile from parens if present: "hevc (Main 10)" -> "Main 10"
                let profile = if let Some(start) = raw_codec_desc.find('(') {
                    if let Some(end) = raw_codec_desc.find(')') {
                        if start < end {
                            Some(raw_codec_desc[start + 1..end].to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

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
                    profile,
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
        // Shorter segments = faster initial load and seeking
        let segment_duration = 2.0;
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
        query_str: &str,
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
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",LANGUAGE=\"{}\",NAME=\"{}\",DEFAULT={},AUTOSELECT={},URI=\"{}/hlsv2/{}/{}/audio-{}.m3u8?{}\"\n",
                audio_group_id, lang, name, is_default, is_autoselect, base_url, info_hash, file_idx, audio.index, query_str
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
            "{}/hlsv2/{}/{}/stream-0.m3u8?{}\n",
            base_url, info_hash, file_idx, query_str
        ));

        m3u
    }

    pub fn get_stream_playlist(
        probe: &ProbeResult,
        _stream_idx: usize,
        segment_base_url: &str,
        audio_track_idx: Option<usize>,
        query_str: &str,
    ) -> String {
        let segments = Self::get_segments(probe.duration);

        let mut m3u = String::from("#EXTM3U\n#EXT-X-VERSION:4\n");
        m3u.push_str("#EXT-X-TARGETDURATION:2\n");
        m3u.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
        m3u.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

        for (i, (_start, dur)) in segments.iter().enumerate() {
            m3u.push_str(&format!("#EXTINF:{},\n", dur));
            let filename = if let Some(audio_idx) = audio_track_idx {
                format!(
                    "{}audio-{}-{}.ts?{}",
                    segment_base_url, audio_idx, i, query_str
                )
            } else {
                format!("{}{}.ts?{}", segment_base_url, i, query_str)
            };
            m3u.push_str(&format!("{}\n", filename));
        }
        m3u.push_str("#EXT-X-ENDLIST\n");
        m3u
    }

    pub async fn probe_hwaccel() -> &'static str {
        static HW_ACCEL_METHOD: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

        HW_ACCEL_METHOD
            .get_or_init(|| async {
                let mut cmd = Command::new("ffmpeg");
                cmd.arg("-hwaccels");
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::null());

                if let Ok(output) = cmd.output().await {
                    let output_str = String::from_utf8_lossy(&output.stdout);
                    let methods: Vec<&str> = output_str.lines().map(|l| l.trim()).collect();

                    // Priority list for HW acceleration
                    if methods.contains(&"cuda") {
                        return "cuda".to_string();
                    } else if methods.contains(&"vaapi") {
                        return "vaapi".to_string();
                    } else if methods.contains(&"qsv") {
                        return "qsv".to_string();
                    } else if methods.contains(&"vulkan") {
                        return "vulkan".to_string();
                    } else if methods.contains(&"videotoolbox") {
                        // macOS
                        return "videotoolbox".to_string();
                    }
                }

                "auto".to_string() // Fallback
            })
            .await
            .as_str()
    }

    pub async fn transcode_segment(
        input_path: &str,
        start: f64,
        duration: f64,
        container: Option<&str>,
        target_audio_codec: &str,
        target_video_codec: &str,
        audio_stream_index: Option<usize>,
    ) -> Result<tokio::process::Child> {
        let hwaccel_method = Self::probe_hwaccel().await;

        let mut cmd = Command::new("ffmpeg");

        // ========== OPTIMIZED INPUT FLAGS FOR LOW LATENCY ==========
        // -fflags: nobuffer (reduce buffering), genpts (generate pts), discardcorrupt, fastseek
        // -flags low_delay: force low-delay codec mode
        // -analyzeduration 500000: 0.5s analysis (needed for proper HEVC initialization)
        // -probesize 1000000: 1MB probing (needed for HEVC with Dolby Vision)
        cmd.args([
            "-hwaccel",
            hwaccel_method,
            "-fflags",
            "+genpts+discardcorrupt+nobuffer+fastseek",
            "-flags",
            "low_delay",
            "-analyzeduration",
            "500000",
            "-probesize",
            "1000000",
        ]);

        if let Some(cont) = container {
            cmd.arg("-f").arg(cont);
        }

        // Reverse-engineered flags from split_output
        cmd.args(["-fflags", "+genpts"]);
        cmd.args(["-noaccurate_seek", "-seek_timestamp", "1"]);
        // -copyts is used in split_output and is critical for correct timing with these flags
        cmd.arg("-copyts");

        // -ss BEFORE -i: Fast keyframe-based seeking (demuxer-level)
        cmd.arg("-ss").arg(format!("{}", start));
        cmd.arg("-i").arg(input_path);
        // -t after -i limits output duration
        cmd.arg("-t").arg(format!("{}", duration));

        // Always disable subtitles to prevent failures with image-based subs
        cmd.arg("-sn");

        // Stream mapping logic
        if target_video_codec == "none" {
            // Audio-only segment: Map ONLY audio (specific or 0:a:0)
            if let Some(audio_idx) = audio_stream_index {
                cmd.args(["-map", &format!("0:{}", audio_idx)]);
            } else {
                cmd.args(["-map", "0:a:0"]);
            }
            cmd.arg("-vn"); // No video
        } else if target_audio_codec == "none" {
            // Video-only segment: Map ONLY video
            cmd.args(["-map", "0:v:0"]);
            cmd.arg("-an"); // No audio
        } else {
            // Combined segment (fallback/legacy): Map both
            cmd.args(["-map", "0:v:0"]);
            if let Some(audio_idx) = audio_stream_index {
                cmd.args(["-map", &format!("0:{}", audio_idx)]);
            } else {
                cmd.args(["-map", "0:a:0"]);
            }
        }

        // Video flags
        if target_video_codec == "none" {
            // Do nothing
        } else if target_video_codec == "copy" {
            cmd.arg("-c:v").arg("copy");
        } else {
            cmd.args(["-c:v", target_video_codec]);

            // Hardware encoders have different parameter support
            let is_hw_encoder = target_video_codec.contains("nvenc")
                || target_video_codec.contains("vaapi")
                || target_video_codec.contains("qsv")
                || target_video_codec.contains("v4l2m2m")
                || target_video_codec.contains("videotoolbox");

            if is_hw_encoder {
                tracing::info!("Using hardware encoder: {}", target_video_codec);
                // Hardware encoders - use their native presets
                if target_video_codec.contains("nvenc") {
                    // NVENC H264 usually requires 8-bit input (yuv420p).
                    // Input is 10-bit HEVC, so we MUST force 8-bit conversion.
                    // Use -rc vbr -tune hq (vbr_hq is deprecated in newer ffmpeg)
                    // OPTIMIZATION: Use p2 (faster than p3) for lower latency
                    // OPTIMIZATION: Reduce bitrate to 4M (max 6M) for better streaming performance
                    cmd.args([
                        "-pix_fmt", "yuv420p", "-preset", "p2", "-rc", "vbr", "-tune", "hq", "-cq",
                        "23", "-b:v", "4M", "-maxrate", "6M", "-bufsize", "12M",
                    ]);
                } else if target_video_codec.contains("vaapi") {
                    // VAAPI requires input to be on a VAAPI surface.
                    // If input is SW decoded, we need to upload it.
                    // Also force nv12 (8-bit) to avoid compatibility issues with 10-bit H264.
                    cmd.args(["-vf", "format=nv12,hwupload"]);
                } else if target_video_codec.contains("qsv") {
                    // QSV also often expects 8-bit nv12 for H264
                    cmd.args(["-vf", "format=nv12", "-preset", "fast"]);
                }
            } else {
                // Software encoder - use full optimization flags
                cmd.args([
                    "-pix_fmt",
                    "yuv420p",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                ]);
            }
        }

        // Audio flags
        if target_audio_codec == "none" {
            // Do nothing
        } else if target_audio_codec == "copy" {
            cmd.arg("-c:a").arg("copy");
        } else {
            cmd.args([
                "-c:a",
                target_audio_codec,
                "-ac",
                "2",
                "-b:a", // Ensure decent bitrate
                "192k",
                "-strict",
                "experimental",
            ]);
        }

        // Output flags
        cmd.args(["-max_muxing_queue_size", "2048"]);
        cmd.args(["-map_metadata", "-1", "-map_chapters", "-1"]);

        // Output format flags
        // Switch to mp4/fMP4 as per split_output
        cmd.args([
            "-f",
            "mp4",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof+delay_moov+dash",
            "-use_editlist",
            "1",
            "-threads",
            "0",
            "pipe:1",
        ]);

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit()); // See ffmpeg errors in console

        tracing::info!("Running ffmpeg: {:?}", cmd);

        let child = cmd.spawn().context("Failed to spawn ffmpeg transcoder")?;
        Ok(child)
    }
}
