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
    // Extract infoHash and fileIdx from the mediaURL
    // Format: http://127.0.0.1:11470/{infoHash}/{fileIdx}?
    let media_url = params.media_url.trim_end_matches('?');
    let parts: Vec<&str> = media_url.split('/').collect();

    if parts.len() < 2 {
        return (StatusCode::BAD_REQUEST, "Invalid mediaURL format").into_response();
    }

    let file_idx_str = parts[parts.len() - 1];
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
/// GET /hlsv2/:hash/master.m3u8?mediaURL=http://127.0.0.1:11470/{infoHash}/{fileIdx}?
pub async fn master_playlist_by_url(
    Path(_hash): Path<String>,
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
    // Generate master playlist - use the REAL info_hash from mediaURL, not the path hash
    // This ensures subsequent stream/segment requests use the correct torrent identifier
    let playlist = enginefs::hls::HlsEngine::get_master_playlist(
        &probe,
        info_hash,
        file_idx,
        "",
        raw_query.as_deref().unwrap_or(""),
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

    let playlist = enginefs::hls::HlsEngine::get_master_playlist(
        &probe,
        &info_hash,
        file_idx,
        "",
        raw_query.as_deref().unwrap_or(""),
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

// Dispatcher for stream playlist or segment to avoid route conflict
pub async fn handle_hls_resource(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path((info_hash, file_idx, resource)): Path<(String, usize, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    if resource.ends_with(".m3u8") {
        get_stream_playlist(State(state), info_hash, file_idx, resource, raw_query).await
    } else {
        get_segment(
            State(state),
            headers,
            info_hash,
            file_idx,
            resource,
            raw_query,
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

async fn get_segment(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    info_hash: String,
    file_idx: usize,
    segment: String,
    raw_query: Option<String>,
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

    // Check if this is an audio segment request "audio-{idx}-{seg}.ts"
    let (seg_index, audio_track_idx) = if segment.starts_with("audio-") {
        let parts: Vec<&str> = segment.trim_end_matches(".ts").split('-').collect();
        if parts.len() >= 3 {
            // audio, idx, seg
            let cur_idx = parts[1].parse::<usize>().ok();
            let cur_seg = parts[2].parse::<usize>().unwrap_or(0);
            (cur_seg, cur_idx)
        } else {
            return (StatusCode::BAD_REQUEST, "Invalid audio segment format").into_response();
        }
    } else {
        match segment.trim_end_matches(".ts").parse::<usize>() {
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
    // Heuristic: "Mozilla" present, but NOT "Stremio" (unless Stremio Web also sends Stremio in UA, but usually we want to treat Stremio App as native)
    // Actually, if it's Stremio Shell, we want native behavior.
    // If it's a browser (Chrome, Safari, etc), we generally need AAC.
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let is_browser = user_agent.contains("Mozilla") && !user_agent.contains("Stremio");

    // Check if audio needs transcoding
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
            // No params -> Browser logic
            if is_browser && audio_codec != "aac" {
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

    let target_video_codec = if audio_track_idx.is_some() {
        // Audio Segment request: STRICTLY NO VIDEO
        "none"
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
        audio_track_idx, // Use scraped audio stream index if available, otherwise default (None)
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let stdout = match child.stdout {
        Some(s) => s,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "No stdout").into_response(),
    };

    let stream = ReaderStream::new(stdout);
    let body = Body::from_stream(stream);

    (
        [
            (axum::http::header::CONTENT_TYPE, "video/mp2t"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
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
