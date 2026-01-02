use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use enginefs::backend::{SubtitleTrack, TorrentHandle};
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
                // Strip query string if present (e.g., "0?tr=..." -> "0")
                let file_part = parts[i + 1].split('?').next().unwrap_or("");
                if let Ok(idx) = file_part.parse::<usize>() {
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
        // Check if this is an embedded subtitle track ID (>= 1000)
        // For embedded subs, file_idx is actually the ID from find_subtitle_tracks
        // We need to find the main video file index for extraction
        if file_idx >= 1000 {
            // Find the main video file (largest file)
            let files = engine.handle.get_files().await;
            if let Some((main_file_idx, _)) = files.iter().enumerate().max_by_key(|(_, f)| f.length)
            {
                // Determine track ID (offset by 1000)
                let track_id = file_idx;

                match engine
                    .extract_embedded_subtitle(main_file_idx, track_id)
                    .await
                {
                    Ok(content) => {
                        // FFmpeg output is already VTT, but let's run it through our parser
                        // to ensure consistent styling if needed, or just return as is.
                        // For now, return as is since ffmpeg does a decent job.
                        return Response::builder()
                            .header("content-type", "text/vtt")
                            .header("access-control-allow-origin", "*")
                            .body(axum::body::Body::from(content))
                            .unwrap();
                    }
                    Err(e) => {
                        return Response::builder()
                            .status(500)
                            .body(axum::body::Body::from(format!("Extraction failed: {}", e)))
                            .unwrap();
                    }
                }
            }
        }

        // Regular external file handling
        if let Some(mut file) = engine.get_file(file_idx, 0, 0).await {
            let mut content = String::new();
            if file.read_to_string(&mut content).await.is_ok() {
                // Use the new subtitle parser which handles SRT, ASS, and VTT
                // with proper styling preservation
                let vtt_content = enginefs::subtitles::convert_to_vtt(&content);

                return Response::builder()
                    .header("content-type", "text/vtt")
                    .header("access-control-allow-origin", "*")
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

    // Use the new subtitle parser which handles SRT, ASS, and VTT
    // with proper styling preservation
    let vtt_content = enginefs::subtitles::convert_to_vtt(&content);

    Response::builder()
        .header("content-type", "text/vtt")
        .header("access-control-allow-origin", "*")
        .body(axum::body::Body::from(vtt_content))
        .unwrap()
}
