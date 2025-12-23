use super::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::fs::File;
use std::io::{Read, BufReader};
use std::path::PathBuf;
use zip::ZipArchive;

pub struct ZipHandler {
    path: PathBuf,
}

impl ZipHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ArchiveReader for ZipHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        let mut entries = Vec::new();
        for i in 0..archive.len() {
            let file = archive.by_index(i)?;
            entries.push(ArchiveEntry {
                path: file.name().to_string(),
                size: file.size(),
                is_dir: file.is_dir(),
            });
        }
        Ok(entries)
    }

    fn open_file(&self, path: &str) -> Result<Box<dyn super::SeekableReader>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        // Find the file by name to check compression and offsets
        let zip_file = archive.by_name(path)?;
        
        // 1. Uncompressed (Stored) - Use RawFileSlice for zero-copy streaming
        if zip_file.compression() == zip::CompressionMethod::Stored {
             let offset = zip_file.data_start();
             let size = zip_file.size();
             drop(zip_file); // release borrow
             drop(archive); // release file
             
             return Ok(Box::new(RawFileSlice::new(self.path.clone(), offset, size)?));
        }
        
        // 2. Compressed (Deflated) - Use Progressive Extraction Cache
        if zip_file.compression() == zip::CompressionMethod::Deflated {
             let size = zip_file.size();
             drop(zip_file); // Release borrow so we can move archive to thread
             drop(archive);  // Drop validation handle

             // Create Cache
             let (cache, mut writer) = super::cache::ProgressiveCache::new(Some(size))?;
             let path_string = path.to_string();
             let msg_path = self.path.clone();

             // Spawn Extraction Thread
             // We need to re-open the archive in the thread because we can't easily move the ZipArchive 
             // with the borrow checker unless we handle it very carefully.
             // Simpler to just pass the path and look up again.
             let archive_path = self.path.clone();
             
             std::thread::spawn(move || {
                 // Re-open archive
                 let open_res = (|| -> Result<()> {
                     let file = File::open(&archive_path)?;
                     let reader = BufReader::new(file);
                     let mut archive = ZipArchive::new(reader)?;
                     let mut zip_file = archive.by_name(&path_string)?;
                     
                     // Stream extraction to cache writer
                     std::io::copy(&mut zip_file, &mut writer)?;
                     Ok(())
                 })();

                 if let Err(e) = open_res {
                     tracing::error!("ZIP extraction failed for {:?}: {}", msg_path, e);
                     writer.set_error(e.to_string());
                 } else {
                     writer.finish();
                 }
             });

             // Return the reader handle immediately
             return Ok(Box::new(cache.reader()?));
        }

        Err(anyhow!("Unsupported compression method (only Stored and Deflated supported)"))
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
        let read_bytes = self.file.read(&mut buf[0..buf_len])?;
        self.pos += read_bytes as u64;
        Ok(read_bytes)
    }
}

impl std::io::Seek for RawFileSlice {
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
             // Clamping is safer for slice reading or erroring? 
             // Standard behavior usually allows seeking past end, but reading returns 0.
             // We'll allow it but read will handle EOF.
        }

        // Physical seek
        self.file.seek(SeekFrom::Start(self.offset + new_pos))?;
        self.pos = new_pos;
        Ok(new_pos)
    }
}
