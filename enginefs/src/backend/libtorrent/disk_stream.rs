use std::io::{Read, Seek};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::priorities::{
    PlaybackIntent, disk_backed_forward_window_pieces_for, disk_backed_sequential_download,
};
use crate::piece_waiter::PieceWaiterRegistry;

pub(crate) struct LibtorrentDiskFileStream {
    handle: libtorrent_sys::LibtorrentHandle,
    info_hash: String,
    file_path: PathBuf,
    display_path: String,
    first_piece: i32,
    last_piece: i32,
    piece_length: u64,
    file_offset: u64,
    file_size: u64,
    file_idx: usize,
    current_pos: u64,
    stream_id: usize,
    playback_intent: PlaybackIntent,
    piece_waiter: Arc<PieceWaiterRegistry>,
    created_at: Instant,
    first_read_logged: bool,
    last_retry_wake: Instant,
    last_wait_log: Instant,
    last_prioritized_piece: i32,
}

impl LibtorrentDiskFileStream {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        handle: libtorrent_sys::LibtorrentHandle,
        info_hash: String,
        file_path: PathBuf,
        display_path: String,
        first_piece: i32,
        last_piece: i32,
        piece_length: u64,
        file_offset: u64,
        file_size: u64,
        file_idx: usize,
        stream_id: usize,
        playback_intent: PlaybackIntent,
        piece_waiter: Arc<PieceWaiterRegistry>,
    ) -> Self {
        Self {
            handle,
            info_hash,
            file_path,
            display_path,
            first_piece,
            last_piece,
            piece_length,
            file_offset,
            file_size,
            file_idx,
            current_pos: 0,
            stream_id,
            playback_intent,
            piece_waiter,
            created_at: Instant::now(),
            first_read_logged: false,
            last_retry_wake: Instant::now(),
            last_wait_log: Instant::now()
                .checked_sub(Duration::from_secs(5))
                .unwrap_or_else(Instant::now),
            last_prioritized_piece: -1,
        }
    }

    fn current_piece(&self) -> std::io::Result<i32> {
        if self.piece_length == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "torrent piece length is zero",
            ));
        }

        Ok(((self.file_offset + self.current_pos) / self.piece_length) as i32)
    }

    fn bytes_available_in_verified_piece(&self, piece: i32) -> usize {
        let current_global = self.file_offset + self.current_pos;
        let piece_end_global = ((piece as u64) + 1).saturating_mul(self.piece_length);
        let remaining_in_piece = piece_end_global.saturating_sub(current_global);
        let remaining_in_file = self.file_size.saturating_sub(self.current_pos);

        remaining_in_piece
            .min(remaining_in_file)
            .min(usize::MAX as u64) as usize
    }

    fn prioritize_from(&mut self, piece: i32) {
        if piece < self.first_piece || piece > self.last_piece {
            return;
        }

        self.handle.set_file_priority(self.file_idx as i32, 7);
        let status = self.handle.status();
        if status.is_paused {
            tracing::info!(
                info_hash = %self.info_hash,
                file_idx = self.file_idx,
                "disk-backed download was paused while active; resuming torrent"
            );
            self.handle.resume();
            let _ = self.handle.force_reannounce();
            let _ = self.handle.force_dht_announce();
        }

        if self.last_prioritized_piece == piece {
            return;
        }
        self.last_prioritized_piece = piece;

        let priority_intent = if self.first_read_logged {
            self.playback_intent.sequential_after_first_byte()
        } else {
            self.playback_intent
        };
        let sequential_download = disk_backed_sequential_download(priority_intent);
        self.handle.set_sequential_download(sequential_download);
        let forward_window =
            disk_backed_forward_window_pieces_for(priority_intent, self.piece_length);
        let priority = if matches!(priority_intent, PlaybackIntent::Background) {
            1
        } else {
            7
        };
        let deadline_jitter = (self.stream_id % 10) as i32 * 5;
        for p in piece..=self.last_piece.min(piece + forward_window) {
            if !self.handle.have_piece(p) {
                let distance = p - piece;
                let deadline = if distance == 0 {
                    0
                } else {
                    distance * 25 + deadline_jitter
                };
                self.handle.set_piece_priority(p, priority);
                self.handle.set_piece_deadline(p, deadline);
            }
        }

        tracing::debug!(
            info_hash = %self.info_hash,
            file_idx = self.file_idx,
            intent = ?self.playback_intent,
            priority_intent = ?priority_intent,
            piece,
            sequential_download,
            forward_window,
            deadline_jitter,
            "disk-backed stream priority window configured"
        );
    }

    fn wait_for_piece(
        &mut self,
        piece: i32,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.prioritize_from(piece);
        self.piece_waiter
            .register(&self.info_hash, piece, self.stream_id, cx.waker().clone());

        if self.last_retry_wake.elapsed() >= Duration::from_millis(50) {
            self.last_retry_wake = Instant::now();
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                waker.wake();
            });
        }

        if self.last_wait_log.elapsed() >= Duration::from_secs(5) {
            self.last_wait_log = Instant::now();
            let status = self.handle.status();
            let verified_piece_count = (self.first_piece..=self.last_piece)
                .filter(|p| self.handle.have_piece(*p))
                .count();
            let verified_bytes_estimate = (verified_piece_count as u64)
                .saturating_mul(self.piece_length)
                .min(self.file_size);
            let request_offset_percent = if self.file_size > 0 {
                (self.current_pos.min(self.file_size) as f64 / self.file_size as f64) * 100.0
            } else {
                0.0
            };
            tracing::info!(
                info_hash = %self.info_hash,
                file_idx = self.file_idx,
                file_path = %self.display_path,
                intent = ?self.playback_intent,
                piece,
                pos = self.current_pos,
                request_offset_percent,
                verified_piece_count,
                verified_bytes_estimate,
                peers = status.num_peers,
                download_rate = status.download_rate,
                paused = status.is_paused,
                "disk-backed download waiting for verified piece"
            );
        }

        std::task::Poll::Pending
    }
}

