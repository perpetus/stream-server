//! Subtitle parsing and conversion module
//!
//! Supports:
//! - SRT (SubRip) format parsing
//! - ASS/SSA (Advanced SubStation Alpha) format parsing with inline styling
//! - VTT (WebVTT) output with styling preservation

use regex::Regex;

/// Subtitle format detection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubFormat {
    Srt,
    Ass,
    Vtt,
    Unknown,
}

/// Subtitle styling information
#[derive(Debug, Clone, Default)]
pub struct CueStyle {
    /// CSS color (e.g., "#FF0000", "red")
    pub color: Option<String>,
    /// Bold text
    pub bold: bool,
    /// Italic text
    pub italic: bool,
    /// Underline text
    pub underline: bool,
}

/// A single subtitle cue
#[derive(Debug, Clone)]
pub struct SubtitleCue {
    /// Start time in milliseconds
    pub start_ms: u64,
    /// End time in milliseconds
    pub end_ms: u64,
    /// Subtitle text (may contain VTT formatting tags)
    pub text: String,
    /// Optional styling
    pub style: Option<CueStyle>,
}

/// Parsed subtitle document
#[derive(Debug, Clone)]
pub struct ParsedSubtitle {
    /// Detected format
    pub format: SubFormat,
    /// List of cues
    pub cues: Vec<SubtitleCue>,
}

impl ParsedSubtitle {
    /// Parse subtitle content, auto-detecting format
    pub fn parse(content: &str) -> Self {
        let format = detect_format(content);
        let cues = match format {
            SubFormat::Srt => parse_srt(content),
            SubFormat::Ass => parse_ass(content),
            SubFormat::Vtt => parse_vtt(content),
            SubFormat::Unknown => parse_srt(content), // Fallback to SRT
        };
        Self { format, cues }
    }

    /// Convert to WebVTT format
    pub fn to_vtt(&self) -> String {
        let mut output = String::from("WEBVTT\n\n");

        for (i, cue) in self.cues.iter().enumerate() {
            // Cue identifier (optional but helpful)
            output.push_str(&format!("{}\n", i + 1));

            // Timestamps
            output.push_str(&format!(
                "{} --> {}\n",
                format_vtt_time(cue.start_ms),
                format_vtt_time(cue.end_ms)
            ));

            // Text with styling
            let styled_text = apply_vtt_styling(&cue.text, cue.style.as_ref());
            output.push_str(&styled_text);
            output.push_str("\n\n");
        }

        output
    }
}

/// Detect subtitle format from content
fn detect_format(content: &str) -> SubFormat {
    let trimmed = content.trim();

    if trimmed.starts_with("WEBVTT") {
        return SubFormat::Vtt;
    }

    if trimmed.contains("[Script Info]") || trimmed.contains("ScriptType:") {
        return SubFormat::Ass;
    }

    // Check for SRT pattern (number, timestamp, text)
    let srt_pattern = Regex::new(r"^\d+\s*\r?\n\d{2}:\d{2}:\d{2},\d{3}").unwrap();
    if srt_pattern.is_match(trimmed) {
        return SubFormat::Srt;
    }

    SubFormat::Unknown
}

/// Parse SRT format
fn parse_srt(content: &str) -> Vec<SubtitleCue> {
    let mut cues = Vec::new();

    // SRT block pattern: index, timestamp line, text lines
    let block_pattern = Regex::new(
        r"(?m)^\d+\s*\r?\n(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*-->\s*(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*\r?\n([\s\S]*?)(?=\r?\n\r?\n\d+\s*\r?\n|\r?\n\r?\n*$|$)"
    ).unwrap();

    for caps in block_pattern.captures_iter(content) {
        let start_ms = parse_time_components(
            caps.get(1).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(2).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(3).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(4).map(|m| m.as_str()).unwrap_or("0"),
        );

        let end_ms = parse_time_components(
            caps.get(5).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(6).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(7).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(8).map(|m| m.as_str()).unwrap_or("0"),
        );

        let text = caps.get(9).map(|m| m.as_str()).unwrap_or("").trim();

        // Parse inline SRT styling tags
        let (clean_text, style) = parse_srt_styling(text);

        cues.push(SubtitleCue {
            start_ms,
            end_ms,
            text: clean_text,
            style,
        });
    }

    cues
}

