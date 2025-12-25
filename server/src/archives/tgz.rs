use super::{ArchiveEntry, ArchiveReader, AsyncSeekableReader, cache::ProgressiveCache};
use anyhow::{anyhow, Result};
use std::path::PathBuf;

use flate2::read::GzDecoder;

pub struct TgzHandler {
    path: PathBuf,
}

impl TgzHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait::async_trait]
impl ArchiveReader for TgzHandler {
    async fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path)?;
            let tar = GzDecoder::new(file);
            let mut archive = tar::Archive::new(tar);
            let mut entries = Vec::new();
            
            for file in archive.entries()? {
                let file = file?;
                entries.push(ArchiveEntry {
                    path: file.path()?.to_string_lossy().to_string(),
                    size: file.size(),
                    is_dir: file.header().entry_type().is_dir(),
                });
            }
            Ok(entries)
        }).await?
    }

    async fn open_file(&self, path: &str) -> Result<Box<dyn AsyncSeekableReader>> {
        // TGZ requires decompression. Random access is impossible without decompressing up to that point.
        // We use ProgressiveCache + spawn_blocking to decompress directly to cache.
        // This supports seeking on result.
        
        let path_clone = self.path.clone();
        let target_path = path.to_string();
        
        // We don't know the file size unless we scanned it in list_files OR we scan now.
        // For efficiency, we scan and extract in one go.
        // But we need to know size for Cache if possible? 
        // Cache works without total_size (Video player might dislike unknown duration, but it works).
        
        // If we want size, we rely on list_files info?
        // Let's assume size unknown for now or implement "scan header first".
        // `tar` entries have size in header!
        // So we will find the header, get size, create cache, then extract.
        
        // Optimization: Create cache immediately with None, start thread.
        // When thread finds header, it can update total_size? 
        // ProgressiveCache `new` takes explicit size.
        // We can just use `None` size.
        
        let (cache, writer) = ProgressiveCache::new(None).await?;
        
        std::thread::spawn(move || {
            let res = (|| -> Result<()> {
                let file = std::fs::File::open(&path_clone)?;
                let tar = GzDecoder::new(file);
                let mut archive = tar::Archive::new(tar);
                
                let mut found = false;
                
                // Clone sync writer
                let mut sync_writer = writer.try_clone_sync().map_err(|e| anyhow!("Failed to clone writer: {}", e))?;
                
                // We iterate. This is slow for large TGZ but it's the only way.
                for entry in archive.entries()? {
                    let mut entry = entry?;
                    if entry.path()?.to_string_lossy() == target_path {
                        found = true;
                        // Extract to writer
                        std::io::copy(&mut entry, &mut sync_writer)?;
                        break;
                    }
                }
                
                if !found {
                    return Err(anyhow!("File not found in TGZ"));
                }
                Ok(())
            })();
            
            if let Err(e) = res {
                writer.set_error(e.to_string());
            } else {
                writer.finish();
            }
        });

        Ok(Box::new(cache.reader().await?))
    }
}
