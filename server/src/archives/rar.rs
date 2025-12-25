use super::{ArchiveEntry, ArchiveReader, AsyncSeekableReader, CacheConfig, cache::ProgressiveCache};
use anyhow::Result;
use std::path::PathBuf;
use async_rar::AsyncRarArchive;
use tokio::io::AsyncWriteExt; // Import AsyncWriteExt for write_all

pub struct RarHandler {
    path: PathBuf,
    cache_config: CacheConfig,
}

impl RarHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path, cache_config: CacheConfig::default() }
    }
    
    pub fn new_with_config(path: PathBuf, cache_config: CacheConfig) -> Self {
        Self { path, cache_config }
    }
}

#[async_trait::async_trait]
impl ArchiveReader for RarHandler {
    async fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let mut archive = AsyncRarArchive::open(&self.path).await?;
        let entries = archive.list_entries().await?;
        
        Ok(entries.into_iter().map(|e| ArchiveEntry {
            path: e.name,
            size: e.size,
            is_dir: e.is_directory,
        }).collect())
    }

    async fn open_file(&self, path: &str) -> Result<Box<dyn AsyncSeekableReader>> {
        let archive_path = self.path.clone();
        let path_string = path.to_string();
        
        // First, get the file size by listing entries
        let mut archive = AsyncRarArchive::open(&self.path).await?;
        let entries = archive.list_entries().await?;
        let file_size = entries
            .iter()
            .find(|e| e.name == path_string)
            .map(|e| e.size)
            .ok_or_else(|| anyhow::anyhow!("File not found in archive: {}", path_string))?;
        
        // Create cache in the configured cache directory
        let cache_dir = self.cache_config.get_dir_or_temp();
        let (cache, mut writer) = ProgressiveCache::new_in_dir(&cache_dir, Some(file_size)).await?;
        
        tokio::spawn(async move {
            let res = async {
                let mut archive = AsyncRarArchive::open(&archive_path).await?;
                let mut stream = archive.stream_entry(path_string).await?;
                
                while let Some(chunk) = stream.recv().await {
                    writer.write_all(&chunk).await?;
                }
                Ok::<_, anyhow::Error>(())
            }.await;

            if let Err(e) = res {
                writer.set_error(e.to_string());
            } else {
                writer.finish();
            }
        });

        Ok(Box::new(cache.reader().await?))
    }
}
