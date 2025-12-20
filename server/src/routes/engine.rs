use crate::state::AppState;
use axum::{
    extract::{Json, State},
    response::IntoResponse,
};
use hex;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct CreateEngineRequest {
    pub from: Option<String>, // Magnet link or URL
    #[serde(alias = "blob")]
    pub torrent: Option<String>, // Torrent blob (hex encoded) - alias "blob" for stremio-core compat
    pub announce: Option<Vec<String>>,
}

pub async fn create_engine(
    State(state): State<AppState>,
    Json(payload): Json<CreateEngineRequest>,
) -> impl IntoResponse {
    let source = if let Some(hex_str) = payload.torrent {
        match hex::decode(hex_str) {
            Ok(bytes) => enginefs::backend::TorrentSource::Bytes(bytes),
            Err(e) => return Json(json!({ "error": format!("Invalid hex blob: {}", e) })),
        }
    } else if let Some(from) = payload.from {
        enginefs::backend::TorrentSource::Url(from)
    } else {
        return Json(json!({ "error": "Missing 'from' or 'torrent' field" }));
    };

    match state.engine.add_torrent(source, payload.announce).await {
        Ok(engine) => {
            let stats = engine.get_statistics().await;
            // Note: guessed_file_idx and file_must_include heuristics are now
            // applied internally by get_statistics() for stream_name/stream_len
            Json(json!(stats))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
pub async fn list_engines(State(state): State<AppState>) -> impl IntoResponse {
    let engines = state.engine.list_engines().await;
    Json(json!(engines))
}

pub async fn remove_engine(
    State(state): State<AppState>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
) -> impl IntoResponse {
    state.engine.remove_engine(&info_hash.to_lowercase()).await;
    Json(json!({}))
}

pub async fn remove_all_engines(State(state): State<AppState>) -> impl IntoResponse {
    let engines = state.engine.list_engines().await;
    for ih in engines {
        state.engine.remove_engine(&ih).await;
    }
    Json(json!({}))
}

/// Stremio-core /{infoHash}/create endpoint for magnet links
#[derive(Deserialize)]
pub struct CreateMagnetRequest {
    pub stream: Option<CreateMagnetStream>,
}

#[derive(Deserialize)]
pub struct CreateMagnetStream {
    #[serde(rename = "infoHash")]
    pub info_hash: Option<String>,
}

pub async fn create_magnet(
    State(state): State<AppState>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
    Json(payload): Json<CreateMagnetRequest>,
) -> impl IntoResponse {
    // Use the info_hash from path or body
    let ih = payload
        .stream
        .as_ref()
        .and_then(|s| s.info_hash.as_ref())
        .map(|s| s.as_str())
        .unwrap_or(&info_hash);

    // Create magnet URL from info hash
    let magnet = format!("magnet:?xt=urn:btih:{}", ih);
    let source = enginefs::backend::TorrentSource::Url(magnet);

    match state.engine.add_torrent(source, None).await {
        Ok(engine) => {
            let stats = engine.get_statistics().await;
            Json(json!(stats))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
