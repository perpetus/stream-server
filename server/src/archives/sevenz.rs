use super::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::io::Read;
use sevenz_rust::{SevenZReader, Password};
use std::fs::File;

pub struct SevenZHandler {
    path: PathBuf,
}

impl SevenZHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ArchiveReader for SevenZHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let mut reader = SevenZReader::new(file, self.path.extension().unwrap_or_default().len() as u64, Password::empty())?;
        let mut entries = Vec::new();
        
        // sevenz-rust usage depends on version, checking standard API
        // Assuming iteration or archive entries
        reader.for_each_entries(|entry, _| {
            entries.push(ArchiveEntry {
                path: entry.name().to_string(),
                size: entry.size(),
                is_dir: entry.is_directory(),
            });
            Ok(true)
        })?;
        
        Ok(entries)
    }

    fn open_file(&self, _path: &str) -> Result<Box<dyn Read + Send>> {
        // sevenz-rust doesn't support random access stream easily?
        // It's primarily for extraction.
        
        // Similar to RAR, we might need a workaround.
        Err(anyhow!("7z streaming not yet implemented."))
    }
}
