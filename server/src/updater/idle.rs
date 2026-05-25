use crate::{diagnostics, state::AppState};

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
    let active_disk_downloads = download_engine
        .streams
        .active_file_streams
        .iter()
        .map(|stream| stream.count)
        .sum::<usize>();
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
