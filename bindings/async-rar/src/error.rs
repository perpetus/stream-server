//! Error types for async-rar

use thiserror::Error;

/// Result type alias for async-rar operations
pub type Result<T> = std::result::Result<T, RarError>;

/// Errors that can occur when working with RAR archives
#[derive(Debug, Error)]
pub enum RarError {
    #[error("Failed to open archive: {0}")]
    OpenFailed(String),

    #[error("Archive requires a password")]
    PasswordRequired,

    #[error("Incorrect password")]
    BadPassword,

    #[error("Archive data is corrupted")]
    BadData,

    #[error("Invalid or damaged archive")]
    BadArchive,

    #[error("Unknown archive format")]
    UnknownFormat,

    #[error("Failed to open file: {0}")]
    FileOpenError(String),

    #[error("Failed to create file: {0}")]
    FileCreateError(String),

    #[error("Read error")]
    ReadError,

    #[error("Write error")]
    WriteError,

    #[error("Buffer too small")]
    SmallBuffer,

    #[error("Missing reference volume")]
    MissingReference,

    #[error("End of archive reached")]
    EndOfArchive,

    #[error("Archive handle is closed")]
    ArchiveClosed,

    #[error("Unknown error (code: {0})")]
    Unknown(i32),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Task join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Invalid path encoding")]
    InvalidPath,
}

impl RarError {
    /// Convert UnRAR error code to RarError
    pub fn from_code(code: i32) -> Self {
        use crate::ffi::*;
        match code {
            RAR_END_ARCHIVE => RarError::EndOfArchive,
            RAR_NO_MEMORY => RarError::Unknown(code),
            RAR_BAD_DATA => RarError::BadData,
            RAR_BAD_ARCHIVE => RarError::BadArchive,
            RAR_UNKNOWN_FORMAT => RarError::UnknownFormat,
            RAR_EOPEN => RarError::FileOpenError("unknown".into()),
            RAR_ECREATE => RarError::FileCreateError("unknown".into()),
            RAR_EREAD => RarError::ReadError,
            RAR_EWRITE => RarError::WriteError,
            RAR_SMALL_BUF => RarError::SmallBuffer,
            RAR_MISSING_PASSWORD => RarError::PasswordRequired,
            RAR_EREFERENCE => RarError::MissingReference,
            RAR_BAD_PASSWORD => RarError::BadPassword,
            _ => RarError::Unknown(code),
        }
    }
}
