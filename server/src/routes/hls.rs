use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use tokio_util::io::ReaderStream;

#[derive(Deserialize)]
pub struct ProbeQuery {
    #[serde(rename = "mediaURL")]
    pub media_url: String,
}

#[derive(Debug, Default, Clone)]
struct TranscodeParams {
    audio_codecs: Vec<String>,
    video_codecs: Vec<String>,
    max_audio_channels: Option<usize>,
}

fn parse_transcode_params(query_str: &str) -> TranscodeParams {
    let mut params = TranscodeParams::default();
    let decoded_query =
        urlencoding::decode(query_str).unwrap_or(std::borrow::Cow::Borrowed(query_str));
    // Simple manual parsing to handle repeated keys which serde_urlencoded might miss/overwrite
    for pair in decoded_query.split('&') {
        let mut split = pair.splitn(2, '=');
        if let (Some(key), Some(value)) = (split.next(), split.next()) {
            match key {
                "audioCodecs" => params.audio_codecs.push(value.trim().to_lowercase()),
                "maxAudioChannels" => {
                    if let Ok(val) = value.parse() {
                        params.max_audio_channels = Some(val);
                    }
                }
                "videoCodecs" => params.video_codecs.push(value.trim().to_lowercase()),
                _ => {}
            }
        }
    }
    params
}

/// Probe endpoint using mediaURL query parameter (for Stremio compatibility)
/// GET /hlsv2/probe?mediaURL=http://127.0.0.1:11470/{infoHash}/{fileIdx}?
pub async fn probe_by_url(Query(params): Query<ProbeQuery>) -> Response {
    let start = std::time::Instant::now();
    // Extract infoHash and fileIdx from the mediaURL
    // Format: http://127.0.0.1:11470/{infoHash}/{fileIdx}?
    let media_url = params.media_url.trim_end_matches('?');
    let parts: Vec<&str> = media_url.split('/').collect();

    if parts.len() < 2 {
        return (StatusCode::BAD_REQUEST, "Invalid mediaURL format").into_response();
    }

    let file_idx_str = parts[parts.len() - 1].split('?').next().unwrap_or("0");
    let info_hash = parts[parts.len() - 2];

    let file_idx: usize = match file_idx_str.parse() {
        Ok(i) => i,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid file index").into_response(),
    };

    // Run ffprobe directly on the mediaURL
    let probe = match enginefs::hls::HlsEngine::probe_video(media_url).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Probe failed: {}", e),
            )
                .into_response()
        }
    };

    let elapsed = start.elapsed();
    tracing::info!("Probe request for {} took {:?}", media_url, elapsed);

    Json(serde_json::json!({
        "infoHash": info_hash,
        "fileIdx": file_idx,
        "duration": probe.duration,
        "streams": probe.streams
    }))
    .into_response()
}

/// Query params for HLS endpoints
#[derive(Deserialize)]
pub struct HlsQuery {
    #[serde(rename = "mediaURL")]
    pub media_url: String,
}

/// Master playlist endpoint using mediaURL query parameter (for Stremio compatibility)
/// GET /hlsv2/{hash}/master.m3u8?mediaURL=http://127.0.0.1:11470/{infoHash}/{fileIdx}?
pub async fn master_playlist_by_url(
    Path(_hash): Path<String>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<HlsQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    // Strip query string from mediaURL and clean up the path
    let media_url = params.media_url.trim_end_matches('?');
    let parts: Vec<&str> = media_url.split('/').collect();

    if parts.len() < 2 {
        return (StatusCode::BAD_REQUEST, "Invalid mediaURL format").into_response();
    }

    // file_idx might have query params appended (e.g., "0?" or "0?foo=bar")
    let file_idx_str = parts[parts.len() - 1].split('?').next().unwrap_or("0");
    // Extract the REAL info_hash from the mediaURL - this is the actual torrent hash
    let info_hash = parts[parts.len() - 2];

    let file_idx: usize = match file_idx_str.parse() {
        Ok(i) => i,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid file index: {}", file_idx_str),
            )
                .into_response()
        }
    };

    // Run ffprobe on the media URL
    let probe = match enginefs::hls::HlsEngine::probe_video(media_url).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Probe failed: {}", e),
            )
                .into_response()
        }
    };

    // Generate master playlist - use the REAL info_hash from mediaURL, not the path hash
    // This ensures subsequent stream/segment requests use the correct torrent identifier
    // Base URL for segments
    let base_url = format!("http://127.0.0.1:11470");

    let query_str = raw_query.as_deref().unwrap_or("");
    let query_map: std::collections::HashMap<String, String> = urlencoding::decode(query_str)
        .unwrap_or(std::borrow::Cow::Borrowed(query_str))
        .split('&')
        .filter_map(|s| {
            let mut parts = s.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
        })
        .collect();
    
    // Determine Profile
    let profile = get_hls_profile(headers.get("user-agent").and_then(|v| v.to_str().ok()), &query_map);

    let playlist = enginefs::hls::HlsEngine::get_master_playlist(
        &probe,
        &info_hash,
        file_idx,
        &base_url,
        query_str,
        profile,
    );

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl",
        )],
        playlist,
    )
        .into_response()
}

