use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
use enginefs::backend::SubtitleTrack;
use regex::Regex;
use serde_json::json;
use tokio::io::AsyncReadExt;

#[derive(serde::Deserialize)]
pub struct OpensubHashQuery {
    #[serde(rename = "videoUrl")]
    pub video_url: Option<String>,
}

pub async fn opensub_hash(
    State(state): State<AppState>,
    Query(query): Query<OpensubHashQuery>,
) -> impl IntoResponse {
    let url = query.video_url.unwrap_or_default();

    // Heuristic: URL like /:infoHash/:fileIdx/...
    let parts: Vec<&str> = url.split('/').collect();
    let mut info_hash = None;
    let mut file_idx = None;

    for (i, part) in parts.iter().enumerate() {
        if part.len() == 40 && hex::decode(part).is_ok() {
            info_hash = Some(part.to_string());
            if i + 1 < parts.len() {
                if let Ok(idx) = parts[i + 1].parse::<usize>() {
                    file_idx = Some(idx);
                }
            }
            break;
        }
    }

    if let (Some(info_hash), Some(file_idx)) = (info_hash, file_idx) {
        if let Some(engine) = state.engine.get_engine(&info_hash).await {
            match engine.get_opensub_hash(file_idx).await {
                Ok(hash) => return Json(json!({ "result": { "hash": hash } })),
                Err(e) => return Json(json!({ "error": e.to_string() })),
            }
        }
    }

    Json(json!({ "error": "Could not identify file from URL" }))
}

pub async fn opensub_hash_path(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
) -> impl IntoResponse {
    let info_hash = info_hash.to_lowercase();
    if let Some(engine) = state.engine.get_engine(&info_hash).await {
        match engine.get_opensub_hash(file_idx).await {
            Ok(hash) => return Json(json!({ "result": { "hash": hash } })),
            Err(e) => return Json(json!({ "error": e.to_string() })),
        }
    }
    Json(json!({ "error": "Engine not found" }))
}

#[derive(serde::Deserialize)]
pub struct SubtitlesTracksQuery {
    #[serde(rename = "subsUrl")]
    pub subs_url: Option<String>,
}

pub async fn subtitles_tracks(
    State(state): State<AppState>,
    Query(query): Query<SubtitlesTracksQuery>,
) -> impl IntoResponse {
    let url = query.subs_url.unwrap_or_default();
    let mut info_hash = None;

    for part in url.split('/') {
        if part.len() == 40 && hex::decode(part).is_ok() {
            info_hash = Some(part.to_string());
            break;
        }
    }

    if let Some(info_hash) = info_hash {
        if let Some(engine) = state.engine.get_engine(&info_hash).await {
            let tracks: Vec<SubtitleTrack> = engine.find_subtitle_tracks().await;

            let result: Vec<serde_json::Value> = tracks
                .into_iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "lang": "Unknown",
                        "label": t.name,
                        "url": format!("/{}/{}/subtitles.vtt", info_hash, t.id)
                    })
                })
                .collect();

            return Json(json!({ "result": result }));
        }
    }

    Json(json!({ "result": [] }))
}

pub async fn get_subtitles_vtt(
    State(state): State<AppState>,
    Path((info_hash, file_idx)): Path<(String, usize)>,
) -> Response {
    if let Some(engine) = state.engine.get_engine(&info_hash).await {
        if let Some(mut file) = engine.get_file(file_idx, 0, 0).await {
            let mut content = String::new();
            if file.read_to_string(&mut content).await.is_ok() {
                // Convert to VTT if needed
                let mut vtt_content = String::new();
                if !content.trim_start().starts_with("WEBVTT") {
                    vtt_content.push_str("WEBVTT\n\n");
                }

                // Replace comma timestamps with dots: 00:00:00,000 -> 00:00:00.000
                // Using regex for safety.
                // Pattern: (\d{2}:\d{2}:\d{2}),(\d{3})
                let re = Regex::new(r"(\d{2}:\d{2}:\d{2}),(\d{3})").unwrap();
                let converted = re.replace_all(&content, "$1.$2");
                vtt_content.push_str(&converted);

                return Response::builder()
                    .header("content-type", "text/vtt")
                    .body(axum::body::Body::from(vtt_content))
                    .unwrap();
            }
        }
    }
    Response::builder()
        .status(404)
        .body(axum::body::Body::empty())
        .unwrap()
}

#[derive(serde::Deserialize)]
pub struct ProxySubtitlesQuery {
    pub from: Option<String>,
}

pub async fn proxy_subtitles_vtt(Query(query): Query<ProxySubtitlesQuery>) -> Response {
    let from_url = match query.from {
        Some(url) => url,
        None => {
            return Response::builder()
                .status(400)
                .body(axum::body::Body::from("Missing 'from' parameter"))
                .unwrap();
        }
    };

    // Fetch the subtitle from the external URL
    let client = reqwest::Client::new();
    let resp = match client.get(&from_url).send().await {
        Ok(r) => r,
        Err(e) => {
            return Response::builder()
                .status(502)
                .body(axum::body::Body::from(format!(
                    "Failed to fetch subtitles: {}",
                    e
                )))
                .unwrap();
        }
    };

    let content = match resp.text().await {
        Ok(c) => c,
        Err(e) => {
            return Response::builder()
                .status(502)
                .body(axum::body::Body::from(format!(
                    "Failed to read subtitles: {}",
                    e
                )))
                .unwrap();
        }
    };

    // Convert to VTT if needed
    let mut vtt_content = String::new();
    if !content.trim_start().starts_with("WEBVTT") {
        vtt_content.push_str("WEBVTT\n\n");
    }

    // Replace comma timestamps with dots: 00:00:00,000 -> 00:00:00.000
    let re = Regex::new(r"(\d{2}:\d{2}:\d{2}),(\d{3})").unwrap();
    let converted = re.replace_all(&content, "$1.$2");
    vtt_content.push_str(&converted);

    Response::builder()
        .header("content-type", "text/vtt")
        .body(axum::body::Body::from(vtt_content))
        .unwrap()
}
