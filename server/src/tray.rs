use anyhow::Context;
use std::sync::{
    RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tao::event_loop::EventLoop;

use tracing::{error, info};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem},
};

pub enum UserEvent {
    OpenWeb,
    OpenLogs,
    Restart,
    ToggleAutoUpdate,
    CheckUpdates,
    InstallUpdate,
    Quit,
    UpdateStats,
}

/// Shared stats for tray display, updated by server thread
#[derive(Default)]
pub struct TrayStats {
    // Store as u64 bits for atomic operations (f64 doesn't have AtomicF64)
    download_speed_bits: AtomicU64,
    upload_speed_bits: AtomicU64,
    peers: AtomicU64,
    active_torrents: AtomicU64,
    update_status: RwLock<String>,
    update_install_enabled: AtomicBool,
    auto_update_enabled: AtomicBool,
}

impl TrayStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(
        &self,
        download_speed: f64,
        upload_speed: f64,
        peers: u64,
        active_torrents: usize,
    ) {
        self.download_speed_bits
            .store(download_speed.to_bits(), Ordering::Relaxed);
        self.upload_speed_bits
            .store(upload_speed.to_bits(), Ordering::Relaxed);
        self.peers.store(peers, Ordering::Relaxed);
        self.active_torrents
            .store(active_torrents as u64, Ordering::Relaxed);
    }

    pub fn download_speed(&self) -> f64 {
        f64::from_bits(self.download_speed_bits.load(Ordering::Relaxed))
    }

    pub fn upload_speed(&self) -> f64 {
        f64::from_bits(self.upload_speed_bits.load(Ordering::Relaxed))
    }

    pub fn peers(&self) -> u64 {
        self.peers.load(Ordering::Relaxed)
    }

    pub fn active_torrents(&self) -> u64 {
        self.active_torrents.load(Ordering::Relaxed)
    }

    pub fn update_update_status(&self, label: String, install_enabled: bool) {
        if let Ok(mut guard) = self.update_status.write() {
            *guard = label;
        }
        self.update_install_enabled
            .store(install_enabled, Ordering::Relaxed);
    }

    pub fn set_auto_update_enabled(&self, enabled: bool) {
        self.auto_update_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn format_update_line(&self) -> String {
        self.update_status
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "Update: unknown".to_string())
    }

    pub fn update_install_enabled(&self) -> bool {
        self.update_install_enabled.load(Ordering::Relaxed)
    }

    pub fn auto_update_enabled(&self) -> bool {
        self.auto_update_enabled.load(Ordering::Relaxed)
    }

    pub fn format_tooltip(&self) -> String {
        let down = format_speed(self.download_speed());
        let up = format_speed(self.upload_speed());
        let peers = self.peers();
        let torrents = self.active_torrents();

        if torrents == 0 {
            "Stream Server - Idle".to_string()
        } else {
            format!("Stream Server\n↓ {} | ↑ {} | {} peers", down, up, peers)
        }
    }

    pub fn format_stats_line(&self) -> String {
        let down = format_speed(self.download_speed());
        let up = format_speed(self.upload_speed());
        let peers = self.peers();
        format!("↓ {} | ↑ {} | {} peers", down, up, peers)
    }
}

/// Format bytes/sec to human-readable speed
pub fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec < 1024.0 {
        format!("{:.0} B/s", bytes_per_sec)
    } else if bytes_per_sec < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1024.0)
    } else if bytes_per_sec < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB/s", bytes_per_sec / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn load_icon() -> anyhow::Result<Icon> {
    // Load 48x48 PNG icon for tray (works better with DPI scaling)
    let icon_bytes = include_bytes!("../../icons/icon_48.png");
    let img = image::load_from_memory(icon_bytes)
        .context("Failed to decode icon")?
        .into_rgba8();
    let (width, height) = img.dimensions();

    Icon::from_rgba(img.into_raw(), width, height).context("Failed to create icon")
}

/// Result of creating system tray, including stats menu item for updates
pub struct TrayHandle {
    pub tray_icon: TrayIcon,
    pub stats_item: MenuItem,
    pub update_item: MenuItem,
    pub auto_update_item: CheckMenuItem,
    pub install_update_item: MenuItem,
    #[allow(dead_code)]
    pub open_id: String,
    #[allow(dead_code)]
    pub quit_id: String,
}

