use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::StreamExt;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct FtpQuery {
    pub lz: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FtpStreamBody {
    pub ftp_url: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/{filename}", get(stream_ftp))
}

async fn stream_ftp(Path(filename): Path<String>, Query(params): Query<FtpQuery>) -> Response {
    let lz_data = match params.lz {
        Some(lz) => lz,
        None => return (StatusCode::BAD_REQUEST, "Missing lz parameter").into_response(),
    };

    // Decompress lz-string (returns Vec<u16>)
    let utf16_data = match lz_str::decompress_from_encoded_uri_component(&lz_data) {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, "Failed to decompress lz data").into_response(),
    };
    let json_str = match String::from_utf16(&utf16_data) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid UTF-16 in lz data").into_response(),
    };

    let body: FtpStreamBody = match serde_json::from_str(&json_str) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to parse FTP body: {}", e),
            )
                .into_response()
        }
    };

    let url = &body.ftp_url;

    // For HTTP/HTTPS URLs, use reqwest (cross-platform)
    // For actual FTP URLs, we need curl or a dedicated FTP library
    if url.starts_with("http://") || url.starts_with("https://") {
        return stream_http(url, &filename).await;
    }

    // For FTP/FTPS, attempt curl (available on Linux/macOS, less common on Windows)
    stream_via_curl(url, &filename).await
}

async fn stream_http(url: &str, filename: &str) -> Response {
    let client = reqwest::Client::new();
    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch URL: {}", e),
            )
                .into_response()
        }
    };

    if !response.status().is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            format!("Upstream returned {}", response.status()),
        )
            .into_response();
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            mime_guess::from_path(filename)
                .first_or_octet_stream()
                .to_string()
        });

    let stream = response
        .bytes_stream()
        .map(|result| result.map_err(std::io::Error::other));

    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", filename),
        )
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn stream_via_curl(url: &str, filename: &str) -> Response {
    use tokio_util::io::ReaderStream;

    // curl is typically available on Linux/macOS, less so on Windows
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["-s", "-L", url])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // On Windows, curl might not be available
            let msg = if cfg!(windows) {
                format!("FTP streaming requires curl to be installed: {}", e)
            } else {
                format!("Failed to spawn curl: {}", e)
            };
            return (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "No stdout").into_response(),
    };

    let stream = ReaderStream::new(stdout);

    let content_type = mime_guess::from_path(filename)
        .first_or_octet_stream()
        .to_string();

    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", filename),
        )
        .body(Body::from_stream(stream))
        .unwrap()
}