pub async fn get_master_playlist(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
    headers: axum::http::HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    let engine = match state.engine.get_or_add_engine(&info_hash).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load engine: {}", e),
            )
                .into_response()
        }
    };

    let port = 11470;
    let stream_url = format!(
        "http://127.0.0.1:{}/stream/{}/{}",
        port, info_hash, file_idx
    );

    let probe = match engine.get_probe_result(file_idx, &stream_url).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let query_str = raw_query.as_deref().unwrap_or("");
    let query_map: std::collections::HashMap<String, String> = urlencoding::decode(query_str)
        .unwrap_or(std::borrow::Cow::Borrowed(query_str))
        .split('&')
        .filter_map(|s| {
            let mut parts = s.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
        })
        .collect();

    let profile = get_hls_profile(headers.get("user-agent").and_then(|v| v.to_str().ok()), &query_map);

    let playlist = enginefs::hls::HlsEngine::get_master_playlist(
        &probe,
        &info_hash,
        file_idx,
        &format!("http://127.0.0.1:{}", port),
        query_str,
        profile,
    );

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl",
        ),
        (
            axum::http::header::CACHE_CONTROL,
            "no-cache, no-store, must-revalidate",
        )],
        playlist,
    )
        .into_response()
}

// Dispatcher for stream playlist or segment to avoid route conflict
// {file_idx} can be numeric (0, 1) or text (subtitle4, audio-1)
pub async fn handle_hls_resource(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path((info_hash, file_idx_str, resource)): Path<(String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    // Parse file_idx - default to 0 if non-numeric (e.g., "subtitle4", "audio-1")
    let file_idx: usize = file_idx_str.parse().unwrap_or(0);
    
    // Determine Profile
    let headers_clone = headers.clone();
    let ua = headers_clone.get("user-agent").and_then(|v| v.to_str().ok());
    
    // We need to parse query params again or pass them down. 
    // handle_hls_resource doesn't have the HashMap readily available cleanly without re-parsing or changing signature.
    // We'll quick-parse the query string again, it's cheap.
    let query_str = raw_query.as_deref().unwrap_or("");
    let query_map: std::collections::HashMap<String, String> = urlencoding::decode(query_str)
        .unwrap_or(std::borrow::Cow::Borrowed(query_str))
        .split('&')
        .filter_map(|s| {
            let mut parts = s.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
        })
        .collect();

    let profile = get_hls_profile(ua, &query_map);
    
    if resource.ends_with(".m3u8") {
        get_stream_playlist(State(state), info_hash, file_idx, resource, raw_query, profile).await
    } else {
        get_segment(
            State(state),
            headers,
            info_hash,
            file_idx,
            resource,
            raw_query,
            profile,
        )
        .await
    }
}

async fn get_stream_playlist(
    State(state): State<AppState>,
    info_hash: String,
    file_idx: usize,
    playlist_name: String,
    raw_query: Option<String>,
    profile: HlsProfile,
) -> Response {
    let engine = match state.engine.get_or_add_engine(&info_hash).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load engine: {}", e),
            )
                .into_response()
        }
    };

    let port = 11470;
    let stream_url = format!(
        "http://127.0.0.1:{}/stream/{}/{}",
        port, info_hash, file_idx
    );
    let probe = match engine.get_probe_result(file_idx, &stream_url).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let segment_base_url = "./";

    // Check if this is an audio playlist
    // Format: "audio-{idx}.m3u8"
    let audio_track_idx = if playlist_name.starts_with("audio-") {
        playlist_name
            .trim_start_matches("audio-")
            .trim_end_matches(".m3u8")
            .parse::<usize>()
            .ok()
    } else {
        None
    };

    // Assuming stream_idx 0 for now as we only support one video stream in probe logic mostly
    let playlist = enginefs::hls::HlsEngine::get_stream_playlist(
        &probe,
        0,
        segment_base_url,
        audio_track_idx,
        raw_query.as_deref().unwrap_or(""),
        profile,
    );

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl",
        ),
        (
            axum::http::header::CACHE_CONTROL,
            "no-cache, no-store, must-revalidate",
        )],
        playlist,
    )
        .into_response()
}

