use crate::archives::ArchiveSession;
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};
use enginefs::backend::TorrentHandle;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Deserialize)]
struct CreateBody {
    url: String,
}

type CreatePayload = Vec<CreateBody>;

#[derive(Serialize)]
struct CreateResponse {
    key: String,
}

#[derive(Deserialize)]
struct StreamParams {
    key: String,
    file: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/create", post(create_session_auto))
        .route("/create/{key}", post(create_session_with_key))
        .route("/stream", get(stream_content_query))
        .route("/stream/{key}", get(stream_redirection))
        .route("/stream/{key}/{file}", get(stream_content_path))
}

async fn resolve_path(url: &str) -> Result<PathBuf, StatusCode> {
    if url.starts_with("http://") || url.starts_with("https://") {
        tracing::info!("Downloading archive from URL: {}", url);
        let response = reqwest::get(url).await.map_err(|e| {
            tracing::error!("Failed to fetch URL {}: {}", url, e);
            StatusCode::BAD_REQUEST
        })?;

        if !response.status().is_success() {
            tracing::error!("URL {} returned status {}", url, response.status());
            return Err(StatusCode::NOT_FOUND);
        }

        let mut content = response.bytes_stream();
        let temp_file = tempfile::NamedTempFile::new().map_err(|e| {
            tracing::error!("Failed to create temp file: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let (file, path) = temp_file.keep().map_err(|e| {
            tracing::error!("Failed to persist temp file: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let mut async_file = tokio::fs::File::from_std(file);

        use futures_util::StreamExt;
        while let Some(chunk) = content.next().await {
            let chunk = chunk.map_err(|e| {
                tracing::error!("Download stream error: {}", e);
                StatusCode::BAD_GATEWAY
            })?;
            use tokio::io::AsyncWriteExt;
            async_file.write_all(&chunk).await.map_err(|e| {
                tracing::error!("Failed to write to temp file: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        }

        tracing::info!("Downloaded {} to {:?}", url, path);
        Ok(path)
    } else {
        let path = PathBuf::from(url);
        if !path.exists() {
            // It might be a valid local path for some setups, but we generally expect existence
            // For torrent relative paths, this function is used by 'create' which assumes local or http.
            return Err(StatusCode::NOT_FOUND);
        }
        Ok(path)
    }
}

async fn create_session_auto(
    State(state): State<AppState>,
    Json(payload): Json<CreatePayload>,
) -> Result<Json<CreateResponse>, StatusCode> {
    let item = payload.first().ok_or(StatusCode::BAD_REQUEST)?;
    let key = Uuid::new_v4().to_string();

    let path = resolve_path(&item.url).await?;

    state.archive_cache.insert(
        key.clone(),
        ArchiveSession {
            path,
            created: std::time::Instant::now(),
        },
    );

    Ok(Json(CreateResponse { key }))
}

async fn create_session_with_key(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(payload): Json<CreatePayload>,
) -> Result<Json<CreateResponse>, StatusCode> {
    let item = payload.first().ok_or(StatusCode::BAD_REQUEST)?;

    let path = resolve_path(&item.url).await?;

    state.archive_cache.insert(
        key.clone(),
        ArchiveSession {
            path,
            created: std::time::Instant::now(),
        },
    );

    Ok(Json(CreateResponse { key }))
}

async fn stream_content_query(
    State(state): State<AppState>,
    headers: header::HeaderMap,
    Query(params): Query<StreamParams>,
) -> Result<Response, StatusCode> {
    let file = params.file.ok_or(StatusCode::BAD_REQUEST)?;
    stream_file(&state, &params.key, &file, &headers).await
}

async fn stream_content_path(
    State(state): State<AppState>,
    headers: header::HeaderMap,
    Path((key, file)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    stream_file(&state, &key, &file, &headers).await
}

async fn stream_redirection(
    State(_state): State<AppState>,
    Path(_key): Path<String>,
) -> Result<Response, StatusCode> {
    Err(StatusCode::NOT_IMPLEMENTED)
}

// New implementation of stream_file
async fn stream_file(
    state: &AppState,
    key: &str,
    file_path_in_archive: &str,
    headers: &header::HeaderMap,
) -> Result<Response, StatusCode> {
    // 1. Determine Input Source
    let archive_reader: Box<dyn crate::archives::ArchiveReader> = if key.starts_with("torrent:") {
        // Format: torrent:<info_hash>/path/to/archive
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() < 2 {
            return Err(StatusCode::BAD_REQUEST);
        }
        let hash_part = parts[0].strip_prefix("torrent:").unwrap();
        // The path part inside the torrent:
        let archive_internal_path = parts.iter().skip(1).copied().collect::<Vec<_>>().join("/");

        let engine = &state.engine;
        // EngineFS uses string info_hash
        // let sha_hash = crate::engine::SHA1::from_hex(&hash_part).map_err(|_| StatusCode::BAD_REQUEST)?;

        if let Some(engine_instance) = engine.get_engine(&hash_part).await {
            // engine_instance is Arc<Engine<H>>
            // We need to find the file inside this engine.
            // Engine has `handle`.
            let handle = &engine_instance.handle;

            // handle is `H: TorrentHandle`.
            let stats = handle.stats().await;
            let files = stats.files;

            // Find index
            if let Some(idx) = files.iter().position(|f| f.name == archive_internal_path) {
                // get_file_reader(idx, offset, priority)
                let reader = handle
                    .get_file_reader(idx, 0, 7, None)
                    .await // 7 = high priority
                    .map_err(|e| {
                        tracing::error!("Failed to get file stream: {}", e);
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;

                // Define Wrapper to bridge enginefs::backend::FileStreamTrait to AsyncSeekableReader
                // FileStreamTrait requires AsyncRead + AsyncSeek + Unpin + Send
                // AsyncSeekableReader requires AsyncRead + AsyncSeek + Unpin + Send + Sync
                // Wait, FileStreamTrait is Send?
                // Checking libtorrent.rs: returns Result<Box<dyn FileStreamTrait>>.
                // We need to wrap it.

                struct BackendStreamWrapper(Box<dyn enginefs::backend::FileStreamTrait>);
                // Safety: We assume FileStreamTrait is Send. LibtorrentFileStream is Send.
                // But generic trait object?
                // enginefs definition: trait FileStreamTrait: AsyncRead + AsyncSeek + Unpin + Send {}
                // So wrapper is Send.
                // Is it Sync? Box<...> is Sync if dyn Trait + Sync.
                // If not Sync, we can't implement AsyncSeekableReader if it requires Sync.
                // server/src/archives/mod.rs: pub trait AsyncSeekableReader: ... + Sync {}
                // We need Sync.
                // LibtorrentFileStream contains Arc<RwLock<...>> which is Sync.
                // BUT Type alias is Box<dyn FileStreamTrait>.
                // If the trait doesn't enforce Sync, we can't guarantee it.
                // However, we can wrap it in a Mutex? No, that's heavy.
                // Or we can just implement UnsafeSync wrapper if we are sure?
                // Or better: Change AsyncSeekableReader to NOT require Sync?
                // sevenz bridge requires Send (for moving to thread). Does it require Sync?
                // It takes `Box<dyn AsyncSeekableReader>`.
                // Bridge moves it to async task (spawn).
                // Async task is Send.
                // So reader must be Send. Sync is only needed if accessed from multiple threads concurrently.
                // We don't do that.
                // So I should REMOVE Sync from AsyncSeekableReader in `mod.rs`.

                // For now, let's wrap and unsafe impl Sync if needed, OR fix mod.rs.
                // Fixing mod.rs is cleaner. I will do that in next step.
                // But for now, let's assume valid.

                // Wrapper impls
                impl tokio::io::AsyncRead for BackendStreamWrapper {
                    fn poll_read(
                        mut self: std::pin::Pin<&mut Self>,
                        cx: &mut std::task::Context<'_>,
                        buf: &mut tokio::io::ReadBuf<'_>,
                    ) -> std::task::Poll<std::io::Result<()>> {
                        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
                    }
                }
                impl tokio::io::AsyncSeek for BackendStreamWrapper {
                    fn start_seek(
                        mut self: std::pin::Pin<&mut Self>,
                        position: std::io::SeekFrom,
                    ) -> std::io::Result<()> {
                        std::pin::Pin::new(&mut self.0).start_seek(position)
                    }
                    fn poll_complete(
                        mut self: std::pin::Pin<&mut Self>,
                        cx: &mut std::task::Context<'_>,
                    ) -> std::task::Poll<std::io::Result<u64>> {
                        std::pin::Pin::new(&mut self.0).poll_complete(cx)
                    }
                }
                // If we need Sync and trait doesn't provide it, we are stuck unless we relax requirement or wrap in Mutex.
                // Mutex provides Sync. Use tokio::sync::Mutex? No, AsyncRead needs &mut.
                // std::sync::Mutex? Blocks.
                // Let's modify `AsyncSeekableReader` to NOT require Sync.

                let wrapped_reader = Box::new(BackendStreamWrapper(reader));

                // We need to ensure wrapped_reader is `AsyncSeekableReader`.
                // Ideally `ArchiveReader` accepts `Box<dyn AsyncSeekableReader>`.

                crate::archives::get_archive_reader_from_stream(
                    wrapped_reader,
                    &archive_internal_path,
                )
                .map_err(|e| {
                    tracing::error!("Failed to create stream reader: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
            } else {
                return Err(StatusCode::NOT_FOUND);
            }
        } else {
            return Err(StatusCode::NOT_FOUND);
        }
    } else {
        // Local Session
        // Build cache config from app state settings
        let settings = state.settings.read().await;
        let cache_config = crate::archives::CacheConfig {
            cache_dir: Some(std::path::PathBuf::from(&settings.cache_root)),
            _cache_size: settings.cache_size as u64,
        };
        drop(settings);

        let session_map = state.archive_cache.clone();
        let session = session_map.get(key).ok_or(StatusCode::NOT_FOUND)?;
        let path = session.path.clone();
        crate::archives::get_archive_reader_with_config(&path, cache_config)
            .await
            .map_err(|e| {
                tracing::error!("Failed to create reader for {:?}: {}", path, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    };

    // 2. Open Entry
    let mut reader = archive_reader
        .open_file(file_path_in_archive)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // 3. Determine Content Length
    let file_size = reader
        .seek(tokio::io::SeekFrom::End(0))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    reader
        .seek(tokio::io::SeekFrom::Start(0))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // 4. Handle Range Requests
    let mut start = 0;
    let mut end = file_size.saturating_sub(1);
    let mut is_partial = false;

    if let Some(range_header) = headers.get(header::RANGE).and_then(|h| h.to_str().ok()) {
        if let Some(parsed) = parse_range(range_header, file_size) {
            start = parsed.0;
            end = parsed.1;
            is_partial = true;
        }
    }

    // Seek to start
    reader
        .seek(tokio::io::SeekFrom::Start(start))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let len = end - start + 1;

    // Limit reader
    let limited_reader = reader.take(len);

    // Convert to Body stream
    let stream = ReaderStream::new(limited_reader);
    let body = Body::from_stream(stream);

    // 5. Build Response
    let mime = mime_guess::from_path(file_path_in_archive).first_or_octet_stream();
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len);

    if is_partial {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, file_size),
        );
    } else {
        builder = builder.status(StatusCode::OK);
    }

    Ok(builder.body(body).unwrap())
}

fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let prefix = "bytes=";
    if !header.starts_with(prefix) {
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
        // Suffix byte range: bytes=-500 (last 500 bytes)
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

    Some((start, std::cmp::min(end, size - 1)))
}
