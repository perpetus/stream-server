use crate::files::FileStreamTrait;
use moka::future::Cache;
use std::future::Future;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio::sync::Mutex;

const CHUNK_SIZE: u64 = 128 * 1024; // 128KB chunks

pub type DataCache = Cache<(usize, u64), Arc<Vec<u8>>>;
type ReadFuture = Pin<Box<dyn Future<Output = io::Result<Arc<Vec<u8>>>> + Send>>;

struct SyncFuture(ReadFuture);
// SAFETY: We only access the future via &mut self, so no concurrent access is possible.
// The future itself is Send, which is sufficient if we don't share it.
unsafe impl Sync for SyncFuture {}

pub struct CachedStream {
    inner: Arc<Mutex<Box<dyn FileStreamTrait>>>,
    cache: DataCache,
    file_index: usize,
    position: u64,
    file_length: u64,
    current_future: Option<SyncFuture>,
}

impl CachedStream {
    pub fn new(
        inner: Box<dyn FileStreamTrait>,
        cache: DataCache,
        file_index: usize,
        file_length: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            cache,
            file_index,
            position: 0,
            file_length,
            current_future: None,
        }
    }
}

impl AsyncSeek for CachedStream {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let new_pos = match position {
            SeekFrom::Start(pos) => pos,
            SeekFrom::Current(diff) => (self.position as i64 + diff) as u64,
            SeekFrom::End(diff) => (self.file_length as i64 + diff) as u64,
        };
        self.position = new_pos;
        // Invalidate current read future if we seek?
        // Actually, if we seek, we might still be waiting for a block we don't need.
        // But for simplicity, we can drop the future.
        // However, `get_with` might still be running in background populating the cache, which is GOOD.
        // So simply dropping the future here to switch to a new read is fine.
        self.current_future = None;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.position))
    }
}

impl AsyncRead for CachedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            // Check if we are done
            if self.position >= self.file_length {
                return Poll::Ready(Ok(()));
            }

            // If we have a pending future, poll it
            // If we have a pending future, poll it
            if let Some(wrapper) = self.current_future.as_mut() {
                match wrapper.0.as_mut().poll(cx) {
                    Poll::Ready(result) => {
                        self.current_future = None; // clear future
                        match result {
                            Ok(data) => {
                                // We have the chunk data. Copy valid part to buf.
                                let chunk_index = self.position / CHUNK_SIZE;
                                let chunk_start = chunk_index * CHUNK_SIZE;
                                let chunk_offset = (self.position - chunk_start) as usize;

                                if chunk_offset >= data.len() {
                                    // Should not happen if logic is correct unless file grew or something weird?
                                    // Or maybe we are at EOF of this chunk?
                                    // Actually, if we sought past the data length of this chunk, we move to next chunk.
                                    // But loop logic below should handle that.
                                    // Just strictly restart loop to recalculate chunk_index/offsets.
                                    continue;
                                }

                                let available = data.len() - chunk_offset;
                                let to_read = std::cmp::min(available, buf.remaining());
                                buf.put_slice(&data[chunk_offset..chunk_offset + to_read]);
                                self.position += to_read as u64;
                                return Poll::Ready(Ok(()));
                            }
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // No future, create one
            let chunk_index = self.position / CHUNK_SIZE;
            let key = (self.file_index, chunk_index);
            let chunk_start = chunk_index * CHUNK_SIZE;

            let cache = self.cache.clone();
            let inner = self.inner.clone();
            let _file_length = self.file_length; // keep just in case or remove? It's unused. remove block capture.

            let fut = async move {
                cache
                    .try_get_with(key, async move {
                        let mut stream = inner.lock().await;

                        // Seek to chunk start by using the inner stream's seek
                        use tokio::io::AsyncReadExt;
                        use tokio::io::AsyncSeekExt;

                        // We need to re-seek deeply because other consumers (or previous reads) might have moved it
                        stream
                            .seek(SeekFrom::Start(chunk_start))
                            .await
                            .map_err(|e| std::sync::Arc::new(e))?;

                        let mut buffer = Vec::with_capacity(CHUNK_SIZE as usize);
                        // Take from mutable reference to avoid moving out of MutexGuard
                        let mut take = (&mut *stream).take(CHUNK_SIZE);
                        take.read_to_end(&mut buffer)
                            .await
                            .map_err(|e| std::sync::Arc::new(e))?;

                        let res: Result<Arc<Vec<u8>>, std::sync::Arc<io::Error>> =
                            Ok(Arc::<Vec<u8>>::new(buffer));
                        res
                    })
                    .await
                    .map_err(|e| {
                        // Unwrap the Arc error or create generic io error
                        // Moka error is Arc<E>.
                        io::Error::new(io::ErrorKind::Other, e.to_string())
                    })
            };

            self.current_future = Some(SyncFuture(Box::pin(fut)));
            // Loop continue to poll the new future immediately
        }
    }
}
