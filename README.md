# Stream Server

A high-performance torrent streaming server with multiple backend support.

## Features

- **Multiple Backends**: Choose between `librqbit` (pure Rust) or `libtorrent` (native C++)
- **Video Streaming**: HTTP streaming with range request support
- **Subtitle Extraction**: Automatic subtitle detection and serving
- **Stremio Compatible**: Full Stremio stats API compatibility

## Quick Start

```bash
# Default build (librqbit backend)
cargo build --release

# With libtorrent backend
cargo build --release --features libtorrent --no-default-features
```

---

## Build Instructions

### Dependencies by OS

<details>
<summary><b>🐧 Arch Linux</b></summary>

```bash
# Rust toolchain
sudo pacman -S rustup
rustup default stable

# For libtorrent backend
sudo pacman -S libtorrent-rasterbar boost pkg-config
```

</details>

<details>
<summary><b>🐧 Ubuntu / Debian</b></summary>

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Build essentials
sudo apt update
sudo apt install build-essential pkg-config libssl-dev

# For libtorrent backend
sudo apt install libtorrent-rasterbar-dev libboost-all-dev
```

</details>

<details>
<summary><b>🐧 Fedora / RHEL</b></summary>

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Build essentials
sudo dnf install gcc gcc-c++ pkg-config openssl-devel

# For libtorrent backend
sudo dnf install rb_libtorrent-devel boost-devel
```

</details>

<details>
<summary><b>🍎 macOS</b></summary>

```bash
# Rust toolchain
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

# 2. Install Visual Studio Build Tools
#    - Select "Desktop development with C++" workload

# 3. For libtorrent backend, use vcpkg:
git clone https://github.com/microsoft/vcpkg
.\vcpkg\bootstrap-vcpkg.bat
.\vcpkg\vcpkg install libtorrent:x64-windows boost:x64-windows

# 4. Set environment variables
$env:PKG_CONFIG_PATH = "C:\path\to\vcpkg\installed\x64-windows\lib\pkgconfig"
```

> ⚠️ **Note**: Windows libtorrent support is experimental. Use `librqbit` (default) for best compatibility.

</details>

---

## Backend Comparison

| Feature | librqbit | libtorrent |
|---------|----------|------------|
| Language | Pure Rust | C++ via FFI |
| Binary Size | Smaller | Larger |
| Maturity | Newer | Battle-tested |
| DHT | ✅ | ✅ |
| uTP | ✅ | ✅ |
| Piece Deadline | ❌ | ✅ |
| Windows | ✅ Easy | ⚠️ Complex |

---

## Build Commands

```bash
# Default (librqbit)
cargo build --release

# libtorrent only
cargo build --release --features libtorrent --no-default-features

# Both backends (runtime selection)
cargo build --release --features "librqbit libtorrent"

# Check without building
cargo check

# Run tests
cargo test

# Run server
cargo run --release -p server
```

---

## Project Structure

```
stream-server/
├── server/           # HTTP server and API routes
├── enginefs/         # Torrent engine abstraction
│   └── src/backend/
│       ├── mod.rs        # TorrentBackend trait
│       ├── librqbit.rs   # Pure Rust backend
│       └── libtorrent.rs # Native C++ backend
└── libtorrent-sys/   # FFI bindings to libtorrent-rasterbar
    ├── src/lib.rs        # Rust cxx bridge
    └── cpp/
        ├── wrapper.h     # C++ header
        └── wrapper.cpp   # C++ implementation
```

---

## Troubleshooting

### libtorrent not found

```bash
# Check if pkg-config can find it
pkg-config --exists libtorrent-rasterbar && echo "Found" || echo "Not found"

# Show include/lib paths
pkg-config --cflags --libs libtorrent-rasterbar
```

### Boost not found

Ensure boost is installed with development headers:
- **Arch**: `boost` (includes headers)
- **Ubuntu**: `libboost-all-dev`
- **Fedora**: `boost-devel`
- **macOS**: `brew install boost`

### C++ compiler errors

Ensure you have a C++17 compatible compiler:
- **GCC**: 7+ (`g++ --version`)
- **Clang**: 5+ (`clang++ --version`)
- **MSVC**: VS 2017+

---

## Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `SAVE_PATH` | `./downloads` | Torrent download location |
| `LISTEN_PORT` | `8080` | HTTP server port |
| `DHT_ENABLED` | `true` | Enable DHT |

---

## License

MIT
