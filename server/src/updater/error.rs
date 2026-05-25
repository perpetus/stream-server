use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateErrorKind {
    NotFound,
    Conflict,
    InvalidRelease,
    Unsupported,
    Io,
    Network,
    Unexpected,
}

#[derive(Debug)]
pub struct UpdateError {
    pub kind: UpdateErrorKind,
    pub message: String,
}

impl UpdateError {
    pub fn new(kind: UpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Conflict, message)
    }

    pub fn invalid_release(message: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::InvalidRelease, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Unsupported, message)
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UpdateError {}

impl From<anyhow::Error> for UpdateError {
    fn from(value: anyhow::Error) -> Self {
        Self::new(UpdateErrorKind::Unexpected, value.to_string())
    }
}

impl From<std::io::Error> for UpdateError {
    fn from(value: std::io::Error) -> Self {
        Self::new(UpdateErrorKind::Io, value.to_string())
    }
}

impl From<reqwest::Error> for UpdateError {
    fn from(value: reqwest::Error) -> Self {
        Self::new(UpdateErrorKind::Network, value.to_string())
    }
}
