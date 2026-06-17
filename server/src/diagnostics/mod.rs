pub mod logging;

use std::{net::SocketAddr, time::Instant};

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sysinfo::{Pid, System};

use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct ProcessMemorySnapshot {
    pub pid: u32,
    pub rss_bytes: u64,
    pub virtual_memory_bytes: u64,
    pub thread_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemorySnapshot {
    pub process: ProcessMemorySnapshot,
    pub engine: enginefs::EngineDiagnosticsSnapshot,
    pub download_engine: enginefs::EngineDiagnosticsSnapshot,
    pub download_disk_cache_bytes: u64,
    pub download_disk_cache_files: u64,
    pub active_disk_downloads: u64,
    pub disk_download_root: String,
    pub download_storage_mode: &'static str,
    pub download_disk_backed_available: bool,
    pub archive_session_count: usize,
    pub nzb_session_count: usize,
    pub active_direct_streams: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrashDumpInfo {
    pub path: String,
    pub bytes: u64,
    pub modified_unix_secs: Option<u64>,
}

pub fn process_memory_snapshot() -> ProcessMemorySnapshot {
    let pid_u32 = std::process::id();
    let mut system = System::new_all();
    system.refresh_all();

    let process = system.process(Pid::from_u32(pid_u32));
    ProcessMemorySnapshot {
        pid: pid_u32,
        rss_bytes: process.map(|process| process.memory()).unwrap_or(0),
        virtual_memory_bytes: process.map(|process| process.virtual_memory()).unwrap_or(0),
        thread_count: current_thread_count(),
    }
}

fn current_thread_count() -> u64 {
    current_thread_count_impl()
}

#[cfg(windows)]
fn current_thread_count_impl() -> u64 {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
    };

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return 0;
        };

        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let pid = std::process::id();
        let mut count = 0u64;

        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid {
                    count += 1;
                }

                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
        count
    }
}

#[cfg(not(windows))]
fn current_thread_count_impl() -> u64 {
    0
}

async fn memory_snapshot_for_state(state: &AppState) -> MemorySnapshot {
    let download_engine = state.download_engine.diagnostics_snapshot().await;
    let (download_disk_cache_bytes, download_disk_cache_files) =
        disk_tree_stats(&state.download_engine.download_dir);
    let active_disk_downloads = download_engine
        .streams
        .active_file_streams
        .iter()
        .map(|stream| stream.count as u64)
        .sum();

    MemorySnapshot {
        process: process_memory_snapshot(),
        engine: state.engine.diagnostics_snapshot().await,
        download_engine,
        download_disk_cache_bytes,
        download_disk_cache_files,
        active_disk_downloads,
        disk_download_root: state.download_engine.download_dir.display().to_string(),
        download_storage_mode: "dynamic",
        download_disk_backed_available: state.download_engine_disk_backed,
        archive_session_count: state.archive_cache.len(),
        nzb_session_count: state.nzb_sessions.len(),
        active_direct_streams: logging::active_direct_streams(),
    }
}

fn disk_tree_stats(root: &std::path::Path) -> (u64, u64) {
    if !root.exists() {
        return (0, 0);
    }

    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .components()
            .any(|component| component.as_os_str() == ".metadata")
        {
            continue;
        }
        if let Ok(metadata) = entry.metadata() {
            bytes = bytes.saturating_add(metadata.len());
            files = files.saturating_add(1);
        }
    }
    (bytes, files)
}

pub fn start_memory_sampler(state: AppState) -> tokio::task::JoinHandle<()> {
    logging::spawn_logged("memory-sampler", async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        let mut last_snapshot_log = Instant::now()
            .checked_sub(logging::MEMORY_SNAPSHOT_INTERVAL)
            .unwrap_or_else(Instant::now);
        let mut last_rss = 0u64;

        loop {
            interval.tick().await;
            let snapshot = memory_snapshot_for_state(&state).await;
            let rss = snapshot.process.rss_bytes;
            let growth = rss.saturating_sub(last_rss);
            let should_log_periodic =
                last_snapshot_log.elapsed() >= logging::MEMORY_SNAPSHOT_INTERVAL;
            let should_log_growth = growth >= logging::MEMORY_GROWTH_ALERT_BYTES;

            if should_log_periodic || should_log_growth {
                tracing::info!(
                    rss_bytes = snapshot.process.rss_bytes,
                    virtual_memory_bytes = snapshot.process.virtual_memory_bytes,
                    thread_count = snapshot.process.thread_count,
                    engine_count = snapshot.engine.streams.engine_count,
                    engine_active_streams = snapshot.engine.streams.engine_active_streams,
                    active_stream_hashes = snapshot.engine.streams.active_streams.len(),
                    active_file_streams = snapshot.engine.streams.active_file_streams.len(),
                    rust_piece_cache_entries = snapshot.engine.memory.rust_piece_cache_entries,
                    rust_piece_cache_bytes = snapshot.engine.memory.rust_piece_cache_bytes,
                    native_storage_bytes = snapshot.engine.memory.native_storage_bytes,
                    native_storage_pieces = snapshot.engine.memory.native_storage_pieces,
                    download_disk_cache_bytes = snapshot.download_disk_cache_bytes,
                    download_disk_cache_files = snapshot.download_disk_cache_files,
                    active_disk_downloads = snapshot.active_disk_downloads,
                    disk_download_root = %snapshot.disk_download_root,
                    download_storage_mode = snapshot.download_storage_mode,
                    download_disk_backed_available = snapshot.download_disk_backed_available,
                    waiter_keys = snapshot.engine.memory.waiter_keys,
                    waiter_wakers = snapshot.engine.memory.waiter_wakers,
                    archive_session_count = snapshot.archive_session_count,
                    nzb_session_count = snapshot.nzb_session_count,
                    active_direct_streams = snapshot.active_direct_streams,
                    growth_bytes = growth,
                    growth_alert = should_log_growth,
                    "memory diagnostics snapshot"
                );
                last_snapshot_log = Instant::now();
                last_rss = rss;
            }
        }
    })
}

pub async fn memory(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(response) = ensure_local(addr) {
        return response;
    }

    Json(memory_snapshot_for_state(&state).await).into_response()
}

pub async fn streams(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(response) = ensure_local(addr) {
        return response;
    }

    Json(state.engine.stream_activity_snapshot().await).into_response()
}

pub async fn crashes(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if let Err(response) = ensure_local(addr) {
        return response;
    }

    Json(list_crashes(&state)).into_response()
}

fn ensure_local(addr: SocketAddr) -> Result<(), Response> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "Diagnostics are local-only").into_response())
    }
}

fn list_crashes(state: &AppState) -> Vec<CrashDumpInfo> {
    let crash_dir = state.log_dir.join("crashes");
    let Ok(entries) = std::fs::read_dir(crash_dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("dmp") {
                return None;
            }

            let metadata = entry.metadata().ok()?;
            let modified_unix_secs = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());

            Some(CrashDumpInfo {
                path: path.display().to_string(),
                bytes: metadata.len(),
                modified_unix_secs,
            })
        })
        .collect()
}