pub fn create_system_tray(event_loop: &EventLoop<UserEvent>) -> anyhow::Result<TrayHandle> {
    // Stats display (disabled - just for showing info)
    let stats_item = MenuItem::new("↓ -- | ↑ -- | 0 peers", false, None);

    let open_item = MenuItem::new("Open Stremio Web", true, None);
    let logs_item = MenuItem::new("Open Logs Folder", true, None);
    let restart_item = MenuItem::new("Restart Server", true, None);
    let update_item = MenuItem::new("Update: unknown", false, None);
    let auto_update_item = CheckMenuItem::new("Auto-check for Updates", true, true, None);
    let check_update_item = MenuItem::new("Check for Updates", true, None);
    let install_update_item = MenuItem::new("Install Update", false, None);
    let quit_item = MenuItem::new("Quit", true, None);

    let version_label = format!("v{}", env!("CARGO_PKG_VERSION"));
    let version_item = MenuItem::new(version_label.as_str(), false, None);

    let menu = Menu::new();
    menu.append_items(&[
        &stats_item,
        &open_item,
        &logs_item,
        &restart_item,
        &update_item,
        &auto_update_item,
        &check_update_item,
        &install_update_item,
        &quit_item,
        &version_item,
    ])
    .context("Failed to append menu items")?;

    let icon = load_icon()?;

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("Stream Server - Idle")
        .build()
        .context("Failed to build tray icon")?;

    let proxy = event_loop.create_proxy();
    let open_id = open_item.id().0.clone();
    let logs_id = logs_item.id().0.clone();
    let restart_id = restart_item.id().0.clone();
    let auto_update_id = auto_update_item.id().0.clone();
    let check_update_id = check_update_item.id().0.clone();
    let install_update_id = install_update_item.id().0.clone();
    let quit_id = quit_item.id().0.clone();

    let open_id_clone = open_id.clone();
    let logs_id_clone = logs_id.clone();
    let restart_id_clone = restart_id.clone();
    let auto_update_id_clone = auto_update_id.clone();
    let check_update_id_clone = check_update_id.clone();
    let install_update_id_clone = install_update_id.clone();
    let quit_id_clone = quit_id.clone();

    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id.0.as_str();
        if id == open_id_clone {
            proxy.send_event(UserEvent::OpenWeb).ok();
        } else if id == logs_id_clone {
            proxy.send_event(UserEvent::OpenLogs).ok();
        } else if id == restart_id_clone {
            proxy.send_event(UserEvent::Restart).ok();
        } else if id == auto_update_id_clone {
            proxy.send_event(UserEvent::ToggleAutoUpdate).ok();
        } else if id == check_update_id_clone {
            proxy.send_event(UserEvent::CheckUpdates).ok();
        } else if id == install_update_id_clone {
            proxy.send_event(UserEvent::InstallUpdate).ok();
        } else if id == quit_id_clone {
            proxy.send_event(UserEvent::Quit).ok();
        }
    }));

    Ok(TrayHandle {
        tray_icon,
        stats_item,
        update_item,
        auto_update_item,
        install_update_item,
        open_id,
        quit_id,
    })
}

pub fn trigger_update_check() {
    std::thread::spawn(|| {
        if let Err(err) = reqwest::blocking::Client::new()
            .post("http://127.0.0.1:11470/update/check")
            .json(&serde_json::json!({ "force": true }))
            .send()
            .and_then(|response| response.error_for_status())
        {
            error!("Failed to trigger update check: {}", err);
        }
    });
}

pub fn trigger_update_install() {
    std::thread::spawn(|| {
        if let Err(err) = reqwest::blocking::Client::new()
            .post("http://127.0.0.1:11470/update/install")
            .json(&serde_json::json!({ "force": false }))
            .send()
            .and_then(|response| response.error_for_status())
        {
            error!("Failed to trigger update install: {}", err);
        }
    });
}

pub fn trigger_auto_update_toggle(enabled: bool) {
    std::thread::spawn(move || {
        if let Err(err) = reqwest::blocking::Client::new()
            .post("http://127.0.0.1:11470/settings")
            .json(&serde_json::json!({ "autoUpdateEnabled": enabled }))
            .send()
            .and_then(|response| response.error_for_status())
        {
            error!("Failed to toggle auto update: {}", err);
            return;
        }

        if enabled {
            trigger_update_check();
        }
    });
}

pub fn open_stremio_web() {
    let url = "https://web.stremio.com/#/?streamingServer=http%3A%2F%2F127.0.0.1%3A11470";
    match open::that(url) {
        Ok(_) => info!("Opened Stremio Web in the browser"),
        Err(e) => error!("Failed to open Stremio Web: {}", e),
    }
}

pub fn open_log_folder() {
    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("stremio-server").join("logs");
        match open::that(&path) {
            Ok(_) => info!("Opened logs folder: {:?}", path),
            Err(e) => error!("Failed to open logs folder: {}", e),
        }
    } else {
        error!("Could not determine config directory");
    }
}
