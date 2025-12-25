#![allow(dead_code)]
use std::io::{Read, Seek, SeekFrom, Result as IoResult, Error, ErrorKind};
use tokio::sync::oneshot;

/// Commands sent from the Sync world to the Async world
enum BridgeCommand {
    Read(usize),      // Request N bytes
    Seek(SeekFrom),   // Request Seek
    Shutdown,
}

/// Responses sent from Async world to Sync world
enum BridgeResponse {
    Read(IoResult<Vec<u8>>),
    Seek(IoResult<u64>),
}

/// A synchronous Read+Seek implementation that proxies requests to an async backend.
/// Usage: Use `create_bridge(async_reader)` to get the bridge and a future.
/// You MUST spawn the future on a Tokio runtime.
pub struct SyncReaderBridge {
    tx: tokio::sync::mpsc::UnboundedSender<(BridgeCommand, oneshot::Sender<BridgeResponse>)>,
}

pub struct AsyncBridgeBackend {
    // Hidden implementation detail handled by the future
}

impl Read for SyncReaderBridge {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let (resp_tx, resp_rx) = oneshot::channel();
        let buf_len = buf.len();
        
        self.tx.send((BridgeCommand::Read(buf_len), resp_tx))
             .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Async backend closed"))?;
             
        let response = resp_rx.blocking_recv()
            .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Response dropped"))?;
            
        match response {
            BridgeResponse::Read(Ok(data)) => {
                let len = std::cmp::min(buf.len(), data.len());
                buf[0..len].copy_from_slice(&data[0..len]);
                Ok(len)
            },
            BridgeResponse::Read(Err(e)) => Err(e),
            _ => Err(Error::new(ErrorKind::InvalidData, "Invalid response type")),
        }
    }
}

impl Seek for SyncReaderBridge {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send((BridgeCommand::Seek(pos), resp_tx))
             .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Async backend closed"))?;
             
        let response = resp_rx.blocking_recv()
            .map_err(|_| Error::new(ErrorKind::BrokenPipe, "Response dropped"))?;
            
        match response {
            BridgeResponse::Seek(res) => res,
            _ => Err(Error::new(ErrorKind::InvalidData, "Invalid response type")),
        }
    }
}

// Factory function
pub fn create_bridge(mut reader: Box<dyn crate::archives::AsyncSeekableReader>) -> (SyncReaderBridge, impl std::future::Future<Output = ()> + Send) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(BridgeCommand, oneshot::Sender<BridgeResponse>)>();
    
    let bridge = SyncReaderBridge { tx };
    
    let background_task = async move {
        while let Some((cmd, respond_to)) = rx.recv().await {
            match cmd {
                BridgeCommand::Read(len) => {
                    let mut buf = vec![0u8; len];
                    match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                        Ok(n) => {
                            buf.truncate(n);
                            let _ = respond_to.send(BridgeResponse::Read(Ok(buf)));
                        },
                        Err(e) => {
                            let _ = respond_to.send(BridgeResponse::Read(Err(e)));
                        }
                    }
                },
                BridgeCommand::Seek(pos) => {
                    let res = tokio::io::AsyncSeekExt::seek(&mut reader, pos).await;
                    let _ = respond_to.send(BridgeResponse::Seek(res));
                },
                BridgeCommand::Shutdown => break,
            }
        }
    };
    
    (bridge, background_task)
}
