use crate::state::AppState;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(serde::Deserialize)]
pub struct StatsParams {
    pub sys: Option<String>, // "1"
}

pub async fn get_stats(
    State(state): State<AppState>,
    Query(params): Query<StatsParams>,
) -> impl IntoResponse {
    let engines = state.engine.get_all_statistics().await;

    // Convert engines HashMap to Value
    let mut root: serde_json::Map<String, Value> = serde_json::Map::new();

    for (hash, stats) in engines {
        root.insert(hash, serde_json::to_value(stats).unwrap_or(Value::Null));
    }

    if params.sys.as_deref() == Some("1") {
        // Basic system info mock (to avoid new crate dep for now, or use std if easy)
        // server.js uses os.loadavg(), os.cpus()
        root.insert(
            "sys".to_string(),
            json!({
                "loadavg": [0.0, 0.0, 0.0], // Placeholder
                "cpus": [] // Placeholder
            }),
        );
    }

    Json(Value::Object(root))
}

pub async fn heartbeat() -> impl IntoResponse {
    Json(json!({ "success": true }))
}

pub async fn network_info() -> impl IntoResponse {
    let mut interfaces = Vec::new();
    if let Ok(if_addrs) = if_addrs::get_if_addrs() {
        for iface in if_addrs {
            if !iface.is_loopback() {
                if let if_addrs::IfAddr::V4(addr) = iface.addr {
                    interfaces.push(addr.ip.to_string());
                }
            }
        }
    }
    Json(json!({ "availableInterfaces": interfaces }))
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct ServerSettings {
    #[serde(rename = "appPath")]
    pub app_path: String,
    #[serde(rename = "serverVersion")]
    pub server_version: String,
    #[serde(rename = "cacheRoot")]
    pub cache_root: String,
    #[serde(rename = "cacheSize")]
    pub cache_size: f64,
    #[serde(rename = "proxyStreamsEnabled")]
    pub proxy_streams_enabled: bool,
    #[serde(rename = "btMaxConnections")]
    pub bt_max_connections: u64,
    #[serde(rename = "btHandshakeTimeout")]
    pub bt_handshake_timeout: u64,
    #[serde(rename = "btRequestTimeout")]
    pub bt_request_timeout: u64,
    #[serde(rename = "btDownloadSpeedSoftLimit")]
    pub bt_download_speed_soft_limit: f64,
    #[serde(rename = "btDownloadSpeedHardLimit")]
    pub bt_download_speed_hard_limit: f64,
    #[serde(rename = "btMinPeersForStable")]
    pub bt_min_peers_for_stable: u64,
    #[serde(rename = "remoteHttps")]
    pub remote_https: Option<String>,
    #[serde(rename = "transcodeProfile")]
    pub transcode_profile: Option<String>,

    /// Cached list of fastest trackers (ranked by RTT)
    #[serde(rename = "cachedTrackers", default)]
    pub cached_trackers: Vec<String>,

    /// Unix timestamp (seconds) when trackers were last updated
    #[serde(rename = "trackersLastUpdated", default)]
    pub trackers_last_updated: i64,

    /// URL to fetch public tracker list (configurable)
    #[serde(rename = "trackersSourceUrl", default = "default_trackers_url")]
    pub trackers_source_url: String,
}

pub fn default_trackers_url() -> String {
    "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt".to_string()
}

impl Default for ServerSettings {
    fn default() -> Self {
        let cache_root = std::env::var("STREMIO_CACHE_ROOT")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.cache/stremio-server", h)))
            .unwrap_or_else(|_| {
                std::env::temp_dir()
                    .join("stremio-cache")
                    .to_string_lossy()
                    .to_string()
            });

        Self {
            app_path: std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/usr/bin/stremio-server".to_string()),
            server_version: "4.20.15".to_string(),
            cache_root,
            cache_size: 10.0 * 1024.0 * 1024.0 * 1024.0, // 10GB
            proxy_streams_enabled: false,
            bt_max_connections: 65535,
            bt_handshake_timeout: 20000,
            bt_request_timeout: 10000,
            bt_download_speed_soft_limit: 0.0,
            bt_download_speed_hard_limit: 0.0,
            bt_min_peers_for_stable: 5,
            remote_https: None,
            transcode_profile: None,
            cached_trackers: Vec::new(),
            trackers_last_updated: 0,
            trackers_source_url: default_trackers_url(),
        }
    }
}

/// Returns server settings in the SettingsResponse format expected by stremio-core
/// Response format: { "baseUrl": "http://...", "values": { ...settings } }
pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let settings = state.settings.read().await;
    Json(json!({
        "baseUrl": "http://127.0.0.1:11470",
        "values": settings.clone()
    }))
}

