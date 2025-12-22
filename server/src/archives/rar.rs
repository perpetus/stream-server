use super::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::io::Read;
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

    fn open_file(&self, _path: &str) -> Result<Box<dyn Read + Send>> {
        // unrar crate allows reading entries as streams
        // We iterate headers until we find the file, then read it?
        // This is inefficient for random access but okay for one-time stream open.
        
        // Important: `unrar` crate might not support seeking easily.
        // But `ArchiveReader` returns `dyn Read`.
        
        let _archive = Archive::new(&self._path).open_for_processing()?;
        
        // We need to return a Reader that iterates until it finds the file and then reads?
        // The `unrar` API usually consumes the archive in a loop.
        // We might need to spawn a thread/channel or use a Cursor if we extract directly.
        // Streaming straight from RAR is HARD without a specialized library or blocking.
        
        // For Proof of Concept, let's assume we can get a Cursor by extracting to memory (if small)
        // OR pipe it.
        
        // Wait, `unrar` has `read_headers()` which returns `Process` object.
        // `Process` can `extract()` or `read()`.
        
        // Real implementation of seeking in RAR is non-trivial.
        // We'll return an error for now and focus on ZIP/7z or investigate `unrar` features properly later.
        
        // TODO: Implement RAR streaming properly.
        Err(anyhow!("RAR streaming not yet implemented (requires pipe logic)."))
    }
}
