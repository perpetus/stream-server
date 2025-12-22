use axum::{
    extract::{Path, State},
    response::{Response, IntoResponse},
    routing::{get, post},
    Router,
    http::StatusCode,
    Json,
};
use crate::state::AppState;
use crate::archives::nzb::session::{NzbSession, NzbConfig, NzbServerConfig};
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
        .route("/create", post(create_nzb_session))
        .route("/create/{key}", post(create_nzb_session_with_key))
        .route("/stream/{key}/{file}", get(stream_nzb_file))
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
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to fetch NZB URL: {}", e)))?
        .text()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read NZB content: {}", e)))?;

    // 2. Map config
    let config = NzbConfig {
        nzb_url: payload.nzb_url.clone(),
        servers: payload.servers.into_iter().map(|s| NzbServerConfig {
            host: s.host,
            port: s.port,
            user: s.user,
            pass: s.pass,
            ssl: s.ssl,
            connections: s.connections,
        }).collect(),
    };

    // 3. Create Session
    let session = NzbSession::new(key.clone(), config, nzb_content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse NZB: {}", e)))?;

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
                return Ok(axum::body::Body::from_stream(reader_stream).into_response());
            }
            Err(e) => {
                tracing::error!("Failed to find/open file {} in session {}: {}", file, key, e);
                return Err(StatusCode::NOT_FOUND);
            }
        }
    }
    
    Err(StatusCode::NOT_FOUND)
}
