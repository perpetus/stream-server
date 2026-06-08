#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use axum::{Router, http::StatusCode, response::Redirect, routing::get};
use enginefs::EngineFS; // This is a type alias in enginefs::lib.rs based on features
use fslock::LockFile;
use state::AppState;
use std::{future::IntoFuture, net::SocketAddr, sync::Arc};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod archives;
mod cache_cleaner;
mod diagnostics;
mod ffmpeg_setup;
mod local_addon;
mod routes;
mod ssdp;
mod state;
mod tray;
mod tui;
mod updater;

#[derive(Clone, Copy, Debug)]
enum ShutdownBehavior {
    ExitProcessOnTimeout,
    ReturnOnTimeout,
}

impl ShutdownBehavior {
    fn exits_process_on_timeout(self) -> bool {
        matches!(self, Self::ExitProcessOnTimeout)
    }
}

#[derive(Clone, Copy, Debug)]
enum ShutdownSource {
    CtrlC,
    Tui,
    External,
}

async fn run_server(
    mut external_shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    tray_stats: Option<std::sync::Arc<tray::TrayStats>>,
    shutdown_behavior: ShutdownBehavior,
) -> anyhow::Result<Option<ShutdownSource>> {
    // Check for --tui flag
    let use_tui = std::env::args().any(|arg| arg == "--tui");

    let (tui_log_layer, tui_rx) = if use_tui {
        let (tx, rx) = crossbeam_channel::bounded(1000); // 1000 log buffer
        (Some(tui::log_layer::TuiLogLayer::new(tx)), Some(rx))
    } else {
        (None, None)
    };

    // Determine paths before logging so file diagnostics always land in the config directory.
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("stremio-server");
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find cache directory"))?
        .join("stremio-server");
    let log_dir = config_dir.join("logs");

    // Ensure directories exist
    tokio::fs::create_dir_all(&config_dir).await?;
    tokio::fs::create_dir_all(&cache_dir).await?;
    tokio::fs::create_dir_all(&log_dir).await?;

    diagnostics::logging::init_process_start();
    diagnostics::logging::install_panic_hook();

    let log_writers = diagnostics::logging::open_log_writers(&log_dir)?;
    let human_log_path = log_writers.human_path.clone();
    let archive_log_path = log_writers.archive_path.clone();
    let json_log_path = log_writers.json_path.clone();
    let human_writer = log_writers.human_writer;
    let archive_writer = log_writers.archive_writer;
    let json_writer = log_writers.json_writer;
    let guards = log_writers.guards;

    let registry = tracing_subscriber::registry().with(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "server=info,tower_http=info,enginefs=info".into()),
    );
    let human_file_layer = tracing_subscriber::fmt::layer()
        .with_writer(human_writer)
        .with_ansi(false);
    let archive_file_layer = tracing_subscriber::fmt::layer()
        .with_writer(archive_writer)
        .with_ansi(false);
    let json_file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(json_writer)
        .with_ansi(false);

    let init_result = if let Some(layer) = tui_log_layer {
        registry
            .with(human_file_layer)
            .with(archive_file_layer)
            .with(json_file_layer)
            .with(layer)
            .try_init()
    } else if std::io::stdout().is_terminal() {
        // Console logging when running in terminal
        registry
            .with(human_file_layer)
            .with(archive_file_layer)
            .with(json_file_layer)
            .with(tracing_subscriber::fmt::layer())
            .try_init()
    } else {
        registry
            .with(human_file_layer)
            .with(archive_file_layer)
            .with(json_file_layer)
            .try_init()
    };

    if init_result.is_ok() {
        diagnostics::logging::store_log_guards(guards);
    }

    // Check/Install FFmpeg on Windows
    // Check/Install FFmpeg on Windows
    if let Err(e) = ffmpeg_setup::setup_ffmpeg().await {
        tracing::warn!("FFmpeg setup failed: {}", e);
    }

    tracing::info!("Config Dir: {:?}", config_dir);
    tracing::info!("Cache/Download Dir: {:?}", cache_dir);
    tracing::info!("Log Dir: {:?}", log_dir);
    diagnostics::logging::install_native_crash_handler(&log_dir);
    diagnostics::logging::log_startup_context(
        &config_dir,
        &cache_dir,
        &log_dir,
        &human_log_path,
        &archive_log_path,
        &json_log_path,
    );

    // Load settings from disk or use defaults
    // We ideally want settings in config_dir
    let mut default_settings = routes::system::ServerSettings::default();
    // Override default cache root to our determined path, so the settings struct reflects reality
    default_settings.cache_root = cache_dir.to_string_lossy().to_string();

    let settings = AppState::load_settings(&config_dir, &default_settings);

    // Wrap settings in Arc<RwLock> early so we can share with TrackerStorageBridge
    let settings_arc = Arc::new(tokio::sync::RwLock::new(settings.clone()));
    let settings_path = config_dir.join("settings.json");

    // Create tracker storage bridge for persisting trackers to settings
    let tracker_storage = Arc::new(state::TrackerStorageBridge::new(
        settings_arc.clone(),
        settings_path.clone(),
    ));

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

    // EngineFS::new_with_storage() passes tracker storage for persistence.
    // We also pass the backend config for prioritization/monitoring logic
    let engine_fs = EngineFS::new_with_storage(
        cache_dir.clone(),
        backend_config.clone(),
        Some(tracker_storage.clone()),
    )
    .await?;
    let engine = Arc::new(engine_fs);
    let (download_engine, download_engine_disk_backed) = match EngineFS::new_disk_backed(
        cache_dir.clone(),
        backend_config,
        Some(tracker_storage),
    )
    .await
    {
        Ok(download_engine_fs) => (Arc::new(download_engine_fs), true),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Disk-backed download engine unavailable at startup; download=1 will use memory-only mode"
            );
            (engine.clone(), false)
        }
    };

    // Create state with the shared settings Arc
    let state = AppState::new_with_shared_settings_log_dir_and_download_engine(
        engine,
        download_engine,
        download_engine_disk_backed,
        settings_arc.clone(),
        config_dir.clone(),
        log_dir.clone(),
    );

    // Apply persisted seeding policy to engines
    {
        let settings = settings_arc.read().await;
        state.engine.set_seeding_enabled(settings.seeding_enabled);
        state
            .download_engine
            .set_seeding_enabled(settings.seeding_enabled);
    }

    // Start Cache Cleaner
    cache_cleaner::start(Arc::new(state.clone()));
    diagnostics::start_memory_sampler(state.clone());
    state
        .updater
        .clone()
        .spawn_background_checker(state.clone());

    // Start SSDP Discovery
    diagnostics::logging::spawn_logged(
        "ssdp-discovery",
        crate::ssdp::start_discovery(state.devices.clone()),
    );

    // Start tray stats collection if tray is active
    if let Some(stats) = tray_stats {
        let engine_clone = state.engine.clone();
        let download_engine_clone = state.download_engine.clone();
        let updater_clone = state.updater.clone();
        let settings_clone = state.settings.clone();
        diagnostics::logging::spawn_logged("tray-stats", async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let mut all_stats = engine_clone.get_all_statistics().await;
                let download_stats = download_engine_clone.get_all_statistics().await;
                for (hash, stats) in download_stats {
                    all_stats.insert(hash, stats);
                }

                let mut total_down = 0.0;
                let mut total_up = 0.0;
                let mut total_peers = 0u64;
                let active = all_stats.len();

                for (_, s) in all_stats {
                    total_down += s.download_speed;
                    total_up += s.upload_speed;
                    total_peers += s.peers;
                }

                stats.update(total_down, total_up, total_peers, active);

                let settings = settings_clone.read().await.clone();
                stats.set_auto_update_enabled(settings.auto_update_enabled);
                stats.set_seeding_enabled(settings.seeding_enabled);
                let update_status = updater_clone.status(settings.update_channel).await;
                let install_enabled = matches!(
                    update_status.state,
                    crate::updater::state::UpdateOperationState::Available
                        | crate::updater::state::UpdateOperationState::Staged
                );
                let label = if let Some(version) = update_status.staged_version {
                    format!("Update: v{} staged", version)
                } else if let Some(version) = update_status.latest_version {
                    format!("Update: v{} available", version)
                } else {
                    match update_status.state {
                        crate::updater::state::UpdateOperationState::Checking => {
                            "Update: checking".to_string()
                        }
                        crate::updater::state::UpdateOperationState::Downloading => {
                            "Update: downloading".to_string()
                        }
                        crate::updater::state::UpdateOperationState::Installing => {
                            "Update: installing".to_string()
                        }
                        crate::updater::state::UpdateOperationState::Failed => {
                            "Update: failed".to_string()
                        }
                        _ => "Update: up to date".to_string(),
                    }
                };
                stats.update_update_status(label, install_enabled);
            }
        });
    }

    // Local Addon Init
    // Ensure localFiles directory exists
    let local_files_dir = config_dir.join("localFiles");
    tokio::fs::create_dir_all(&local_files_dir).await?;

    // Start background scan
    local_addon::scan_background(
        local_files_dir.to_string_lossy().to_string(),
        state.local_index.clone(),
    )
    .await;

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
        .nest("/update", routes::update::router())
        .route("/diagnostics/memory", get(diagnostics::memory))
        .route("/diagnostics/streams", get(diagnostics::streams))
        .route("/diagnostics/crashes", get(diagnostics::crashes))
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
        .route(
            "/{infoHash}/create",
            get(routes::engine::create_magnet_get).post(routes::engine::create_magnet),
        )
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
            get(routes::stream::stream_video).head(routes::stream::head_stream_video),
        )
        .route(
            "/{infoHash}/{fileIdx}",
            get(routes::stream::stream_video).head(routes::stream::head_stream_video),
        )
        .route(
            "/subtitles.vtt",
            get(routes::subtitles::proxy_subtitles_vtt),
        )
        .route(
            "/subtitles.{ext}",
            get(routes::subtitles::proxy_subtitles_ext),
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
        .route("/subtitlesTracks", get(routes::subtitles::subtitles_tracks))
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
        .route("/hlsv2/status", get(routes::hls::hls_status))
        .route("/hlsv2/{id}/destroy", get(routes::hls::hls_destroy))
        .route("/hlsv2/{id}/burn", get(routes::hls::hls_burn))
        .route(
            "/hlsv2/{hash}/master.m3u8",
            get(routes::hls::master_playlist_by_url),
        )
        .route("/hlsv2/{id}/{resource}", get(routes::hls::hls_v2_resource))
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
        // Route for nested fMP4 segment paths (video0/init.mp4, video0/segment1.m4s)
        .route(
            "/hlsv2/{infoHash}/{fileIdx}/{track}/{segment}",
            get(routes::hls::handle_hls_fmp4_segment),
        )
        // HLS probe with query params for Stremio compatibility
        .route("/hlsv2/probe", get(routes::hls::probe_by_url))
        // Legacy generic probe
        .route("/probe", get(routes::hls::probe_by_url))
        .route("/probe/{infoHash}/{fileIdx}", get(routes::hls::get_probe))
        .route("/tracks/{*url}", get(routes::hls::get_tracks_by_url))
        .route(
            "/{first}/{second}/{resource}",
            get(routes::hls::legacy_hls_resource),
        )
        .route(
            "/{first}/{second}/{variant}/{seg}",
            get(routes::hls::legacy_hls_segment),
        )
        .route("/thumb.jpg", get(|| async { StatusCode::NOT_FOUND }))
        .nest("/casting", routes::casting::router())
        .route("/favicon.ico", get(|| async { StatusCode::NOT_FOUND }))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 11470));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {}", addr);
    // Print to stdout explicitly so stremio-shell-ng can detect startup
    // (tracing may go to file when no terminal is attached)
    println!("listening on {}", addr);
    println!("EngineFS server started at http://127.0.0.1:11470");

    let (shutdown_started_tx, shutdown_started_rx) =
        tokio::sync::oneshot::channel::<ShutdownSource>();

    // Shutdown signal
    let shutdown = async move {
        let source = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Ctrl+C received, shutting down");
                ShutdownSource::CtrlC
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("Shutdown signal received from TUI, shutting down");
                ShutdownSource::Tui
            }
            _ = external_shutdown_rx.recv() => {
                tracing::info!("Shutdown signal received from Tray, shutting down");
                ShutdownSource::External
            }
        };

        let _ = shutdown_started_tx.send(source);
    };

    // HTTPS Server (Port 12470)
    let https_cert_path = config_dir.join("https-cert.pem");
    let https_key_path = config_dir.join("https-key.pem");

    if https_cert_path.exists() && https_key_path.exists() {
        tracing::info!("Found HTTPS certificates, starting HTTPS server on port 12470");
        let https_app = app.clone();
        let https_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(https_cert_path, https_key_path)
                .await?;

        let https_addr = SocketAddr::from(([0, 0, 0, 0], 12470));
        diagnostics::logging::spawn_logged("https-server", async move {
            if let Err(e) = axum_server::bind_rustls(https_addr, https_config)
                .serve(https_app.into_make_service_with_connect_info::<SocketAddr>())
                .await
            {
                tracing::error!("HTTPS server error: {}", e);
            }
        });
    } else {
        tracing::info!(
            "No HTTPS certificates found in {:?}, skipping HTTPS server (Port 12470)",
            config_dir
        );
    }

    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .into_future();

    tokio::pin!(server);

    let shutdown_source = tokio::select! {
        result = &mut server => {
            result?;
            None
        }
        Ok(source) = shutdown_started_rx => {
            let timeout = std::time::Duration::from_secs(3);
            match tokio::time::timeout(timeout, &mut server).await {
                Ok(result) => {
                    result?;
                }
                Err(_) => {
                    if shutdown_behavior.exits_process_on_timeout() {
                        tracing::warn!(
                            ?source,
                            timeout_secs = timeout.as_secs(),
                            "Shutdown taking too long, forcing process exit"
                        );
                        std::process::exit(0);
                    }

                    tracing::warn!(
                        ?source,
                        timeout_secs = timeout.as_secs(),
                        "Shutdown taking too long, dropping server future so restart can continue"
                    );
                }
            }
            Some(source)
        }
    };

    Ok(shutdown_source)
}

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() -> anyhow::Result<()> {
    // Try to attach to parent console (if any), so proper CLI usage still works
    // even though we are a windows subsystem app.
    #[cfg(windows)]
    let attached_console = unsafe {
        use windows::Win32::System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, SetConsoleCtrlHandler,
        };
        let attached = AttachConsole(ATTACH_PARENT_PROCESS).is_ok();
        if attached {
            // GUI-subsystem processes that attach to a parent console have
            // Ctrl+C handling disabled by default.  Re-enable it so
            // tokio::signal::ctrl_c() (which relies on SetConsoleCtrlHandler
            // internally) can actually receive the CTRL_C_EVENT.
            let _ = SetConsoleCtrlHandler(None, false);
        }
        attached
    };
    #[cfg(not(windows))]
    let attached_console = true;

    // Determine lockfile path before we start anything
    // We typically use the temp dir or config dir
    let lock_path = std::env::temp_dir().join("stream-server.lock");
    let mut lockfile = LockFile::open(&lock_path)?;

    if !lockfile.try_lock()? {
        if attached_console {
            eprintln!("Exiting, another instance is running.");
        }
        return Ok(());
    }

    let args: Vec<String> = std::env::args().collect();
    // --no-tray: Disable tray icon (for embedded use in stremio-shell-ng)
    let no_tray = args.iter().any(|a| a == "--no-tray");
    // Silent mode is true if:
    // 1. Explicitly requested via --silent
    // 2. OR if we failed to attach to a console (implies Explorer launch)
    // BUT: if --no-tray is specified, we run in normal mode without tray
    let silent_mode = !no_tray && (args.iter().any(|a| a == "--silent") || !attached_console);

    // Initialize Tokio Runtime
    let rt = tokio::runtime::Runtime::new()?;

    if silent_mode {
        let event_loop = EventLoopBuilder::<tray::UserEvent>::with_user_event().build();
        let tray_handle = match tray::create_system_tray(&event_loop) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Failed to create tray icon: {}", e);
                return Err(e);
            }
        };

        // Shared stats for tray display
        let tray_stats = Arc::new(tray::TrayStats::new());

        // Shared state for controlling the server thread
        // The server thread will put its current shutdown sender here
        let server_control: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<()>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let keep_running = Arc::new(AtomicBool::new(true));

        let server_control_clone = server_control.clone();
        let keep_running_clone = keep_running.clone();
        let tray_stats_clone = tray_stats.clone();

        // Spawn server thread with restart loop
        std::thread::spawn(move || {
            loop {
                if !keep_running_clone.load(Ordering::Relaxed) {
                    break;
                }

                // Channel for tray -> server shutdown for this iteration
                let (tx, rx) = tokio::sync::mpsc::channel(1);

                // Publish the sender
                *server_control_clone.lock().unwrap() = Some(tx);

                // Run server (blocking on runtime)
                // run_server returns Ok(Some(source)) on graceful shutdown,
                // Ok(None) if the server future completed without a signal.
                match rt.block_on(run_server(
                    rx,
                    Some(tray_stats_clone.clone()),
                    ShutdownBehavior::ReturnOnTimeout,
                )) {
                    Ok(Some(ShutdownSource::CtrlC)) => {
                        // Ctrl+C should kill the entire process. We cannot
                        // just break the loop because the main thread is
                        // blocked inside the tao event loop (event_loop.run())
                        // which would keep the process alive.
                        tracing::info!("Ctrl+C in tray mode, forcing process exit");
                        std::process::exit(0);
                    }
                    Ok(_) => {
                        // External (tray restart) or server completed — loop will
                        // check keep_running and either restart or exit.
                    }
                    Err(e) => {
                        eprintln!("Server crashed: {}", e);
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }

                // Clear the sender
                *server_control_clone.lock().unwrap() = None;
            }
        });

        // Spawn stats update timer thread
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if proxy.send_event(tray::UserEvent::UpdateStats).is_err() {
                    break; // Event loop closed
                }
            }
        });

        let tray_icon = tray_handle.tray_icon;
        let stats_item = tray_handle.stats_item;
        let update_item = tray_handle.update_item;
        let auto_update_item = tray_handle.auto_update_item;
        let seeding_item = tray_handle.seeding_item;
        let install_update_item = tray_handle.install_update_item;

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;

            match event {
                Event::UserEvent(e) => match e {
                    tray::UserEvent::OpenWeb => {
                        tray::open_stremio_web();
                    }
                    tray::UserEvent::OpenLogs => {
                        tray::open_log_folder();
                    }
                    tray::UserEvent::Restart => {
                        // Signal shutdown to current instance; loop will restart it since keep_running is true
                        tracing::info!("Restart requested from tray");
                        let tx = server_control.lock().unwrap().take();
                        if let Some(tx) = tx {
                            let _ = tx.blocking_send(());
                        }
                    }
                    tray::UserEvent::ToggleSeeding => {
                        let enabled = seeding_item.is_checked();
                        tray_stats.set_seeding_enabled(enabled);
                        tray::trigger_seeding_toggle(enabled);
                    }
                    tray::UserEvent::ToggleAutoUpdate => {
                        let enabled = auto_update_item.is_checked();
                        tray_stats.set_auto_update_enabled(enabled);
                        tray::trigger_auto_update_toggle(enabled);
                    }
                    tray::UserEvent::CheckUpdates => {
                        tray::trigger_update_check();
                    }
                    tray::UserEvent::InstallUpdate => {
                        tray::trigger_update_install();
                    }
                    tray::UserEvent::Quit => {
                        keep_running.store(false, Ordering::Relaxed);
                        tracing::info!("Quit requested from tray");
                        let tx = server_control.lock().unwrap().take();
                        if let Some(tx) = tx {
                            let _ = tx.blocking_send(());
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                    tray::UserEvent::UpdateStats => {
                        // Update tooltip and menu item with latest stats
                        let tooltip = tray_stats.format_tooltip();
                        let stats_line = tray_stats.format_stats_line();

                        tray_icon.set_tooltip(Some(&tooltip)).ok();
                        stats_item.set_text(&stats_line);
                        update_item.set_text(&tray_stats.format_update_line());
                        auto_update_item.set_checked(tray_stats.auto_update_enabled());
                        seeding_item.set_checked(tray_stats.seeding_enabled());
                        install_update_item.set_enabled(tray_stats.update_install_enabled());
                    }
                },
                _ => (),
            }
        });
    } else {
        // Normal mode: Run on main thread (blocking), no tray
        // Create a dummy channel for external shutdown that we hold open but never send to
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = rt.block_on(run_server(rx, None, ShutdownBehavior::ExitProcessOnTimeout))?;
    }

    Ok(())
}

async fn root_redirect() -> Redirect {
    let base_url = "http://127.0.0.1:11470";
    let encoded_url = urlencoding::encode(base_url);
    Redirect::temporary(&format!(
        "https://web.stremio.com/#/?streamingServer={}",
        encoded_url
    ))
}