async fn get_segment(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    info_hash: String,
    file_idx: usize,
    segment: String,
    raw_query: Option<String>,
    profile: HlsProfile,
) -> Response {
    let engine = match state.engine.get_or_add_engine(&info_hash).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load engine: {}", e),
            )
                .into_response()
        }
    };

    // Check if this is an audio segment request "audio-{idx}-{seg}.ts" or "audio-{idx}-{seg}.m4s"
    let segment_name = segment.trim_end_matches(".ts").trim_end_matches(".m4s");
    let (seg_index, audio_track_idx) = if segment_name.starts_with("audio-") {
        let parts: Vec<&str> = segment_name.split('-').collect();
        if parts.len() >= 3 {
            // audio, idx, seg
            let cur_idx = parts[1].parse::<usize>().ok();
            let cur_seg = parts[2].parse::<usize>().unwrap_or(0);
            (cur_seg, cur_idx)
        } else {
            return (StatusCode::BAD_REQUEST, "Invalid audio segment format").into_response();
        }
    } else {
        match segment_name.parse::<usize>() {
            Ok(i) => (i, None),
            Err(_) => return (StatusCode::BAD_REQUEST, "Invalid segment").into_response(),
        }
    };

    let port = 11470;
    let stream_url = format!(
        "http://127.0.0.1:{}/stream/{}/{}",
        port, info_hash, file_idx
    );
    let probe = match engine.get_probe_result(file_idx, &stream_url).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let segments = enginefs::hls::HlsEngine::get_segments(probe.duration);
    let (start, duration) = match segments.get(seg_index) {
        Some(&s) => s,
        None => return (StatusCode::NOT_FOUND, "Segment out of bounds").into_response(),
    };

    // Check if client is a browser
    // Browsers typically need AAC audio and H.264 video for compatibility.
    // Native apps (Stremio Shell) can handle more codecs directly.
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Improved browser detection: check for common browser identifiers
    // but exclude Stremio Shell which has native codec support
    let is_browser = (user_agent.contains("Mozilla")
        || user_agent.contains("Chrome")
        || user_agent.contains("Safari")
        || user_agent.contains("Firefox")
        || user_agent.contains("Edge"))
        && !user_agent.contains("Stremio");

    if is_browser {
        tracing::info!("HLS segment request from browser client (UA: {})", user_agent);
    } else {
        tracing::info!("HLS segment request from native app (UA: {})", user_agent);
    }

    // Audio codecs that browsers cannot play natively and require transcoding to AAC
    let browser_incompatible_audio = ["ac3", "eac3", "dts", "truehd", "mlp", "flac", "pcm", "opus"];

    // Check if audio needs transcoding and select target codec
    // If it's a browser, we generally require AAC.
    // If the source is already AAC, we don't need to transcode.
    // Note: probe.streams might have multiple audio streams, we are transcoding the default one usually.
    // We should check the codec of the audio stream we are using.
    let audio_codec = if let Some(idx) = audio_track_idx {
        probe
            .streams
            .iter()
            .find(|s| s.index == idx)
            .map(|s| s.codec_name.as_str())
            .unwrap_or("unknown")
    } else {
        probe
            .streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .map(|s| s.codec_name.as_str())
            .unwrap_or("unknown")
    };

    let transcode_params = parse_transcode_params(raw_query.as_deref().unwrap_or(""));

    // Check if audio needs transcoding and select target codec
    let target_audio_codec = if audio_track_idx.is_some() {
        // Audio Segment request: We MUST have audio.
        // If it's a browser, we generally require AAC.
        if !transcode_params.audio_codecs.is_empty() {
            let codec_supported = transcode_params
                .audio_codecs
                .iter()
                .any(|c| audio_codec.contains(c));

            // TODO: Implement channel count check when we have that info in probe results
            let _ = transcode_params.max_audio_channels;
            let channels_ok = true;

            if codec_supported && channels_ok {
                "copy"
            } else {
                // Pick best target from requested
                if transcode_params
                    .audio_codecs
                    .iter()
                    .any(|c| c.contains("aac"))
                {
                    "aac"
                } else if transcode_params
                    .audio_codecs
                    .iter()
                    .any(|c| c.contains("opus"))
                {
                    "libopus"
                } else if transcode_params
                    .audio_codecs
                    .iter()
                    .any(|c| c.contains("mp3"))
                {
                    "libmp3lame"
                } else {
                    "aac" // Safe fallback
                }
            }
        } else {
            // No params -> Browser logic with comprehensive codec checking
            // Check if audio codec is incompatible with browsers
            let needs_transcode = is_browser
                && (audio_codec != "aac"
                    && browser_incompatible_audio
                        .iter()
                        .any(|c| audio_codec.contains(c)));
            if needs_transcode || (is_browser && audio_codec != "aac") {
                "aac"
            } else {
                "copy"
            }
        }
    } else {
        // Video Segment request: STRICTLY NO AUDIO
        "none"
    };

    let video_stream = probe.streams.iter().find(|s| s.codec_type == "video");

    let video_codec = video_stream
        .map(|s| s.codec_name.as_str())
        .unwrap_or("unknown");

    // Check for 10-bit profile (e.g. "Main 10")
    let is_10bit = video_stream
        .and_then(|s| s.profile.as_deref())
        .map(|p| p.contains("10"))
        .unwrap_or(false);

    // Check if video needs transcoding
    // Logic:
    // 1. If videoCodecs params exist:
    //    - If source codec in videoCodecs AND (not 10bit OR client supports 10bit?) -> copy.
    //      (For now, assume "hevc" param != 10bit support safely)
    //    - Else -> transcode to h264.
    // 2. If no params -> transcode to h264 (safe default for browsers/compatibility).

    // Get transcode profile from settings for hardware acceleration
    let settings = state.settings.read().await;
    let transcode_profile = settings.transcode_profile.clone();
    drop(settings);

    let config = profile.get_config();
    let target_video_codec = if audio_track_idx.is_some() {
        // Audio Segment request: STRICTLY NO VIDEO
        "none"
    } else if config.force_transcode_video {
        // Profile enforces transcoding (e.g. Browser profile)
         match transcode_profile.as_deref() {
            Some("nvenc") => "h264_nvenc",
            Some("vaapi") => "h264_vaapi",
            Some("qsv") => "h264_qsv",
            Some("v4l2m2m") => "h264_v4l2m2m",
            Some("videotoolbox") => "h264_videotoolbox",
            _ => "libx264",
        }
    } else if !transcode_params.video_codecs.is_empty() {
        let codec_supported = transcode_params
            .video_codecs
            .iter()
            .any(|c| video_codec.contains(c));

        // Force transcode if 10-bit, unless we strictly know client handles it.
        // SAFE DEFAULT: Transcode 10-bit to 8-bit H264.
        if !codec_supported || is_10bit {
            // Use hardware encoder if profile is set
            match transcode_profile.as_deref() {
                Some("nvenc") => "h264_nvenc",
                Some("vaapi") => "h264_vaapi",
                Some("qsv") => "h264_qsv",
                Some("v4l2m2m") => "h264_v4l2m2m",
                Some("videotoolbox") => "h264_videotoolbox",
                _ => "libx264",
            }
        } else {
            "copy"
        }
    } else {
        // Default to transcode if unknown - use hardware if available
        match transcode_profile.as_deref() {
            Some("nvenc") => "h264_nvenc",
            Some("vaapi") => "h264_vaapi",
            Some("qsv") => "h264_qsv",
            Some("v4l2m2m") => "h264_v4l2m2m",
            Some("videotoolbox") => "h264_videotoolbox",
            _ => "libx264",
        }
    };
    
    // Audio Force Transcode Check
     let target_audio_codec = if config.force_transcode_audio && target_audio_codec == "copy" {
         "aac"
     } else {
         target_audio_codec
     };

    if target_video_codec != "copy" || target_audio_codec != "copy" {
        tracing::debug!(
            "Transcoding segment: video_target={} (source={}, requested={:?}, 10bit={}), audio_target={} (source={}, requested={:?})",
            target_video_codec,
            video_codec,
            transcode_params.video_codecs,
            is_10bit,
            target_audio_codec,
            audio_codec,
            transcode_params.audio_codecs
        );
    } else {
        tracing::debug!(
            "Direct streaming segment (Video: {}, Audio: {})",
            video_codec,
            audio_codec
        );
    }

    let child = match enginefs::hls::HlsEngine::transcode_segment(
        &stream_url,
        start,
        duration,
        Some(&probe.container),
        target_audio_codec,
        target_video_codec,
        video_codec, // Pass the source video codec for BSF selection
        audio_track_idx, // Use scraped audio stream index if available, otherwise default (None)
        is_10bit,        // Use 10-bit detection as proxy for HDR
        is_browser,      // Use fMP4 for browser, MPEG-TS for native
        &profile.get_config(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let stdout = match enginefs::hls::TranscodeStream::new(child) {
        Some(s) => s,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "No stdout").into_response(),
    };

    let stream = ReaderStream::new(stdout);
    let body = Body::from_stream(stream);

    // Use video/mp4 for browser (fMP4), video/mp2t for native (MPEG-TS)
    let content_type = if is_browser { "video/mp4" } else { "video/mp2t" };

    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=3600, immutable",
            ),
        ],
        body,
    )
        .into_response()
}

