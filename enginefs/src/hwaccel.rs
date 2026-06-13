//! Hardware acceleration configuration for HLS transcoding
//!
//! Parses the `transcode_profile` setting to select appropriate hardware encoder.
//! Supports OS-specific hardware encoders with automatic detection and fallback:
//! - Windows: NVENC (NVIDIA), QSV (Intel)
//! - Linux: NVENC, VAAPI, QSV, V4L2M2M
//! - macOS: VideoToolbox

/// Hardware encoder configuration
#[derive(Debug, Clone)]
pub struct HwAccelConfig {
    /// Encoder name (e.g., "h264_nvenc", "libx264")
    pub encoder: String,
    /// Hardware acceleration method (e.g., "cuda", "qsv", "vaapi")
    pub hwaccel: Option<String>,
    /// Device path (e.g., "/dev/dri/renderD128" for VAAPI on Linux)
    pub device: Option<String>,
    /// Additional encoder-specific arguments
    pub extra_args: Vec<String>,
    /// Pixel format for hardware encoding
    pub pix_fmt: Option<String>,
}

impl Default for HwAccelConfig {
    fn default() -> Self {
        Self::software()
    }
}

impl HwAccelConfig {
    /// Select encoder based on transcode_profile setting
    ///
    /// # Arguments
    /// * `available` - List of available hardware accelerators from probe_hwaccel()
    /// * `transcode_profile` - User's transcode profile setting (e.g., "hw:nvenc", "sw", "auto")
    ///
    /// # Profile format
    /// - `hw:nvenc` / `hw:nvidia` - NVIDIA NVENC
    /// - `hw:qsv` / `hw:intel` - Intel Quick Sync
    /// - `hw:vaapi` - VAAPI (Linux)
    /// - `hw:videotoolbox` / `hw:vt` - VideoToolbox (macOS)
    /// - `hw:v4l2` - V4L2M2M (ARM/Raspberry Pi)
    /// - `sw` / `software` / `cpu` - Software encoding
    /// - `auto` / None - Auto-select best available
    pub fn from_transcode_profile(available: &[String], transcode_profile: Option<&str>) -> Self {
        let profile = transcode_profile.map(|s| s.to_lowercase());

        match profile.as_deref() {
            // Hardware profiles
            Some(p) if p.starts_with("hw:") => {
                let hw_type = &p[3..];
                match hw_type {
                    "nvenc" | "nvidia" | "cuda" => {
                        if encoder_listed(available, "nvenc") {
                            Self::nvenc()
                        } else {
                            tracing::warn!(
                                "NVENC requested but not available, falling back to software"
                            );
                            Self::software()
                        }
                    }
                    "qsv" | "intel" | "quicksync" => {
                        if encoder_listed(available, "qsv") {
                            Self::qsv()
                        } else {
                            tracing::warn!(
                                "QSV requested but not available, falling back to software"
                            );
                            Self::software()
                        }
                    }
                    "vaapi" => {
                        if encoder_listed(available, "vaapi") {
                            Self::vaapi()
                        } else {
                            tracing::warn!(
                                "VAAPI requested but not available, falling back to software"
                            );
                            Self::software()
                        }
                    }
                    "videotoolbox" | "vt" | "apple" => {
                        if encoder_listed(available, "videotoolbox") {
                            Self::videotoolbox()
                        } else {
                            tracing::warn!(
                                "VideoToolbox requested but not available, falling back to software"
                            );
                            Self::software()
                        }
                    }
                    "v4l2" | "v4l2m2m" => {
                        if encoder_listed(available, "v4l2m2m") {
                            Self::v4l2m2m()
                        } else {
                            tracing::warn!(
                                "V4L2M2M requested but not available, falling back to software"
                            );
                            Self::software()
                        }
                    }
                    _ => {
                        tracing::warn!(
                            "Unknown hardware profile '{}', falling back to auto",
                            hw_type
                        );
                        Self::auto_select(available)
                    }
                }
            }
            // Software encoding
            Some("sw") | Some("software") | Some("cpu") => Self::software(),
            // Auto-select
            Some("auto") | None => Self::auto_select(available),
            // Accept bare hardware encoder names (nvenc, qsv, vaapi, etc.)
            Some("nvenc") | Some("nvidia") | Some("cuda") => {
                if encoder_listed(available, "nvenc") {
                    Self::nvenc()
                } else {
                    tracing::warn!("NVENC requested but not available, falling back to software");
                    Self::software()
                }
            }
            Some("qsv") | Some("intel") | Some("quicksync") => {
                if encoder_listed(available, "qsv") {
                    Self::qsv()
                } else {
                    tracing::warn!("QSV requested but not available, falling back to software");
                    Self::software()
                }
            }
            Some("vaapi") => {
                if encoder_listed(available, "vaapi") {
                    Self::vaapi()
                } else {
                    tracing::warn!("VAAPI requested but not available, falling back to software");
                    Self::software()
                }
            }
            Some("videotoolbox") | Some("vt") => {
                if encoder_listed(available, "videotoolbox") {
                    Self::videotoolbox()
                } else {
                    tracing::warn!(
                        "VideoToolbox requested but not available, falling back to software"
                    );
                    Self::software()
                }
            }
            Some("v4l2") | Some("v4l2m2m") => {
                if encoder_listed(available, "v4l2m2m") {
                    Self::v4l2m2m()
                } else {
                    tracing::warn!("V4L2M2M requested but not available, falling back to software");
                    Self::software()
                }
            }
            // Unknown profile, try auto
            Some(other) => {
                tracing::warn!("Unknown transcode profile '{}', using auto", other);
                Self::auto_select(available)
            }
        }
    }

