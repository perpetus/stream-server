use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use axum::http::HeaderMap;
use enginefs::backend::TorrentHandle;
use futures_util::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::AsyncSeekExt;

/// Guard that calls on_stream_end when dropped
struct StreamGuard<S> {
    inner: S,
    engine: Arc<crate::state::AppState>,
    info_hash: String,
    file_idx: usize,
    notified: bool,
}

impl<S: Stream + Unpin> Stream for StreamGuard<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S> Drop for StreamGuard<S> {
    fn drop(&mut self) {
        if !self.notified {
            self.notified = true;
            let engine = self.engine.clone();
            let info_hash = self.info_hash.clone();
            let file_idx = self.file_idx;

            // Spawn a task to notify stream end since Drop is sync
            tokio::spawn(async move {
                engine.engine.on_stream_end(&info_hash, file_idx).await;
                tracing::debug!(
                    "StreamGuard: Notified stream end for {} file_idx={}",
                    info_hash,
                    file_idx
                );
            });
        }
    }
}

fn parse_trackers(query_str: Option<String>) -> Vec<String> {
    let mut trackers = Vec::new();
    if let Some(q) = query_str {
        for (key, val) in url::form_urlencoded::parse(q.as_bytes()) {
            if key == "tr" {
                trackers.push(val.into_owned());
            }
        }
    }
    trackers
}

fn content_type_for_name(name: &str) -> &'static str {
    if name.ends_with(".mp4") {
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
    }
}

pub async fn head_stream_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((info_hash, idx)): Path<(String, usize)>,
    RawQuery(query_str): RawQuery,
) -> Response {
    let request_start = Instant::now();
    let info_hash = info_hash.to_lowercase();
    let trackers = parse_trackers(query_str);

    let engine = if let Some(e) = state.engine.get_engine(&info_hash).await {
        e
    } else {
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        let source = enginefs::backend::TorrentSource::Url(magnet);
        match state.engine.add_torrent(source, Some(trackers)).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("head_stream_video: Failed to create engine: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create engine: {}", e),
                )
                    .into_response();
            }
        }
    };

    let files = engine.handle.get_files().await;
    let Some(file_info) = files.get(idx) else {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    };
    let size = file_info.length;
    let name = &file_info.name;

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (start, end, is_partial) = if let Some(range) = &range_header {
        if let Some((start, end)) = parse_range(range, size) {
            (start, end, true)
        } else {
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable").into_response();
        }
    } else {
        (0, size.saturating_sub(1), false)
    };

    let content_length = end.saturating_sub(start) + 1;
    let mut res_headers = header::HeaderMap::new();
    res_headers.insert(
        header::CONTENT_TYPE,
        content_type_for_name(name).parse().unwrap(),
    );
    res_headers.insert(header::CONTENT_LENGTH, content_length.into());
    res_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    if is_partial {
        res_headers.insert(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, size).parse().unwrap(),
        );
    }

    tracing::debug!(
        "head_stream_video: Responded in {:?} for {} idx={} range {}-{}",
        request_start.elapsed(),
        info_hash,
        idx,
        start,
        end
    );

    if is_partial {
        (StatusCode::PARTIAL_CONTENT, res_headers, Body::empty()).into_response()
    } else {
        (StatusCode::OK, res_headers, Body::empty()).into_response()
    }
}

