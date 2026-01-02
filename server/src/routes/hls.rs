use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use enginefs::backend::TorrentHandle;
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
                .into_response();
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
    _headers: axum::http::HeaderMap,
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
                .into_response();
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
                .into_response();
        }
    };

    // Generate master playlist - use the REAL info_hash from mediaURL, not the path hash
    // This ensures subsequent stream/segment requests use the correct torrent identifier
    // Base URL for segments
    let base_url = format!("http://127.0.0.1:11470");

    let query_str = raw_query.as_deref().unwrap_or("");

    let playlist = enginefs::hls::HlsEngine::get_master_playlist(
        &probe, &info_hash, file_idx, &base_url, query_str,
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
    _headers: axum::http::HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    let engine = match state.engine.get_or_add_engine(&info_hash).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load engine: {}", e),
            )
                .into_response();
        }
    };

    // --- Stream Lifecycle: Notify start and focus bandwidth for HLS ---
    state.engine.on_stream_start(&info_hash, file_idx).await;
    state.engine.focus_torrent(&info_hash).await;

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

    let playlist = enginefs::hls::HlsEngine::get_master_playlist(
        &probe,
        &info_hash,
        file_idx,
        &format!("http://127.0.0.1:{}", port),
        query_str,
    );

    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl",
            ),
            (
                axum::http::header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate",
            ),
        ],
        playlist,
    )
        .into_response()
}

// Dispatcher for stream playlist or segment to avoid route conflict
// {file_idx} can be numeric (0, 1) or text (subtitle4, audio-1)
pub async fn handle_hls_resource(
    State(state): State<AppState>,
    _headers: axum::http::HeaderMap,
    Path((info_hash, file_idx_str, resource)): Path<(String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    // Parse file_idx - default to 0 if non-numeric (e.g., "subtitle4", "audio-1")
    let file_idx: usize = file_idx_str.parse().unwrap_or(0);

    if resource.ends_with(".m3u8") {
        get_stream_playlist(State(state), info_hash, file_idx, resource, raw_query).await
    } else {
        get_segment(
            State(state),
            _headers,
            info_hash,
            file_idx,
            resource,
            raw_query,
        )
        .await
    }
}

// Handler for fMP4 nested paths like /hlsv2/{infoHash}/{fileIdx}/video0/init.mp4
pub async fn handle_hls_fmp4_segment(
    State(state): State<AppState>,
    _headers: axum::http::HeaderMap,
    Path((info_hash, file_idx_str, track, segment)): Path<(String, String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    let file_idx: usize = file_idx_str.parse().unwrap_or(0);
    // Combine track and segment into resource path format expected by get_segment
    let resource = format!("{}/{}", track, segment);

    get_segment(
        State(state),
        _headers,
        info_hash,
        file_idx,
        resource,
        raw_query,
    )
    .await
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
                .into_response();
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

    let playlist = enginefs::hls::HlsEngine::get_stream_playlist(
        &probe,
        0,
        segment_base_url,
        audio_track_idx,
        raw_query.as_deref().unwrap_or(""),
    );

    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl",
            ),
            (
                axum::http::header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate",
            ),
        ],
        playlist,
    )
        .into_response()
}

async fn get_segment(
    State(state): State<AppState>,
    _headers: axum::http::HeaderMap,
    info_hash: String,
    file_idx: usize,
    segment: String,
    _raw_query: Option<String>,
) -> Response {
    let engine = match state.engine.get_or_add_engine(&info_hash).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load engine: {}", e),
            )
                .into_response();
        }
    };

    // Parse segment type from filename
    // Video: "0.ts", "1.ts", etc.
    // Audio: "audio-{track_idx}-{segment_idx}.ts"
    let segment_name = segment.trim_end_matches(".ts");
    let (seg_index, audio_track_idx) = if segment_name.starts_with("audio-") {
        let parts: Vec<&str> = segment_name.split('-').collect();
        if parts.len() >= 3 {
            let track_idx = parts[1].parse::<usize>().ok();
            let seg_idx = parts[2].parse::<usize>().unwrap_or(0);
            (seg_idx, track_idx)
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

    // Get local file path for transcoding (HTTP seeking is unreliable)
    let transcode_input_path = engine
        .handle
        .get_file_path(file_idx)
        .await
        .unwrap_or_else(|| stream_url.clone());

    let probe = match engine.get_probe_result(file_idx, &stream_url).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let segments = enginefs::hls::HlsEngine::get_segments(probe.duration);
    let (start, duration) = match segments.get(seg_index) {
        Some(&s) => s,
        None => return (StatusCode::NOT_FOUND, "Segment out of bounds").into_response(),
    };

    let config = enginefs::hls::TranscodeConfig::browser();

    // Dispatch to video or audio transcoding based on segment type
    let child = if let Some(audio_idx) = audio_track_idx {
        // Audio-only segment
        tracing::debug!(
            "Transcoding audio segment: track={}, segment={}, start={:.2}s, input={}",
            audio_idx,
            seg_index,
            start,
            transcode_input_path
        );
        match enginefs::hls::HlsEngine::transcode_audio_segment(
            &transcode_input_path,
            start,
            duration,
            audio_idx,
            &config,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    } else {
        // Video-only segment
        tracing::debug!(
            "Transcoding video segment: segment={}, start={:.2}s",
            seg_index,
            start
        );
        match enginefs::hls::HlsEngine::transcode_video_segment(
            &transcode_input_path,
            start,
            duration,
            &config,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    };

    let stdout = match enginefs::hls::TranscodeStream::new(child) {
        Some(s) => s,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "No stdout").into_response(),
    };

    let stream = ReaderStream::new(stdout);
    let body = Body::from_stream(stream);

    // MPEG-TS content type for HLS segments
    (
        [
            (axum::http::header::CONTENT_TYPE, "video/mp2t"),
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
