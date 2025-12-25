use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::pin::Pin;
use std::task::Poll;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HlsProfile {
    Native,
    Browser,
}

#[derive(Debug, Clone)]
pub struct TranscodeConfig {
    pub video_codec: String,      // "libx264", "hevc_nvenc", "copy", "auto"
    pub audio_codec: String,      // "aac", "copy", "auto"
    pub bitrate: String,          // "20M", "8M"
    pub bufsize: String,          // "40M", "16M"
    pub segment_duration: f64,    // 2.0
    pub container_format: String, // "mpegts"
    pub force_transcode_video: bool,
    pub force_transcode_audio: bool,
    pub ffmpeg_flags: Vec<String>,
}

impl HlsProfile {
    pub fn get_config(&self) -> TranscodeConfig {
        match self {
            HlsProfile::Native => TranscodeConfig {
                // Native Profile: Max Quality, Trust the player
                video_codec: "auto".to_string(), // Tries copy, falls back to high quality
                audio_codec: "auto".to_string(), // Tries copy
                bitrate: "20M".to_string(),
                bufsize: "40M".to_string(),
                segment_duration: 2.0,
                container_format: "mpegts".to_string(),
                force_transcode_video: false,
                force_transcode_audio: false,
                ffmpeg_flags: vec![], // Trust default behavior
            },
            HlsProfile::Browser => TranscodeConfig {
                // Browser Profile: Max Compatibility
                // Browsers usually need H.264 + AAC
                // We use "veryfast" to ensure low latency transcoding
                // We force "yuv420p" and "main" profile for max compatibility
                video_codec: "libx264".to_string(),
                audio_codec: "aac".to_string(),
                bitrate: "8M".to_string(),
                bufsize: "16M".to_string(),
                segment_duration: 2.0,
                container_format: "mpegts".to_string(), 
                force_transcode_video: true, 
                force_transcode_audio: true,
                ffmpeg_flags: vec![
                    "-profile:v".to_string(), "main".to_string(),
                ],
            },
        }
    }
}

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

#[derive(Debug, Clone)]
pub struct TranscodeProfile {
    pub name: &'static str,
    pub input_args: Vec<&'static str>,
    pub hw_accel: &'static str, // Value for -hwaccel
    pub encoders: std::collections::HashMap<&'static str, &'static str>, // codec -> encoder name
    pub decoders: std::collections::HashMap<&'static str, &'static str>, // codec -> decoder name
    pub extra_output_args: Vec<&'static str>,
    pub preset: Option<&'static str>,
    pub pixel_format: Option<&'static str>, // e.g., nv12, yuv420p
    pub scale_filter: Option<&'static str>, // e.g., scale_cuda, scale_vaapi
}

#[derive(Debug)]
pub struct TranscodeProcess {
    pub inner: tokio::process::Child,
}

impl Drop for TranscodeProcess {
    fn drop(&mut self) {
        // Kill the ffmpeg process when the handle is dropped
        let _ = self.inner.start_kill();
        tracing::debug!("TranscodeProcess dropped, killed ffmpeg process");
    }
}

impl TranscodeProcess {
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.inner.wait().await
    }
}

pub struct TranscodeStream {
    _process: TranscodeProcess,
    stdout: tokio::process::ChildStdout,
}

impl TranscodeStream {
    pub fn new(mut process: TranscodeProcess) -> Option<Self> {
        let stdout = process.inner.stdout.take()?;
        Some(Self { _process: process, stdout })
    }
}

impl AsyncRead for TranscodeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl HlsEngine {
    fn get_hw_profiles() -> std::collections::HashMap<&'static str, TranscodeProfile> {
        let mut profiles = std::collections::HashMap::new();

