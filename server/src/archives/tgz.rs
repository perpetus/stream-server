use super::{ArchiveEntry, ArchiveReader};
use anyhow::Result;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver};
use tar::Archive;

pub struct TgzHandler {
    path: PathBuf,
}

impl TgzHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ArchiveReader for TgzHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);
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
        Ok(Box::new(TgzEntryReader::new(self.path.clone(), path.to_string())?))
    }
}

struct TgzEntryReader {
    receiver: Receiver<Result<Vec<u8>, Option<std::io::Error>>>,
    buffer: Vec<u8>,
    pos: usize,
}

impl TgzEntryReader {
    fn new(path: PathBuf, target_path: String) -> Result<Self> {
        // Use a sync channel to provide backpressure
        let (sender, receiver) = sync_channel(4); // 4 chunks buffer
        
        std::thread::spawn(move || {
            let res = (|| -> Result<()> {
                let file = File::open(&path)?;
                let decoder = GzDecoder::new(file);
                let mut archive = Archive::new(decoder);
                
                for entry in archive.entries()? {
                    let mut entry = entry?;
                    if entry.path()?.to_string_lossy() == target_path {
                         let mut buf = [0u8; 64 * 1024]; // 64KB chunks
                         loop {
                             match entry.read(&mut buf) {
                                 Ok(0) => break, // EOF
                                 Ok(n) => {
                                     if sender.send(Ok(buf[..n].to_vec())).is_err() {
                                         return Ok(()); // Receiver dropped
                                     }
                                 },
                                 Err(e) => {
                                     let _ = sender.send(Err(Some(e)));
                                     return Ok(());
                                 }
                             }
                         }
                         return Ok(());
                    }
                }
                let _ = sender.send(Err(Some(std::io::Error::new(std::io::ErrorKind::NotFound, "File not found"))));
                Ok(())
            })();
            
            if let Err(e) = res {
                tracing::error!("TGZ thread error: {}", e);
            }
        });
        
        Ok(Self { receiver, buffer: Vec::new(), pos: 0 })
    }
}

impl Read for TgzEntryReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // If we have data in buffer, return it
        if self.pos < self.buffer.len() {
            let len = (self.buffer.len() - self.pos).min(buf.len());
            buf[..len].copy_from_slice(&self.buffer[self.pos..self.pos+len]);
            self.pos += len;
            return Ok(len);
        }
        
        // Else fetch new chunk
        match self.receiver.recv() {
            Ok(Ok(data)) => {
                if data.is_empty() {
                    return Ok(0); // EOF
                }
                self.buffer = data;
                self.pos = 0;
                // Recurse to copy data
                self.read(buf)
            },
            Ok(Err(Some(e))) => Err(e),
            Ok(Err(None)) => Err(std::io::Error::new(std::io::ErrorKind::Other, "Unknown error from thread")),
            Err(_) => Ok(0), // Sender closed, EOF
        }
    }
}

impl std::io::Seek for TgzEntryReader {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "Seeking not supported for TGZ streams"))
    }
}
