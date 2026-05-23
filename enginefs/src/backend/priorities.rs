use serde::{Deserialize, Serialize};

/// Minimum bytes needed before playback can start.
pub const MIN_STARTUP_BYTES: u64 = 16 * 1024 * 1024; // 16MB

/// Maximum pieces to prioritize before first byte is delivered.
pub const MAX_STARTUP_PIECES: i32 = 4;

/// Minimum pieces to prioritize before first byte is delivered.
pub const MIN_STARTUP_PIECES: i32 = 2;

/// Aggressive seek/read-ahead defaults. These are internal on purpose: tuning is
/// driven by runtime measurements and logs rather than user-facing settings.
pub const MIN_SEEK_HOT_PIECES: i32 = 24;
pub const SEEK_IMMEDIATE_PIECES: i32 = 12;
pub const MAX_HOT_PIECES: i32 = 96;
pub const MAX_WARM_PIECES: i32 = 192;
pub const BLOCKED_REPLAN_INTERVAL_MS: u64 = 250;

/// Start treating reads as "container metadata" when they fall in the last 10MB
/// or the last 5% of the file, whichever starts earlier.
pub fn container_metadata_start(file_size: u64) -> u64 {
    file_size
        .saturating_sub(10 * 1024 * 1024)
        .min(file_size.saturating_mul(95) / 100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackIntent {
    DirectInitial,
    DirectSeek,
    DirectSequential,
    HlsInitial,
    HlsSeek,
    HlsSequential,
    DownloadFull,
    DownloadRange,
    ContainerMetadata,
    InternalProbe,
    Background,
}

impl PlaybackIntent {
    pub fn is_hls(self) -> bool {
        matches!(self, Self::HlsInitial | Self::HlsSeek | Self::HlsSequential)
    }

    pub fn sequential_after_first_byte(self) -> Self {
        match self {
            Self::DirectInitial | Self::DirectSeek | Self::DirectSequential => {
                Self::DirectSequential
            }
            Self::HlsInitial | Self::HlsSeek | Self::HlsSequential => Self::HlsSequential,
            Self::DownloadFull | Self::DownloadRange => self,
            other => other,
        }
    }

    pub fn seek_for_same_family(self) -> Self {
        if self.is_hls() {
            Self::HlsSeek
        } else if matches!(self, Self::DownloadFull | Self::DownloadRange) {
            Self::DownloadRange
        } else {
            Self::DirectSeek
        }
    }
}

pub fn disk_backed_sequential_download(intent: PlaybackIntent) -> bool {
    matches!(intent, PlaybackIntent::DownloadFull)
}

pub fn disk_backed_forward_window_pieces(intent: PlaybackIntent) -> i32 {
    match intent {
        PlaybackIntent::DownloadFull => 15,
        PlaybackIntent::DownloadRange => 3,
        PlaybackIntent::DirectInitial | PlaybackIntent::HlsInitial => MAX_STARTUP_PIECES - 1,
        PlaybackIntent::DirectSeek | PlaybackIntent::HlsSeek => 7,
        PlaybackIntent::DirectSequential | PlaybackIntent::HlsSequential => 7,
        PlaybackIntent::ContainerMetadata => 1,
        PlaybackIntent::InternalProbe => 1,
        PlaybackIntent::Background => 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryPressure {
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PriorityBand {
    Immediate,
    Hot,
    Warm,
    Metadata,
    Background,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EngineCacheConfig {
    pub size: u64,
    pub enabled: bool,
}

impl Default for EngineCacheConfig {
    fn default() -> Self {
        Self {
            size: 10 * 1024 * 1024 * 1024, // 10 GB
            enabled: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PriorityContext {
    pub intent: PlaybackIntent,
    pub current_piece: i32,
    pub first_piece: i32,
    pub last_piece: i32,
    pub piece_length: u64,
    pub file_size: u64,
    pub bitrate_bytes_per_sec: Option<u64>,
    pub download_rate_bytes_per_sec: u64,
    pub peers: u64,
    pub cache_size_bytes: u64,
    pub memory_pressure: MemoryPressure,
    pub consecutive_waits: u32,
    pub first_byte_sent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityAssignment {
    pub piece_idx: i32,
    pub piece_priority: i32,
    pub deadline: i32,
    pub band: PriorityBand,
}

pub type PriorityItem = PriorityAssignment;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityDecision {
    pub assignments: Vec<PriorityAssignment>,
    pub target_window_pieces: i32,
    pub immediate_pieces: i32,
    pub hot_window_pieces: i32,
    pub warm_window_pieces: i32,
    pub reason: String,
}

pub struct PlaybackPriorityPolicy;

impl PlaybackPriorityPolicy {
    pub fn decide(ctx: PriorityContext) -> PriorityDecision {
        if ctx.piece_length == 0
            || ctx.current_piece < ctx.first_piece
            || ctx.last_piece < ctx.first_piece
        {
            return PriorityDecision {
                assignments: Vec::new(),
                target_window_pieces: 0,
                immediate_pieces: 0,
                hot_window_pieces: 0,
                warm_window_pieces: 0,
                reason: "invalid-context".to_string(),
            };
        }

        let max_cache_pieces = if ctx.cache_size_bytes > 0 {
            (ctx.cache_size_bytes / ctx.piece_length).max(1) as i32
        } else {
            MAX_HOT_PIECES
        };
        let remaining_pieces = ctx.last_piece.saturating_sub(ctx.current_piece) + 1;

        let bitrate_ratio = ctx
            .bitrate_bytes_per_sec
            .filter(|bitrate| *bitrate > 0)
            .map(|bitrate| ctx.download_rate_bytes_per_sec as f64 / bitrate as f64);

        let mut reason = match ctx.intent {
            PlaybackIntent::DirectInitial | PlaybackIntent::HlsInitial => "initial".to_string(),
            PlaybackIntent::DirectSeek | PlaybackIntent::HlsSeek => "seek".to_string(),
            PlaybackIntent::DirectSequential | PlaybackIntent::HlsSequential => {
                "sequential".to_string()
            }
            PlaybackIntent::DownloadFull => "download-full".to_string(),
            PlaybackIntent::DownloadRange => "download-range".to_string(),
            PlaybackIntent::ContainerMetadata => "container-metadata".to_string(),
            PlaybackIntent::InternalProbe => "internal-probe".to_string(),
            PlaybackIntent::Background => "background".to_string(),
        };

        let (mut immediate, mut hot, mut warm) = match ctx.intent {
            PlaybackIntent::DirectInitial | PlaybackIntent::HlsInitial if !ctx.first_byte_sent => {
                let target_bytes = ctx
                    .bitrate_bytes_per_sec
                    .map(|bitrate| bitrate.saturating_mul(10))
                    .unwrap_or(MIN_STARTUP_BYTES)
                    .max(MIN_STARTUP_BYTES)
                    .max(ctx.piece_length);
                let pieces = target_bytes.saturating_add(ctx.piece_length.saturating_sub(1))
                    / ctx.piece_length;
                let pieces = (pieces as i32).clamp(MIN_STARTUP_PIECES, MAX_STARTUP_PIECES);
                (pieces, pieces, 0)
            }
            PlaybackIntent::DirectInitial
            | PlaybackIntent::DirectSequential
            | PlaybackIntent::HlsInitial
            | PlaybackIntent::HlsSequential => {
                let mut hot = dynamic_hot_window(&ctx, bitrate_ratio);
                if ctx.intent.is_hls() {
                    hot = hot.min(48);
                    reason.push_str("-hls-cap");
                }
                (2, hot, 32)
            }
            PlaybackIntent::DownloadFull => (2, 16, 0),
            PlaybackIntent::DownloadRange => (1, 4, 0),
            PlaybackIntent::DirectSeek | PlaybackIntent::HlsSeek => {
                let mut hot = dynamic_hot_window(&ctx, bitrate_ratio).max(MIN_SEEK_HOT_PIECES);
                let mut immediate = SEEK_IMMEDIATE_PIECES;
                if ctx.consecutive_waits >= 3 {
                    hot = (hot * 2).min(MAX_HOT_PIECES);
                    immediate = (immediate * 2).min(hot);
                    reason.push_str("-blocked-expand");
                }
                if ctx.intent.is_hls() && ctx.consecutive_waits < 3 {
                    hot = hot.min(48);
                    reason.push_str("-hls-cap");
                }
                (immediate, hot, 32)
            }
            PlaybackIntent::ContainerMetadata => (1, 2, 0),
            PlaybackIntent::InternalProbe => (0, 2, 0),
            PlaybackIntent::Background => (0, 4, 0),
        };

        if matches!(ctx.memory_pressure, MemoryPressure::High) {
            hot = hot.min(if ctx.intent.is_hls() {
                16
            } else {
                MIN_SEEK_HOT_PIECES
            });
            warm = 0;
            reason.push_str("-memory-clamp");
        }

        if matches!(
            ctx.intent,
            PlaybackIntent::Background | PlaybackIntent::InternalProbe
        ) {
            warm = 0;
        }

        hot = hot
            .clamp(0, MAX_HOT_PIECES)
            .min(max_cache_pieces)
            .min(remaining_pieces);
        warm = warm
            .clamp(0, MAX_WARM_PIECES)
            .min(max_cache_pieces.saturating_sub(hot))
            .min(remaining_pieces.saturating_sub(hot));
        immediate = immediate.min(hot).max(0);

        let target_window = hot + warm;
        let mut assignments = Vec::with_capacity(target_window as usize);
        for distance in 0..target_window {
            let piece_idx = ctx.current_piece + distance;
            if piece_idx > ctx.last_piece {
                break;
            }

            let (band, piece_priority, deadline) = assignment_for(&ctx, distance, immediate, hot);
            assignments.push(PriorityAssignment {
                piece_idx,
                piece_priority,
                deadline,
                band,
            });
        }

        PriorityDecision {
            assignments,
            target_window_pieces: target_window,
            immediate_pieces: immediate,
            hot_window_pieces: hot,
            warm_window_pieces: warm,
            reason,
        }
    }
}

fn dynamic_hot_window(ctx: &PriorityContext, bitrate_ratio: Option<f64>) -> i32 {
    let mut hot = if let Some(ratio) = bitrate_ratio {
        if ratio >= 3.0 {
            96
        } else if ratio >= 1.5 {
            48
        } else if ratio >= 1.0 {
            32
        } else {
            MIN_SEEK_HOT_PIECES
        }
    } else if ctx.download_rate_bytes_per_sec > 10 * 1024 * 1024 {
        96
    } else if ctx.download_rate_bytes_per_sec > 5 * 1024 * 1024 {
        48
    } else if ctx.download_rate_bytes_per_sec > 1024 * 1024 {
        MIN_SEEK_HOT_PIECES
    } else {
        16
    };

    if let Some(bitrate) = ctx.bitrate_bytes_per_sec.filter(|bitrate| *bitrate > 0) {
        let pieces_for_10s = ((bitrate.saturating_mul(10)) / ctx.piece_length).max(1) as i32;
        hot = hot.max(pieces_for_10s);
    }

    if ctx.peers < 3 {
        hot = hot.min(MIN_SEEK_HOT_PIECES);
    }

    hot
}

fn assignment_for(
    ctx: &PriorityContext,
    distance: i32,
    immediate_pieces: i32,
    hot_pieces: i32,
) -> (PriorityBand, i32, i32) {
    match ctx.intent {
        PlaybackIntent::ContainerMetadata => (PriorityBand::Metadata, 4, 150 + distance * 50),
        PlaybackIntent::InternalProbe => (PriorityBand::Background, 1, 1_000 + distance * 250),
        PlaybackIntent::Background => (PriorityBand::Background, 1, 20_000 + distance * 200),
        _ if distance < immediate_pieces => (PriorityBand::Immediate, 7, distance * 25),
        _ if distance < hot_pieces => (PriorityBand::Hot, 4, 1_500 + distance * 150),
        _ => (PriorityBand::Warm, 2, 10_000 + distance * 250),
    }
}

/// Backward-compatible wrapper used by older callers and tests.
pub fn calculate_priorities(
    current_piece: i32,
    total_pieces: i32,
    piece_length: u64,
    config: &EngineCacheConfig,
    priority: u8,
    download_speed: u64,
    bitrate: Option<u64>,
) -> Vec<PriorityItem> {
    let intent = if priority >= 250 {
        PlaybackIntent::InternalProbe
    } else if priority >= 100 {
        PlaybackIntent::DirectSeek
    } else if priority == 0 {
        PlaybackIntent::Background
    } else {
        PlaybackIntent::DirectSequential
    };

    PlaybackPriorityPolicy::decide(PriorityContext {
        intent,
        current_piece,
        first_piece: 0,
        last_piece: total_pieces.saturating_sub(1),
        piece_length,
        file_size: total_pieces.max(0) as u64 * piece_length,
        bitrate_bytes_per_sec: bitrate,
        download_rate_bytes_per_sec: download_speed,
        peers: 8,
        cache_size_bytes: if config.enabled { config.size } else { 0 },
        memory_pressure: MemoryPressure::Normal,
        consecutive_waits: 0,
        first_byte_sent: true,
    })
    .assignments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_context(intent: PlaybackIntent) -> PriorityContext {
        PriorityContext {
            intent,
            current_piece: 100,
            first_piece: 0,
            last_piece: 999,
            piece_length: 1024 * 1024,
            file_size: 1000 * 1024 * 1024,
            bitrate_bytes_per_sec: None,
            download_rate_bytes_per_sec: 2 * 1024 * 1024,
            peers: 10,
            cache_size_bytes: 1024 * 1024 * 1024,
            memory_pressure: MemoryPressure::Normal,
            consecutive_waits: 0,
            first_byte_sent: true,
        }
    }

    #[test]
    fn initial_before_first_byte_is_small() {
        let mut ctx = base_context(PlaybackIntent::DirectInitial);
        ctx.current_piece = 0;
        ctx.first_byte_sent = false;
        let decision = PlaybackPriorityPolicy::decide(ctx);

        assert!(decision.target_window_pieces >= MIN_STARTUP_PIECES);
        assert!(decision.target_window_pieces <= MAX_STARTUP_PIECES);
        assert_eq!(decision.assignments[0].deadline, 0);
        assert_eq!(decision.assignments[0].piece_priority, 7);
    }

    #[test]
    fn initial_after_first_byte_expands() {
        let decision = PlaybackPriorityPolicy::decide(base_context(PlaybackIntent::DirectInitial));

        assert!(decision.hot_window_pieces >= MIN_SEEK_HOT_PIECES);
        assert_eq!(decision.assignments[0].band, PriorityBand::Immediate);
    }

    #[test]
    fn direct_seek_has_minimum_hot_window() {
        let decision = PlaybackPriorityPolicy::decide(base_context(PlaybackIntent::DirectSeek));

        assert!(decision.hot_window_pieces >= MIN_SEEK_HOT_PIECES);
        assert_eq!(decision.immediate_pieces, SEEK_IMMEDIATE_PIECES);
        assert_eq!(decision.assignments[0].piece_priority, 7);
        assert_eq!(decision.assignments[3].piece_priority, 7);
        assert_eq!(decision.assignments[4].piece_priority, 7);
        assert_eq!(
            decision.assignments[SEEK_IMMEDIATE_PIECES as usize - 1].piece_priority,
            7
        );
        assert_eq!(
            decision.assignments[SEEK_IMMEDIATE_PIECES as usize].piece_priority,
            4
        );
    }

    #[test]
    fn fast_swarm_expands_seek_window() {
        let mut ctx = base_context(PlaybackIntent::DirectSeek);
        ctx.download_rate_bytes_per_sec = 12 * 1024 * 1024;
        let decision = PlaybackPriorityPolicy::decide(ctx);

        assert!(decision.hot_window_pieces >= 96);
    }

    #[test]
    fn hls_window_is_capped_before_blocking() {
        let mut ctx = base_context(PlaybackIntent::HlsSeek);
        ctx.download_rate_bytes_per_sec = 12 * 1024 * 1024;
        let decision = PlaybackPriorityPolicy::decide(ctx);

        assert_eq!(decision.hot_window_pieces, 48);
    }

    #[test]
    fn hls_blocking_can_expand_to_aggressive_window() {
        let mut ctx = base_context(PlaybackIntent::HlsSeek);
        ctx.download_rate_bytes_per_sec = 12 * 1024 * 1024;
        ctx.consecutive_waits = 3;
        let decision = PlaybackPriorityPolicy::decide(ctx);

        assert!(decision.hot_window_pieces > 48);
    }

    #[test]
    fn memory_pressure_clamps_window() {
        let mut ctx = base_context(PlaybackIntent::DirectSeek);
        ctx.download_rate_bytes_per_sec = 12 * 1024 * 1024;
        ctx.memory_pressure = MemoryPressure::High;
        let decision = PlaybackPriorityPolicy::decide(ctx);

        assert_eq!(decision.hot_window_pieces, MIN_SEEK_HOT_PIECES);
        assert_eq!(decision.warm_window_pieces, 0);
    }

    #[test]
    fn internal_probe_uses_low_priority() {
        let decision = PlaybackPriorityPolicy::decide(base_context(PlaybackIntent::InternalProbe));

        assert!(
            decision
                .assignments
                .iter()
                .all(|item| item.piece_priority <= 1)
        );
    }

    #[test]
    fn compatibility_wrapper_still_returns_priorities() {
        let config = EngineCacheConfig {
            size: 200 * 1024 * 1024,
            enabled: true,
        };
        let priorities = calculate_priorities(0, 1000, 10 * 1024 * 1024, &config, 1, 0, None);

        assert!(!priorities.is_empty());
        assert_eq!(priorities[0].piece_idx, 0);
    }

    #[test]
    fn disk_backed_sequential_is_only_for_full_downloads() {
        assert!(disk_backed_sequential_download(
            PlaybackIntent::DownloadFull
        ));
        assert!(!disk_backed_sequential_download(
            PlaybackIntent::DownloadRange
        ));
        assert!(!disk_backed_sequential_download(
            PlaybackIntent::DirectInitial
        ));
        assert!(!disk_backed_sequential_download(PlaybackIntent::DirectSeek));
        assert!(!disk_backed_sequential_download(
            PlaybackIntent::ContainerMetadata
        ));
    }

    #[test]
    fn disk_backed_container_metadata_uses_tiny_window() {
        assert_eq!(
            disk_backed_forward_window_pieces(PlaybackIntent::ContainerMetadata),
            1
        );
        assert!(
            disk_backed_forward_window_pieces(PlaybackIntent::DownloadFull)
                > disk_backed_forward_window_pieces(PlaybackIntent::DownloadRange)
        );
    }

    #[test]
    fn download_range_priority_is_bounded() {
        let decision = PlaybackPriorityPolicy::decide(base_context(PlaybackIntent::DownloadRange));

        assert_eq!(decision.hot_window_pieces, 4);
        assert_eq!(decision.warm_window_pieces, 0);
        assert_eq!(decision.assignments[0].piece_priority, 7);
    }
}
