use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use tracing::{error, info};

const FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const SHA256_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.sha256";

/// Orchestrate the setup: check, download, verify, extract, configure path.
pub async fn setup_ffmpeg() -> Result<()> {
    if !cfg!(target_os = "windows") {
        info!("Not on Windows, skipping automatic FFmpeg download.");
        return Ok(());
    }

    if check_ffmpeg_availability() {
        info!("FFmpeg is already available in PATH.");
        return Ok(());
    }

    info!("FFmpeg not found. Attempting auto-download...");

    // Determine install location: ./tools/ffmpeg
    let install_dir = std::env::current_exe()?
        .parent()
        .context("Failed to get exe parent dir")?
        .join("tools")
        .join("ffmpeg");

    let bin_dir = install_dir.join("bin");

    // Check if we already installed it but it's just not in PATH
    if bin_dir.join("ffmpeg.exe").exists() {
        info!(
            "FFmpeg binaries found in {:?}, valid but not in PATH. Adding to PATH.",
            bin_dir
        );
        add_to_path(&bin_dir);
        return Ok(());
    }

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

fn check_ffmpeg_availability() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        }
    }
    Ok(())
}

fn add_to_path(path: &Path) {
    let new_path_os = path.as_os_str();
    if let Ok(current_path) = std::env::var("PATH") {
        // Simple append, better join management would be nice but this suffices
        let mut paths = std::env::split_paths(&current_path).collect::<Vec<_>>();
        paths.push(path.to_path_buf());
        if let Ok(new_path) = std::env::join_paths(paths) {
            std::env::set_var("PATH", new_path);
            info!("Added {:?} to process PATH", path);
        }
    } else {
        std::env::set_var("PATH", new_path_os);
    }
}
