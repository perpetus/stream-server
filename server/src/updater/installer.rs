use super::{
    error::{UpdateError, UpdateErrorKind},
    staging::StagedUpdate,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPlan {
    pub target_version: String,
    pub parent_pid: u32,
    pub restart_command: Vec<String>,
    pub standalone_target: Option<PathBuf>,
    pub stremio_dir: Option<PathBuf>,
    pub assets: Vec<ApplyAsset>,
    pub allow_stop_stremio: bool,
    pub msi_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyAsset {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub sha256: String,
}

pub fn prepare_apply_plan(
    staged: &StagedUpdate,
    allow_stop_stremio: bool,
) -> Result<(PathBuf, PathBuf), UpdateError> {
    if !cfg!(windows) {
        return Err(UpdateError::unsupported(
            "automatic installation is only supported on Windows in this release",
        ));
    }

    let current_exe = std::env::current_exe().map_err(UpdateError::from)?;
    let helper = staged.assets.updater.clone();
    let mut assets = Vec::new();
    let server_hash = staged_hash(staged, "stream-server-windows-amd64.exe")?;
    let runtime_hash = staged_hash(staged, "stremio-runtime-windows-amd64.exe")?;

    let mut msi_source = None;
    if parent_dir_writable(&current_exe) {
        push_asset_unique(
            &mut assets,
            ApplyAsset {
                source: staged.assets.server.clone(),
                destination: current_exe.clone(),
                sha256: server_hash.clone(),
            },
        );
    } else if let Some(msi) = &staged.assets.msi {
        msi_source = Some(msi.clone());
    } else {
        return Err(UpdateError::new(
            UpdateErrorKind::Conflict,
            format!(
                "current executable directory is not writable and no MSI asset is staged: {}",
                current_exe.display()
            ),
        ));
    }

    let stremio_dir = default_stremio_dir();
    let mut plan_stremio_dir = None;
    if let Some(dir) = stremio_dir {
        let server_js = dir.join("server.js");
        if server_js.exists() {
            plan_stremio_dir = Some(dir.clone());
            push_asset_unique(
                &mut assets,
                ApplyAsset {
                    source: staged.assets.runtime.clone(),
                    destination: dir.join("stremio-runtime.exe"),
                    sha256: runtime_hash,
                },
            );
            push_asset_unique(
                &mut assets,
                ApplyAsset {
                    source: staged.assets.server.clone(),
                    destination: dir.join("stream-server.exe"),
                    sha256: server_hash,
                },
            );
        } else if dir.exists() {
            return Err(UpdateError::new(
                UpdateErrorKind::Conflict,
                format!(
                    "server.js was not found in Stremio directory {}",
                    dir.display()
                ),
            ));
        }
    }

    let restart_command = vec![current_exe.display().to_string(), "--silent".to_string()];
    let plan = ApplyPlan {
        target_version: staged.version.clone(),
        parent_pid: std::process::id(),
        restart_command,
        standalone_target: Some(current_exe),
        stremio_dir: plan_stremio_dir,
        assets,
        allow_stop_stremio,
        msi_source,
    };

    let plan_path = staged.dir.join("apply-plan.json");
    let data = serde_json::to_string_pretty(&plan)
        .map_err(|err| UpdateError::new(UpdateErrorKind::Unexpected, err.to_string()))?;
    std::fs::write(&plan_path, data).map_err(UpdateError::from)?;
    Ok((helper, plan_path))
}

pub fn spawn_helper(helper: &Path, plan_path: &Path) -> Result<(), UpdateError> {
    Command::new(helper)
        .arg("--apply")
        .arg(plan_path)
        .spawn()
        .map(|_| ())
        .map_err(|err| {
            UpdateError::new(
                UpdateErrorKind::Io,
                format!("failed to spawn updater helper {}: {err}", helper.display()),
            )
        })
}

fn staged_hash(staged: &StagedUpdate, name: &str) -> Result<String, UpdateError> {
    staged.hashes.get(name).cloned().ok_or_else(|| {
        UpdateError::invalid_release(format!("staged checksum manifest missing {name}"))
    })
}

fn push_asset_unique(assets: &mut Vec<ApplyAsset>, asset: ApplyAsset) {
    if assets
        .iter()
        .any(|existing| same_path(&existing.destination, &asset.destination))
    {
        return;
    }
    assets.push(asset);
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn parent_dir_writable(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let probe = parent.join(format!(
        ".stream-server-update-probe-{}",
        std::process::id()
    ));
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

fn default_stremio_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("STREMIO_INSTALL_DIR") {
        return Some(PathBuf::from(value));
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|dir| dir.join("Programs").join("Stremio"))
}
