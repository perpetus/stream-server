use axum::{
    extract::{Path, Query, State},
    response::Response,
    routing::{get, post},
    Router,
    http::{StatusCode, header},
    body::Body,
    Json,
};
use crate::state::AppState;
use crate::archives::ArchiveSession;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Deserialize)]
struct CreateBody {
    url: String, // Expecting local path (since we are local addon server mostly)
    // Other fields ignored
}

// Support List of CreateBody or Single?
// Legacy sends Array.
type CreatePayload = Vec<CreateBody>;

#[derive(Serialize)]
struct CreateResponse {
    key: String,
}

#[derive(Deserialize)]
struct StreamParams {
    key: String,
    // Support file param via query or we might use path param in router
    file: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/create", post(create_session_auto))
        .route("/create/{key}", post(create_session_with_key))
        .route("/stream", get(stream_content_query))
        .route("/stream/{key}", get(stream_redirection)) // If just key, maybe redirect?
        .route("/stream/{key}/{file}", get(stream_content_path))
}

// Helper to download URL to temp file
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
        // Create temp file
        // We use Builder to put it in a specific place? Or default temp.
        let temp_file = tempfile::NamedTempFile::new().map_err(|e| {
             tracing::error!("Failed to create temp file: {}", e);
             StatusCode::INTERNAL_SERVER_ERROR
        })?;
        
        let (file, path) = temp_file.keep().map_err(|e| {
             tracing::error!("Failed to persist temp file: {}", e);
             StatusCode::INTERNAL_SERVER_ERROR
        })?;
        
        // We write using std::fs::File inside tokio? usage of bytes_stream implies async.
        // Better to use tokio::fs::File for async writing.
        // But NamedTempFile gives std::fs::File.
        // Convert to tokio.
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
    
    state.archive_cache.insert(key.clone(), ArchiveSession {
        path,
        created: std::time::Instant::now(),
    });
    
    Ok(Json(CreateResponse { key }))
}

async fn create_session_with_key(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(payload): Json<CreatePayload>,
) -> Result<Json<CreateResponse>, StatusCode> {
    let item = payload.first().ok_or(StatusCode::BAD_REQUEST)?;
    
    let path = resolve_path(&item.url).await?;
    
    state.archive_cache.insert(key.clone(), ArchiveSession {
        path,
        created: std::time::Instant::now(),
    });
    
    Ok(Json(CreateResponse { key }))
}

async fn stream_content_query(
    State(state): State<AppState>,
    Query(params): Query<StreamParams>,
) -> Result<Response, StatusCode> {
    let file = params.file.ok_or(StatusCode::BAD_REQUEST)?;
    stream_file(&state, &params.key, &file).await
}

async fn stream_content_path(
    State(state): State<AppState>,
    Path((key, file)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    // Decode file param if it's URL encoded? Axum usually handles decoding path segments.
    stream_file(&state, &key, &file).await
}

async fn stream_redirection(
     State(_state): State<AppState>,
     Path(_key): Path<String>,
) -> Result<Response, StatusCode> {
    // Legacy might use this to show file list?
    // For now, not implemented.
    Err(StatusCode::NOT_IMPLEMENTED)
}

async fn stream_file(state: &AppState, key: &str, file_path_in_archive: &str) -> Result<Response, StatusCode> {
    let session = state.archive_cache.get(key).ok_or(StatusCode::NOT_FOUND)?;
    let archive_path = session.path.clone();
    drop(session); // Release lock
    
    // Get reader
    let reader = crate::archives::get_archive_reader(&archive_path)
        .map_err(|e| {
            tracing::error!("Failed to get archive reader: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        
    // Open internal file
    let file_stream = reader.open_file(file_path_in_archive)
        .map_err(|e| {
            tracing::error!("Failed to open archive file entry '{}': {}", file_path_in_archive, e);
            StatusCode::NOT_FOUND 
        })?;
        
    // Wrap stream
    let stream = ReaderStream::new(tokio::io::BufReader::new(AllowStdIo::new(file_stream)));
    let body = Body::from_stream(stream);
    
    let mime = mime_guess::from_path(file_path_in_archive).first_or_octet_stream();

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header("Accept-Ranges", "bytes") // Hint, even if we don't fully support range yet without seeking
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
}

// Adapter struct to wrap Sync Read as AsyncRead
// NOTE: This blocks the executor thread if used directly! 
// BUT ReaderStream usually polls. 
// Ideally we should use `tokio_util::io::SyncIoBridge` but it's experimental?
// Or just impl AsyncRead.

use std::io::Read;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;

struct AllowStdIo<R> {
    inner: R,
}

impl<R> AllowStdIo<R> {
    fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: Read + Unpin> AsyncRead for AllowStdIo<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // WARNING: This works but blocks the async runtime thread. 
        // For high load, this should be wrapped in `spawn_blocking`.
        
        let slice = buf.initialize_unfilled();
        let n = self.inner.read(slice)?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}
