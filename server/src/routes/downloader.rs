use axum::{
    Json, Router,
    extract::Query,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde_json::json;
use std::collections::HashMap;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/getAll", get(get_all))
        .route("/get", get(get_by_id))
        .route("/add", get(add))
        .route("/pause", get(pause))
        .route("/resume", get(resume))
        .route("/remove", get(remove))
}

async fn get_all() -> impl IntoResponse {
    Json(json!([]))
}

async fn get_by_id(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    match params.get("id") {
        Some(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": "there is no download with this id" } })),
        ),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": "missing id" } })),
        ),
    }
}

async fn add(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    match params.get("url") {
        Some(_) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": { "message": "HTTP file downloader is not implemented" } })),
        ),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "code": 1, "message": "missing url" } })),
        ),
    }
}

async fn pause() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": { "message": "HTTP file downloader is not implemented" } })),
    )
}

async fn resume() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": { "message": "HTTP file downloader is not implemented" } })),
    )
}

async fn remove() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": { "message": "HTTP file downloader is not implemented" } })),
    )
}
