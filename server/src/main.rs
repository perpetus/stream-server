use axum::{
    http::StatusCode,
    response::Redirect,
    routing::{get, post},
    Router,
};
use enginefs::EngineFS;
use state::AppState;
use std::{net::SocketAddr, sync::Arc};
use tokio;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use urlencoding;

mod routes;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // EngineFS::new() now handles its own librqbit session and background cleanup.
    let engine_fs = EngineFS::new().await?;
    let engine = Arc::new(engine_fs);
    let state = AppState { engine };

    let app = Router::new()
        .route("/", get(root_redirect))
        .route("/heartbeat", get(routes::system::heartbeat))
        .route("/stats.json", get(routes::system::get_stats))
        .route("/network-info", get(routes::system::network_info))
        .route(
            "/settings",
            get(routes::system::get_settings).post(routes::system::set_settings),
        )
        .route("/list", get(routes::engine::list_engines))
        .route("/removeAll", get(routes::engine::remove_all_engines))
        // Torrent creation - support both patterns
        .route(
            "/create",
            get(routes::engine::create_engine).post(routes::engine::create_engine),
        )
        .route("/:infoHash/create", post(routes::engine::create_magnet))
        .route("/:infoHash/remove", get(routes::engine::remove_engine))
        .route(
            "/:infoHash/stats.json",
            get(routes::system::get_engine_stats),
        )
        .route(
            "/:infoHash/:idx/stats.json",
            get(routes::system::get_file_stats),
        )
        // Stream routes - both patterns for compatibility
        .route(
            "/stream/:infoHash/:fileIdx",
            get(routes::stream::stream_video),
        )
        .route("/:infoHash/:fileIdx", get(routes::stream::stream_video))
        .route("/subtitles.vtt", get(routes::subtitles::get_subtitles_vtt))
        .route(
            "/opensubHash/:infoHash/:fileIdx",
            get(routes::subtitles::opensub_hash),
        )
        .route(
            "/subtitlesTracks/:infoHash",
            get(routes::subtitles::subtitles_tracks),
        )
        .route("/device-info", get(routes::system::get_device_info))
        .route("/hwaccel-profiler", get(routes::system::hwaccel_profiler))
        .route("/get-https", get(routes::system::get_https))
        .nest("/yt", routes::youtube::router())
        .nest("/rar", routes::archive::router())
        .nest("/zip", routes::archive::router())
        .nest("/7zip", routes::archive::router())
        .nest("/tar", routes::archive::router())
        .nest("/tgz", routes::archive::router())
        .nest("/local-addon", routes::local_addon::router())
        .nest("/proxy", routes::proxy::router())
        .route("/samples/:filename", get(routes::system::get_samples))
        // HLS routes with query params for Stremio compatibility
        .route(
            "/hlsv2/:hash/master.m3u8",
            get(routes::hls::master_playlist_by_url),
        )
        // HLS routes with path params (original)
        .route(
            "/hlsv2/:infoHash/:fileIdx/master.m3u8",
            get(routes::hls::get_master_playlist),
        )
        .route(
            "/hlsv2/:infoHash/:fileIdx/:resource",
            get(routes::hls::handle_hls_resource),
        )
        // HLS probe with query params for Stremio compatibility
        .route("/hlsv2/probe", get(routes::hls::probe_by_url))
        .route("/probe/:infoHash/:fileIdx", get(routes::hls::get_probe))
        .route("/tracks/:infoHash/:fileIdx", get(routes::hls::get_tracks))
        .nest("/casting", routes::casting::router())
        .route("/favicon.ico", get(|| async { StatusCode::NOT_FOUND }))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 11470));
    tracing::info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn root_redirect() -> Redirect {
    let base_url = "http://127.0.0.1:11470";
    let encoded_url = urlencoding::encode(base_url);
    Redirect::temporary(&format!(
        "https://app.strem.io/#/?streamingServer={}",
        encoded_url
    ))
}