pub async fn stream_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((info_hash, idx)): Path<(String, usize)>,
    RawQuery(query_str): RawQuery,
) -> Response {
    let request_start = Instant::now();
    let info_hash = info_hash.to_lowercase();

    tracing::debug!(
        "stream_video: Request for info_hash={} idx={}",
        info_hash,
        idx
    );

    // Parse trackers from query string 'tr=url&tr=url2'
    let trackers = parse_trackers(query_str);
    tracing::debug!("stream_video: Found {} trackers", trackers.len());

    // Try to get existing engine, or auto-create from info hash
    let engine = if let Some(e) = state.engine.get_engine(&info_hash).await {
        tracing::debug!("stream_video: Engine found in cache");
        e
    } else {
        // Auto-create engine from magnet link
        tracing::debug!(
            "stream_video: Auto-creating engine for info_hash: {}",
            info_hash
        );
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        // Note: usage of enginefs::backend::TorrentSource requires enginefs dependency or import
        let source = enginefs::backend::TorrentSource::Url(magnet);

        match state.engine.add_torrent(source, Some(trackers)).await {
            Ok(e) => {
                tracing::debug!("stream_video: Engine created successfully");
                e
            }
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

    // --- Stream Lifecycle: Notify start and focus bandwidth ---
    state.engine.on_stream_start(&info_hash, idx).await;
    state.engine.focus_torrent(&info_hash).await;

    let files = engine.handle.get_files().await;
    let Some(file_info) = files.get(idx) else {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    };
    let size = file_info.length;

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (start, end, is_partial) = if let Some(range) = &range_header {
        if let Some((start, end)) = parse_range(range, size) {
            (start, end, true)
        } else {
            tracing::warn!("stream_video: Invalid range header '{}'", range);
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable").into_response();
        }
    } else {
        (0, size.saturating_sub(1), false)
    };
    let start_offset_hint = start;

    // Parse priority from enginefs-prio header
    let priority: u8 = if let Some(prio_val) = headers.get("enginefs-prio") {
        prio_val.to_str().unwrap_or("1").parse().unwrap_or(1)
    } else {
        1
    };

    // Await the async get_file
    tracing::debug!(
        "stream_video: Calling get_file({}) with offset {} and priority {}",
        idx,
        start_offset_hint,
        priority
    );
    if let Some(mut file) = engine.get_file(idx, start_offset_hint, priority).await {
        tracing::debug!(
            "stream_video: get_file returned success. Size={}",
            file.size
        );
        let size = file.size;
        let name = file.name.clone();

        tracing::debug!(
            "stream_video: Range request: {}-{} (total {})",
            start,
            end,
            size
        );

        if start >= size {
            tracing::warn!("stream_video: Range not satisfiable");
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable").into_response();
        }

        // Seek to the start position
        if start > 0 {
            tracing::debug!("stream_video: Seeking to {}", start);
            if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                tracing::warn!("Seek error: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Seek failed").into_response();
            }
            tracing::debug!("stream_video: Seek complete");
        }

        let content_length = end - start + 1;

        let mut res_headers = header::HeaderMap::new();

        let mime = content_type_for_name(&name);

        // Log detected file type
        tracing::info!("Media file detected: {} ({})", name, mime);

        res_headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());

        res_headers.insert(header::CONTENT_LENGTH, content_length.into());
        res_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
        if is_partial {
            res_headers.insert(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, end, size).parse().unwrap(),
            );
        }

        // Limit the stream to the requested range if necessary
        // For now, tokio_util::io::ReaderStream reads until EOF.
        // If the client respects Content-Length, it should be fine.
        // Actually, for better compliance, we should wrap it in a Take.
        let reader = tokio::io::AsyncReadExt::take(file, content_length);

        // Use ReaderStream to convert AsyncRead to Stream for Axum Body
        // OPTIMIZATION: Use 256KB buffer for improved throughput with large pieces
        // Larger buffer = fewer poll_read calls = less priority calculation overhead
        let base_stream = tokio_util::io::ReaderStream::with_capacity(reader, 262144);

        // Wrap with StreamGuard to notify when stream ends
        let guarded_stream = StreamGuard {
            inner: base_stream,
            engine: Arc::new(state.clone()),
            info_hash: info_hash.clone(),
            file_idx: idx,
            notified: false,
        };
        let body = Body::from_stream(guarded_stream);

        tracing::info!(
            "startup: direct stream response ready in {:?} (range {}-{}, partial={})",
            request_start.elapsed(),
            start,
            end,
            is_partial
        );

        if is_partial {
            (StatusCode::PARTIAL_CONTENT, res_headers, body).into_response()
        } else {
            (StatusCode::OK, res_headers, body).into_response()
        }
    } else {
        (StatusCode::NOT_FOUND, "File not found").into_response()
    }
}

fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let prefix = "bytes=";
    if !header.starts_with(prefix) || size == 0 {
        return None;
    }

    let range_str = &header[prefix.len()..];
    let parts: Vec<&str> = range_str.split('-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start_str = parts[0];
    let end_str = parts[1];

    if start_str.is_empty() {
        let suffix: u64 = end_str.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = size.saturating_sub(suffix);
        return Some((start, size - 1));
    }

    let start: u64 = start_str.parse().ok()?;
    let end = if end_str.is_empty() {
        size - 1
    } else {
        end_str.parse().ok()?
    };

    if start > end || start >= size {
        return None;
    }

    Some((start, end.min(size - 1)))
}