impl tokio::io::AsyncRead for LibtorrentDiskFileStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.current_pos >= self.file_size || buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }

        let piece = match self.current_piece() {
            Ok(piece) => piece,
            Err(err) => return std::task::Poll::Ready(Err(err)),
        };

        if piece < self.first_piece || piece > self.last_piece {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read position is outside selected torrent file",
            )));
        }

        if !self.handle.have_piece(piece) {
            return self.wait_for_piece(piece, cx);
        }

        let verified_available = self.bytes_available_in_verified_piece(piece);
        if verified_available == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        let to_read = buf.remaining().min(verified_available);

        let mut file = match std::fs::File::open(&self.file_path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return self.wait_for_piece(piece, cx);
            }
            Err(err) => return std::task::Poll::Ready(Err(err)),
        };

        if let Err(err) = file.seek(std::io::SeekFrom::Start(self.current_pos)) {
            return std::task::Poll::Ready(Err(err));
        }

        let mut scratch = vec![0u8; to_read.min(256 * 1024)];
        let read = match file.read(&mut scratch) {
            Ok(0) => return self.wait_for_piece(piece, cx),
            Ok(read) => read,
            Err(err) => return std::task::Poll::Ready(Err(err)),
        };

        buf.put_slice(&scratch[..read]);
        self.current_pos += read as u64;
        if !self.first_read_logged {
            self.first_read_logged = true;
            tracing::info!(
                info_hash = %self.info_hash,
                file_idx = self.file_idx,
                intent = ?self.playback_intent,
                piece,
                elapsed_ms = self.created_at.elapsed().as_millis() as u64,
                "disk-backed first bytes ready"
            );
        }
        self.prioritize_from(piece.saturating_add(1));

        std::task::Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncSeek for LibtorrentDiskFileStream {
    fn start_seek(
        mut self: std::pin::Pin<&mut Self>,
        position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        let new_pos = match position {
            std::io::SeekFrom::Start(pos) => pos,
            std::io::SeekFrom::Current(delta) => {
                (self.current_pos as i64).saturating_add(delta).max(0) as u64
            }
            std::io::SeekFrom::End(delta) => {
                (self.file_size as i64).saturating_add(delta).max(0) as u64
            }
        };

        self.current_pos = new_pos.min(self.file_size);
        self.last_prioritized_piece = -1;
        Ok(())
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        std::task::Poll::Ready(Ok(self.current_pos))
    }
}

impl Drop for LibtorrentDiskFileStream {
    fn drop(&mut self) {
        self.piece_waiter.unregister_stream(self.stream_id);
    }
}
