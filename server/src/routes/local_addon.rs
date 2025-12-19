use crate::state::AppState;
use axum::{extract::Path, response::IntoResponse, routing::get, Json, Router};
use serde_json::json;
use walkdir::WalkDir;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/manifest.json", get(get_manifest))
        .route("/catalog/:type/:id.json", get(get_catalog))
        .route("/meta/:type/:id.json", get(get_meta))
        .route("/stream/:type/:id.json", get(get_stream))
}

pub async fn get_manifest() -> impl IntoResponse {
    Json(json!({
        "id": "org.stremio.local-rust",
        "name": "Local Files (Rust)",
        "description": "Ported local addon for Stremio",
        "version": "0.1.0",
        "resources": ["catalog", "meta", "stream"],
        "types": ["movie", "series", "other"],
        "catalogs": [
            {
                "type": "other",
                "id": "local-files",
                "name": "Local Files"
            }
        ]
    }))
}

pub async fn get_catalog(Path((_type, _id)): Path<(String, String)>) -> impl IntoResponse {
    // Basic implementation: scan a directory (e.g., Downloads) and return meta
    let mut metas = Vec::new();

    // For now, let's hardcode a path or use a placeholder
    let watch_path = "/home/matthew/Downloads";

    for entry in WalkDir::new(watch_path)
        .max_depth(3)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if ["mp4", "mkv", "avi", "mov"].contains(&ext.to_lowercase().as_str()) {
                    let filename = path.file_name().unwrap_or_default().to_string_lossy();
                    metas.push(json!({
                        "id": format!("local:{}", filename),
                        "type": "other",
                        "name": filename,
                        "posterShape": "square"
                    }));
                }
            }
        }
        if metas.len() >= 100 {
            break;
        }
    }

    Json(json!({ "metas": metas }))
}

pub async fn get_meta(Path((_type, id)): Path<(String, String)>) -> impl IntoResponse {
    Json(json!({
        "meta": {
            "id": id,
            "type": "other",
            "name": id.trim_start_matches("local:"),
            "posterShape": "square"
        }
    }))
}

pub async fn get_stream(Path((_type, id)): Path<(String, String)>) -> impl IntoResponse {
    // Return a local file stream URL
    let filename = id.trim_start_matches("local:");
    // We need to resolve the actual path. For now, assume it's under Downloads.
    let stream_url = format!(
        "http://127.0.0.1:11470/stream/local/{}",
        urlencoding::encode(filename)
    );

    Json(json!({
        "streams": [
            {
                "title": "Watch Local",
                "url": stream_url
            }
        ]
    }))
}
