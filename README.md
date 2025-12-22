# Stream Server

**A blazing-fast, self-hosted torrent streaming server** — a modern, high-performance alternative to traditional streaming servers. Built in Rust for maximum performance and reliability.

[![Release Build](https://github.com/perpetus/stream-server/actions/workflows/release.yml/badge.svg)](https://github.com/perpetus/stream-server/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)

## 🌟 Why Stream Server?

| Feature | Stream Server | Traditional Servers |
|---------|--------------|---------------------|
| **Performance** | ⚡ Native Rust | Node.js overhead |
| **Memory Usage** | ~50MB | ~200MB+ |
| **HLS Transcoding** | ✅ Built-in | ✅ |
| **Self-hosted** | ✅ Full control | ⚠️ Often cloud-dependent |
| **Seekable Streams** | ✅ Instant | ⚠️ Variable |
| **Archive Streaming** | ✅ RAR/ZIP/7Z | ✅ |

> **Seamless Migration**: Drop-in compatible with existing streaming setups. Same API endpoints, same functionality — just faster.

---

## ✨ Features

### Core Streaming
- **🚀 High Performance**: Native Rust with optional C++ libtorrent backend
- **📺 HLS Transcoding**: Real-time video transcoding via FFmpeg (master.m3u8, stream.m3u8)
- **🔧 Multiple Backends**: `librqbit` (pure Rust) or `libtorrent` (battle-tested C++)
- **📡 HTTP Range Requests**: Full support for instant seeking

### Media Support
- **📝 Subtitle Extraction**: Automatic detection, OpenSubtitles hash calculation
- **🎬 Video Probing**: FFprobe integration for track analysis
- **📦 Archive Streaming**: Direct playback from RAR, ZIP, 7Z, TAR archives

### API Compatibility
- **🔌 Stats API**: `/stats.json` for server status and torrent info
- **🌐 Network Info**: `/network-info` endpoint for interface discovery
- **💓 Heartbeat**: `/heartbeat` for health checks
- **⚙️ Settings**: Runtime-configurable via `/settings`

---

## 📦 Installation

### Pre-built Binaries

Download from [Releases](https://github.com/perpetus/stream-server/releases):

| Platform | Download |
|----------|----------|
| Windows | `stream-server-windows-amd64.exe` / `.msi` |
| Linux (Debian/Ubuntu) | `.deb` package |
| Linux (Universal) | `.AppImage` |
| Arch Linux | `.pkg.tar.zst` |

### Build from Source

```bash
# Default build (librqbit - recommended)
cargo build --release

# With libtorrent backend (advanced)
cargo build --release --features libtorrent --no-default-features
```

---

## 🚀 Quick Start

```bash
# Run the server
./stream-server

# Or with cargo
cargo run --release -p server
```

The server starts on `http://localhost:11470` by default (compatible with standard streaming server port).

### API Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /{hash}/{fileIdx}` | Stream torrent file |
| `GET /{hash}/{fileIdx}/master.m3u8` | HLS master playlist |
| `GET /{hash}/{fileIdx}/stream.m3u8` | HLS stream playlist |
| `GET /stats.json` | Server statistics |
| `GET /settings` | Get current settings |
| `POST /settings` | Update settings |
| `GET /network-info` | Available network interfaces |
| `GET /heartbeat` | Health check |
| `GET /opensubHash?videoUrl=` | OpenSubtitles hash |
| `GET /subtitles.vtt?from=` | Subtitle conversion |
| `GET /probe?url=` | Video probe/analysis |
| `GET /rar/{path}` | Stream from RAR archive |
| `GET /zip/{path}` | Stream from ZIP archive |

---

## ⚙️ Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `SAVE_PATH` | `./downloads` | Torrent download location |
| `LISTEN_PORT` | `11470` | HTTP server port |
| `DHT_ENABLED` | `true` | Enable DHT for peer discovery |
| `BT_MAX_CONNECTIONS` | `35` | Maximum peer connections |
| `BT_HANDSHAKE_TIMEOUT` | `20000` | Handshake timeout (ms) |
| `BT_REQUEST_TIMEOUT` | `4000` | Request timeout (ms) |
| `CACHE_SIZE` | `2GB` | Download cache size |

---

## 🔧 Build Instructions

<details>
<summary><b>🐧 Arch Linux</b></summary>

```bash
sudo pacman -S rustup
rustup default stable

# For libtorrent backend
sudo pacman -S libtorrent-rasterbar boost pkg-config
```

</details>

<details>
<summary><b>🐧 Ubuntu / Debian</b></summary>

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

sudo apt update
sudo apt install build-essential pkg-config libssl-dev

# For libtorrent backend
sudo apt install libtorrent-rasterbar-dev libboost-all-dev
```

</details>

<details>
<summary><b>🐧 Fedora / RHEL</b></summary>

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

sudo dnf install gcc gcc-c++ pkg-config openssl-devel

# For libtorrent backend
sudo dnf install rb_libtorrent-devel boost-devel
```

</details>

<details>
<summary><b>🍎 macOS</b></summary>

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# For libtorrent backend
brew install libtorrent-rasterbar boost pkg-config
```

</details>

<details>
<summary><b>🪟 Windows</b></summary>

```powershell
# 1. Install Rust from https://rustup.rs
# 2. Install Visual Studio Build Tools with "Desktop development with C++"
# 3. For libtorrent, use vcpkg:
git clone https://github.com/microsoft/vcpkg
.\vcpkg\bootstrap-vcpkg.bat
.\vcpkg\vcpkg install libtorrent:x64-windows-static
```

</details>

---

## 📊 Backend Comparison

| Feature | librqbit | libtorrent |
|---------|----------|------------|
| **Language** | Pure Rust | C++ via FFI |
| **Binary Size** | Smaller | Larger |
| **Maturity** | Newer | Battle-tested |
| **DHT** | ✅ | ✅ |
| **uTP** | ✅ | ✅ |
| **Piece Deadline** | ❌ | ✅ |
| **Windows Setup** | ✅ Easy | ⚠️ Complex |

---

## 📁 Project Structure

```
stream-server/
├── server/           # HTTP server and API routes
├── enginefs/         # Torrent engine abstraction
│   └── src/backend/
│       ├── librqbit.rs   # Pure Rust backend
│       └── libtorrent.rs # Native C++ backend
└── libtorrent-sys/   # FFI bindings to libtorrent-rasterbar
```

---

## 🐛 Troubleshooting

<details>
<summary><b>libtorrent not found</b></summary>

```bash
pkg-config --exists libtorrent-rasterbar && echo "Found" || echo "Not found"
pkg-config --cflags --libs libtorrent-rasterbar
```

</details>

<details>
<summary><b>Boost not found</b></summary>

Install boost development headers:
- **Arch**: `boost`
- **Ubuntu**: `libboost-all-dev`
- **Fedora**: `boost-devel`
- **macOS**: `brew install boost`

</details>

---

## 📄 License

MIT License - see [LICENSE](LICENSE) for details.

---

<p align="center">
  <b>⭐ Star this repo if you find it useful!</b>
</p>

## Keywords

`torrent streaming` `streaming server` `self-hosted streaming` `hls transcoding` `rust torrent` `libtorrent` `video streaming server` `media server` `torrent player` `stream torrents` `stremio alternative` `enginefs`
