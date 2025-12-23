pub mod parser;
pub mod nntp;
pub mod session;
pub mod stream;

use crate::archives::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::path::PathBuf;

pub struct NzbHandler {
    path: PathBuf,
}

impl NzbHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

// NOTE: This is the ArchiveReader implementation for "local nzb files" (if any).
// The HTTP payload creates a "virtual" session which might use a different struct.
// But for consistency, `get_archive_reader` returns this.

impl ArchiveReader for NzbHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        // Read file content
        let content = std::fs::read_to_string(&self.path)?;
        let nzb = parser::parse_nzb_xml(&content)?;
        
        // Convert nzb files to archive entries
        let entries = nzb.files.into_iter().map(|f| {
            // Determine filename from subject? 
            // Subject often looks like: "Category - Some.File.Name.mkv yEnc" or "File.Name.rar (1/10)"
            // For now, use subject as path, or try to parse generic filename.
            ArchiveEntry {
                path: f.subject.clone(), // TODO: subject parsing
                size: f.segments.segments.iter().map(|s| s.bytes).sum(),
                is_dir: false,
            }
        }).collect();
        
        Ok(entries)
    }

    fn open_file(&self, _path: &str) -> Result<Box<dyn crate::archives::SeekableReader>> {
        Err(anyhow!("Direct NZB file streaming from disk not fully implemented (requires NNTP connection details)"))
    }
}
