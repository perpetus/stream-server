use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use axum::http::HeaderMap;
use enginefs::backend::{TorrentHandle, priorities::PlaybackIntent};
use futures_util::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::AsyncSeekExt;

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Guard that calls on_stream_end when dropped.
struct StreamLifecycleGuard {
    engine: Arc<enginefs::EngineFS>,
    info_hash: String,
    file_idx: usize,
    stream_id: u64,
    notified: bool,
}

impl StreamLifecycleGuard {
    fn new(
        engine: Arc<enginefs::EngineFS>,
        info_hash: String,
        file_idx: usize,
        stream_id: u64,
    ) -> Self {
        crate::diagnostics::logging::direct_stream_started();
        Self {
            engine,
            info_hash,
            file_idx,
            stream_id,
            notified: false,
        }
    }

    fn notify_end(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        crate::diagnostics::logging::direct_stream_ended();

        let engine = self.engine.clone();
        let info_hash = self.info_hash.clone();
        let file_idx = self.file_idx;
        let stream_id = self.stream_id;

        tokio::spawn(async move {
            engine.on_stream_end(&info_hash, file_idx).await;
            tracing::debug!(
                stream_id,
                info_hash = %info_hash,
                file_idx,
                "Stream lifecycle guard notified stream end"
            );
        });
    }
}

impl Drop for StreamLifecycleGuard {
    fn drop(&mut self) {
        self.notify_end();
    }
}

/// Guard that keeps the stream lifecycle alive for the response body.
struct StreamGuard<S> {
    inner: S,
    _lifecycle: StreamLifecycleGuard,
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

fn query_has_hls_intent(query_str: Option<&str>) -> bool {
    query_str
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes()).any(|(key, value)| {
                (key == "enginefs-intent" && value.eq_ignore_ascii_case("hls"))
                    || (key == "hls" && (value == "1" || value.eq_ignore_ascii_case("true")))
            })
        })
        .unwrap_or(false)
}

