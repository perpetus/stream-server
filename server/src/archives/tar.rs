use super::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::PathBuf;
use tar::Archive;

pub struct TarHandler {
    path: PathBuf,
}

impl TarHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ArchiveReader for TarHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let mut archive = Archive::new(file);
        let mut entries = Vec::new();

        for file in archive.entries()? {
            let file = file?;
            let path = file.path()?.to_string_lossy().to_string();
            let size = file.size();
            let is_dir = file.header().entry_type().is_dir();

            entries.push(ArchiveEntry {
                path,
                size,
                is_dir,
            });
        }
        Ok(entries)
    }

    fn open_file(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let file = File::open(&self.path)?;
        let mut archive = Archive::new(file);
        
        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()?.to_string_lossy() == path {
                let offset = entry.raw_file_position();
                let size = entry.size();
                return Ok(Box::new(RawFileSlice::new(self.path.clone(), offset, size)?));
            }
        }
        
        Err(anyhow!("File not found in TAR archive"))
    }
}

struct RawFileSlice {
    file: File,
    remaining: u64,
}

impl RawFileSlice {
    fn new(path: PathBuf, offset: u64, size: u64) -> Result<Self> {
        let mut file = File::open(path)?;
        file.seek(std::io::SeekFrom::Start(offset))?;
        Ok(Self { file, remaining: size })
    }
}

impl Read for RawFileSlice {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let max_read = buf.len().min(self.remaining as usize);
        let read_bytes = self.file.read(&mut buf[0..max_read])?;
        self.remaining -= read_bytes as u64;
        Ok(read_bytes)
    }
}

