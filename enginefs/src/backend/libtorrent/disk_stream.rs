use std::io::{Read, Seek};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::priorities::{
    BLOCKED_REPLAN_INTERVAL_MS, PlaybackIntent, disk_backed_file_baseline_priority,
    disk_backed_forward_window_pieces_for, disk_backed_sequential_download,
};
use crate::metadata_pins::MetadataPinRegistry;
use crate::piece_waiter::PieceWaiterRegistry;

const INITIAL_FIRST_BYTE_WINDOW_PIECES: i32 = 3;
const URGENT_REASSERT_INTERVAL_MS: u64 = 250;

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
    metadata_pins: Arc<MetadataPinRegistry>,
    created_at: Instant,
    first_read_logged: bool,
    last_retry_wake: Instant,
    last_wait_log: Instant,
    last_prioritized_piece: i32,
    consecutive_waits: u32,
    last_blocked_replan: Instant,
    last_stall_reannounce: Instant,
    last_urgent_reassert: Instant,
    urgent_reassert_count: u32,
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
        metadata_pins: Arc<MetadataPinRegistry>,
    ) -> Self {
        let mut handle = handle;
        // Pieces verified moments ago (e.g. the shared boundary piece of the
        // previous episode) may still be in libtorrent's write cache; start a
        // flush so direct file reads see real bytes instead of preallocated
        // zeros.
        handle.flush_cache();
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
            metadata_pins,
            created_at: Instant::now(),
            first_read_logged: false,
            last_retry_wake: Instant::now(),
            last_wait_log: Instant::now()
                .checked_sub(Duration::from_secs(5))
                .unwrap_or_else(Instant::now),
            last_prioritized_piece: -1,
            consecutive_waits: 0,
            last_blocked_replan: Instant::now()
                .checked_sub(Duration::from_millis(BLOCKED_REPLAN_INTERVAL_MS))
                .unwrap_or_else(Instant::now),
            last_stall_reannounce: Instant::now(),
            last_urgent_reassert: Instant::now()
                .checked_sub(Duration::from_millis(URGENT_REASSERT_INTERVAL_MS))
                .unwrap_or_else(Instant::now),
            urgent_reassert_count: 0,
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

    fn active_forward_window(&self, intent: PlaybackIntent, configured_forward_window: i32) -> i32 {
        if self.first_read_logged {
            return configured_forward_window;
        }

        match intent {
            // Cold starts only need a tiny head cluster before the first byte;
            // seeks need the full configured cluster because players commonly
            // pull several pieces immediately for demux/decode after the seek.
            PlaybackIntent::DirectInitial | PlaybackIntent::HlsInitial => {
                configured_forward_window.min(INITIAL_FIRST_BYTE_WINDOW_PIECES)
            }
            _ => configured_forward_window,
        }
    }

    fn priority_intent(&self) -> PlaybackIntent {
        if self.first_read_logged {
            self.playback_intent.sequential_after_first_byte()
        } else {
            self.playback_intent
        }
    }

    /// Pinned metadata (Cues/moov) pieces for this torrent that are still
    /// missing. Verified pins are dropped as a side effect so the set shrinks
    /// toward empty and the head-window cap lifts automatically.
    fn pinned_metadata_missing(&self) -> Vec<i32> {
        let pinned = self.metadata_pins.pinned(&self.info_hash);
        if pinned.is_empty() {
            return Vec::new();
        }
        let mut verified = Vec::new();
        let mut missing = Vec::new();
        for p in pinned {
            if self.handle.have_piece(p) {
                verified.push(p);
            } else {
                missing.push(p);
            }
        }
        if !verified.is_empty() {
            self.metadata_pins.unpin_pieces(&self.info_hash, &verified);
        }
        missing
    }

    /// Re-assert pinned metadata pieces at top priority with an immediate
    /// deadline so a head stream's `set_file_priority` can't strip the rare
    /// Cues/moov region while it is still downloading.
    fn apply_pinned_metadata(&mut self, pinned_missing: &[i32]) {
        for &p in pinned_missing {
            if p >= self.first_piece && p <= self.last_piece && !self.handle.have_piece(p) {
                self.handle.set_piece_priority(p, 7);
                self.handle.set_piece_deadline(p, 0);
            }
        }
    }

    fn prioritize_from(&mut self, piece: i32) {
        if piece < self.first_piece || piece > self.last_piece {
            return;
        }

        // Only rejoin the swarm when the piece we are about to serve is actually
        // missing. Re-reading an already-downloaded piece must not resume a
        // torrent that the seeding-disabled policy paused, or playback of a
        // complete file would keep waking it back into seeding.
        if !self.handle.have_piece(piece) {
            let status = self.handle.status();
            if status.is_paused {
                tracing::warn!(
                    info_hash = %self.info_hash,
                    file_idx = self.file_idx,
                    state = status.state,
                    finished = status.is_finished,
                    auto_managed = status.is_auto_managed,
                    error = %status.error,
                    "disk-backed download was paused while active; resuming torrent"
                );
                self.handle.resume();
                // prioritize_from is called on every retry (~every 50ms via the
                // wait_for_piece wake timer), but reannounce/dht_announce are
                // rate-limited on the tracker/DHT side -- a tracker ignores
                // repeat announces faster than its min-interval, and a DHT
                // get_peers lookup needs several round-trips to finish, which a
                // brand-new lookup every 50ms cancels before it ever completes.
                // Reuse the same cooldown as the stall-escalation reannounce
                // below so the two paths can't double up on spam either.
                if self.last_stall_reannounce.elapsed() >= Duration::from_secs(10) {
                    self.last_stall_reannounce = Instant::now();
                    let _ = self.handle.force_reannounce();
                    let _ = self.handle.force_dht_announce();
                }
            }
        }

        if self.last_prioritized_piece == piece {
            return;
        }
        self.last_prioritized_piece = piece;

        let priority_intent = self.priority_intent();
        let sequential_download = disk_backed_sequential_download(priority_intent);
        self.handle.set_sequential_download(sequential_download);
        let configured_forward_window =
            disk_backed_forward_window_pieces_for(priority_intent, self.piece_length);
        let forward_window = self.active_forward_window(priority_intent, configured_forward_window);

        // A tail-seek (Cues/moov) stream records its window as metadata-critical
        // so concurrent head streams rank it above their own read-ahead until it
        // verifies. The background metadata inspector pins the same region, so a
        // head stream backs off even before the player issues the tail request.
        let is_metadata_stream = matches!(
            priority_intent,
            PlaybackIntent::ContainerMetadata | PlaybackIntent::InternalProbe
        );
        if is_metadata_stream {
            let window_end = self.last_piece.min(piece + forward_window);
            let window: Vec<i32> = (piece..=window_end)
                .filter(|p| !self.handle.have_piece(*p))
                .collect();
            self.metadata_pins.pin_pieces(&self.info_hash, window);
        }
        let pinned_missing = self.pinned_metadata_missing();
        let cues_pending = !pinned_missing.is_empty();

        // While rare Cues/moov pieces are still missing, a head stream drops its
        // read-ahead window from 7 to 4 so the few peers that hold the tail feed
        // it first. The head's CURRENT piece still stays urgent via
        // reassert_requested_piece below; only read-ahead yields. The metadata
        // stream itself keeps full priority; background reads stay at 1.
        let priority = if matches!(priority_intent, PlaybackIntent::Background) {
            1
        } else if cues_pending && !is_metadata_stream {
            4
        } else {
            7
        };
        // Set the file's baseline (whole file at priority 1 for streaming so it
        // stays "wanted" and the torrent never falsely finishes after the head;
        // 7 for explicit downloads) first, then raise only the active forward
        // window below. set_file_priority resets stale per-piece priorities so a
        // previous request's window cannot keep unrelated pieces ahead.
        let file_baseline = disk_backed_file_baseline_priority(priority_intent);
        self.handle
            .set_file_priority(self.file_idx as i32, file_baseline);
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
        self.reassert_requested_piece(piece, "initial-window");
        // Re-assert pinned Cues/moov LAST so the set_file_priority above (and the
        // baseline-0 wipe inside reassert) can't strip the rare tail region.
        self.apply_pinned_metadata(&pinned_missing);

        tracing::debug!(
            info_hash = %self.info_hash,
            file_idx = self.file_idx,
            intent = ?self.playback_intent,
            priority_intent = ?priority_intent,
            piece,
            sequential_download,
            forward_window,
            configured_forward_window,
            file_baseline,
            cues_pending,
            deadline_jitter,
            "disk-backed stream priority window configured"
        );
    }

    fn reassert_requested_piece(&mut self, piece: i32, reason: &'static str) {
        if piece < self.first_piece || piece > self.last_piece || self.handle.have_piece(piece) {
            return;
        }

        let priority_intent = self.priority_intent();
        let file_baseline = disk_backed_file_baseline_priority(priority_intent);
        if file_baseline == 0 {
            self.handle.set_file_priority(self.file_idx as i32, 0);
        }
        self.handle.set_piece_priority(piece, 7);
        self.handle.set_piece_deadline(piece, 0);
        self.last_urgent_reassert = Instant::now();
        self.urgent_reassert_count = self.urgent_reassert_count.saturating_add(1);

        if self.urgent_reassert_count <= 3 || self.urgent_reassert_count % 20 == 0 {
            let status = self.handle.status();
            tracing::info!(
                info_hash = %self.info_hash,
                file_idx = self.file_idx,
                intent = ?self.playback_intent,
                priority_intent = ?priority_intent,
                piece,
                reassert_count = self.urgent_reassert_count,
                peers = status.num_peers,
                download_rate = status.download_rate,
                file_baseline,
                elapsed_ms = self.created_at.elapsed().as_millis() as u64,
                reason,
                "requested piece forced urgent"
            );
        }
    }

    /// Re-assert deadlines for a blocking piece and expand the window when the
    /// stream keeps waiting. Without this, `prioritize_from`'s
    /// `last_prioritized_piece` guard means deadlines are set exactly once per
    /// piece, so a choked swarm can stall the stream indefinitely with no
    /// recovery until the player itself times out and re-requests.
    fn escalate_blocked_piece(&mut self, piece: i32) {
        if self.last_blocked_replan.elapsed() < Duration::from_millis(BLOCKED_REPLAN_INTERVAL_MS) {
            return;
        }
        self.last_blocked_replan = Instant::now();

        let priority_intent = self.priority_intent();
        let mut forward_window =
            disk_backed_forward_window_pieces_for(priority_intent, self.piece_length);
        forward_window = self.active_forward_window(priority_intent, forward_window);
        if self.consecutive_waits >= 3 {
            forward_window = (forward_window * 2).min(63);
        }

        let file_baseline = disk_backed_file_baseline_priority(priority_intent);
        if file_baseline == 0 {
            self.handle.set_file_priority(self.file_idx as i32, 0);
        }
        for p in piece..=self.last_piece.min(piece + forward_window) {
            if !self.handle.have_piece(p) {
                let distance = p - piece;
                self.handle.set_piece_priority(p, 7);
                self.handle.set_piece_deadline(p, distance * 25);
            }
        }
        self.reassert_requested_piece(piece, "blocked-replan");
        // Keep pinned Cues/moov urgent (deadline 0) so they out-rank this
        // window's read-ahead deadlines even though both sit at priority 7.
        let pinned_missing = self.pinned_metadata_missing();
        self.apply_pinned_metadata(&pinned_missing);

        // A long stall with barely any progress usually means the current
        // peers are choking us; announce to find fresh peers sooner.
        if self.consecutive_waits >= 100
            && self.last_stall_reannounce.elapsed() >= Duration::from_secs(30)
        {
            let status = self.handle.status();
            if status.download_rate < 256 * 1024 {
                self.last_stall_reannounce = Instant::now();
                let _ = self.handle.force_reannounce();
                let _ = self.handle.force_dht_announce();
                tracing::info!(
                    info_hash = %self.info_hash,
                    file_idx = self.file_idx,
                    piece,
                    consecutive_waits = self.consecutive_waits,
                    peers = status.num_peers,
                    download_rate = status.download_rate,
                    "disk-backed stream stalled; forcing tracker and DHT reannounce"
                );
            }
        }

        tracing::debug!(
            info_hash = %self.info_hash,
            file_idx = self.file_idx,
            intent = ?priority_intent,
            piece,
            forward_window,
            file_baseline,
            consecutive_waits = self.consecutive_waits,
            "disk-backed blocked piece re-prioritized"
        );
    }

    fn wait_for_piece(
        &mut self,
        piece: i32,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.consecutive_waits = self.consecutive_waits.saturating_add(1);
        self.prioritize_from(piece);
        if self.last_urgent_reassert.elapsed() >= Duration::from_millis(URGENT_REASSERT_INTERVAL_MS)
        {
            self.reassert_requested_piece(piece, "wait-reassert");
        }
        self.escalate_blocked_piece(piece);
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
            let configured_forward_window =
                disk_backed_forward_window_pieces_for(self.playback_intent, self.piece_length);
            let active_forward_window =
                self.active_forward_window(self.playback_intent, configured_forward_window);
            let cluster_end = self.last_piece.min(piece + active_forward_window);
            let ready_in_active_window = (piece..=cluster_end)
                .filter(|p| self.handle.have_piece(*p))
                .count();
            let missing_in_active_window =
                (cluster_end.saturating_sub(piece) + 1).max(0) as usize - ready_in_active_window;
            let piece_availability = self
                .handle
                .piece_availability()
                .get(piece as usize)
                .copied()
                .unwrap_or(-1);
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
                active_forward_window,
                ready_in_active_window,
                missing_in_active_window,
                piece_availability,
                peers = status.num_peers,
                download_rate = status.download_rate,
                paused = status.is_paused,
                auto_managed = status.is_auto_managed,
                state = status.state,
                finished = status.is_finished,
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
            // A verified piece (have_piece == true) whose bytes are not yet on
            // disk -- write-back lag, or the file not yet extended -- reads as
            // EOF here. Force a flush so it materialises instead of waiting
            // forever; without this the stream can deadlock when the torrent has
            // gone idle and would never flush on its own.
            Ok(0) => {
                self.handle.flush_cache();
                return self.wait_for_piece(piece, cx);
            }
            Ok(read) => read,
            Err(err) => return std::task::Poll::Ready(Err(err)),
        };

        // A verified piece can still be in libtorrent's write cache before it
        // reaches the OS file, in which case the preallocated region reads as
        // zeros. No media container starts with an all-zero header, so treat
        // zeros at the very start of the file as "not flushed yet" and wait
        // instead of serving garbage that makes the player report an invalid
        // file format.
        if self.current_pos == 0
            && !self.first_read_logged
            && scratch[..read].iter().all(|&b| b == 0)
        {
            tracing::info!(
                info_hash = %self.info_hash,
                file_idx = self.file_idx,
                piece,
                read,
                "disk-backed file head reads as zeros; waiting for disk cache flush"
            );
            self.handle.flush_cache();
            return self.wait_for_piece(piece, cx);
        }

        buf.put_slice(&scratch[..read]);
        self.current_pos += read as u64;
        self.consecutive_waits = 0;
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
