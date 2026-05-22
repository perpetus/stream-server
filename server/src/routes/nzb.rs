use crate::archives::nzb::session::{NzbConfig, NzbServerConfig, NzbSession};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
// use std::path::PathBuf;

#[derive(Deserialize, Debug)]
struct NzbServer {
    host: String,
    port: u16,
    user: Option<String>,
    pass: Option<String>,
    ssl: bool,
    connections: u32,
}

#[derive(Deserialize, Debug)]
struct CreateNzbBody {
    servers: Vec<NzbServer>,
    #[serde(rename = "nzbUrl")]
    nzb_url: String,
}

#[derive(Serialize)]
struct CreateResponse {
    key: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/create", get(nzb_unsupported).post(create_nzb_session))
        .route(
            "/create/{key}",
            get(nzb_unsupported).post(create_nzb_session_with_key),
        )
        .route("/stream", get(stream_nzb_query))
        .route("/stream/{key}/{*file}", get(stream_nzb_file))
}

async fn nzb_unsupported() -> Response {
    crate::routes::compat::unsupported("NZB lz create compatibility route")
}

async fn create_nzb_session(
    State(state): State<AppState>,
    Json(payload): Json<CreateNzbBody>,
) -> Result<Json<CreateResponse>, (StatusCode, String)> {
    let key = Uuid::new_v4().to_string();
    create_session_internal(state, key, payload).await
}

async fn create_nzb_session_with_key(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(payload): Json<CreateNzbBody>,
) -> Result<Json<CreateResponse>, (StatusCode, String)> {
    create_session_internal(state, key, payload).await
}

async fn create_session_internal(
    state: AppState,
    key: String,
    payload: CreateNzbBody,
) -> Result<Json<CreateResponse>, (StatusCode, String)> {
    // 1. Fetch NZB content
    let nzb_content = reqwest::get(&payload.nzb_url)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to fetch NZB URL: {}", e),
            )
        })?
        .text()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read NZB content: {}", e),
            )
        })?;

    // 2. Map config
    let config = NzbConfig {
        nzb_url: payload.nzb_url.clone(),
        servers: payload
            .servers
            .into_iter()
            .map(|s| NzbServerConfig {
                host: s.host,
                port: s.port,
                user: s.user,
                pass: s.pass,
                ssl: s.ssl,
                connections: s.connections,
            })
            .collect(),
    };

    // 3. Create Session
    let session = NzbSession::new(key.clone(), config, nzb_content)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse NZB: {}", e),
            )
        })?;

    state.nzb_sessions.insert(key.clone(), session);

    tracing::info!("Created NZB session {}", key);

    Ok(Json(CreateResponse { key }))
}

async fn stream_nzb_file(
    State(state): State<AppState>,
    Path((key, file)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    if let Some(session) = state.nzb_sessions.get(&key) {
        match session.stream_file(&file) {
            Ok(stream) => {
                let reader_stream = tokio_util::io::ReaderStream::new(stream);
                return Ok(Response::builder()
                    .header(
                        "transferMode.dlna.org",
                        crate::routes::compat::DLNA_TRANSFER_MODE,
                    )
                    .header(
                        "contentFeatures.dlna.org",
                        crate::routes::compat::DLNA_CONTENT_FEATURES,
                    )
                    .body(axum::body::Body::from_stream(reader_stream))
                    .unwrap());
            }
            Err(e) => {
                tracing::error!(
                    "Failed to find/open file {} in session {}: {}",
                    file,
                    key,
                    e
                );
                return Err(StatusCode::NOT_FOUND);
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct NzbStreamQuery {
    key: String,
}

async fn stream_nzb_query(
    State(state): State<AppState>,
    Query(query): Query<NzbStreamQuery>,
) -> Result<Response, StatusCode> {
    let session = state
        .nzb_sessions
        .get(&query.key)
        .ok_or(StatusCode::NOT_FOUND)?;
    let Some(first_file) = session.nzb.files.first() else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(Redirect::temporary(&format!(
        "./stream/{}/{}",
        urlencoding::encode(&query.key),
        urlencoding::encode(&first_file.subject)
    ))
    .into_response())
}
