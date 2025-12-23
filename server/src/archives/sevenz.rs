use super::{ArchiveEntry, ArchiveReader};
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use sevenz_rust::{SevenZReader, Password};
use std::fs::File;

pub struct SevenZHandler {
    path: PathBuf,
}

impl SevenZHandler {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ArchiveReader for SevenZHandler {
    fn list_files(&self) -> Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let mut reader = SevenZReader::new(file, self.path.extension().unwrap_or_default().len() as u64, Password::empty())?;
        let mut entries = Vec::new();
        
        // sevenz-rust usage depends on version, checking standard API
        // Assuming iteration or archive entries
        reader.for_each_entries(|entry, _| {
            entries.push(ArchiveEntry {
                path: entry.name().to_string(),
                size: entry.size(),
                is_dir: entry.is_directory(),
            });
            Ok(true)
        })?;
        
        Ok(entries)
    }

    fn open_file(&self, path: &str) -> Result<Box<dyn super::SeekableReader>> {
        // 1. Find entry to get size
        let entries = self.list_files()?;
        let entry_info = entries.iter().find(|e| e.path == path)
            .ok_or_else(|| anyhow!("File not found in 7z archive"))?;
        let entry_size = entry_info.size;
        let path_string = path.to_string();
        let archive_path = self.path.clone();

        // 2. Create Cache
        let (cache, mut writer) = super::cache::ProgressiveCache::new(Some(entry_size))?;

        // 3. Spawn Extraction Thread
        std::thread::spawn(move || {
            let res = (|| -> Result<()> {
                let file = File::open(&archive_path)?;
                let mut reader = SevenZReader::new(file, archive_path.extension().unwrap_or_default().len() as u64, Password::empty())?;
                
                reader.for_each_entries(|entry, reader| {
                    if entry.name() == path_string {
                        // Found it, stream to writer
                        std::io::copy(reader, &mut writer)?;
                        // Stop iteration
                        return Ok(false);
                    }
                    Ok(true)
                })?;
                Ok(())
            })();

            if let Err(e) = res {
                tracing::error!("7z extraction failed: {}", e);
                writer.set_error(e.to_string());
            } else {
                writer.finish();
            }
        });

        Ok(Box::new(cache.reader()?))
    }
}