pub async fn get_probe(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
) -> Response {
    let engine = match state.engine.get_engine(&info_hash).await {
        Some(e) => e,
        None => return (StatusCode::NOT_FOUND, "Engine not found").into_response(),
    };

    let port = 11470;
    let stream_url = format!(
        "http://127.0.0.1:{}/stream/{}/{}",
        port, info_hash, file_idx
    );
    let probe = match engine.get_probe_result(file_idx, &stream_url).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(probe).into_response()
}

pub async fn get_tracks(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
) -> Response {
    let engine = match state.engine.get_engine(&info_hash).await {
        Some(e) => e,
        None => return (StatusCode::NOT_FOUND, "Engine not found").into_response(),
    };

    let port = 11470;
    let stream_url = format!(
        "http://127.0.0.1:{}/stream/{}/{}",
        port, info_hash, file_idx
    );
    let probe = match engine.get_probe_result(file_idx, &stream_url).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(probe.streams).into_response()
}
// Helper to determine detection
use enginefs::hls::HlsProfile;

fn get_hls_profile(user_agent: Option<&str>, params: &std::collections::HashMap<String, String>) -> HlsProfile {
    // 1. Check Query Param Override
    if let Some(p) = params.get("profile") {
        if p == "browser" {
            tracing::info!("HLS Profile Override: Browser (via query param)");
            return HlsProfile::Browser;
        }
        if p == "native" {
            tracing::info!("HLS Profile Override: Native (via query param)");
            return HlsProfile::Native;
        }
    }

    // 2. Check User Agent
    if let Some(ua) = user_agent {
        let ua_lower = ua.to_lowercase();
        // Known native players usually verifyable
        if ua_lower.contains("stremio") || ua_lower.contains("vlc") || ua_lower.contains("libmpv") || ua_lower.contains("mpv") {
            tracing::info!(user_agent = ?ua, "HLS Profile Detected: Native");
            return HlsProfile::Native;
        }
        // Browsers
        if ua_lower.contains("mozilla") || ua_lower.contains("chrome") || ua_lower.contains("safari") {
            tracing::info!(user_agent = ?ua, "HLS Profile Detected: Browser");
            return HlsProfile::Browser;
        }
    }

    // 3. Default to Native (safe default for our primary use case)
    tracing::info!(user_agent = ?user_agent, "HLS Profile Default: Native (Unknown Client)");
    HlsProfile::Native
}
