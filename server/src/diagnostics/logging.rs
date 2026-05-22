use std::{
    backtrace::Backtrace,
    future::Future,
    path::{Path, PathBuf},
    sync::OnceLock,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use chrono::Local;
use tokio::task::JoinHandle;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

static LOG_GUARDS: OnceLock<Vec<WorkerGuard>> = OnceLock::new();
static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static ACTIVE_DIRECT_STREAMS: AtomicU64 = AtomicU64::new(0);

pub struct LogWriters {
    pub human_writer: NonBlocking,
    pub archive_writer: NonBlocking,
    pub json_writer: NonBlocking,
    pub human_path: PathBuf,
    pub archive_path: PathBuf,
    pub json_path: PathBuf,
    pub guards: Vec<WorkerGuard>,
}

pub fn init_process_start() {
    let _ = PROCESS_START.set(Instant::now());
}

pub fn uptime_secs() -> u64 {
    PROCESS_START.get_or_init(Instant::now).elapsed().as_secs()
}

pub fn active_direct_streams() -> u64 {
    ACTIVE_DIRECT_STREAMS.load(Ordering::Relaxed)
}

pub fn direct_stream_started() {
    ACTIVE_DIRECT_STREAMS.fetch_add(1, Ordering::Relaxed);
}

pub fn direct_stream_ended() {
    ACTIVE_DIRECT_STREAMS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            Some(count.saturating_sub(1))
        })
        .ok();
}

pub fn open_log_writers(log_dir: &Path) -> std::io::Result<LogWriters> {
    std::fs::create_dir_all(log_dir)?;

    let human_path = log_dir.join("server_current.log");
    let archive_path = log_dir.join(format!(
        "server_{}.log",
        Local::now().format("%Y-%m-%d_%H-%M-%S")
    ));
    let json_path = log_dir.join(format!(
        "server_{}.jsonl",
        Local::now().format("%Y-%m-%d_%H")
    ));

    let human_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&human_path)?;
    let archive_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&archive_path)?;
    let json_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&json_path)?;

    let (human_writer, human_guard) = tracing_appender::non_blocking(human_file);
    let (archive_writer, archive_guard) = tracing_appender::non_blocking(archive_file);
    let (json_writer, json_guard) = tracing_appender::non_blocking(json_file);

    Ok(LogWriters {
        human_writer,
        archive_writer,
        json_writer,
        human_path,
        archive_path,
        json_path,
        guards: vec![human_guard, archive_guard, json_guard],
    })
}

pub fn store_log_guards(guards: Vec<WorkerGuard>) {
    let _ = LOG_GUARDS.set(guards);
}

pub fn install_panic_hook() {
    static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();
    if PANIC_HOOK_INSTALLED.set(()).is_err() {
        return;
    }

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("unnamed");
        let location = panic_info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let message = panic_message(panic_info);
        let memory = crate::diagnostics::process_memory_snapshot();
        let backtrace = Backtrace::force_capture();

        tracing::error!(
            panic.message = %message,
            panic.location = %location,
            thread.name = %thread_name,
            thread.id = ?current_thread.id(),
            process.id = std::process::id(),
            uptime_secs = uptime_secs(),
            active_direct_streams = active_direct_streams(),
            rss_bytes = memory.rss_bytes,
            virtual_memory_bytes = memory.virtual_memory_bytes,
            backtrace = %backtrace,
            "process panic captured"
        );

        previous(panic_info);
    }));
}

fn panic_message(panic_info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(message) = panic_info.payload().downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic_info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

pub fn spawn_logged<F>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let joined = tokio::spawn(fut).await;
        match joined {
            Ok(()) => tracing::warn!(task = name, "long-running task returned"),
            Err(err) if err.is_panic() => {
                tracing::error!(task = name, error = %err, "long-running task panicked")
            }
            Err(err) => tracing::warn!(task = name, error = %err, "long-running task cancelled"),
        }
    })
}

pub fn log_startup_context(
    config_dir: &Path,
    cache_dir: &Path,
    log_dir: &Path,
    human_log: &Path,
    archive_log: &Path,
    json_log: &Path,
) {
    let exe_path = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let args = std::env::args().collect::<Vec<_>>();

    tracing::info!(
        server.version = env!("CARGO_PKG_VERSION"),
        git_sha = option_env!("GIT_SHA").unwrap_or("unknown"),
        process.id = std::process::id(),
        executable = %exe_path,
        config_dir = %config_dir.display(),
        cache_dir = %cache_dir.display(),
        log_dir = %log_dir.display(),
        human_log = %human_log.display(),
        archive_log = %archive_log.display(),
        json_log = %json_log.display(),
        args = ?args,
        "server startup context"
    );
}

pub fn install_native_crash_handler(log_dir: &Path) {
    let crash_dir = log_dir.join("crashes");
    if let Err(err) = std::fs::create_dir_all(&crash_dir) {
        tracing::warn!(error = %err, path = %crash_dir.display(), "failed to create crash dump directory");
        return;
    }

    log_existing_crash_dumps(&crash_dir);

    #[cfg(windows)]
    unsafe {
        install_windows_exception_filter(crash_dir);
    }
}

fn log_existing_crash_dumps(crash_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(crash_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("dmp") {
            tracing::warn!(dump_path = %path.display(), "previous crash dump found");
        }
    }
}

#[cfg(windows)]
static WINDOWS_CRASH_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(windows)]
unsafe fn install_windows_exception_filter(crash_dir: PathBuf) {
    use windows::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;

    let _ = WINDOWS_CRASH_DIR.set(crash_dir);
    unsafe {
        SetUnhandledExceptionFilter(Some(windows_exception_filter));
    }
}

#[cfg(windows)]
unsafe extern "system" fn windows_exception_filter(
    exception_info: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        System::{
            Diagnostics::Debug::{
                EXCEPTION_EXECUTE_HANDLER, MINIDUMP_EXCEPTION_INFORMATION,
                MiniDumpWithFullMemoryInfo, MiniDumpWithHandleData, MiniDumpWithThreadInfo,
                MiniDumpWithUnloadedModules, MiniDumpWriteDump,
            },
            Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId},
        },
    };

    if let Some(crash_dir) = WINDOWS_CRASH_DIR.get() {
        let path = crash_dir.join(format!(
            "server_{}_pid{}.dmp",
            Local::now().format("%Y-%m-%d_%H-%M-%S"),
            std::process::id()
        ));

        if let Ok(file) = std::fs::File::create(&path) {
            let mut exception = MINIDUMP_EXCEPTION_INFORMATION {
                ThreadId: unsafe { GetCurrentThreadId() },
                ExceptionPointers: exception_info as *mut _,
                ClientPointers: false.into(),
            };
            let dump_type = MiniDumpWithThreadInfo
                | MiniDumpWithUnloadedModules
                | MiniDumpWithFullMemoryInfo
                | MiniDumpWithHandleData;

            let result = unsafe {
                MiniDumpWriteDump(
                    GetCurrentProcess(),
                    GetCurrentProcessId(),
                    HANDLE(file.as_raw_handle()),
                    dump_type,
                    Some(&mut exception),
                    None,
                    None,
                )
            };

            match result {
                Ok(()) => tracing::error!(dump_path = %path.display(), "native crash dump written"),
                Err(err) => {
                    tracing::error!(dump_path = %path.display(), error = %err, "failed to write native crash dump")
                }
            }
        }
    }

    EXCEPTION_EXECUTE_HANDLER
}

pub const MEMORY_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(60);
pub const MEMORY_GROWTH_ALERT_BYTES: u64 = 128 * 1024 * 1024;
