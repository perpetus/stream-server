use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

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
            pixel_format: Some("yuv420p"),
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
            pixel_format: Some("yuv420p"),
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
        let bandwidth = 20_000_000;
        if !audio_streams.is_empty() {
            m3u.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"avc1.640033,mp4a.40.2\",AUDIO=\"{}\"\n",
                bandwidth, audio_group_id
            ));
        } else {
            m3u.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"avc1.640033,mp4a.40.2\"\n",
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
        audio_stream_index: Option<usize>,
        is_hdr: bool,
    ) -> anyhow::Result<tokio::process::Child> {
        let profile_key = Self::probe_hwaccel().await;
        let profiles = Self::get_hw_profiles();
        let profile = profiles.get(profile_key);

        let mut cmd = tokio::process::Command::new("ffmpeg");

        // Input Args (from profile or default)
        if let Some(p) = profile {
            cmd.args(&p.input_args);
        }

        // ========== OPTIMIZED INPUT FLAGS FOR LOW LATENCY ==========
        cmd.args([
            "-fflags", "+genpts+discardcorrupt+nobuffer",
            "-flags", "low_delay",
            "-analyzeduration", "500000",
            "-probesize", "1000000",
        ]);

        if let Some(cont) = container {
            cmd.arg("-f").arg(cont);
            if cont.contains("matroska") || cont.contains("webm") {
               cmd.args(["-seek2any", "1"]);
            }
        }

        cmd.args([
            "-fflags", "+genpts", 
            "-noaccurate_seek", 
            "-seek_timestamp", "1", 
            "-ignore_unknown",
            "-mpegts_copyts", "1",
            "-map", "-0:d?", 
            "-map", "-0:t?"
        ]);
        
        cmd.arg("-ss").arg(format!("{}", start));
        cmd.arg("-i").arg(input_path);
        cmd.arg("-t").arg(format!("{}", duration));

        let is_video_transcoding = target_video_codec != "copy" && target_video_codec != "none";
        let is_audio_transcoding = target_audio_codec != "copy" && target_audio_codec != "none";

        if is_video_transcoding || is_audio_transcoding {
             cmd.arg("-ss").arg(format!("{}", start));
             cmd.arg("-output_ts_offset").arg(format!("{}", start));
        }

        cmd.arg("-sn");

        // Stream mapping logic
        if target_video_codec == "none" {
            if let Some(audio_idx) = audio_stream_index {
                cmd.args(["-map", &format!("0:{}", audio_idx)]);
            } else {
                cmd.args(["-map", "0:a:0"]);
            }
            cmd.arg("-vn");
        } else if target_audio_codec == "none" {
            cmd.args(["-map", "0:v:0"]);
            cmd.arg("-an");
        } else {
            cmd.args(["-map", "0:v:0"]);
            if let Some(audio_idx) = audio_stream_index {
                cmd.args(["-map", &format!("0:{}", audio_idx)]);
            } else {
                cmd.args(["-map", "0:a:0"]);
            }
        }

        // --- Video Transcoding Logic with Profile ---
        if target_video_codec == "none" {
            // Do nothing
        } else if target_video_codec == "copy" {
            cmd.arg("-c:v").arg("copy");
        } else if let Some(p) = profile {
            tracing::info!("Using HW Profile: {}", p.name);
            
            // 1. Select Encoder
            // default to codec name if not in map, but usually we want the profile's encoder
            let encoder = p.encoders.get(target_video_codec).unwrap_or(&target_video_codec);
            cmd.args(["-c:v", encoder]);

            // 2. Add Profile Output Args (bitrate, bufsize, etc)
            // Replace placeholders: {bitrate} -> 20M, {bufsize} -> 40M
            for arg in &p.extra_output_args {
                let val = arg.replace("{bitrate}", "20M").replace("{bufsize}", "40M");
                cmd.arg(val);
            }

            // 3. Preset / Tune
            if let Some(preset) = p.preset {
                cmd.args(["-preset", preset]);
                if p.name.contains("nvenc") {
                     cmd.args(["-rc", "vbr", "-tune", "hq", "-cq", "23"]);
                }
            }

            // 4. Pixel Format
            if let Some(pix_fmt) = p.pixel_format {
                 if p.name.contains("qsv") || p.name.contains("vaapi") {
                      // QSV/VAAPI often handle format in filters
                 } else {
                      cmd.args(["-pix_fmt", pix_fmt]);
                 }
            }

            // 5. Scaling / Filters
            // Construct -vf: "format=nv12,hwupload" or "scale_cuda..."
            let mut filters = Vec::new();

            // HDR Tone Mapping (if source is HDR/10-bit)
            // MUST happen before format conversion if possible, or part of it
            if is_hdr {
                tracing::info!("Applying HDR Tone Mapping (BT.2020 -> BT.709)");
                // zscale is better but often missing. colorspace is standard.
                // We use a complex filter chain usually, but for simple transcoding:
                filters.push("colorspace=all=bt709:trc=bt709:format=yuv420p".to_string());
            }
            
            // VAAPI speciial handling
            if p.name == "vaapi" {
                filters.push("format=nv12".to_string());
                filters.push("hwupload".to_string());
            } else if p.name.contains("qsv") {
                 filters.push("format=nv12".to_string());
                 // hwupload might be needed if software decode
            } else if p.name.contains("nvenc") {
                 // Nothing specific usually if inputs are compatible, else scale_cuda
            }

            // Add standard HLS flags
            cmd.args(["-g", "60", "-sc_threshold", "0"]);
            if p.name.contains("vaapi") || p.name.contains("qsv") {
                 cmd.args(["-keyint_min", "60"]);
            }

             if !filters.is_empty() {
                cmd.arg("-vf").arg(filters.join(","));
            }

        } else {
            // Fallback CPU
             cmd.args(["-c:v", target_video_codec]);
             cmd.args([
                "-pix_fmt", "yuv420p",
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-g", "60",
                "-keyint_min", "60",
                "-sc_threshold", "0",
                "-profile:v", "high",
                "-level", "4.1",
            ]);
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

        // Output format flags
        // Use MPEG-TS for HLS segment compatibility (.ts files in playlist)
        // This ensures browsers can properly parse and play segments
        // NOTE: Do NOT use -reset_timestamps when using separate audio/video streams
        // as it breaks synchronization. Use -copyts instead to preserve timestamps.
        cmd.args([
            "-f",
            "mpegts",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            // Copy timestamps from input - CRITICAL for audio/video sync
            "-copyts",
            // Set output timestamp offset to match segment start time
            "-output_ts_offset",
            &format!("{}", start),
            // Frequent PAT/PMT tables for mid-segment player joins
            "-pat_period",
            "0.1",
            // Avoid negative timestamps which can cause playback issues
            "-avoid_negative_ts",
            "make_zero",
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