    /// Auto-select best available encoder.
    ///
    /// The "available" list can come from `ffmpeg -encoders`, which only proves
    /// the encoder was compiled into FFmpeg. It does not prove that the local
    /// GPU, driver, pixel formats, or session limits can actually open it. Auto
    /// mode therefore uses hardware only when the caller marks it as verified.
    /// Explicit profiles such as `hw:nvenc` still opt in to listed encoders.
    fn auto_select(available: &[String]) -> Self {
        if encoder_verified(available, "nvenc") {
            tracing::info!("Auto-selected NVENC hardware encoder");
            Self::nvenc()
        } else if encoder_verified(available, "qsv") {
            tracing::info!("Auto-selected Intel QSV hardware encoder");
            Self::qsv()
        } else if encoder_verified(available, "videotoolbox") {
            tracing::info!("Auto-selected VideoToolbox hardware encoder");
            Self::videotoolbox()
        } else if encoder_verified(available, "vaapi") {
            tracing::info!("Auto-selected VAAPI hardware encoder");
            Self::vaapi()
        } else if encoder_verified(available, "v4l2m2m") {
            tracing::info!("Auto-selected V4L2M2M hardware encoder");
            Self::v4l2m2m()
        } else {
            if ["nvenc", "qsv", "videotoolbox", "vaapi", "v4l2m2m"]
                .iter()
                .any(|encoder| encoder_listed(available, encoder))
            {
                tracing::info!(
                    available = ?available,
                    "Hardware encoders are listed but not verified for auto mode; using software (libx264)"
                );
            } else {
                tracing::info!("No hardware encoders available, using software (libx264)");
            }
            Self::software()
        }
    }

    /// NVIDIA NVENC encoder (Windows/Linux)
    pub fn nvenc() -> Self {
        Self {
            encoder: "h264_nvenc".into(),
            hwaccel: Some("cuda".into()),
            device: None,
            extra_args: vec![
                "-preset".into(),
                "p4".into(), // Balanced preset (p1=fastest, p7=slowest)
                "-rc".into(),
                "vbr".into(), // Variable bitrate
            ],
            pix_fmt: Some("yuv420p".into()),
        }
    }

    /// Intel Quick Sync Video encoder (Windows/Linux)
    pub fn qsv() -> Self {
        Self {
            encoder: "h264_qsv".into(),
            hwaccel: Some(if cfg!(target_os = "windows") { "d3d11va".into() } else { "qsv".into() }),
            device: None,
            extra_args: vec!["-preset".into(), "veryfast".into()],
            pix_fmt: None, // QSV handles format internally
        }
    }

