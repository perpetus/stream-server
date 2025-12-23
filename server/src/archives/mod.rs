use anyhow::Result;
use std::path::Path;


pub mod zip;
pub mod rar;
pub mod sevenz;
pub mod tar;
pub mod tgz;
pub mod nzb;
pub mod cache;

/// Represents a file inside an archive
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArchiveEntry {
    pub path: String,       // Internal path in archive
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArchiveSession {
    pub path: std::path::PathBuf,
    pub created: std::time::Instant,
}

/// Helper to identify if a path is a supported archive
pub fn is_archive(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let ext = ext.to_lowercase();
        // Check compound extension for .tar.gz
        if ext == "gz" {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if stem.to_lowercase().ends_with(".tar") {
                    return true;
                }
            }
        }
        return matches!(ext.as_str(), "zip" | "rar" | "7z" | "tar" | "tgz" | "nzb");
    }
    false
}

/// Helper to identify if a path is a split archive part (e.g. .part01.rar)
/// Returns (base_name, part_number) if it is.
#[allow(dead_code)]
pub fn is_split_archive_part(path: &Path) -> Option<(String, u32)> {
    if let Some(_file_name) = path.file_stem().and_then(|s| s.to_str()) {
         // Regex-like check for .partXXX or .rXX
         // Simple heuristic: ends with .part\d+
         // TODO: Implement robust split detection
    }
    None
}

/// Trait combining Read, Seek, and Send for trait objects
pub trait SeekableReader: std::io::Read + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Seek + Send> SeekableReader for T {}

/// Trait for Archive implementations
pub trait ArchiveReader: Send + Sync {
    /// List all files in the archive
    fn list_files(&self) -> Result<Vec<ArchiveEntry>>;

    /// Open a stream for a specific file inside the archive
    fn open_file(&self, path: &str) -> Result<Box<dyn SeekableReader>>;
}

pub fn get_archive_reader(path: &Path) -> Result<Box<dyn ArchiveReader>> {
    let path_str = path.to_string_lossy().to_lowercase();
    
    if path_str.ends_with(".zip") {
        Ok(Box::new(zip::ZipHandler::new(path.to_path_buf())))
    } else if path_str.ends_with(".rar") {
        Ok(Box::new(rar::RarHandler::new(path.to_path_buf())))
    } else if path_str.ends_with(".7z") {
        Ok(Box::new(sevenz::SevenZHandler::new(path.to_path_buf())))
    } else if path_str.ends_with(".tar") {
        Ok(Box::new(tar::TarHandler::new(path.to_path_buf())))
    } else if path_str.ends_with(".tar.gz") || path_str.ends_with(".tgz") {
        Ok(Box::new(tgz::TgzHandler::new(path.to_path_buf())))
    } else if path_str.ends_with(".nzb") {
        Ok(Box::new(nzb::NzbHandler::new(path.to_path_buf())))
    } else {
        Err(anyhow::anyhow!("Unsupported archive type: {:?}", path.extension()))
    }
}
