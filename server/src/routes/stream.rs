use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use axum::http::HeaderMap;
use tokio::io::AsyncSeekExt;

pub async fn stream_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((info_hash, idx)): Path<(String, usize)>,
    RawQuery(query_str): RawQuery,
) -> Response {
    let info_hash = info_hash.to_lowercase();

    // Parse trackers from query string 'tr=url&tr=url2'
    let mut trackers = Vec::new();
    if let Some(q) = query_str {
        for (key, val) in url::form_urlencoded::parse(q.as_bytes()) {
            if key == "tr" {
                trackers.push(val.into_owned());
            }
        }
    }

    // Try to get existing engine, or auto-create from info hash
    let engine = if let Some(e) = state.engine.get_engine(&info_hash).await {
        e
    } else {
        // Auto-create engine from magnet link
        tracing::info!("Auto-creating engine for info_hash: {}", info_hash);
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        // Note: usage of enginefs::backend::TorrentSource requires enginefs dependency or import
        let source = enginefs::backend::TorrentSource::Url(magnet);

        match state.engine.add_torrent(source, Some(trackers)).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("Failed to create engine: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create engine: {}", e),
                )
                    .into_response();
            }
        }
    };

    // Await the async get_file
    if let Some(mut file) = engine.get_file(idx).await {
        let size = file.size;
        let name = file.name.clone();

        // Handle Range header
        let (start, end) = if let Some(range_header) = headers.get(header::RANGE) {
            let range_str = range_header.to_str().unwrap_or("");
            if range_str.starts_with("bytes=") {
                let parts: Vec<&str> = range_str["bytes=".len()..].split('-').collect();
                if parts.len() == 2 {
                    let start = parts[0].parse::<u64>().unwrap_or(0);
                    let end = parts[1].parse::<u64>().unwrap_or(size - 1);
                    (start, end)
                } else {
                    (0, size - 1)
                }
            } else {
                (0, size - 1)
            }
        } else {
            (0, size - 1)
        };

        if start >= size {
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable").into_response();
        }

        // Seek to the start position
        if start > 0 {
            if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                tracing::warn!("Seek error: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Seek failed").into_response();
            }
        }

        let content_length = end - start + 1;

        let mut res_headers = header::HeaderMap::new();
        let mime = if name.ends_with(".mp4") {
            "video/mp4"
        } else if name.ends_with(".mkv") {
            "video/x-matroska"
        } else if name.ends_with(".ts") {
            "video/mp2t"
        } else if name.ends_with(".avi") {
            "video/x-msvideo"
        } else if name.ends_with(".mov") {
            "video/quicktime"
        } else if name.ends_with(".wmv") {
            "video/x-ms-wmv"
        } else if name.ends_with(".webm") {
            "video/webm"
        } else if name.ends_with(".mp3") {
            "audio/mpeg"
        } else if name.ends_with(".m4a") {
            "audio/mp4"
        } else if name.ends_with(".aac") {
            "audio/aac"
        } else if name.ends_with(".flac") {
            "audio/flac"
        } else if name.ends_with(".wav") {
            "audio/wav"
        } else if name.ends_with(".ogg") {
            "audio/ogg"
        } else if name.ends_with(".opus") {
            "audio/opus"
        } else if name.ends_with(".ac3") {
            "audio/ac3"
        } else if name.ends_with(".eac3") || name.ends_with(".ec3") {
            "audio/eac3"
        } else {
            "application/octet-stream"
        };
        res_headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
        res_headers.insert(header::CONTENT_LENGTH, content_length.into());
        res_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
        res_headers.insert(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, size).parse().unwrap(),
        );

        // Limit the stream to the requested range if necessary
        // For now, tokio_util::io::ReaderStream reads until EOF.
        // If the client respects Content-Length, it should be fine.
        // Actually, for better compliance, we should wrap it in a Take.
        let reader = tokio::io::AsyncReadExt::take(file, content_length);

        // Use ReaderStream to convert AsyncRead to Stream for Axum Body
        let stream = tokio_util::io::ReaderStream::new(reader);
        let body = Body::from_stream(stream);

        if headers.contains_key(header::RANGE) {
            (StatusCode::PARTIAL_CONTENT, res_headers, body).into_response()
        } else {
            (StatusCode::OK, res_headers, body).into_response()
        }
    } else {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    }
}
