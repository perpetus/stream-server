use crate::{diagnostics, state::AppState};
use std::collections::HashSet;

pub async fn install_blockers(state: &AppState) -> Vec<String> {
    let mut blockers = Vec::new();

    if diagnostics::logging::active_direct_streams() > 0 {
        blockers.push("activeStreams".to_string());
    }

    let engine = state.engine.diagnostics_snapshot().await;
    if engine.streams.engine_active_streams > 0 || !engine.streams.active_streams.is_empty() {
        push_once(&mut blockers, "activeStreams");
    }

    let download_engine = state.download_engine.diagnostics_snapshot().await;
    let mut active_disk_files = HashSet::new();
    for stream in &download_engine.streams.active_file_streams {
        if stream.count > 0 {
            active_disk_files.insert((stream.info_hash.clone(), stream.file_idx));
        }
    }
    for lease in &download_engine.streams.active_playback_leases {
        active_disk_files.insert((lease.info_hash.clone(), lease.file_idx));
    }
    let active_disk_downloads = active_disk_files.len();
    if active_disk_downloads > 0 {
        blockers.push("activeDownloads".to_string());
    }

    if !state.archive_cache.is_empty() {
        blockers.push("activeArchiveSessions".to_string());
    }

    if !state.nzb_sessions.is_empty() {
        blockers.push("activeNzbSessions".to_string());
    }

    blockers
}

fn push_once(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}
