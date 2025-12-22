use crate::state::AppState;
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Redirect},
    routing::get,
    Json, Router,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/{id}", get(youtube_handler))
}

pub async fn youtube_handler(Path(id): Path<String>) -> impl IntoResponse {
    if id.ends_with(".json") {
        let real_id = id.trim_end_matches(".json").to_string();
        get_youtube_json(Path(real_id)).await.into_response()
    } else {
        redirect_youtube(Path(id)).await.into_response()
    }
}

pub async fn redirect_youtube(Path(id): Path<String>) -> impl IntoResponse {
    let video_url = format!("https://www.youtube.com/watch?v={}", id);
    let video = match rusty_ytdl::Video::new(video_url) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid YouTube ID").into_response(),
    };

    let video_info = match video.get_info().await {
        Ok(info) => info,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get video info: {}", e),
            )
                .into_response()
        }
    };

    // Filter for a format that has both audio and video, prioritizing mp4
    let format = video_info
        .formats
        .iter()
        .find(|f| f.has_audio && f.has_video && f.mime_type.mime.to_string().contains("video/mp4"))
        .or_else(|| {
            video_info
                .formats
                .iter()
                .find(|f| f.has_audio && f.has_video)
        });

    match format {
        Some(f) => Redirect::temporary(&f.url).into_response(),
        None => (StatusCode::NOT_FOUND, "No suitable video format found").into_response(),
    }
}

pub async fn get_youtube_json(Path(id): Path<String>) -> impl IntoResponse {
    let video_url = format!("https://www.youtube.com/watch?v={}", id);
    let video = match rusty_ytdl::Video::new(video_url) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid YouTube ID").into_response(),
    };

    let video_info = match video.get_info().await {
        Ok(info) => info,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get video info: {}", e),
            )
                .into_response()
        }
    };

    // Parity with getYoutubeStream in server.js: filter for "audioandvideo"
    let format = video_info
        .formats
        .iter()
        .find(|f| f.has_audio && f.has_video && f.mime_type.mime.to_string().contains("video/mp4"))
        .or_else(|| {
            video_info
                .formats
                .iter()
                .find(|f| f.has_audio && f.has_video)
        });

    match format {
        Some(f) => Json(serde_json::json!({
            "url": f.url,
            "itag": f.itag,
            "quality": format!("{:?}", f.quality).to_lowercase(),
            "container": f.mime_type.container.to_lowercase(),
            "hasVideo": f.has_video,
            "hasAudio": f.has_audio,
            "isLive": f.is_live,
            "isHLS": f.is_hls,
            "isDashMPD": f.is_dash_mpd,
            "approxDurationMs": f.approx_duration_ms,
            "mimeType": f.mime_type.mime.to_string()
        }))
        .into_response(),
        None => (StatusCode::NOT_FOUND, "No suitable video format found").into_response(),
    }
}
