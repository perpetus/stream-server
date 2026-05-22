#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const READY_LINE: &str = "EngineFS server started at http://127.0.0.1:11470";
const ENDPOINT_LABEL: &str = "127.0.0.1:11470";
const READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const EXISTING_SERVER_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct StubError {
    code: i32,
    message: String,
}

impl StubError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn unexpected(message: impl Into<String>) -> Self {
        Self::new(1, message)
    }
}

fn main() {
    let exit_code = match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("stremio-runtime stub error: {}", err.message);
            err.code
        }
    };

    std::process::exit(exit_code);
}

fn run() -> Result<i32, StubError> {
    let exe_path = env::current_exe()
        .map_err(|err| StubError::unexpected(format!("failed to resolve current exe: {err}")))?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| StubError::unexpected("failed to resolve current exe directory"))?
        .to_path_buf();

    let server_js = env::args_os()
        .nth(1)
        .ok_or_else(|| StubError::new(2, "missing server.js argument"))?;
    validate_server_js(&server_js)?;

    let real_server = real_server_path(&exe_dir);
    if !real_server.exists() {
        return Err(StubError::new(
            2,
            format!(
                "stream-server.exe was not found at {}",
                real_server.display()
            ),
        ));
    }

    if endpoint_ready() {
        print_ready_line()?;
        return monitor_existing_server();
    }

    let mut child = spawn_real_server(&real_server, &exe_dir)?;
    let stdout_thread = child.stdout.take().map(forward_stdout);
    let stderr_thread = child.stderr.take().map(forward_stderr);

    let started = Instant::now();
    loop {
        if endpoint_ready() {
            print_ready_line()?;
            let status = child.wait().map_err(|err| {
                StubError::unexpected(format!("failed to wait for server: {err}"))
            })?;
            join_forwarder(stdout_thread);
            join_forwarder(stderr_thread);
            return Ok(status_code(status));
        }

        if let Some(status) = child.try_wait().map_err(|err| {
            StubError::unexpected(format!("failed to check stream-server.exe status: {err}"))
        })? {
            eprintln!("stream-server.exe exited before readiness: {status}");
            join_forwarder(stdout_thread);
            join_forwarder(stderr_thread);
            return Ok(status.code().unwrap_or(3));
        }

        if started.elapsed() >= READINESS_TIMEOUT {
            eprintln!(
                "stream-server.exe did not become reachable at {} within {} seconds",
                ENDPOINT_LABEL,
                READINESS_TIMEOUT.as_secs()
            );
            terminate_child(&mut child);
            join_forwarder(stdout_thread);
            join_forwarder(stderr_thread);
            return Ok(5);
        }

        thread::sleep(READINESS_POLL_INTERVAL);
    }
}

fn validate_server_js(server_js: &OsString) -> Result<(), StubError> {
    let path = PathBuf::from(server_js);
    if path.exists() {
        Ok(())
    } else {
        Err(StubError::new(
            2,
            format!("server.js argument does not exist: {}", path.display()),
        ))
    }
}

fn real_server_path(exe_dir: &Path) -> PathBuf {
    env::var_os("STREMIO_REAL_SERVER_EXE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| exe_dir.join("stream-server.exe"))
}

fn spawn_real_server(real_server: &Path, exe_dir: &Path) -> Result<Child, StubError> {
    let mut command = Command::new(real_server);
    command
        .current_dir(exe_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_command(&mut command);

    command.spawn().map_err(|err| {
        StubError::unexpected(format!("failed to launch {}: {err}", real_server.display()))
    })
}

fn configure_command(command: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

fn monitor_existing_server() -> Result<i32, StubError> {
    loop {
        thread::sleep(EXISTING_SERVER_POLL_INTERVAL);
        if !endpoint_ready() {
            eprintln!("external stream server disappeared at {}", ENDPOINT_LABEL);
            return Ok(4);
        }
    }
}

fn endpoint_ready() -> bool {
    TcpStream::connect_timeout(&endpoint_addr(), Duration::from_millis(200)).is_ok()
}

fn endpoint_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 11470))
}

fn print_ready_line() -> Result<(), StubError> {
    let mut stdout = io::stdout();
    writeln!(stdout, "{READY_LINE}")
        .and_then(|_| stdout.flush())
        .map_err(|err| StubError::unexpected(format!("failed to print readiness line: {err}")))
}

fn forward_stdout(mut source: impl io::Read + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stdout = io::stdout();
        let _ = io::copy(&mut source, &mut stdout);
        let _ = stdout.flush();
    })
}

fn forward_stderr(mut source: impl io::Read + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stderr = io::stderr();
        let _ = io::copy(&mut source, &mut stderr);
        let _ = stderr.flush();
    })
}

fn join_forwarder(handle: Option<thread::JoinHandle<()>>) {
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