/// Parse ASS/SSA format
fn parse_ass(content: &str) -> Vec<SubtitleCue> {
    let mut cues = Vec::new();

    // Find [Events] section and parse Dialogue lines
    let dialogue_pattern = Regex::new(
        r"Dialogue:\s*\d+,(\d+):(\d{2}):(\d{2})\.(\d{2}),(\d+):(\d{2}):(\d{2})\.(\d{2}),[^,]*,[^,]*,\d+,\d+,\d+,[^,]*,(.*)"
    ).unwrap();

    for caps in dialogue_pattern.captures_iter(content) {
        // ASS uses centiseconds (1/100th second)
        let start_ms = parse_ass_time(
            caps.get(1).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(2).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(3).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(4).map(|m| m.as_str()).unwrap_or("0"),
        );

        let end_ms = parse_ass_time(
            caps.get(5).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(6).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(7).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(8).map(|m| m.as_str()).unwrap_or("0"),
        );

        let raw_text = caps.get(9).map(|m| m.as_str()).unwrap_or("");

        // Parse ASS inline tags and convert to clean text with styling
        let (clean_text, style) = parse_ass_styling(raw_text);

        cues.push(SubtitleCue {
            start_ms,
            end_ms,
            text: clean_text,
            style,
        });
    }

    cues
}

