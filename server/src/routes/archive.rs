use crate::state::AppState;
use axum::{
    extract::{Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
// use futures_util::StreamExt;
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use tempfile::tempfile;
use zip::ZipArchive;

#[derive(Debug, Deserialize)]
pub struct ArchiveQuery {
    pub lz: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStreamBody {
    pub urls: Vec<ArchiveUrl>,
    pub file_idx: Option<usize>,
    pub file_must_include: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveUrl {
    pub url: String,
    // Core sends bytes sometimes, but we might not need it for simple streaming
    pub _bytes: Option<u64>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/*rest", get(archive_handler))
}

pub async fn archive_handler(
    Path(rest): Path<String>,
    Query(query): Query<ArchiveQuery>,
) -> impl IntoResponse {
    // Check if this is a remote archive creation request
    if rest == "create" {
        return handle_remote_archive(query).await;
    }

    // Existing legacy/local logic
    // Parity with /rar/... etc.
    // rest format usually: path/to/archive.zip/file/inside
    // We'll try to find the archive boundary.
    let parts: Vec<&str> = rest.split('/').collect();

    // Simplistic check for .zip or .rar
    if let Some(archive_idx) = parts
        .iter()
        .position(|&p| p.ends_with(".zip") || p.ends_with(".rar"))
    {
        let is_zip = parts[archive_idx].ends_with(".zip");
        let archive_path = parts[..=archive_idx].join("/");
        let file_path = parts[archive_idx + 1..].join("/");

        if is_zip {
            let file = match File::open(&archive_path) {
                Ok(f) => f,
                Err(_) => return (StatusCode::NOT_FOUND, "Archive not found").into_response(),
            };
            let mut archive = match ZipArchive::new(file) {
                Ok(a) => a,
                Err(_) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to open zip")
                        .into_response()
                }
            };
            let mut zip_file = match archive.by_name(&file_path) {
                Ok(f) => f,
                Err(_) => return (StatusCode::NOT_FOUND, "File not found in zip").into_response(),
            };
            let mut buffer = Vec::new();
            if zip_file.read_to_end(&mut buffer).is_err() {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to read file from zip",
                )
                    .into_response();
            }
            let content_type = mime_guess::from_path(&file_path)
                .first_raw()
                .unwrap_or("application/octet-stream");
            return ([(header::CONTENT_TYPE, content_type)], buffer).into_response();
        } else {
            // RAR handling
            // Note: unrar crate might need system dependencies or static linking
            let archive = match unrar::Archive::new(&archive_path).open_for_processing() {
                Ok(a) => a,
                Err(_) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to open rar")
                        .into_response()
                }
            };

            if let Ok(Some(_header)) = archive.read_header() {
                return (
                    StatusCode::NOT_IMPLEMENTED,
                    "RAR streaming is still being refined (unrar-lib dependency)",
                )
                    .into_response();
            }

            return (StatusCode::NOT_FOUND, "File not found in rar").into_response();
        }
    }

    (StatusCode::NOT_IMPLEMENTED, "Unknown archive path format").into_response()
}

async fn handle_remote_archive(query: ArchiveQuery) -> Response {
    let lz_data = match query.lz {
        Some(lz) => lz,
        None => return (StatusCode::BAD_REQUEST, "Missing lz parameter").into_response(),
    };

    // Decompress lz-string
    let utf16_data = match lz_str::decompress_from_encoded_uri_component(&lz_data) {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, "Failed to decompress lz data").into_response(),
    };
    let json_str = match String::from_utf16(&utf16_data) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid UTF-16 in lz data").into_response(),
    };

    let body: ArchiveStreamBody = match serde_json::from_str(&json_str) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to parse archive body: {}", e),
            )
                .into_response()
        }
    };

    if body.urls.is_empty() {
        return (StatusCode::BAD_REQUEST, "No archive URLs provided").into_response();
    }

    // For now, take the first URL
    let archive_url = &body.urls[0].url;

    // Download to temp file
    let mut temp = match tempfile() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create temp file: {}", e),
            )
                .into_response()
        }
    };

    let client = reqwest::Client::new();
    let mut response = match client.get(archive_url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch upstream archive: {}", e),
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

    while let Some(chunk) = match response.chunk().await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to download archive chunk: {}", e),
            )
                .into_response()
        }
    } {
        if let Err(e) = std::io::Write::write_all(&mut temp, &chunk) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write to temp file: {}", e),
            )
                .into_response();
        }
    }

    // Seek back to start
    if let Err(e) = temp.seek(SeekFrom::Start(0)) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to seek temp file: {}", e),
        )
            .into_response();
    }

    // Process ZIP
    let is_zip = archive_url.to_lowercase().ends_with(".zip");

    if is_zip {
        return handle_zip_extraction(temp, &body).await;
    } else {
        (
            StatusCode::NOT_IMPLEMENTED,
            "Remote RAR streaming not yet fully implemented",
        )
            .into_response()
    }
}

async fn handle_zip_extraction(file: File, body: &ArchiveStreamBody) -> Response {
    let mut archive = match ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to open zip: {}", e),
            )
                .into_response()
        }
    };

    // Find file index
    let target_index = if let Some(idx) = body.file_idx {
        idx
    } else if let Some(includes) = &body.file_must_include {
        let mut found_idx = None;
        for i in 0..archive.len() {
            if let Ok(f) = archive.by_index(i) {
                let name = f.name();
                if !includes.is_empty() && name.contains(&includes[0]) {
                    found_idx = Some(i);
                    break;
                }
            }
        }
        match found_idx {
            Some(i) => i,
            None => {
                return (StatusCode::NOT_FOUND, "No matching file found in zip").into_response()
            }
        }
    } else {
        // Default to largest
        let mut largest_idx = 0;
        let mut max_size = 0;
        for i in 0..archive.len() {
            if let Ok(f) = archive.by_index(i) {
                if f.size() > max_size {
                    max_size = f.size();
                    largest_idx = i;
                }
            }
        }
        largest_idx
    };

    let mut zip_file = match archive.by_index(target_index) {
        Ok(f) => f,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "Target file index not found in zip").into_response()
        }
    };

    let mut buffer = Vec::new();
    if let Err(e) = zip_file.read_to_end(&mut buffer) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to extract file: {}", e),
        )
            .into_response();
    }

    let fname = zip_file.name().to_string();
    let content_type = mime_guess::from_path(&fname)
        .first_raw()
        .unwrap_or("application/octet-stream");

    ([(header::CONTENT_TYPE, content_type)], buffer).into_response()
}
