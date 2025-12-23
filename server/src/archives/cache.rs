use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::path::PathBuf;
use tempfile::NamedTempFile;

/// Shared state between the Extractor (Writer) and Player (Reader)
struct SharedState {
    written_bytes: u64,
    total_size: Option<u64>, // Known if derived from archive header
    is_complete: bool,
    error: Option<String>,
}

/// The core cache controller
#[derive(Clone)]
pub struct ProgressiveCache {
    state: Arc<(Mutex<SharedState>, Condvar)>,
    temp_path: PathBuf,
    // Keep the temp file struct alive as long as Cache exists to prevent early deletion
    _temp_file_handle: Arc<NamedTempFile>, 
}

impl ProgressiveCache {
    pub fn new(total_size: Option<u64>) -> io::Result<(Self, CacheWriter)> {
        let temp_file = NamedTempFile::new()?;
        let temp_path = temp_file.path().to_path_buf();
        let handle = Arc::new(temp_file);

        let state = Arc::new((
            Mutex::new(SharedState {
                written_bytes: 0,
                total_size,
                is_complete: false,
                error: None,
            }),
            Condvar::new(),
        ));

        let cache = ValidatedCache {
            _state: state.clone(),
            _temp_path: temp_path.clone(),
            _temp_file_handle: handle,
        };

        let writer = CacheWriter {
            state: state.clone(),
            file: OpenOptions::new().write(true).open(&temp_path)?,
        };

        // Return a wrapper that can spawn readers
        Ok((
            ProgressiveCache { 
                state, 
                temp_path, 
                _temp_file_handle: cache._temp_file_handle 
            }, 
            writer
        ))
    }

    pub fn reader(&self) -> io::Result<ProgressiveReader> {
        let file = File::open(&self.temp_path)?;
        Ok(ProgressiveReader {
            state: self.state.clone(),
            file,
            pos: 0,
        })
    }
}

// Inner struct just for holding the Arc logic clearly
struct ValidatedCache {
    _state: Arc<(Mutex<SharedState>, Condvar)>,
    _temp_path: PathBuf,
    _temp_file_handle: Arc<NamedTempFile>,
}

/// Writer used by the background extraction task
pub struct CacheWriter {
    state: Arc<(Mutex<SharedState>, Condvar)>,
    file: File,
}

impl Write for CacheWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.file.write(buf)?;
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.written_bytes += written as u64;
        cvar.notify_all();
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl CacheWriter {
    pub fn finish(&mut self) {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.is_complete = true;
        cvar.notify_all();
    }

    pub fn set_error(&mut self, err: String) {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.error = Some(err);
        state.is_complete = true; // Stop readers
        cvar.notify_all();
    }
}

/// Reader returned to the client/player
pub struct ProgressiveReader {
    state: Arc<(Mutex<SharedState>, Condvar)>,
    file: File,
    pos: u64,
}

impl Read for ProgressiveReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().unwrap();

        loop {
            if let Some(err) = &state.error {
                return Err(io::Error::new(io::ErrorKind::Other, err.clone()));
            }

            // If we have data available to read at current pos
            if self.pos < state.written_bytes {
                // Determine how much we can read
                let available = state.written_bytes - self.pos;
                let max_read = std::cmp::min(buf.len() as u64, available);
                
                // Unlock before IO to avoid blocking the writer
                drop(state);

                // Seek and Read
                self.file.seek(SeekFrom::Start(self.pos))?;
                let bytes_read = self.file.read(&mut buf[0..max_read as usize])?;
                self.pos += bytes_read as u64;
                return Ok(bytes_read);
            }

            // If extraction is done and we are at the end
            if state.is_complete {
                return Ok(0); // EOF
            }

            // Otherwise, wait for more data
            state = cvar.wait(state).unwrap();
        }
    }
}

impl Seek for ProgressiveReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (lock, _) = &*self.state;
        let state = lock.lock().unwrap();

        let new_pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(p) => {
                // If we know total size, we can calculate end. 
                // However, for compressed streams, total_size should be extracted from header.
                if let Some(total) = state.total_size {
                     if p < 0 {
                         total.saturating_sub(p.abs() as u64)
                     } else {
                         total + p as u64
                     }
                } else {
                    // If unknown, we can only seek relative to *written*? No, standard Seek behaviour expects final file.
                    // If unrar header gave us unpacked_size, we use that.
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "SeekFrom::End requires known total size"));
                }
            },
            SeekFrom::Current(p) => {
                let current = self.pos as i64;
                if current + p < 0 {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid seek to negative position"));
                }
                (current + p) as u64
            }
        };

        // If seeking explicitly past written data, logic dictates we just set pos.
        // The next `read` call will block until that data exists.
        // We do strictly VALIDATE that it doesn't exceed total_size if known.
        if let Some(total) = state.total_size {
            if new_pos > total {
                 return Err(io::Error::new(io::ErrorKind::InvalidInput, "Seek past end of file"));
            }
        }

        self.pos = new_pos;
        Ok(new_pos)
    }
}