/// Parse VTT format (passthrough with minimal processing)
fn parse_vtt(content: &str) -> Vec<SubtitleCue> {
    let mut cues = Vec::new();

    // VTT timestamp pattern
    let cue_pattern = Regex::new(
        r"(\d{2}):(\d{2}):(\d{2})\.(\d{3})\s*-->\s*(\d{2}):(\d{2}):(\d{2})\.(\d{3})[^\n]*\n([\s\S]*?)(?=\n\n|\n*$)"
    ).unwrap();

    for caps in cue_pattern.captures_iter(content) {
        let start_ms = parse_time_components(
            caps.get(1).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(2).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(3).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(4).map(|m| m.as_str()).unwrap_or("0"),
        );

        let end_ms = parse_time_components(
            caps.get(5).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(6).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(7).map(|m| m.as_str()).unwrap_or("0"),
            caps.get(8).map(|m| m.as_str()).unwrap_or("0"),
        );

        let text = caps
            .get(9)
            .map(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        cues.push(SubtitleCue {
            start_ms,
            end_ms,
            text,
            style: None,
        });
    }

    cues
}

/// Parse time components (HH:MM:SS,mmm or HH:MM:SS.mmm)
fn parse_time_components(hours: &str, mins: &str, secs: &str, millis: &str) -> u64 {
    let h: u64 = hours.parse().unwrap_or(0);
    let m: u64 = mins.parse().unwrap_or(0);
    let s: u64 = secs.parse().unwrap_or(0);
    let ms: u64 = millis.parse().unwrap_or(0);

    h * 3600000 + m * 60000 + s * 1000 + ms
}

/// Parse ASS time format (H:MM:SS.cc where cc is centiseconds)
fn parse_ass_time(hours: &str, mins: &str, secs: &str, centis: &str) -> u64 {
    let h: u64 = hours.parse().unwrap_or(0);
    let m: u64 = mins.parse().unwrap_or(0);
    let s: u64 = secs.parse().unwrap_or(0);
    let cs: u64 = centis.parse().unwrap_or(0);

    h * 3600000 + m * 60000 + s * 1000 + cs * 10
}

/// Parse SRT inline styling tags (<b>, <i>, <u>, <font color="...">)
fn parse_srt_styling(text: &str) -> (String, Option<CueStyle>) {
    let mut style = CueStyle::default();
    let mut has_style = false;

    // Check for basic tags
    if text.contains("<b>") || text.contains("<B>") {
        style.bold = true;
        has_style = true;
    }
    if text.contains("<i>") || text.contains("<I>") {
        style.italic = true;
        has_style = true;
    }
    if text.contains("<u>") || text.contains("<U>") {
        style.underline = true;
        has_style = true;
    }

    // Check for font color
    let color_pattern = Regex::new(r#"<font\s+color=["']?([^"'>]+)["']?>"#).unwrap();
    if let Some(caps) = color_pattern.captures(text) {
        style.color = Some(caps.get(1).unwrap().as_str().to_string());
        has_style = true;
    }

    // Strip all HTML-like tags for clean text
    let tag_pattern = Regex::new(r"<[^>]+>").unwrap();
    let clean = tag_pattern.replace_all(text, "").to_string();

    (clean, if has_style { Some(style) } else { None })
}

/// Parse ASS inline styling tags ({\b1}, {\i1}, {\c&HBBGGRR&}, etc.)
fn parse_ass_styling(text: &str) -> (String, Option<CueStyle>) {
    let mut style = CueStyle::default();
    let mut has_style = false;

    // Check for bold {\b1}
    if text.contains(r"{\b1}") || text.contains(r"{\b1\") {
        style.bold = true;
        has_style = true;
    }

    // Check for italic {\i1}
    if text.contains(r"{\i1}") || text.contains(r"{\i1\") {
        style.italic = true;
        has_style = true;
    }

    // Check for underline {\u1}
    if text.contains(r"{\u1}") || text.contains(r"{\u1\") {
        style.underline = true;
        has_style = true;
    }

    // Check for color {\c&HBBGGRR&} or {\1c&HBBGGRR&}
    let color_pattern = Regex::new(r"\\(?:1?c)&H([0-9A-Fa-f]{6})&").unwrap();
    if let Some(caps) = color_pattern.captures(text) {
        let bgr = caps.get(1).unwrap().as_str();
        // Convert BGR to RGB
        if bgr.len() == 6 {
            let r = &bgr[4..6];
            let g = &bgr[2..4];
            let b = &bgr[0..2];
            style.color = Some(format!("#{}{}{}", r, g, b));
            has_style = true;
        }
    }

    // Strip ASS override tags and convert \N to newlines
    let tag_pattern = Regex::new(r"\{[^}]*\}").unwrap();
    let clean = tag_pattern.replace_all(text, "");
    let clean = clean.replace(r"\N", "\n").replace(r"\n", "\n");

    (
        clean.to_string(),
        if has_style { Some(style) } else { None },
    )
}

/// Format milliseconds as VTT timestamp (HH:MM:SS.mmm)
fn format_vtt_time(ms: u64) -> String {
    let hours = ms / 3600000;
    let mins = (ms % 3600000) / 60000;
    let secs = (ms % 60000) / 1000;
    let millis = ms % 1000;

    format!("{:02}:{:02}:{:02}.{:03}", hours, mins, secs, millis)
}

/// Apply VTT styling tags to text
fn apply_vtt_styling(text: &str, style: Option<&CueStyle>) -> String {
    let mut result = text.to_string();

    if let Some(s) = style {
        // Wrap with styling tags
        if s.bold {
            result = format!("<b>{}</b>", result);
        }
        if s.italic {
            result = format!("<i>{}</i>", result);
        }
        if s.underline {
            result = format!("<u>{}</u>", result);
        }
        // Note: VTT color requires CSS classes or styles which may not work in all players
        // We'll use the <c> tag with color class
        if let Some(ref color) = s.color {
            result = format!("<c.{}>{}</c>", color.trim_start_matches('#'), result);
        }
    }

    result
}

/// Convert raw subtitle content to VTT format (convenience function)
pub fn convert_to_vtt(content: &str) -> String {
    ParsedSubtitle::parse(content).to_vtt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_srt() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nHello world";
        assert_eq!(detect_format(srt), SubFormat::Srt);
    }

    #[test]
    fn test_detect_ass() {
        let ass = "[Script Info]\nScriptType: v4.00+";
        assert_eq!(detect_format(ass), SubFormat::Ass);
    }

    #[test]
    fn test_detect_vtt() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nHello";
        assert_eq!(detect_format(vtt), SubFormat::Vtt);
    }

    #[test]
    fn test_parse_srt() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nHello world\n\n2\n00:00:05,000 --> 00:00:08,000\nGoodbye";
        let parsed = ParsedSubtitle::parse(srt);
        assert_eq!(parsed.cues.len(), 2);
        assert_eq!(parsed.cues[0].start_ms, 1000);
        assert_eq!(parsed.cues[0].text, "Hello world");
    }

    #[test]
    fn test_vtt_output() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nHello world\n\n";
        let vtt = convert_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:04.000"));
    }
}
