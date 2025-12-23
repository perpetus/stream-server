use super::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::path::PathBuf;

use unrar::Archive;

pub struct RarHandler {
    _path: PathBuf,
}

impl RarHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { _path: path }
    }
}

impl ArchiveReader for RarHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        // unrar crate usage
        let mut archive = Archive::new(&self._path).open_for_processing()?;
        let mut entries = Vec::new();
        
        while let Some(header) = archive.read_header()? {
             let entry = header.entry();
             entries.push(ArchiveEntry {
                 path: entry.filename.to_string_lossy().to_string(),
                 size: entry.unpacked_size as u64,
                 is_dir: entry.is_directory(),
             });
             archive = header.skip()?;
        }
        Ok(entries)
    }

    fn open_file(&self, _path: &str) -> Result<Box<dyn super::SeekableReader>> {
        // RAR streaming requires unrar 0.5+ cursor support or pipe, which is complex.
        // For now, we return error until implemented properly to avoid build errors.
        Err(anyhow!("RAR streaming not yet implemented")) 
    }
}
