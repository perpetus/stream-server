use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Pid, System};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplyPlan {
    target_version: String,
    parent_pid: u32,
    restart_command: Vec<String>,
    standalone_target: Option<PathBuf>,
    stremio_dir: Option<PathBuf>,
    assets: Vec<ApplyAsset>,
    allow_stop_stremio: bool,
    msi_source: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ApplyAsset {
    source: PathBuf,
    destination: PathBuf,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RollbackPlan {
    target_version: String,
    files: Vec<RollbackFile>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RollbackFile {
    backup: PathBuf,
    destination: PathBuf,
}

fn main() {
    if let Err(err) = run() {
        log_line(&format!("updater-helper error: {err:#}"));
        eprintln!("updater-helper error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [flag, path] if flag == "--apply" => apply_update(Path::new(path)),
        [flag, path] if flag == "--rollback" => rollback_update(Path::new(path)),
        _ => Err(anyhow!(
            "usage: stream-server-updater.exe --apply <apply-plan.json> | --rollback <rollback-plan.json>"
        )),
    }
}

fn apply_update(plan_path: &Path) -> Result<()> {
    let plan: ApplyPlan = read_json(plan_path)?;
    log_line(&format!(
        "applying stream-server update target_version={}",
        plan.target_version
    ));

    wait_for_parent_exit(plan.parent_pid, Duration::from_secs(30));
    verify_assets(&plan.assets)?;

    if let Some(stremio_dir) = &plan.stremio_dir {
        ensure_stremio_runtime_backup(stremio_dir)?;
    }

    let backup_dir = data_local_dir()
        .join("stremio-server")
        .join("updates")
        .join("backup")
        .join(format!("v{}", plan.target_version));
    fs::create_dir_all(&backup_dir)
        .with_context(|| format!("failed to create backup directory {}", backup_dir.display()))?;

    let mut rollback_files = Vec::new();
    for (idx, asset) in plan.assets.iter().enumerate() {
        let backup = replace_file(asset, &backup_dir, idx)?;
        if let Some(backup) = backup {
            rollback_files.push(RollbackFile {
                backup,
                destination: asset.destination.clone(),
            });
        }
    }

    let rollback_plan = RollbackPlan {
        target_version: plan.target_version.clone(),
        files: rollback_files,
    };
    let rollback_path = backup_dir.join("rollback-plan.json");
    write_json(&rollback_path, &rollback_plan)?;

    if let Some(msi) = &plan.msi_source {
        launch_msi_elevated(msi)?;
    }

    if !plan.restart_command.is_empty() {
        restart_server(&plan.restart_command)?;
    }

    log_line("update apply completed");
    Ok(())
}

fn rollback_update(plan_path: &Path) -> Result<()> {
    let plan: RollbackPlan = read_json(plan_path)?;
    log_line(&format!(
        "rolling back stream-server update target_version={}",
        plan.target_version
    ));

    for file in &plan.files {
        if !file.backup.exists() {
            return Err(anyhow!(
                "rollback backup is missing: {}",
                file.backup.display()
            ));
        }
        if let Some(parent) = file.destination.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create destination directory {}",
                    parent.display()
                )
            })?;
        }
        fs::copy(&file.backup, &file.destination).with_context(|| {
            format!(
                "failed to restore {} to {}",
                file.backup.display(),
                file.destination.display()
            )
        })?;
    }

    log_line("rollback completed");
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_string_pretty(value)?;
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

fn wait_for_parent_exit(parent_pid: u32, timeout: Duration) {
    if parent_pid == 0 {
        return;
    }

    let started = Instant::now();
    let mut system = System::new_all();
    while started.elapsed() < timeout {
        system.refresh_all();
        if system.process(Pid::from_u32(parent_pid)).is_none() {
            log_line(&format!("parent process exited pid={parent_pid}"));
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }

    log_line(&format!(
        "parent process still present after timeout pid={parent_pid}"
    ));
}

fn verify_assets(assets: &[ApplyAsset]) -> Result<()> {
    for asset in assets {
        let actual = sha256_file(&asset.source)?;
        if !actual.eq_ignore_ascii_case(asset.sha256.trim()) {
            return Err(anyhow!(
                "checksum mismatch for {}: expected {}, got {}",
                asset.source.display(),
                asset.sha256,
                actual
            ));
        }
    }
    Ok(())
}

fn ensure_stremio_runtime_backup(stremio_dir: &Path) -> Result<()> {
    let server_js = stremio_dir.join("server.js");
    if !server_js.exists() {
        return Err(anyhow!(
            "server.js was not found at {}",
            server_js.display()
        ));
    }

    let runtime = stremio_dir.join("stremio-runtime.exe");
    let backup = stremio_dir.join("stremio-runtime.node.exe");
    if backup.exists() {
        log_line(&format!(
            "stremio-runtime.node.exe already exists, leaving untouched: {}",
            backup.display()
        ));
        return Ok(());
    }

    if runtime.exists() {
        fs::copy(&runtime, &backup).with_context(|| {
            format!(
                "failed to back up {} to {}",
                runtime.display(),
                backup.display()
            )
        })?;
        log_line(&format!(
            "created stremio runtime backup: {}",
            backup.display()
        ));
    }

    Ok(())
}

fn replace_file(asset: &ApplyAsset, backup_dir: &Path, index: usize) -> Result<Option<PathBuf>> {
    let actual = sha256_file(&asset.source)?;
    if !actual.eq_ignore_ascii_case(asset.sha256.trim()) {
        return Err(anyhow!(
            "checksum mismatch before replace for {}",
            asset.source.display()
        ));
    }

    let parent = asset
        .destination
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", asset.destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let backup = if asset.destination.exists() {
        let file_name = asset
            .destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let backup = backup_dir.join(format!("{index}-{file_name}"));
        fs::copy(&asset.destination, &backup).with_context(|| {
            format!(
                "failed to back up {} to {}",
                asset.destination.display(),
                backup.display()
            )
        })?;
        Some(backup)
    } else {
        None
    };

    let temp = parent.join(format!(
        ".{}.update-tmp",
        asset
            .destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("stream-server")
    ));
    if temp.exists() {
        fs::remove_file(&temp)
            .with_context(|| format!("failed to remove stale temp file {}", temp.display()))?;
    }
    fs::copy(&asset.source, &temp).with_context(|| {
        format!(
            "failed to copy {} to {}",
            asset.source.display(),
            temp.display()
        )
    })?;

    if asset.destination.exists() {
        fs::remove_file(&asset.destination)
            .with_context(|| format!("failed to remove {}", asset.destination.display()))?;
    }
    fs::rename(&temp, &asset.destination).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp.display(),
            asset.destination.display()
        )
    })?;

    let installed = sha256_file(&asset.destination)?;
    if !installed.eq_ignore_ascii_case(asset.sha256.trim()) {
        return Err(anyhow!(
            "installed checksum mismatch for {}",
            asset.destination.display()
        ));
    }

    log_line(&format!("replaced {}", asset.destination.display()));
    Ok(backup)
}

