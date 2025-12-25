use anyhow::Context;
use tao::event_loop::EventLoop;

use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIcon, TrayIconBuilder, Icon,
};
use tracing::{info, error};

pub enum UserEvent {
    OpenWeb,
    Restart,
    Quit,
}

pub fn load_icon() -> anyhow::Result<Icon> {
    // Generate a simple 32x32 green icon programmatically since we don't have an asset
    let width = 32;
    let height = 32;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..height {
        for _ in 0..width {
            // Green color
            rgba.push(0);   // R
            rgba.push(255); // G
            rgba.push(0);   // B
            rgba.push(255); // A
        }
    }
    Icon::from_rgba(rgba, width, height).context("Failed to create icon")
}

pub fn create_system_tray(
    event_loop: &EventLoop<UserEvent>,
) -> anyhow::Result<(TrayIcon, String, String)> {
    let open_item = MenuItem::new("Open Stremio Web", true, None);
    let restart_item = MenuItem::new("Restart Server", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    let version_label = format!("v{}", env!("CARGO_PKG_VERSION"));
    let version_item = MenuItem::new(version_label.as_str(), false, None);

    let menu = Menu::new();
    menu.append_items(&[&open_item, &restart_item, &quit_item, &version_item])
        .context("Failed to append menu items")?;

    let icon = load_icon()?;

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("Stream Server")
        .build()
        .context("Failed to build tray icon")?;

    let proxy = event_loop.create_proxy();
    let open_id = open_item.id().0.clone();
    let restart_id = restart_item.id().0.clone();
    let quit_id = quit_item.id().0.clone();
    
    let open_id_clone = open_id.clone();
    let restart_id_clone = restart_id.clone();
    let quit_id_clone = quit_id.clone();
    
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id.0.as_str();
        if id == open_id_clone {
            proxy.send_event(UserEvent::OpenWeb).ok();
        } else if id == restart_id_clone {
            proxy.send_event(UserEvent::Restart).ok();
        } else if id == quit_id_clone {
            proxy.send_event(UserEvent::Quit).ok();
        }
    }));

    Ok((
        tray_icon,
        open_id,
        quit_id,
    ))
}

pub fn open_stremio_web() {
    let url = "https://app.strem.io/#/?streamingServer=http%3A%2F%2F127.0.0.1%3A11470";
    match open::that(url) {
        Ok(_) => info!("Opened Stremio Web in the browser"),
        Err(e) => error!("Failed to open Stremio Web: {}", e),
    }
}
