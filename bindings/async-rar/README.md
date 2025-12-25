# async-rar

Async Rust bindings for the UnRAR library using [autocxx](https://google.github.io/autocxx/).

## Features

- **Async Support**: Non-blocking operations using `tokio::spawn_blocking`
- **Cross-Platform**: Works on Windows, Linux, and macOS
- **Safe API**: Rust-idiomatic wrappers around the C++ UnRAR library
- **Password Support**: Extract password-protected archives

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
async-rar = "0.1"
tokio = { version = "1", features = ["rt-multi-thread"] }
```

## Setup

Before building, you need to download the UnRAR source code:

1. Download from [RARLAB](https://www.rarlab.com/rar_add.htm)
2. Extract to `vendor/unrar/` directory

```bash
mkdir -p vendor
cd vendor
# Download and extract unrarsrc-x.x.x.tar.gz
tar -xzf unrarsrc-*.tar.gz
mv unrar-* unrar
```

## Usage

### List Archive Contents

```rust
use async_rar::list_entries;

#[tokio::main]
async fn main() -> async_rar::Result<()> {
    let entries = list_entries("archive.rar").await?;
    
    for entry in entries {
        println!("{}: {} bytes", entry.name, entry.size);
    }
    
    Ok(())
}
```

### Extract All Files

```rust
use async_rar::extract_all;

#[tokio::main]
async fn main() -> async_rar::Result<()> {
    extract_all("archive.rar", "./output").await?;
    Ok(())
}
```

### Password-Protected Archives

```rust
use async_rar::extract_all_with_password;

#[tokio::main]
async fn main() -> async_rar::Result<()> {
    extract_all_with_password("encrypted.rar", "./output", "password").await?;
    Ok(())
}
```

### Using Archive Handle

```rust
use async_rar::AsyncRarArchive;

#[tokio::main]
async fn main() -> async_rar::Result<()> {
    let mut archive = AsyncRarArchive::open("archive.rar").await?;
    
    // List entries
    let entries = archive.list_entries().await?;
    println!("Found {} entries", entries.len());
    
    // Extract
    archive.extract_all("./output").await?;
    
    Ok(())
}
```

## Requirements

- Rust 2024 edition
- C++17 compatible compiler
- LLVM/Clang (for autocxx/bindgen)

## License

MIT License

**Note**: The UnRAR library has its own license that permits free use for handling RAR archives but prohibits using it to recreate the RAR compression algorithm. See [UnRAR License](https://www.rarlab.com/license.htm).