fn playback_intent_for_request(
    query_str: Option<&str>,
    priority: u8,
    start: u64,
    file_size: u64,
) -> PlaybackIntent {
    if priority == 255 {
        return PlaybackIntent::InternalProbe;
    }
    if priority == 0 {
        return PlaybackIntent::Background;
    }
    if start > 0 && start >= enginefs::backend::priorities::container_metadata_start(file_size) {
        return PlaybackIntent::ContainerMetadata;
    }

    let is_hls = query_has_hls_intent(query_str);
    match (is_hls, start == 0) {
        (true, true) => PlaybackIntent::HlsInitial,
        (true, false) => PlaybackIntent::HlsSeek,
        (false, true) => PlaybackIntent::DirectInitial,
        (false, false) => PlaybackIntent::DirectSeek,
    }
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
    let trackers = parse_trackers(query_str.clone());

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
    let stream_id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);

    tracing::debug!(
        stream_id,
        info_hash = %info_hash,
        file_idx = idx,
        "stream_video request"
    );

    // Parse trackers from query string 'tr=url&tr=url2'
    let trackers = parse_trackers(query_str.clone());
    tracing::debug!(
        stream_id,
        tracker_count = trackers.len(),
        "stream_video trackers parsed"
    );

    // Try to get existing engine, or auto-create from info hash
    let engine = if let Some(e) = state.engine.get_engine(&info_hash).await {
        tracing::debug!(stream_id, "stream_video engine found in cache");
        e
    } else {
        // Auto-create engine from magnet link
        tracing::debug!(
            stream_id,
            info_hash = %info_hash,
            "stream_video auto-creating engine"
        );
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        // Note: usage of enginefs::backend::TorrentSource requires enginefs dependency or import
        let source = enginefs::backend::TorrentSource::Url(magnet);

        match state.engine.add_torrent(source, Some(trackers)).await {
            Ok(e) => {
                tracing::debug!(stream_id, "stream_video engine created successfully");
                e
            }
            Err(e) => {
                tracing::error!(stream_id, error = %e, "stream_video failed to create engine");
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
        tracing::warn!(
            stream_id,
            info_hash = %info_hash,
            file_idx = idx,
            "stream_video file index not found before stream start"
        );
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    };
    let size = file_info.length;
    let name = file_info.name.clone();

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (start, end, is_partial) = if let Some(range) = &range_header {
        if let Some((start, end)) = parse_range(range, size) {
            (start, end, true)
        } else {
            tracing::warn!(
                stream_id,
                info_hash = %info_hash,
                file_idx = idx,
                range = %range,
                "stream_video invalid range header"
            );
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable").into_response();
        }
    } else {
        (0, size.saturating_sub(1), false)
    };
    let start_offset_hint = start;
    let is_container_metadata_probe =
        start > 0 && start >= enginefs::backend::priorities::container_metadata_start(size);

    // Parse priority from enginefs-prio header
    let priority: u8 = if let Some(prio_val) = headers.get("enginefs-prio") {
        prio_val.to_str().unwrap_or("1").parse().unwrap_or(1)
    } else {
        1
    };
    let playback_intent = playback_intent_for_request(query_str.as_deref(), priority, start, size);

    // --- Stream Lifecycle: Notify start only after validation has succeeded. ---
    state.engine.on_stream_start(&info_hash, idx).await;
    let lifecycle =
        StreamLifecycleGuard::new(state.engine.clone(), info_hash.clone(), idx, stream_id);
    state.engine.focus_torrent(&info_hash).await;

    // Await the async get_file
    tracing::debug!(
        stream_id,
        info_hash = %info_hash,
        file_idx = idx,
        start_offset = start_offset_hint,
        priority,
        intent = ?playback_intent,
        "stream_video calling get_file"
    );
    if let Some(mut file) = engine
        .get_file_with_intent(idx, start_offset_hint, priority, playback_intent)
        .await
    {
        tracing::debug!(
            stream_id,
            info_hash = %info_hash,
            file_idx = idx,
            size = file.size,
            "stream_video get_file returned success"
        );
        let size = file.size;
        let name = if file.name.is_empty() {
            name
        } else {
            file.name.clone()
        };

        tracing::debug!(
            stream_id,
            info_hash = %info_hash,
            file_idx = idx,
            "stream_video range request: {}-{} (total {})",
            start,
            end,
            size
        );

        if start >= size {
            tracing::warn!(
                stream_id,
                info_hash = %info_hash,
                file_idx = idx,
                start,
                size,
                "stream_video range not satisfiable"
            );
            return (StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable").into_response();
        }

        let readiness_timeout = match playback_intent {
            PlaybackIntent::DirectInitial => Some(Duration::from_secs(5)),
            PlaybackIntent::DirectSeek => Some(Duration::from_secs(1)),
            PlaybackIntent::HlsInitial => Some(Duration::from_secs(10)),
            PlaybackIntent::HlsSeek => Some(Duration::from_secs(1)),
            _ => None,
        };
        let enforce_readiness_before_headers = readiness_timeout.is_some();
        if !enforce_readiness_before_headers {
            tracing::info!(
                stream_id,
                info_hash = %info_hash,
                file_idx = idx,
                range_start = start,
                range_end = end,
                file_size = size,
                intent = ?playback_intent,
                container_metadata_probe = is_container_metadata_probe,
                "direct stream readiness bypassed"
            );
        } else {
            let readiness_timeout = readiness_timeout.expect("readiness timeout checked");
            let readiness = match engine
                .handle
                .wait_for_piece_ready(idx, start_offset_hint, readiness_timeout, playback_intent)
                .await
            {
                Ok(readiness) => readiness,
                Err(e) => {
                    tracing::warn!(
                        stream_id,
                        info_hash = %info_hash,
                        file_idx = idx,
                        intent = ?playback_intent,
                        error = %e,
                        "direct stream readiness failed"
                    );
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("Stream piece could not be prepared: {}", e),
                    )
                        .into_response();
                }
            };

            if !readiness.ready {
                let readiness_timeout_is_fatal = false;
                if readiness_timeout_is_fatal {
                    tracing::warn!(
                        stream_id,
                        info_hash = %info_hash,
                        file_idx = idx,
                        intent = ?playback_intent,
                        piece = readiness.piece,
                        ready_pieces = readiness.ready_pieces,
                        target_pieces = readiness.target_pieces,
                        peers = readiness.peers,
                        download_rate = readiness.download_rate,
                        elapsed_ms = readiness.elapsed_ms,
                        timeout_ms = readiness_timeout.as_millis() as u64,
                        reason = %readiness.reason,
                        fatal = true,
                        "direct stream readiness timed out"
                    );
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!(
                            "Stream piece was not ready after {}s (piece {}, peers {}, rate {} B/s)",
                            readiness_timeout.as_secs(),
                            readiness.piece,
                            readiness.peers,
                            readiness.download_rate
                        ),
                    )
                        .into_response();
                }

                tracing::warn!(
                    stream_id,
                    info_hash = %info_hash,
                    file_idx = idx,
                    intent = ?playback_intent,
                    piece = readiness.piece,
                    ready_pieces = readiness.ready_pieces,
                    target_pieces = readiness.target_pieces,
                    peers = readiness.peers,
                    download_rate = readiness.download_rate,
                    elapsed_ms = readiness.elapsed_ms,
                    timeout_ms = readiness_timeout.as_millis() as u64,
                    reason = %readiness.reason,
                    fatal = false,
                    "direct stream readiness timed out; continuing with body wait"
                );
            } else {
                tracing::info!(
                    stream_id,
                    info_hash = %info_hash,
                    file_idx = idx,
                    intent = ?playback_intent,
                    piece = readiness.piece,
                    ready_pieces = readiness.ready_pieces,
                    target_pieces = readiness.target_pieces,
                    peers = readiness.peers,
                    download_rate = readiness.download_rate,
                    elapsed_ms = readiness.elapsed_ms,
                    reason = %readiness.reason,
                    "direct stream readiness satisfied"
                );
            }
        }

        // Seek to the start position
        if start > 0 {
            tracing::debug!(
                stream_id,
                info_hash = %info_hash,
                file_idx = idx,
                start,
                "stream_video seeking"
            );
            if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                tracing::warn!(
                    stream_id,
                    info_hash = %info_hash,
                    file_idx = idx,
                    error = %e,
                    "stream_video seek error"
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, "Seek failed").into_response();
            }
            tracing::debug!(stream_id, "stream_video seek complete");
        }

        let content_length = end - start + 1;

        let mut res_headers = header::HeaderMap::new();

        let mime = content_type_for_name(&name);

        // Log detected file type
        tracing::info!(
            stream_id,
            info_hash = %info_hash,
            file_idx = idx,
            content_type = mime,
            file_name = %name,
            "media file detected"
        );

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
            _lifecycle: lifecycle,
        };
        let body = Body::from_stream(guarded_stream);

        tracing::info!(
            stream_id,
            info_hash = %info_hash,
            file_idx = idx,
            elapsed_ms = request_start.elapsed().as_millis() as u64,
            range_start = start,
            range_end = end,
            partial = is_partial,
            "startup: direct stream response ready"
        );

        if is_partial {
            (StatusCode::PARTIAL_CONTENT, res_headers, body).into_response()
        } else {
            (StatusCode::OK, res_headers, body).into_response()
        }
    } else {
        tracing::warn!(
            stream_id,
            info_hash = %info_hash,
            file_idx = idx,
            "stream_video get_file returned none after stream start"
        );
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
