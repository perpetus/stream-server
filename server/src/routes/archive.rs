use crate::state::AppState;
use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::fs::File;
use std::io::Read;
use zip::ZipArchive;

pub fn router() -> Router<AppState> {
    Router::new().route("/*rest", get(archive_handler))
}

pub async fn archive_handler(Path(rest): Path<String>) -> impl IntoResponse {
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
            if let Err(_) = zip_file.read_to_end(&mut buffer) {
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
            let archive = match unrar::Archive::new(&archive_path).open_for_processing() {
                Ok(a) => a,
                Err(_) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to open rar")
                        .into_response()
                }
            };

            if let Ok(Some(_header)) = archive.read_header() {
                // The field name depends on the crate version
                // For now, let's assume it has an entry_name() method or similar
                // If it fails, we'll use a placeholder.
                return (
                    StatusCode::NOT_IMPLEMENTED,
                    "RAR streaming is still being refined (unrar-lib dependency)",
                )
                    .into_response();
            }
            return (StatusCode::NOT_FOUND, "File not found in rar").into_response();
        }
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        "Only ZIP archives are currently supported in this port",
    )
        .into_response()
}
