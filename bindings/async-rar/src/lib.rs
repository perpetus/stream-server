//! async-rar: Async Rust bindings for UnRAR
//!
//! This crate provides safe, async Rust bindings for extracting RAR archives
//! using the UnRAR C++ library.
//!
//! # Example
//!
//! ```no_run
//! use async_rar::{AsyncRarArchive, list_entries, extract_all};
//!
//! #[tokio::main]
//! async fn main() -> async_rar::Result<()> {
//!     // List entries in an archive
//!     let entries = list_entries("archive.rar").await?;
//!     for entry in entries {
//!         println!("{}: {} bytes", entry.name, entry.size);
//!     }
//!
//!     // Extract all files
//!     extract_all("archive.rar", "./output").await?;
//!
//!     // Or use the archive handle for more control
//!     let mut archive = AsyncRarArchive::open("archive.rar").await?;
//!     archive.set_password("secret".to_string()).await?;
//!     archive.extract_all("./output").await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Cross-Platform Support
//!
//! This crate supports Windows, Linux, and macOS. The UnRAR library is
//! compiled from source as part of the build process.

mod ffi;
mod error;
mod archive;
mod async_archive;

// Public API
pub use error::{RarError, Result};
pub use archive::{RarArchive, RarEntry, OpenMode};
pub use async_archive::{
    AsyncRarArchive,
    list_entries,
    extract_all,
    extract_all_with_password,
};
