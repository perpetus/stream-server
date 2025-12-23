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

    fn open_file(&self, path: &str) -> Result<Box<dyn super::SeekableReader>> {
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
    offset: u64,
    size: u64,
    pos: u64,
}

impl RawFileSlice {
    fn new(path: PathBuf, offset: u64, size: u64) -> Result<Self> {
        let mut file = File::open(path)?;
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(offset))?;
        Ok(Self { file, offset, size, pos: 0 })
    }
}

impl Read for RawFileSlice {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.size {
            return Ok(0);
        }
        let max_read = (self.size - self.pos) as usize;
        let buf_len = buf.len().min(max_read);
        
        // Ensure we are reading from the correct physical position
        // Since we share the file handle (or if seek moved it), we should seek before read if we want to be safe,
        // or assume we own the cursor.
        // But since we impl Seek, we might have moved the cursor.
        // Actually, let's just make sure.
        use std::io::Seek;
        self.file.seek(std::io::SeekFrom::Start(self.offset + self.pos))?;
        
        let read_bytes = self.file.read(&mut buf[0..buf_len])?;
        self.pos += read_bytes as u64;
        Ok(read_bytes)
    }
}

impl Seek for RawFileSlice {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let new_pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(p) => {
                if p < 0 {
                    self.size.saturating_sub(p.abs() as u64)
                } else {
                    self.size + p as u64
                }
            },
            SeekFrom::Current(p) => {
                 let current = self.pos as i64;
                 if current + p < 0 {
                     return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid seek"));
                 }
                 (current + p) as u64
            }
        };

        if new_pos > self.size {
             // Clamping? Or allow? Standard files allow holes.
             // We'll just allow it, read will return 0.
        }

        self.pos = new_pos;
        Ok(new_pos)
    }
}

