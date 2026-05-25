use super::{assets::RequiredAssets, staging::StagedUpdate, version::UpdateChannel};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpdateOperationState {
    Idle,
    Checking,
    Available,
    Downloading,
    Staged,
    WaitingForIdle,
    Installing,
    UpToDate,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub state: UpdateOperationState,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub channel: UpdateChannel,
    pub release_url: Option<String>,
    pub last_checked_unix_secs: Option<i64>,
    pub staged_version: Option<String>,
    pub install_blocked_by: Vec<String>,
    pub error: Option<String>,
}

impl UpdateStatus {
    pub fn new(channel: UpdateChannel) -> Self {
        Self {
            state: UpdateOperationState::Idle,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            latest_version: None,
            channel,
            release_url: None,
            last_checked_unix_secs: None,
            staged_version: None,
            install_blocked_by: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCandidate {
    pub version: String,
    pub tag: String,
    pub release_url: String,
    pub prerelease: bool,
    pub assets: RequiredAssets,
}

#[derive(Debug, Clone)]
pub struct UpdateInner {
    pub status: UpdateStatus,
    pub candidate: Option<UpdateCandidate>,
    pub staged: Option<StagedUpdate>,
}

impl UpdateInner {
    pub fn new(channel: UpdateChannel) -> Self {
        Self {
            status: UpdateStatus::new(channel),
            candidate: None,
            staged: None,
        }
    }
}
