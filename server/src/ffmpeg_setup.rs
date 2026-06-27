use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info, warn};

const JELLYFIN_FFMPEG_DIR_URL: &str = "https://repo.jellyfin.org/files/ffmpeg/windows/latest-7.x/win64/";
const POINTER_URL: &str = "https://repo.jellyfin.org/files/ffmpeg/windows/latest-7.x/win64/win64-clang-gpl.txt";

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
    
    info!("Fetching Jellyfin FFmpeg pointer from {}...", POINTER_URL);
    let pointer_response = reqwest::get(POINTER_URL).await?.error_for_status()?;
    let pointer_text = pointer_response.text().await?;
    let zip_filename = pointer_text.trim();
    if zip_filename.is_empty() || !zip_filename.ends_with(".zip") {
        return Err(anyhow::anyhow!("Invalid zip filename in pointer file: '{}'", zip_filename));
    }
    
    let zip_url = format!("{}{}", JELLYFIN_FFMPEG_DIR_URL, zip_filename);
    let zip_path = temp_dir.path().join("ffmpeg.zip");

    info!("Downloading Jellyfin FFmpeg from {}...", zip_url);
    download_file(&zip_url, &zip_path)
        .await
        .context("Failed to download zip")?;

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



fn extract_ffmpeg(zip_path: &Path, target_dir: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    std::fs::create_dir_all(target_dir)?;

    let mut extracted_ffmpeg = false;
    let mut extracted_ffprobe = false;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_owned();

        let path = Path::new(&name);
        if let Some(file_name) = path.file_name() {
            let file_name_str = file_name.to_string_lossy();
            
            // Check if it's an executable or library we want to extract
            let is_target_file = file_name_str == "ffmpeg.exe" 
                || file_name_str == "ffprobe.exe"
                || file_name_str.ends_with(".dll");

            // Or if it was inside a bin/ directory in the archive
            let is_in_bin = path.components().any(|c| c.as_os_str() == "bin");

            if (is_target_file || is_in_bin) && !file.is_dir() {
                let dest_path = target_dir.join("bin").join(file_name);
                std::fs::create_dir_all(dest_path.parent().unwrap())?;
                let mut outfile = std::fs::File::create(&dest_path)?;
                std::io::copy(&mut file, &mut outfile)?;

                if file_name_str == "ffmpeg.exe" {
                    extracted_ffmpeg = true;
                } else if file_name_str == "ffprobe.exe" {
                    extracted_ffprobe = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_download_and_install_jellyfin_ffmpeg() {
        let temp_dir = tempfile::tempdir().unwrap();
        let install_dir = temp_dir.path().join("ffmpeg_install");
        let result = download_and_install(&install_dir).await;
        assert!(result.is_ok(), "Failed to download and install: {:?}", result);
        
        let bin_dir = install_dir.join("bin");
        assert!(bin_dir.join("ffmpeg.exe").exists(), "ffmpeg.exe not found");
        assert!(bin_dir.join("ffprobe.exe").exists(), "ffprobe.exe not found");
    }
}
