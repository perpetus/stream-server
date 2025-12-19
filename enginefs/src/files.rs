use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek}; // Added for Arc

pub struct FileHandle {
    pub size: u64,
    pub name: String,
    pub stream: Box<dyn FileStreamTrait>,
    pub engine: Arc<crate::engine::Engine>, // Added this field
}

pub trait FileStreamTrait: AsyncRead + AsyncSeek + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncSeek + Unpin + Send + Sync> FileStreamTrait for T {}

impl FileHandle {
    pub fn new(
        size: u64,
        name: String,
        stream: Box<dyn FileStreamTrait>,
        engine: Arc<crate::engine::Engine>,
    ) -> Self {
        Self {
            size,
            name,
            stream,
            engine,
        }
    }
}

impl AsyncRead for FileHandle {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
use std::sync::atomic::Ordering;

impl Drop for FileHandle {
    fn drop(&mut self) {
        self.engine.active_streams.fetch_sub(1, Ordering::SeqCst);
    }
}

impl AsyncSeek for FileHandle {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut self.stream).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut self.stream).poll_complete(cx)
    }
}