    /// VAAPI encoder (Linux - Intel/AMD)
    pub fn vaapi() -> Self {
        Self {
            encoder: "h264_vaapi".into(),
            hwaccel: Some("vaapi".into()),
            device: Some("/dev/dri/renderD128".into()),
            extra_args: vec!["-vaapi_device".into(), "/dev/dri/renderD128".into()],
            pix_fmt: None, // VAAPI handles format internally
        }
    }

    /// VideoToolbox encoder (macOS)
    pub fn videotoolbox() -> Self {
        Self {
            encoder: "h264_videotoolbox".into(),
            hwaccel: Some("videotoolbox".into()),
            device: None,
            extra_args: vec![
                "-realtime".into(),
                "1".into(),
                "-allow_sw".into(),
                "1".into(), // Allow software fallback
            ],
            pix_fmt: None,
        }
    }

    /// V4L2 Memory-to-Memory encoder (Linux - Raspberry Pi, ARM)
    pub fn v4l2m2m() -> Self {
        Self {
            encoder: "h264_v4l2m2m".into(),
            hwaccel: None,
            device: None,
            extra_args: vec![],
            pix_fmt: None,
        }
    }

    /// Software encoder (libx264) - universal fallback
    pub fn software() -> Self {
        Self {
            encoder: "libx264".into(),
            hwaccel: None,
            device: None,
            extra_args: vec![
                "-preset".into(),
                "veryfast".into(),
                "-tune".into(),
                "zerolatency".into(),
            ],
            pix_fmt: Some("yuv420p".into()),
        }
    }

    /// Check if this is a hardware encoder
    pub fn is_hardware(&self) -> bool {
        self.hwaccel.is_some()
    }

    /// Get encoder name for logging
    pub fn name(&self) -> &str {
        &self.encoder
    }
}

fn encoder_verified(available: &[String], encoder: &str) -> bool {
    available.iter().any(|name| {
        let normalized = name.to_ascii_lowercase();
        normalized == format!("{encoder}:verified")
            || normalized == format!("{encoder}:usable")
            || normalized == format!("{encoder}:ok")
    })
}

fn encoder_listed(available: &[String], encoder: &str) -> bool {
    available
        .iter()
        .any(|name| name.eq_ignore_ascii_case(encoder))
        || encoder_verified(available, encoder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_select_ignores_unverified_nvenc() {
        let available = vec!["nvenc".to_string(), "qsv".to_string()];
        let config = HwAccelConfig::from_transcode_profile(&available, None);
        assert_eq!(config.encoder, "libx264");
    }

    #[test]
    fn test_auto_select_with_verified_nvenc() {
        let available = vec!["nvenc:verified".to_string(), "qsv".to_string()];
        let config = HwAccelConfig::from_transcode_profile(&available, None);
        assert_eq!(config.encoder, "h264_nvenc");
        assert_eq!(config.pix_fmt.as_deref(), Some("yuv420p"));
    }

    #[test]
    fn test_explicit_nvenc_can_use_listed_encoder() {
        let available = vec!["nvenc".to_string()];
        let config = HwAccelConfig::from_transcode_profile(&available, Some("hw:nvenc"));
        assert_eq!(config.encoder, "h264_nvenc");
    }

    #[test]
    fn test_software_fallback() {
        let available: Vec<String> = vec![];
        let config = HwAccelConfig::from_transcode_profile(&available, Some("auto"));
        assert_eq!(config.encoder, "libx264");
    }

    #[test]
    fn test_explicit_hw_profile() {
        let available = vec!["nvenc".to_string(), "qsv".to_string()];
        let config = HwAccelConfig::from_transcode_profile(&available, Some("hw:qsv"));
        assert_eq!(config.encoder, "h264_qsv");
    }

    #[test]
    fn test_software_profile() {
        let available = vec!["nvenc".to_string()];
        let config = HwAccelConfig::from_transcode_profile(&available, Some("sw"));
        assert_eq!(config.encoder, "libx264");
    }

    #[test]
    fn test_unavailable_hw_fallback() {
        let available = vec!["qsv".to_string()];
        let config = HwAccelConfig::from_transcode_profile(&available, Some("hw:nvenc"));
        // Should fall back to software since nvenc not available
        assert_eq!(config.encoder, "libx264");
    }
}
