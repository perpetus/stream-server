use super::{ArchiveEntry, ArchiveReader, AsyncSeekableReader, CacheConfig, cache::ProgressiveCache};
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use async_sevenz::SevenZ;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct SevenZHandler {
    path: Option<PathBuf>,
    _reader: Arc<Mutex<Option<Box<dyn AsyncSeekableReader>>>>,
    cache_config: CacheConfig,
}

impl SevenZHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path), _reader: Arc::new(Mutex::new(None)), cache_config: CacheConfig::default() }
    }
    
    pub fn new_with_config(path: PathBuf, cache_config: CacheConfig) -> Self {
        Self { path: Some(path), _reader: Arc::new(Mutex::new(None)), cache_config }
    }
    
    pub fn new_with_reader(reader: Box<dyn AsyncSeekableReader>) -> Self {
        Self { path: None, _reader: Arc::new(Mutex::new(Some(reader))), cache_config: CacheConfig::default() }
    }
}


#[async_trait::async_trait]
impl ArchiveReader for SevenZHandler {
    async fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let archive = if let Some(path) = &self.path {
            let file = tokio::fs::File::open(path).await?;
             // async-sevenz expects Unpin + AsyncRead + AsyncSeek
             // tokio::fs::File implements AsyncRead/Seek but not Unpin automatically? 
             // compat wrapper usually works. 
             // compat wrapper usually works. 
             // But my open() in lib.rs takes R: AsyncRead + AsyncSeek + Unpin
             // File fits.
             // Wait, compat gives futures::io traits. async-sevenz uses tokio::io traits.
             // So I should pass tokio::fs::File directly if it satisfies traits.
             // async-sevenz::SevenZ::open calls internal logic.
             // But SevenZ::open takes generic R.
             // Let's check imports in sevenz.rs of bindings.
             
             // In bindings/async-sevenz/lib.rs:
             // use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
             
             // tokio::fs::File implements tokio::io::AsyncRead/Seek.
             // So generic R = tokio::fs::File should work.
             
            SevenZ::open(file).await.map_err(|e| anyhow!("Failed to open 7z: {}", e))?
        } else {
             // Generic Stream
             // We can't clone the reader easily if it's a Box<dyn>.
             // If list_files is called, we might consume the reader if we are not careful.
             // Re-using the logic from generic stream is hard if we only have one-shot reader.
             // But usually list_files is called first.
             // If we lock and take it, we can't put it back easily unless we design SevenZ to return underlying reader.
             // For now, return error as generic stream usually uses `new_with_reader` for creating handler,
             // then calls list_files.
             
             // If we take the reader, we consume it for this operation.
             // Then subsequent open_file will fail.
             // We need a way to share the reader or clone the source.
             // Since reader is Box<dyn AsyncSeekableReader>, maybe we can't clone.
             // But `SevenZ` likely keeps ownership.
             
             // For now, let's implement listing for file path only, as that is the main use case for `stream-server`.
             return Err(anyhow!("Listing files from generic stream not supported yet"));
        };
        
        let entries = archive.entries().await.map_err(|e| anyhow!("Failed to list entries: {}", e))?;
        
        Ok(entries.into_iter().map(|e| ArchiveEntry {
            path: e.name,
            size: e.size,
            is_dir: e.is_dir,
        }).collect())
    }

    async fn open_file(&self, path: &str) -> Result<Box<dyn AsyncSeekableReader>> {
        let path_string = path.to_string();
        
        // We need to re-open the archive for extraction because SevenZ struct consumes the reader/is bound to it.
        // And we need a separate reader for parallel access if possible, or just new access.
        
        if let Some(p) = &self.path {
            let archive_path = p.clone();
            
            // First, get the file size by opening archive and finding the entry
            let file = tokio::fs::File::open(&archive_path).await?;
            let archive = SevenZ::open(file).await.map_err(|e| anyhow!("Failed to open 7z: {}", e))?;
            
            // Find index and size
            let index = archive.get_entry_index(&path_string).await.map_err(|e| anyhow!("Failed to find file: {}", e))?;
            let idx = index.ok_or_else(|| anyhow!("File not found in archive: {}", path_string))?;
            let file_size = archive.get_entry_size(idx).await.map_err(|e| anyhow!("Failed to get file size: {}", e))?;
            
            // Create cache in the configured cache directory
            let cache_dir = self.cache_config.get_dir_or_temp();
            let (cache, writer) = ProgressiveCache::new_in_dir(&cache_dir, Some(file_size)).await?;


            // Spawn extraction task
            tokio::spawn(async move {
                let res = (async || -> Result<()> {
                    // Re-open archive for extraction (the previous one was just for metadata)
                    let file = tokio::fs::File::open(&archive_path).await?;
                    let archive = SevenZ::open(file).await.map_err(|e| anyhow!("Failed to open 7z: {}", e))?;
                    
                    let extract_writer = writer.try_clone_sync().map_err(|e| anyhow!("Clone failed: {}", e))?;
                    let controller = writer.try_clone_sync().map_err(|e| anyhow!("Clone failed: {}", e))?;
                    
                    if let Err(e) = archive.extract(idx, extract_writer).await {
                         controller.set_error(e.to_string());
                    } else {
                         controller.finish();
                    }
                    
                    Ok(())
                })().await;
                
                if let Err(e) = res {
                    tracing::error!("7z extraction failed: {}", e);
                }
            });
            
            let reader = cache.reader().await?;
            Ok(Box::new(reader))
        } else {
            // Stream-based access not yet supported
            Err(anyhow!("Streaming 7z from remote source not supported yet"))
        }
    }
}
