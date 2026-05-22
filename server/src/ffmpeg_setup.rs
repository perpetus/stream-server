use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info, warn};

const FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const SHA256_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.sha256";

/// Orchestrate the setup: check, download, verify, extract, configure path.
pub async fn setup_ffmpeg() -> Result<()> {
    if !cfg!(target_os = "windows") {
        info!("Not on Windows, skipping automatic FFmpeg download.");
        return Ok(());
    }

    if check_ffmpeg_tools_available() {
        info!("FFmpeg and FFprobe are already available in PATH.");
        return Ok(());
    }

    info!("FFmpeg not found. Attempting auto-download...");

    let install_dirs = install_dir_candidates();

    // Check if we already installed it but it's just not in PATH
    for install_dir in &install_dirs {
        let bin_dir = install_dir.join("bin");
        if ffmpeg_tools_exist(&bin_dir) {
            info!(
                "FFmpeg binaries found in {:?}, valid but not in PATH. Adding to PATH.",
                bin_dir
            );
            add_to_path(&bin_dir);
            return Ok(());
        }
    }

    let install_dir = select_writable_install_dir(&install_dirs)?;
    let bin_dir = install_dir.join("bin");

    match download_and_install(&install_dir).await {
        Ok(_) => {
            info!("FFmpeg installed successfully to {:?}", install_dir);
            add_to_path(&bin_dir);
            Ok(())
        }
        Err(e) => {
            error!(
                "Failed to auto-download FFmpeg: {}. Transcoding may fail.",
                e
            );
            Err(e)
        }
    }
}

fn install_dir_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            dirs.push(parent.join("tools").join("ffmpeg"));
        } else {
            warn!("Could not determine executable parent directory for FFmpeg install");
        }
    } else {
        warn!("Could not determine executable path for FFmpeg install");
    }

    if let Some(data_dir) = dirs::data_local_dir() {
        dirs.push(data_dir.join("stremio-server").join("tools").join("ffmpeg"));
    }

    if let Some(cache_dir) = dirs::cache_dir() {
        dirs.push(
            cache_dir
                .join("stremio-server")
                .join("tools")
                .join("ffmpeg"),
        );
    }

    dirs.push(
        std::env::temp_dir()
            .join("stremio-server")
            .join("tools")
            .join("ffmpeg"),
    );

    dedupe_paths(dirs)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn select_writable_install_dir(candidates: &[PathBuf]) -> Result<PathBuf> {
    let mut failures = Vec::new();

    for candidate in candidates {
        match ensure_writable_dir(candidate) {
            Ok(()) => return Ok(candidate.clone()),
            Err(err) => {
                warn!(
                    install_dir = %candidate.display(),
                    error = %err,
                    "FFmpeg install directory is not writable; trying next candidate"
                );
                failures.push(format!("{} ({})", candidate.display(), err));
            }
        }
    }

    Err(anyhow::anyhow!(
        "No writable FFmpeg install directory found. Tried: {}",
        failures.join(", ")
    ))
}

fn ensure_writable_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| {
        format!(
            "Failed to create FFmpeg install directory {}",
            dir.display()
        )
    })?;

    let probe_path = dir.join(format!(".write-test-{}.tmp", std::process::id()));
    std::fs::write(&probe_path, b"write test").with_context(|| {
        format!(
            "Failed to write to FFmpeg install directory {}",
            dir.display()
        )
    })?;
    let _ = std::fs::remove_file(probe_path);

    Ok(())
}

fn check_ffmpeg_tools_available() -> bool {
    command_available("ffmpeg") && command_available("ffprobe")
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ffmpeg_tools_exist(bin_dir: &Path) -> bool {
    bin_dir.join("ffmpeg.exe").exists() && bin_dir.join("ffprobe.exe").exists()
}

async fn download_and_install(install_dir: &Path) -> Result<()> {
    let temp_dir = tempfile::Builder::new().prefix("ffmpeg_dl").tempdir()?;
    let zip_path = temp_dir.path().join("ffmpeg.zip");

    info!("Downloading FFmpeg from {}...", FFMPEG_URL);
    download_file(FFMPEG_URL, &zip_path)
        .await
        .context("Failed to download zip")?;

    info!("Downloading SHA256 checksum...");
    let sha_path = temp_dir.path().join("ffmpeg.zip.sha256");
    download_file(SHA256_URL, &sha_path)
        .await
        .context("Failed to download sha256")?;

    info!("Verifying checksum...");
    verify_checksum(&zip_path, &sha_path).context("Checksum verification failed")?;

    info!("Extracting...");
    extract_ffmpeg(&zip_path, install_dir)?;

    Ok(())
}

async fn download_file(url: &str, target_path: &Path) -> Result<()> {
    let response = reqwest::get(url).await?.error_for_status()?;
    let content = response.bytes().await?;
    std::fs::write(target_path, content)?;
    Ok(())
}

fn verify_checksum(file_path: &Path, sha_path: &Path) -> Result<()> {
    let expected_hash_str = std::fs::read_to_string(sha_path)?;
    // Format is usually "HASH  filename" or just "HASH"
    let expected_hash = expected_hash_str
        .split_whitespace()
        .next()
        .context("Empty SHA256 file")?;

    let mut file = std::fs::File::open(file_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    let result = hasher.finalize();
    let calculated_hash = hex::encode(result);

    if calculated_hash.eq_ignore_ascii_case(expected_hash) {
        info!("Checksum matched: {}", calculated_hash);
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Checksum mismatch! Expected: {}, Calculated: {}",
            expected_hash,
            calculated_hash
        ))
    }
}

fn extract_ffmpeg(zip_path: &Path, target_dir: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    std::fs::create_dir_all(target_dir)?;

    let mut extracted_ffmpeg = false;
    let mut extracted_ffprobe = false;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_owned();

        // We only care about bin/ffmpeg.exe and bin/ffprobe.exe
        if name.ends_with("bin/ffmpeg.exe") || name.ends_with("bin/ffprobe.exe") {
            let dest_path = target_dir
                .join("bin")
                .join(Path::new(&name).file_name().unwrap());
            std::fs::create_dir_all(dest_path.parent().unwrap())?;
            let mut outfile = std::fs::File::create(&dest_path)?;
            std::io::copy(&mut file, &mut outfile)?;

            if name.ends_with("bin/ffmpeg.exe") {
                extracted_ffmpeg = true;
            } else if name.ends_with("bin/ffprobe.exe") {
                extracted_ffprobe = true;
            }
        }
    }

    if extracted_ffmpeg && extracted_ffprobe {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "FFmpeg archive did not contain required binaries: ffmpeg={}, ffprobe={}",
            extracted_ffmpeg,
            extracted_ffprobe
        ))
    }
}

fn add_to_path(path: &Path) {
    let new_path_os = path.as_os_str();
    if let Ok(current_path) = std::env::var("PATH") {
        // Simple append, better join management would be nice but this suffices
        let mut paths = std::env::split_paths(&current_path).collect::<Vec<_>>();
        paths.push(path.to_path_buf());
        if let Ok(new_path) = std::env::join_paths(paths) {
            unsafe {
                std::env::set_var("PATH", new_path);
            }
            info!("Added {:?} to process PATH", path);
        }
    } else {
        unsafe {
            std::env::set_var("PATH", new_path_os);
        }
    }
}