        // --- NVENC (Windows) ---
        profiles.insert("nvenc-win", TranscodeProfile {
            name: "nvenc-win",
            input_args: vec![
                "-init_hw_device", "cuda=cu:0", 
                "-filter_hw_device", "cu", 
                "-hwaccel", "cuda", 
                "-hwaccel_output_format", "cuda", 
                "-threads", "1"
            ],
            hw_accel: "cuda",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_nvenc"),
                ("hevc", "hevc_nvenc"),
            ]),
            decoders: std::collections::HashMap::new(),
            extra_output_args: vec!["-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: Some("p1"),
            pixel_format: None,
            scale_filter: Some("scale_cuda"),
        });

        // --- NVENC (Linux) ---
        profiles.insert("nvenc-linux", TranscodeProfile {
            name: "nvenc-linux",
            input_args: vec![
                "-init_hw_device", "cuda=cu:0", 
                "-filter_hw_device", "cu", 
                "-hwaccel", "cuda", 
                "-hwaccel_output_format", "cuda"
            ],
            hw_accel: "cuda",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_nvenc"),
                ("hevc", "hevc_nvenc"),
            ]),
            decoders: std::collections::HashMap::from([
                ("hevc", "hevc_cuvid"),
                ("h264", "h264_cuvid"),
                ("av1", "av1_cuvid"),
                ("vp9", "vp9_cuvid"),
            ]),
            extra_output_args: vec!["-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: Some("p1"),
            pixel_format: None,
            scale_filter: Some("scale_cuda"),
        });

        // --- QSV (Windows) ---
        profiles.insert("qsv-win", TranscodeProfile {
            name: "qsv-win",
            input_args: vec![
                "-init_hw_device", "d3d11va=dx11:,vendor=0x8086", 
                "-init_hw_device", "qsv=qs@dx11", 
                "-filter_hw_device", "qs", 
                "-hwaccel", "d3d11va", 
                "-hwaccel_output_format", "d3d11", 
                "-threads", "3"
            ],
            hw_accel: "d3d11va",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_qsv"),
                ("hevc", "hevc_qsv"),
            ]),
            decoders: std::collections::HashMap::new(),
            extra_output_args: vec!["-look_ahead", "0", "-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: Some("veryfast"),
            pixel_format: Some("nv12"),
            scale_filter: Some("scale_qsv"),
        });

         // --- QSV (Linux) ---
        profiles.insert("qsv-linux", TranscodeProfile {
            name: "qsv-linux",
            input_args: vec![
                "-init_hw_device", "vaapi=va:,driver=iHD,kernel_driver=i915", 
                "-init_hw_device", "qsv=qs@va", 
                "-filter_hw_device", "qs", 
                "-hwaccel", "vaapi", 
                "-hwaccel_output_format", "vaapi"
            ],
            hw_accel: "vaapi",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_qsv"),
                ("hevc", "hevc_qsv"),
            ]),
            decoders: std::collections::HashMap::new(),
            extra_output_args: vec!["-look_ahead", "0", "-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: Some("veryfast"),
            pixel_format: Some("nv12"),
            scale_filter: Some("scale_vaapi"),
        });

        // --- AMF (Windows) ---
        profiles.insert("amf", TranscodeProfile {
            name: "amf",
            input_args: vec![
                 "-init_hw_device", "d3d11va=dx11:,vendor=0x1002", 
                 "-init_hw_device", "opencl=ocl@dx11", 
                 "-filter_hw_device", "ocl", 
                 "-hwaccel", "d3d11va", 
                 "-hwaccel_output_format", "d3d11"
            ],
            hw_accel: "d3d11va",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_amf"),
                ("hevc", "hevc_amf"),
            ]),
            decoders: std::collections::HashMap::new(),
            extra_output_args: vec!["-quality", "speed", "-rc", "cbr", "-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: None,
            pixel_format: Some("nv12"),
            scale_filter: Some("scale_opencl"),
        });

        // --- VAAPI (Linux) ---
        profiles.insert("vaapi", TranscodeProfile {
            name: "vaapi",
            input_args: vec![
                "-init_hw_device", "vaapi=va:/dev/dri/renderD128", 
                "-filter_hw_device", "va", 
                "-hwaccel", "vaapi", 
                "-hwaccel_output_format", "vaapi"
            ],
            hw_accel: "vaapi",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_vaapi"),
                ("hevc", "hevc_vaapi"),
                ("vp9", "vp9_vaapi"),
            ]),
            decoders: std::collections::HashMap::new(),
            extra_output_args: vec!["-rc_mode", "VBR", "-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: None,
            pixel_format: Some("nv12"),
            scale_filter: Some("scale_vaapi"),
        });

        // --- VideoToolbox (macOS) ---
        profiles.insert("videotoolbox", TranscodeProfile {
            name: "videotoolbox",
            input_args: vec![
                "-init_hw_device", "videotoolbox=vt", 
                "-hwaccel", "videotoolbox"
            ],
            hw_accel: "videotoolbox",
            encoders: std::collections::HashMap::from([
                ("h264", "h264_videotoolbox"),
                ("hevc", "hevc_videotoolbox"),
            ]),
            decoders: std::collections::HashMap::new(),
            extra_output_args: vec!["-b:v", "{bitrate}", "-maxrate", "{bitrate}", "-bufsize", "{bufsize}"],
            preset: None,
            pixel_format: Some("nv12"),
            scale_filter: None, 
        });

        profiles
    }

    pub async fn probe_video(file_path: &str) -> Result<ProbeResult> {
        tracing::info!("Starting probe_video for {}", file_path);
        // "Improve it without change in functionality":
        // We use ffmpeg -i as the JS did, but we use robust async process handling.

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-i").arg(file_path);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        tracing::info!("Spawning ffmpeg probe command: {:?}", cmd);
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

        tracing::info!("Reading ffmpeg stderr...");
        while let Ok(Some(line)) = reader.next_line().await {
            let line = line.trim();
            // tracing::trace!("ffmpeg stderr: {}", line);

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
                    tracing::info!("Found container: {}", container);
                }
            }

            if let Some(caps) = re_duration.captures(line) {
                let h: f64 = caps[1].parse().unwrap_or(0.0);
                let m: f64 = caps[2].parse().unwrap_or(0.0);
                let s: f64 = caps[3].parse().unwrap_or(0.0);
                duration = h * 3600.0 + m * 60.0 + s;
                tracing::info!("Found duration: {}s", duration);
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
                tracing::info!(
                    index,
                    type_str = %type_str,
                    codec = %codec_name,
                    "Probe found stream"
                );
            }
        }

        tracing::info!("Finished reading stderr, waiting for child process...");
        // We don't care about exit status being non-zero because ffmpeg -i without output always errors.
        let status = child.wait().await;
        tracing::info!("ffmpeg probe finished with status: {:?}", status);

        Ok(ProbeResult {
            duration,
            container,
            streams,
        })
    }

    pub fn get_segments(duration: f64) -> Vec<(f64, f64)> {
        // Shorter segments = faster initial load and seeking
        // 2.0s is a good balance for efficiency vs latency
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
        profile: HlsProfile,
    ) -> String {
        let config = profile.get_config();
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
            // Propagate profile to the audio playlist URL
             let mut audio_query = query_str.to_string();
             if let HlsProfile::Browser = profile {
                if !audio_query.contains("profile=browser") {
                    if !audio_query.is_empty() { audio_query.push('&'); }
                    audio_query.push_str("profile=browser");
                }
             }

            m3u.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",LANGUAGE=\"{}\",NAME=\"{}\",DEFAULT={},AUTOSELECT={},URI=\"{}/hlsv2/{}/{}/audio-{}.m3u8?{}\"\n",
                audio_group_id, lang, name, is_default, is_autoselect, base_url, info_hash, file_idx, audio.index, audio_query
            ));
        }

        // Video stream with audio group reference
        // Use bandwidth from profile
        let bandwidth = if config.bitrate.ends_with("M") {
             config.bitrate.trim_end_matches("M").parse::<u64>().unwrap_or(20) * 1_000_000
        } else {
             20_000_000
        };
        
        let codecs = match profile {
            HlsProfile::Native => "avc1.640033,mp4a.40.2", // Generic high
            HlsProfile::Browser => "avc1.4d4028,mp4a.40.2", // Main Profile Level 4.0 (Matches ffmpeg flags)
        };

        if !audio_streams.is_empty() {
            m3u.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}\",AUDIO=\"{}\"\n",
                bandwidth, codecs, audio_group_id
            ));
        } else {
            m3u.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}\"\n",
                bandwidth, codecs
            ));
        }
        // Video stream playlist URL
        let mut stream_query = query_str.to_string();
        if let HlsProfile::Browser = profile {
            if !stream_query.contains("profile=browser") {
                if !stream_query.is_empty() { stream_query.push('&'); }
                stream_query.push_str("profile=browser");
            }
        }

        m3u.push_str(&format!(
            "{}/hlsv2/{}/{}/stream-0.m3u8?{}\n",
            base_url, info_hash, file_idx, stream_query
        ));

        m3u
    }

    pub fn get_stream_playlist(
        probe: &ProbeResult,
        _stream_idx: usize,
        segment_base_url: &str,
        audio_track_idx: Option<usize>,
        query_str: &str,
        profile: HlsProfile,
    ) -> String {
        let segments = Self::get_segments(probe.duration);

        // Ensure profile is propagated in the query string
        let mut final_query = query_str.to_string();
        match profile {
            HlsProfile::Browser => {
                if !final_query.contains("profile=browser") {
                    if !final_query.is_empty() { final_query.push('&'); }
                    final_query.push_str("profile=browser");
                }
            }
            HlsProfile::Native => {
                // Optional: Force native if needed, but usually default
                 if !final_query.contains("profile=native") && !final_query.contains("profile=") {
                    // Don't clutter unless necessary
                 }
            }
        }

        let mut m3u = String::from("#EXTM3U\n#EXT-X-VERSION:4\n");
        m3u.push_str("#EXT-X-TARGETDURATION:2\n");
        m3u.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
        m3u.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

        // Use .m4s for browser (fMP4), .ts for native (MPEG-TS)
        let ext = match profile {
            HlsProfile::Browser => "m4s",
            HlsProfile::Native => "ts",
        };

        for (i, (_start, dur)) in segments.iter().enumerate() {
            m3u.push_str(&format!("#EXTINF:{},\n", dur));
            let filename = if let Some(audio_idx) = audio_track_idx {
                format!(
                    "{}audio-{}-{}.{}?{}",
                    segment_base_url, audio_idx, i, ext, final_query
                )
            } else {
                format!("{}{}.{}?{}", segment_base_url, i, ext, final_query)
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
                let os = std::env::consts::OS;

                if let Ok(output) = cmd.output().await {
                    let output_str = String::from_utf8_lossy(&output.stdout);
                    let methods: Vec<&str> = output_str.lines().map(|l| l.trim()).collect();

                    if methods.contains(&"cuda") {
                        if os == "windows" { return "nvenc-win".to_string(); }
                        else { return "nvenc-linux".to_string(); }
                    } 
                    if methods.contains(&"qsv") {
                         if os == "windows" { return "qsv-win".to_string(); }
                         else { return "qsv-linux".to_string(); }
                    }
                    if methods.contains(&"videotoolbox") {
                        return "videotoolbox".to_string(); 
                    }
                    if methods.contains(&"d3d11va") && methods.contains(&"opencl") {
                        // AMF usually requires D3D11VA and OpenCL
                        if os == "windows" { return "amf".to_string(); }
                    }
                    if methods.contains(&"vaapi") {
                        return "vaapi".to_string();
                    }
                }

                "cpu".to_string() // Fallback
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
        input_video_codec: &str,
        audio_stream_index: Option<usize>,
        is_hdr: bool,
        is_browser: bool,  // Use fMP4 for browser, MPEG-TS for native
        config: &TranscodeConfig, 
    ) -> anyhow::Result<TranscodeProcess> {
        let profile_key = Self::probe_hwaccel().await;
        let profiles = Self::get_hw_profiles();
        let profile = profiles.get(profile_key);

        let mut cmd = tokio::process::Command::new("ffmpeg");

        // Input Args (from profile or default)
        // NOTE: For browser, we skip hardware decoding because CUDA/QSV decoders
        // fail when seeking to non-keyframe positions. We still use HW encoding.
        if let Some(p) = profile {
            if is_browser {
                // Browser: use software decoding (skip hwaccel input args)
                cmd.args(["-threads", "0"]);
            } else {
                // Native: use full hardware acceleration
                cmd.args(&p.input_args);
            }
        }

        // ========== INPUT FLAGS ==========
        if is_browser {
            // Browser: higher analysis for better compatibility
            cmd.args([
                "-fflags", "+genpts",
                "-analyzeduration", "10000000", // 10s
                "-probesize", "10000000", // 10MB
            ]);
        } else {
            // Native: optimized for low latency
            cmd.args([
                "-fflags", "+genpts+discardcorrupt+nobuffer",
                "-flags", "low_delay",
                "-analyzeduration", "500000",  // 0.5s - faster start
                "-probesize", "1000000",       // 1MB - faster start
                "-noaccurate_seek",
                "-seek_timestamp", "1",
                "-mpegts_copyts", "1",
            ]);
        }

        if let Some(cont) = container {
            cmd.arg("-f").arg(cont);
            // For Matroska/WebM, allow seeking to any frame
            if cont.contains("matroska") || cont.contains("webm") {
                cmd.args(["-seek2any", "1"]);
            }
        }

        cmd.arg("-ignore_unknown");

        // Seeking strategy:
        // - Native: -ss BEFORE -i (fast input seeking)
        // - Browser: -ss AFTER -i (slow but accurate output seeking)
        if !is_browser {
            cmd.arg("-ss").arg(format!("{}", start));
        }
        cmd.arg("-i").arg(input_path);
        
        // ========== OUTPUT OPTIONS ==========

        // For browser, apply seeking after input (output seeking)
        // Also set output_ts_offset to align timestamps across segments
        if is_browser {
            cmd.arg("-ss").arg(format!("{}", start));
            // Critical: offset output timestamps to match the seek position
            // Without this, each segment starts at timestamp 0 instead of its actual position
            cmd.arg("-output_ts_offset").arg(format!("{}", start));
        }

        // 1. Map Streams (Must be after input)
        // Disable data and attachments
        cmd.args(["-map", "-0:d?", "-map", "-0:t?"]);
        
        // Map Video
        // Only map video if not explicitly set to "none"
        if target_video_codec != "none" {
            cmd.args(["-map", "0:v:0"]);
        } else {
            cmd.arg("-vn");
        }

        // Map Audio (if requested)
        if target_audio_codec != "none" {
            if let Some(a_idx) = audio_stream_index {
                // Map specific audio track
                 cmd.arg("-map").arg(format!("0:{}", a_idx));
            } else {
                 // Map default audio track
                 cmd.args(["-map", "0:a:0"]);
            }
        } else {
             // No audio requested
             cmd.arg("-an");
        }

        // Output Duration (Apply to output)
        cmd.arg("-t").arg(format!("{}", duration));

        // Disable subtitles
        cmd.arg("-sn");

        // 2. Video Codec & Settings
        if target_video_codec == "none" {
            // Do nothing, -vn already handled
        } else if target_video_codec == "copy" {
             cmd.args(["-c:v", "copy"]);
             // copying to mpegts requires bitstream filters for mp4/mkv input
             if input_video_codec == "h264" {
                 cmd.args(["-bsf:v", "h264_mp4toannexb"]);
             } else if input_video_codec == "hevc" {
                 cmd.args(["-bsf:v", "hevc_mp4toannexb"]);
             }
        } else {
             // ... encoder settings ...
             let encoder = if let Some(p) = profile {
                p.encoders.get(target_video_codec).unwrap_or(&target_video_codec)
            } else {
                target_video_codec
            };
            cmd.args(["-c:v", encoder]);

            // Add Profile Output Args (bitrate, bufsize, etc)
            if let Some(p) = profile {
                for arg in &p.extra_output_args {
                    let val = arg.replace("{bitrate}", &config.bitrate).replace("{bufsize}", &config.bufsize);
                    cmd.arg(val);
                }
                 // Preset / Tune
                if let Some(preset) = p.preset {
                    cmd.args(["-preset", preset]);
                }

                // Hardware encoder tuning differs between browser and native
                if p.name.contains("nvenc") || p.name.contains("qsv") || p.name.contains("amf") {
                    if is_browser {
                        // Browser: low-latency options
                        cmd.args(["-bf", "0"]);    // No B-frames for lower latency
                        if p.name.contains("nvenc") {
                            cmd.args(["-delay", "0"]);
                            cmd.args(["-rc", "cbr"]);
                        }
                    } else {
                        // Native: maximum quality options
                        if p.name.contains("nvenc") {
                            cmd.args([
                                "-rc", "vbr",           // Variable bitrate for quality
                                "-tune", "hq",          // High quality tuning
                                "-cq", "19",            // Lower CQ = higher quality (range 0-51)
                                "-b:v", "0",            // Let CQ control quality
                                "-profile:v", "high",   // High profile for better quality
                                "-level", "4.2",        // High level for 4K support
                            ]);
                            // Color metadata preservation
                            cmd.args([
                                "-color_primaries", "bt709",
                                "-color_trc", "bt709",
                                "-colorspace", "bt709",
                            ]);
                        } else if p.name.contains("qsv") {
                            cmd.args(["-global_quality", "20"]);
                        } else if p.name.contains("amf") {
                            cmd.args(["-quality", "quality"]);
                        }
                    }
                }
                
                // Pixel Format
                if let Some(pix_fmt) = p.pixel_format {
                    // QSV/VAAPI often handle format in filters, check exceptions if needed
                    if !p.name.contains("qsv") && !p.name.contains("vaapi") {
                        cmd.args(["-pix_fmt", pix_fmt]);
                    }
                }
            }
            
            // Software Encoder Optimization
            if encoder == "libx264" {
                 cmd.args(["-preset", "veryfast"]);
                 // Add zerolatency for software encoder on browser too
                 if is_browser {
                     cmd.args(["-tune", "zerolatency"]);
                 }
            }

            if !config.ffmpeg_flags.is_empty() {
                cmd.args(&config.ffmpeg_flags);
            }

            // 4. Muxer Settings (fMP4 / MPEGTS)
            // config.container_format is usually "mpegts"
            // cmd.args([
            //     "-f", &config.container_format,
            //     "-muxdelay", "0",
            // ]);

            // 5. Scaling / Filters
            let mut filters = Vec::new();




            if is_hdr {
                tracing::info!("Applying HDR Tone Mapping (BT.2020 -> BT.709)");
                filters.push("colorspace=all=bt709:trc=bt709:format=yuv420p".to_string());
            }
            
            if let Some(p) = profile {
                // VAAPI / QSV specific filters
                if p.name == "vaapi" {
                    filters.push("format=nv12".to_string());
                    filters.push("hwupload".to_string());
                } else if p.name.contains("qsv") {
                    filters.push("format=nv12".to_string());
                }
                
                if p.name.contains("vaapi") || p.name.contains("qsv") {
                     cmd.args(["-keyint_min", "60"]);
                }
            }

            // Standard GOP args
            cmd.args(["-g", "60"]);

             if !filters.is_empty() {
                cmd.arg("-vf").arg(filters.join(","));
            }
        }


        // Audio flags
        if target_audio_codec == "none" {
            // Do nothing
        } else if target_audio_codec == "copy" {
            cmd.arg("-c:a").arg("copy");
            // REVERSE ENGINEERED: When copying AAC to MPEG-TS, this filter is often required
            cmd.args(["-bsf:a", "aac_adtstoasc"]);
        } else {
            cmd.args([
                "-c:a",
                target_audio_codec,
                "-ac",
                "2",
                "-b:a", // Ensure decent bitrate
                "192k",
                // REVERSE ENGINEERED: -async 1 handles drift, apad fills gaps
                "-async", "1",
                "-filter:a", "apad",
                "-strict",
                "experimental",
            ]);
        }

        // Output flags
        cmd.args(["-max_muxing_queue_size", "2048"]);
        cmd.args(["-map_metadata", "-1", "-map_chapters", "-1"]);

        // Output format flags - fMP4 for browser (more robust), MPEG-TS for native
        if is_browser {
            // fMP4 (fragmented MP4) - used by old server, better for HTTP streaming
            cmd.args([
                "-movflags", "frag_keyframe+empty_moov+default_base_moof+delay_moov+dash",
                "-use_editlist", "1",
                "-avoid_negative_ts", "make_zero",
                "-f", "mp4",
                "-threads", "0",
                "pipe:1",
            ]);
        } else {
            // MPEG-TS for native clients (VLC, Stremio Shell, etc.)
            cmd.args([
                "-f", "mpegts",
                "-muxdelay", "0",
                "-muxpreload", "0",
                "-copyts",               // Preserve timestamps for sync
                "-output_ts_offset", &format!("{}", start), // Align segment timestamps
                "-pat_period", "0.1",    // Frequent PAT/PMT for mid-segment joins
                "-avoid_negative_ts", "make_zero",
                "-threads", "0",
                "pipe:1",
            ]);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit()); // See ffmpeg errors in console

        // Log the full command for debugging
        let args: Vec<String> = cmd.as_std().get_args().map(|s| s.to_string_lossy().to_string()).collect();
        tracing::info!("Running ffmpeg command: ffmpeg {}", args.join(" "));

        let child = cmd.spawn().context("Failed to spawn ffmpeg transcoder")?;
        Ok(TranscodeProcess { inner: child })
    }
}
