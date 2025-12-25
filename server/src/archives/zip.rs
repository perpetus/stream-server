use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio::io::BufReader;
use super::{ArchiveEntry, ArchiveReader, AsyncSeekableReader, cache::ProgressiveCache};
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use async_zip::tokio::read::seek::ZipFileReader;
use tokio::fs::File;

use tokio::sync::Mutex;
use std::sync::Arc;

pub struct ZipHandler {
    path: Option<PathBuf>,
    // Wrap in Arc<Mutex<Option>> to allow taking it out in `open_file` (one-shot)
    reader: Arc<Mutex<Option<Box<dyn AsyncSeekableReader>>>>, 
}

impl ZipHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { 
            path: Some(path), 
            reader: Arc::new(Mutex::new(None)) 
        }
    }
    
    pub fn new_with_reader(reader: Box<dyn AsyncSeekableReader>) -> Self {
        Self { 
            path: None, 
            reader: Arc::new(Mutex::new(Some(reader))) 
        }
    }
}

#[async_trait::async_trait]
impl ArchiveReader for ZipHandler {
    async fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        // Limitation: list_files destroys the reader if we use `with_ident`?
        // `with_ident` takes ownership of reader.
        // We can't list files then open file with the same reader if it's a stream that can't be cloned.
        // BUT `stream_file` in `routes/archive.rs` calls `get_archive_reader` then `open_file`. It does NOT call `list_files`.
        // `list_files` is used by `list_archive_content` route which opens a NEW reader.
        
        let mut reader_guard = self.reader.lock().await;
        
        if let Some(path) = &self.path {
            // Local file: Open fresh
            let file = File::open(path).await?;
            let archive = ZipFileReader::new(BufReader::new(file).compat()).await?;
            let entries = archive.file().entries().iter().map(|e| ArchiveEntry {
                path: e.filename().as_str().unwrap_or_default().to_string(),
                size: e.uncompressed_size(),
                is_dir: e.dir().unwrap_or(false),
            }).collect();
            Ok(entries)
        } else if let Some(reader) = reader_guard.take() {
             // We TAKE the reader. It is consumed.
             // This works for "One Shot" listing.
             // But if we want to list then open? 
             // We can't put it back easily because `ZipFileReader` consumes it.
             // unless we use `into_inner()`?
             
             let archive = ZipFileReader::new(BufReader::new(reader).compat()).await?;
             let entries = archive.file().entries().iter().map(|e| ArchiveEntry {
                 path: e.filename().as_str().unwrap_or_default().to_string(),
                 size: e.uncompressed_size(),
                 is_dir: e.dir().unwrap_or(false),
             }).collect();
             
             // Put reader back? 
             // `archive.into_inner()`             // Recover reader
             let returned_reader = archive.into_inner().into_inner().into_inner();
             *reader_guard = Some(returned_reader);
             
             Ok(entries)
        } else {
             Err(anyhow!("No source available or already consumed"))
        }
    }

    async fn open_file(&self, path: &str) -> Result<Box<dyn AsyncSeekableReader>> {
        let mut reader_guard = self.reader.lock().await;
        
        let reader_box: Box<dyn AsyncSeekableReader> = if let Some(p) = &self.path {
            Box::new(File::open(p).await?)
        } else if let Some(r) = reader_guard.take() {
            r
        } else {
            return Err(anyhow!("Archive source already consumed"));
        };

        let mut archive = ZipFileReader::new(BufReader::new(reader_box).compat()).await?;
        
        let index = archive.file().entries().iter().position(|e| 
            e.filename().as_str().unwrap_or_default() == path
        ).ok_or(anyhow!("File not found in archive"))?;
        
        let entry = archive.file().entries().get(index).unwrap();
        let size = entry.uncompressed_size();
        
        // Use ProgressiveCache for robust seeking
        let (cache, mut writer) = ProgressiveCache::new(Some(size)).await?;
        
        tokio::spawn(async move {
            let entry_reader = archive.reader_with_entry(index).await;
            if let Ok(r) = entry_reader {
                 use tokio_util::compat::FuturesAsyncReadCompatExt;
                 let mut r = r.compat(); 
                 let copy_res = tokio::io::copy(&mut r, &mut writer).await;
                 if let Err(e) = copy_res {
                     writer.set_error(e.to_string());
                 } else {
                     writer.finish();
                 }
             } else {
                 writer.set_error("Failed to open entry".to_string());
             }
        });

        Ok(Box::new(cache.reader().await?))
    }
}