pub async fn set_settings(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    tracing::debug!("set_settings: received payload: {:?}", payload);

    // Merge with existing settings
    let mut settings = state.settings.write().await;

    if let Some(obj) = payload.as_object() {
        // Update fields that are present in the payload
        if let Some(v) = obj.get("transcodeProfile") {
            if v.is_null() {
                settings.transcode_profile = None;
            } else if let Some(s) = v.as_str() {
                settings.transcode_profile = Some(s.to_string());
            }
        }
        if let Some(v) = obj.get("cacheSize") {
            if let Some(n) = v.as_f64() {
                settings.cache_size = n;
            }
        }
        if let Some(v) = obj.get("proxyStreamsEnabled") {
            if let Some(b) = v.as_bool() {
                settings.proxy_streams_enabled = b;
            }
        }
        if let Some(v) = obj.get("btMaxConnections") {
            if let Some(n) = v.as_u64() {
                settings.bt_max_connections = n;
            }
        }
        if let Some(v) = obj.get("btHandshakeTimeout") {
            if let Some(n) = v.as_u64() {
                settings.bt_handshake_timeout = n;
            }
        }
        if let Some(v) = obj.get("btRequestTimeout") {
            if let Some(n) = v.as_u64() {
                settings.bt_request_timeout = n;
            }
        }
        if let Some(v) = obj.get("btDownloadSpeedSoftLimit") {
            if let Some(n) = v.as_f64() {
                settings.bt_download_speed_soft_limit = n;
            }
        }
        if let Some(v) = obj.get("btDownloadSpeedHardLimit") {
            if let Some(n) = v.as_f64() {
                settings.bt_download_speed_hard_limit = n;
            }
        }
        if let Some(v) = obj.get("btMinPeersForStable") {
            if let Some(n) = v.as_u64() {
                settings.bt_min_peers_for_stable = n;
            }
        }
        if let Some(v) = obj.get("remoteHttps") {
            if v.is_null() {
                settings.remote_https = None;
            } else if let Some(s) = v.as_str() {
                settings.remote_https = Some(s.to_string());
            }
        }
    }

    // Build new speed profile from updated settings
    let new_profile = enginefs::backend::TorrentSpeedProfile {
        bt_download_speed_hard_limit: settings.bt_download_speed_hard_limit,
        bt_download_speed_soft_limit: settings.bt_download_speed_soft_limit,
        bt_handshake_timeout: settings.bt_handshake_timeout,
        bt_max_connections: settings.bt_max_connections,
        bt_min_peers_for_stable: settings.bt_min_peers_for_stable,
        bt_request_timeout: settings.bt_request_timeout,
    };

    // Release the write lock before saving
    drop(settings);

    // Apply new speed profile to libtorrent session dynamically
    state.engine.update_speed_profile(&new_profile).await;

    // Save to disk
    if let Err(e) = state.save_settings().await {
        tracing::error!("Failed to save settings: {}", e);
        return Json(json!({ "success": false, "error": e.to_string() }));
    }

    Json(json!({ "success": true }))
}
pub async fn get_device_info() -> impl IntoResponse {
    let profiles = probe_hwaccel().await;
    Json(json!({
        "availableHardwareAccelerations": profiles
    }))
}

pub async fn hwaccel_profiler() -> impl IntoResponse {
    let profiles = probe_hwaccel().await;
    Json(json!({
        "success": true,
        "profiles": profiles
    }))
}

async fn probe_hwaccel() -> Vec<String> {
    let mut profiles = Vec::new();
    let output = match tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .await
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return profiles,
    };

    if output.contains("h264_nvenc") {
        profiles.push("nvenc".to_string());
    }
    if output.contains("h264_vaapi") {
        profiles.push("vaapi".to_string());
    }
    if output.contains("h264_vdpau") {
        profiles.push("vdpau".to_string());
    }
    if output.contains("h264_qsv") {
        profiles.push("qsv".to_string());
    }
    if output.contains("h264_omx") {
        profiles.push("omx".to_string());
    }
    if output.contains("h264_v4l2m2m") {
        profiles.push("v4l2m2m".to_string());
    }
    if output.contains("h264_videotoolbox") {
        profiles.push("videotoolbox".to_string());
    }
    if output.contains("h264_mediacodec") {
        profiles.push("mediacodec".to_string());
    }

    profiles
}

