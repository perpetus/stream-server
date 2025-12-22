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

    fn open_file(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        // ZipArchive requires mutable access to read a file
        // Since we need to return a Read object that owns the resource, 
        // we can't easily return a reference to a temporary archive.
        // We have to open a fresh archive handle for each read
        
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;
        
        // We need to return an object that owns the archive and implements Read
        // This is tricky because ZipFile borrows from ZipArchive.
        // We can create a self-referential struct or read the whole file into memory (bad for streaming).
        // OR: Since we are in local addon, maybe we just read chunks?
        
        // BETTER APPROACH: Return a struct that holds the File and the ZipArchive
        // But `zip` crate design makes this hard safely without `rental` or `ouroboros`.
        
        // FOR NOW: We will implement a wrapper that opens and copies? No, that's slow.
        // Wait! `ZipArchive` owns the Reader. `ZipFile` borrows `ZipArchive`.
        
        // If we can't return a Reader easily, maybe we change the Trait to return `Vec<u8>` or similar?
        // No, streaming needs a Reader.
        
        // Workaround: We find the offset in the ZIP file and return a SectionReader of the raw file?
        // Only works for STORE method. For DEFLATE, we need the decompressor.
        
        // Alternative: Use `ouroboros` or unsafe to hold the Archive.
        // OR: Just clone the file handle and read into a buffer?
        
        // Simplest valid Rust solution: Read the entire entry into memory if small? 
        // No, video files are GBs.
        
        // Let's iterate: Return a struct `ZipStream` which opens the file on creation.
        // But `ArchiveReader::open_file` returns `Box<dyn Read>`.
        
        // We can implement a struct `OwnedZipFile` that holds the `ZipArchive<BufReader<File>>` 
        // but `ZipArchive` doesn't expose `by_name` returning an owned struct.
        
        // CHECK: Does `zip` crate support owned file extraction?
        // "ZipFile lifetime is bound to ZipArchive".
        
        // HACK: We can just wrap the `ZipArchive` in a Mutex/Arc?
        // No, the `ZipFile` relies on the `ZipArchive` being alive.
        
        // Let's try finding the offset.
        let zip_file = archive.by_name(path)?;
        if zip_file.compression() == zip::CompressionMethod::Stored {
             let offset = zip_file.data_start();
             let size = zip_file.size();
             drop(zip_file); // release borrow
             drop(archive); // release file
             
             // Open independent handle for raw reading
             // Logic to read raw slice of file
             return Ok(Box::new(RawFileSlice::new(self.path.clone(), offset, size)?));
        }
        
        // For compressed files (Deflate), we are stuck unless we use a different library or `ouroboros`.
        // Most video files in ZIPs are STORED (0 compression) anyway because they are already compressed.
        // If they ARE compressed, we might just fail format support or read into memory (if < 100MB).
        
        Err(anyhow!("Streaming compressed ZIP entries not yet fully supported (only Stored)."))
    }
}

#[allow(dead_code)]
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
