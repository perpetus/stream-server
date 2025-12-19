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
    Query(params): Query<HlsQuery>,
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
    let playlist = enginefs::hls::HlsEngine::get_master_playlist(&probe, info_hash, file_idx, "");

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

    let playlist = enginefs::hls::HlsEngine::get_master_playlist(&probe, &info_hash, file_idx, "");

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
) -> Response {
    if resource.ends_with(".m3u8") {
        get_stream_playlist(State(state), info_hash, file_idx, resource).await
    } else {
        get_segment(State(state), headers, info_hash, file_idx, resource).await
    }
}

async fn get_stream_playlist(
    State(state): State<AppState>,
    info_hash: String,
    file_idx: usize,
    _playlist: String,
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

    // Assuming stream_idx 0 for now as we only support one video stream in probe logic mostly
    let playlist = enginefs::hls::HlsEngine::get_stream_playlist(&probe, 0, segment_base_url);

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

    let seg_index: usize = match segment.trim_end_matches(".ts").parse() {
        Ok(i) => i,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid segment").into_response(),
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
    let audio_codec = probe
        .streams
        .iter()
        .find(|s| s.codec_type == "audio")
        .map(|s| s.codec_name.as_str())
        .unwrap_or("unknown");

    let transcode_audio = is_browser && audio_codec != "aac";

    let child = match enginefs::hls::HlsEngine::transcode_segment(
        &stream_url,
        start,
        duration,
        Some(&probe.container),
        transcode_audio,
        None, // Use default audio stream (first one)
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