pub async fn get_https(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let ip_address = match params.get("ipAddress") {
        Some(ip) => ip,
        None => return (StatusCode::BAD_REQUEST, "Missing ipAddress").into_response(),
    };
    let auth_key = match params.get("authKey") {
        Some(key) => key,
        None => return (StatusCode::BAD_REQUEST, "Missing authKey").into_response(),
    };

    let client = reqwest::Client::new();
    let api_url = "https://api.strem.io/api/certificateGet";

    let payload = json!({
        "authKey": auth_key,
        "ipAddress": ip_address
    });

    let resp = match client.post(api_url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("API error: {}", e),
            )
                .into_response();
        }
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("JSON error: {}", e),
            )
                .into_response();
        }
    };

    // Parity with http_client_804.js: parse certificate response
    let result = &json["result"];
    if result.is_null() {
        return (StatusCode::NOT_FOUND, "No certificate found in response").into_response();
    }

    let cert_data_str = match result["certificate"].as_str() {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "Certificate field missing").into_response(),
    };

    let cert_data: serde_json::Value = match serde_json::from_str(cert_data_str) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to parse inner certificate JSON",
            )
                .into_response();
        }
    };

    // Save to disk for main.rs HTTPS listener
    if let (Some(cert), Some(key)) = (
        cert_data["certificate"].as_str(),
        cert_data["privateKey"].as_str(),
    ) {
        let cert_path = state.config_dir.join("https-cert.pem");
        let key_path = state.config_dir.join("https-key.pem");

        if let Err(e) = tokio::fs::write(&cert_path, cert).await {
            tracing::error!("Failed to write https-cert.pem: {}", e);
        }
        if let Err(e) = tokio::fs::write(&key_path, key).await {
            tracing::error!("Failed to write https-key.pem: {}", e);
        }
        tracing::info!("Saved HTTPS certificates to {:?}", state.config_dir);
    }

    let domain = format!(
        "{}-{}",
        ip_address.replace(".", "-"),
        cert_data["commonName"]
            .as_str()
            .unwrap_or("")
            .replace("*", "")
    );

    // We should save this to disk, but for the API response:
    Json(json!({
        "ipAddress": ip_address,
        "domain": domain,
        "port": 11470 // Default port
    }))
    .into_response()
}

pub async fn get_samples(
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    // Parity with /samples/:filename
    (
        StatusCode::NOT_FOUND,
        format!("Sample {} not found", filename),
    )
        .into_response()
}

pub async fn get_engine_stats(
    State(state): State<AppState>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
) -> Response {
    let info_hash = info_hash.to_lowercase();

    // Try to get existing engine, or auto-create from info hash
    let engine = if let Some(e) = state.engine.get_engine(&info_hash).await {
        e
    } else {
        tracing::info!("Auto-creating engine for stats request: {}", info_hash);
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        let source = enginefs::backend::TorrentSource::Url(magnet);
        match state.engine.add_torrent(source, None).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("Failed to create engine: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create engine: {}", e),
                )
                    .into_response();
            }
        }
    };

    let stats = engine.get_statistics().await;
    Json(serde_json::to_value(stats).unwrap()).into_response()
}

pub async fn get_file_stats(
    State(state): State<AppState>,
    axum::extract::Path((info_hash, idx)): axum::extract::Path<(String, usize)>,
) -> Response {
    let info_hash = info_hash.to_lowercase();

    // Try to get existing engine, or auto-create from info hash
    let engine = if let Some(e) = state.engine.get_engine(&info_hash).await {
        e
    } else {
        tracing::info!("Auto-creating engine for file stats request: {}", info_hash);
        let magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
        let source = enginefs::backend::TorrentSource::Url(magnet);
        match state.engine.add_torrent(source, None).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("Failed to create engine: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create engine: {}", e),
                )
                    .into_response();
            }
        }
    };

    let stats = engine.get_statistics().await;
    if idx >= stats.files.len() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            "File index out of bounds",
        )
            .into_response();
    }
    Json(serde_json::to_value(stats).unwrap()).into_response()
}
