use axum::{
    http::StatusCode,
    response::Redirect,
    routing::{get, post},
    Router,
};
use enginefs::EngineFS; // This is a type alias in enginefs::lib.rs based on features
use state::AppState;
use std::{net::SocketAddr, sync::Arc};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cache_cleaner;
mod ffmpeg_setup;
mod routes;
mod state;
mod tui;
mod local_addon;
mod archives;
mod ssdp;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Check for --tui flag
    let use_tui = std::env::args().any(|arg| arg == "--tui");

    let (tui_log_layer, tui_rx) = if use_tui {
        let (tx, rx) = crossbeam_channel::bounded(1000); // 1000 log buffer
        (Some(tui::log_layer::TuiLogLayer::new(tx)), Some(rx))
    } else {
        (None, None)
    };

    let registry = tracing_subscriber::registry().with(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "server=info,tower_http=info,enginefs=info".into()),
    );

    if let Some(layer) = tui_log_layer {
        registry.with(layer).init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }

    // Check/Install FFmpeg on Windows
    // Check/Install FFmpeg on Windows
    if let Err(e) = ffmpeg_setup::setup_ffmpeg().await {
        tracing::warn!("FFmpeg setup failed: {}", e);
    }

    // Determine paths
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("stremio-server");
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find cache directory"))?
        .join("stremio-server");

    // Ensure directories exist
    tokio::fs::create_dir_all(&config_dir).await?;
    tokio::fs::create_dir_all(&cache_dir).await?;

    tracing::info!("Config Dir: {:?}", config_dir);
    tracing::info!("Cache/Download Dir: {:?}", cache_dir);

    // Load settings from disk or use defaults
    // We ideally want settings in config_dir
    let mut default_settings = routes::system::ServerSettings::default();
    // Override default cache root to our determined path, so the settings struct reflects reality
    default_settings.cache_root = cache_dir.to_string_lossy().to_string();

    let settings = AppState::load_settings(&config_dir, &default_settings);

    // Create BackendConfig from settings
    let backend_config = enginefs::backend::BackendConfig {
        cache: enginefs::backend::priorities::EngineCacheConfig {
            size: settings.cache_size as u64,
            enabled: true,
        },
        growler: enginefs::backend::Growler::default(),
        peer_search: enginefs::backend::PeerSearch {
            min: settings.bt_min_peers_for_stable,
            ..Default::default()
        },
        swarm_cap: enginefs::backend::SwarmCap::default(),
        speed_profile: enginefs::backend::TorrentSpeedProfile {
            bt_download_speed_hard_limit: settings.bt_download_speed_hard_limit,
            bt_download_speed_soft_limit: settings.bt_download_speed_soft_limit,
            bt_handshake_timeout: settings.bt_handshake_timeout,
            bt_max_connections: settings.bt_max_connections,
            bt_min_peers_for_stable: settings.bt_min_peers_for_stable,
            bt_request_timeout: settings.bt_request_timeout,
        },
    };

    // EngineFS::new() now handles its own librqbit session and background cleanup.
    // We pass the cache_dir as the root for downloads
    // We also pass the backend config for prioritization/monitoring logic
    let engine_fs = EngineFS::new(cache_dir.clone(), backend_config).await?;
    let engine = Arc::new(engine_fs);

    // Create state once
    let state = AppState::new(engine, settings, config_dir.clone());

    // Start Cache Cleaner
    // Start Cache Cleaner
    cache_cleaner::start(Arc::new(state.clone()));

    // Start SSDP Discovery
    tokio::spawn(crate::ssdp::start_discovery(state.devices.clone()));

    // Local Addon Init
    // Ensure localFiles directory exists
    let local_files_dir = config_dir.join("localFiles");
    tokio::fs::create_dir_all(&local_files_dir).await?;
    
    // Start background scan
    local_addon::scan_background(local_files_dir.to_string_lossy().to_string(), state.local_index.clone()).await;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

    // Start TUI if enabled
    if use_tui {
        if let Some(rx) = tui_rx {
            tui::start_tui(Arc::new(state.clone()), rx, shutdown_tx);
        }
    }

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
        .route("/{infoHash}/create", post(routes::engine::create_magnet))
        .route("/{infoHash}/remove", get(routes::engine::remove_engine))
        .route(
            "/{infoHash}/stats.json",
            get(routes::system::get_engine_stats),
        )
        .route(
            "/{infoHash}/{idx}/stats.json",
            get(routes::system::get_file_stats),
        )
        .route("/{infoHash}/peers", get(routes::peers::get_peers))
        // Stream routes - both patterns for compatibility
        .route(
            "/stream/{infoHash}/{fileIdx}",
            get(routes::stream::stream_video).head(routes::stream::stream_video),
        )
        .route(
            "/{infoHash}/{fileIdx}",
            get(routes::stream::stream_video).head(routes::stream::stream_video),
        )
        .route(
            "/subtitles.vtt",
            get(routes::subtitles::proxy_subtitles_vtt),
        )
        .route(
            "/{infoHash}/{fileIdx}/subtitles.vtt",
            get(routes::subtitles::get_subtitles_vtt),
        )
        .route("/opensubHash", get(routes::subtitles::opensub_hash))
        .route(
            "/opensubHash/{infoHash}/{fileIdx}",
            get(routes::subtitles::opensub_hash_path),
        )
        .route(
            "/subtitlesTracks",
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
        .nest("/nzb", routes::nzb::router())
        .nest("/local-addon", local_addon::get_router())
        .nest("/proxy", routes::proxy::router())

        .nest("/ftp", routes::ftp::router())
        .route("/samples/{filename}", get(routes::system::get_samples))
        // HLS routes with query params for Stremio compatibility
        .route(
            "/hlsv2/{hash}/master.m3u8",
            get(routes::hls::master_playlist_by_url),
        )
        // HLS routes with path params (original)
        .route(
            "/hlsv2/{infoHash}/{fileIdx}/master.m3u8",
            get(routes::hls::get_master_playlist),
        )
        // Generic route for HLS resources (segments, audio, subtitles)
        // {fileIdx} accepts both numeric (0, 1, 2) and string identifiers (subtitle4, audio-1)
        .route(
            "/hlsv2/{infoHash}/{fileIdx}/{resource}",
            get(routes::hls::handle_hls_resource),
        )
        // HLS probe with query params for Stremio compatibility
        .route("/hlsv2/probe", get(routes::hls::probe_by_url))
        // Legacy generic probe
        .route("/probe", get(routes::hls::probe_by_url))
        .route("/probe/{infoHash}/{fileIdx}", get(routes::hls::get_probe))
        .route("/tracks/{infoHash}/{fileIdx}", get(routes::hls::get_tracks))
        .nest("/casting", routes::casting::router())
        .route("/favicon.ico", get(|| async { StatusCode::NOT_FOUND }))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 11470));
    tracing::info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Shutdown signal
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Ctrl+C received, shutting down");
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("Shutdown signal received from TUI, shutting down");
            }
        }
        
        // FORCE EXIT TIMER
        // Libtorrent often hangs on shutdown trying to contact trackers.
        // We give the graceful shutdown 1 second to finish, otherwise we pull the plug.
        // This stops the "hanging terminal" experience.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            tracing::warn!("Shutdown taking too long, forcing exit...");
            std::process::exit(0);
        });
    };

    // HTTPS Server (Port 12470)
    let https_cert_path = config_dir.join("https-cert.pem");
    let https_key_path = config_dir.join("https-key.pem");

    if https_cert_path.exists() && https_key_path.exists() {
        tracing::info!("Found HTTPS certificates, starting HTTPS server on port 12470");
        let https_app = app.clone();
        let https_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            https_cert_path,
            https_key_path,
        )
        .await?;

        let https_addr = SocketAddr::from(([0, 0, 0, 0], 12470));
        tokio::spawn(async move {
            if let Err(e) = axum_server::bind_rustls(https_addr, https_config)
                .serve(https_app.into_make_service())
                .await
            {
                tracing::error!("HTTPS server error: {}", e);
            }
        });
    } else {
        tracing::info!("No HTTPS certificates found in {:?}, skipping HTTPS server (Port 12470)", config_dir);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

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
