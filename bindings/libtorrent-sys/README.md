# libtorrent-sys

Rust FFI bindings to [libtorrent-rasterbar](https://libtorrent.org/) via [cxx](https://cxx.rs/).

## Requirements

- **libtorrent-rasterbar** 2.0+
- **Boost** (headers)
- **C++17** compiler
- **pkg-config**

## Installation by OS

### Arch Linux
```bash
sudo pacman -S libtorrent-rasterbar boost
```

### Ubuntu / Debian
```bash
sudo apt install libtorrent-rasterbar-dev libboost-all-dev
```

### Fedora
```bash
sudo dnf install rb_libtorrent-devel boost-devel
```

### macOS
```bash
brew install libtorrent-rasterbar boost
```

### Windows (vcpkg)
```powershell
vcpkg install libtorrent:x64-windows boost:x64-windows
```

## API Overview

### Session Management
```rust
use libtorrent_sys::{LibtorrentSession, SessionSettings};

let settings = SessionSettings {
    enable_dht: true,
    enable_upnp: true,
    ..Default::default()
};
let mut session = LibtorrentSession::new(settings)?;
```

### Adding Torrents
```rust
// From magnet
let handle = session.add_magnet("magnet:?xt=...", "/downloads")?;

// From .torrent file
let params = AddTorrentParams {
    torrent_data: std::fs::read("file.torrent")?,
    save_path: "/downloads".to_string(),
    ..Default::default()
};
let handle = session.add_torrent(&params)?;
```

### Streaming
```rust
// Enable sequential mode
handle.set_sequential_download(true);

// Prioritize specific pieces
handle.set_piece_deadline(0, 0);   // First piece ASAP
handle.set_piece_deadline(1, 100); // Second piece within 100ms
```

### Status & Files
```rust
let status = handle.status();
println!("Progress: {:.1}%", status.progress * 100.0);
println!("Speed: {} KB/s", status.download_rate / 1024);

for file in handle.files() {
    println!("{}: {} bytes", file.path, file.size);
}
```

## Bound Functions

| Category | Functions |
|----------|-----------|
| Session | `create`, `pause`, `resume`, `apply_settings` |
| Torrents | `add_magnet`, `add_torrent`, `remove`, `find` |
| Handle | `pause`, `resume`, `status`, `files`, `peers` |
| Streaming | `set_sequential_download`, `set_piece_deadline` |
| DHT | `is_dht_running`, `add_dht_node`, `dht_get_peers` |
| State | `save_state`, `load_state` |

## License

MIT