fn restart_server(command: &[String]) -> Result<()> {
    let exe = &command[0];
    let args = &command[1..];
    Command::new(exe)
        .args(args)
        .spawn()
        .with_context(|| format!("failed to restart {}", exe))?;
    log_line(&format!("restarted {}", exe));
    Ok(())
}

#[cfg(windows)]
fn launch_msi_elevated(msi: &Path) -> Result<()> {
    let quoted = msi.display().to_string().replace('\'', "''");
    let arg_list = format!("'/i', '{}'", quoted);
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "Start-Process msiexec.exe -ArgumentList {} -Verb RunAs -Wait",
                arg_list
            ),
        ])
        .spawn()
        .with_context(|| format!("failed to launch elevated MSI {}", msi.display()))?;
    log_line(&format!("launched elevated MSI {}", msi.display()));
    Ok(())
}

#[cfg(not(windows))]
fn launch_msi_elevated(msi: &Path) -> Result<()> {
    let _ = msi;
    Err(anyhow!("MSI elevation is only supported on Windows"))
}

fn sha256_file(path: &Path) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let digest = Sha256::digest(&data);
    Ok(hex::encode(digest))
}

fn appdata_dir() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_local_dir())
}

fn data_local_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

fn log_line(line: &str) {
    let log_dir = appdata_dir().join("stremio-server").join("logs");
    let _ = fs::create_dir_all(&log_dir);
    let path = log_dir.join("updater_current.log");
    let stamp = chrono_like_stamp();
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            writeln!(file, "{stamp} {line}")
        });
}

fn chrono_like_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix={now}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stremio_backup_is_not_overwritten() {
        let root =
            env::temp_dir().join(format!("stream-server-updater-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("server.js"), "server").unwrap();
        fs::write(root.join("stremio-runtime.exe"), "runtime").unwrap();
        fs::write(root.join("stremio-runtime.node.exe"), "original-backup").unwrap();

        ensure_stremio_runtime_backup(&root).unwrap();

        let backup = fs::read_to_string(root.join("stremio-runtime.node.exe")).unwrap();
        assert_eq!(backup, "original-backup");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_plan_refuses_missing_server_js() {
        let root = env::temp_dir().join(format!(
            "stream-server-updater-missing-server-js-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let err = ensure_stremio_runtime_backup(&root).unwrap_err();
        assert!(err.to_string().contains("server.js"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn replace_file_creates_backup_and_installs_new_bytes() {
        let root = env::temp_dir().join(format!(
            "stream-server-updater-replace-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let source_dir = root.join("source");
        let dest_dir = root.join("dest");
        let backup_dir = root.join("backup");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        fs::create_dir_all(&backup_dir).unwrap();

        let source = source_dir.join("stream-server.exe");
        let destination = dest_dir.join("stream-server.exe");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();
        let hash = sha256_file(&source).unwrap();
        let asset = ApplyAsset {
            source,
            destination: destination.clone(),
            sha256: hash,
        };

        let backup = replace_file(&asset, &backup_dir, 0).unwrap().unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert_eq!(fs::read(backup).unwrap(), b"old");
        let _ = fs::remove_dir_all(&root);
    }
}
