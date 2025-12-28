# Stream Server

<div align="center">

**🚀 Open Source Torrent Streaming Engine**

*A modern, high-performance alternative to Stremio's closed-source `server.js`*

[![Release Build](https://github.com/perpetus/stream-server/actions/workflows/release.yml/badge.svg)](https://github.com/perpetus/stream-server/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Open Source](https://img.shields.io/badge/Open%20Source-✓-brightgreen?style=flat-square)](https://github.com/perpetus/stream-server)

</div>

---

## 💡 About

Stream Server is a **fully open-source** replacement for Stremio's proprietary `server.js`. While Stremio's streaming backend remains closed-source, this project provides complete transparency, community-driven development, and the freedom to run your streaming engine locally on your own terms.

**Built in Rust** for maximum performance, minimal memory footprint, and rock-solid reliability.

---

## 🌟 Why Stream Server?

| Feature | Stream Server | Stremio server.js |
|---------|--------------|-------------------|
| **Open Source** | ✅ **Yes** (MIT License) | ❌ Closed source |
| **Performance** | ⚡ Native Rust | Node.js overhead |
| **Memory Usage** | ~50MB | ~200MB+ |
| **Control** | ✅ Full local control | ⚠️ Limited |
| **Customizable** | ✅ Fork & modify | ❌ No access |
| **HLS Transcoding** | ✅ Built-in | ✅ |
| **Seekable Streams** | ✅ Instant | ⚠️ Variable |
| **Archive Streaming** | ✅ RAR/ZIP/7Z | ✅ |

> **Seamless Migration**: Drop-in compatible with existing Stremio setups. Same API endpoints, same functionality — just faster and open source.

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

`stremio server.js alternative` `open source streaming engine` `torrent streaming` `local streaming` `desktop streaming` `hls transcoding` `rust torrent` `libtorrent` `video streaming server` `media engine` `torrent player` `stream torrents` `stremio alternative` `enginefs` `stremio open source`
