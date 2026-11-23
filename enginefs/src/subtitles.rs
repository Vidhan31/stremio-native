//! Subtitle parsing and conversion module
//!
//! Supports:
//! - SRT (SubRip) format parsing
//! - ASS/SSA (Advanced SubStation Alpha) format parsing with inline styling
//! - VTT (WebVTT) output with styling preservation

use regex::Regex;
use std::fmt::Write as _;
use std::sync::LazyLock;

static SRT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+\s*\r?\n\d{2}:\d{2}:\d{2},\d{3}").expect("valid regex"));
static SRT_TIMESTAMP_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*-->\s*(\d{2}):(\d{2}):(\d{2}),(\d{3})")
        .expect("valid regex")
});
static DIALOGUE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"Dialogue:\s*\d+,(\d+):(\d{2}):(\d{2})\.(\d{2}),(\d+):(\d{2}):(\d{2})\.(\d{2}),[^,]*,[^,]*,\d+,\d+,\d+,[^,]*,(.*)"
    ).expect("valid regex")
});
static VTT_CUE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(\d{2}):(\d{2}):(\d{2})\.(\d{3})\s*-->\s*(\d{2}):(\d{2}):(\d{2})\.(\d{3})")
        .expect("valid regex")
});
static HTML_COLOR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<font\s+color=["']?([^"'>]+)["']?>"#).expect("valid regex"));
static HTML_TAG_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid regex"));
static ASS_COLOR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\(?:1?c)&H([0-9A-Fa-f]{6})&").expect("valid regex"));
static ASS_TAG_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{[^}]*\}").expect("valid regex"));

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
            SubFormat::Srt | SubFormat::Unknown => parse_srt(content), // Fallback to SRT
            SubFormat::Ass => parse_ass(content),
            SubFormat::Vtt => parse_vtt(content),
        };
        Self { format, cues }
    }

    /// Convert to WebVTT format
    pub fn to_vtt(&self) -> String {
        let mut output = String::from("WEBVTT\n\n");

        for (i, cue) in self.cues.iter().enumerate() {
            // Cue identifier (optional but helpful)
            let _ = writeln!(output, "{}", i + 1);

            // Timestamps
            let _ = writeln!(
                output,
                "{} --> {}",
                format_vtt_time(cue.start_ms),
                format_vtt_time(cue.end_ms)
            );

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
    if SRT_PATTERN.is_match(trimmed) {
        return SubFormat::Srt;
    }

    SubFormat::Unknown
}

/// Parse SRT format
fn parse_srt(content: &str) -> Vec<SubtitleCue> {
    let mut cues = Vec::new();
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");

    for block in normalized.split("\n\n") {
        let mut lines = block.lines();
        let Some(_) = lines.next() else {
            continue;
        };
        let Some(timestamp_line) = lines.next() else {
            continue;
        };
        let Some(caps) = SRT_TIMESTAMP_PATTERN.captures(timestamp_line) else {
            continue;
        };

        let start_ms = parse_time_components(
            caps.get(1).map_or("0", |m| m.as_str()),
            caps.get(2).map_or("0", |m| m.as_str()),
            caps.get(3).map_or("0", |m| m.as_str()),
            caps.get(4).map_or("0", |m| m.as_str()),
        );

        let end_ms = parse_time_components(
            caps.get(5).map_or("0", |m| m.as_str()),
            caps.get(6).map_or("0", |m| m.as_str()),
            caps.get(7).map_or("0", |m| m.as_str()),
            caps.get(8).map_or("0", |m| m.as_str()),
        );

        let text = lines.collect::<Vec<_>>().join("\n");
        let text = text.trim();

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
    for caps in DIALOGUE_PATTERN.captures_iter(content) {
        // ASS uses centiseconds (1/100th second)
        let start_ms = parse_ass_time(
            caps.get(1).map_or("0", |m| m.as_str()),
            caps.get(2).map_or("0", |m| m.as_str()),
            caps.get(3).map_or("0", |m| m.as_str()),
            caps.get(4).map_or("0", |m| m.as_str()),
        );

        let end_ms = parse_ass_time(
            caps.get(5).map_or("0", |m| m.as_str()),
            caps.get(6).map_or("0", |m| m.as_str()),
            caps.get(7).map_or("0", |m| m.as_str()),
            caps.get(8).map_or("0", |m| m.as_str()),
        );

        let raw_text = caps.get(9).map_or("", |m| m.as_str());

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
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");

    for block in normalized.split("\n\n") {
        let lines = block.lines().collect::<Vec<_>>();
        let Some(timestamp_idx) = lines.iter().position(|line| line.contains("-->")) else {
            continue;
        };
        let Some(caps) = VTT_CUE_PATTERN.captures(lines[timestamp_idx]) else {
            continue;
        };

        let start_ms = parse_time_components(
            caps.get(1).map_or("0", |m| m.as_str()),
            caps.get(2).map_or("0", |m| m.as_str()),
            caps.get(3).map_or("0", |m| m.as_str()),
            caps.get(4).map_or("0", |m| m.as_str()),
        );

        let end_ms = parse_time_components(
            caps.get(5).map_or("0", |m| m.as_str()),
            caps.get(6).map_or("0", |m| m.as_str()),
            caps.get(7).map_or("0", |m| m.as_str()),
            caps.get(8).map_or("0", |m| m.as_str()),
        );

        let text = lines
            .iter()
            .skip(timestamp_idx + 1)
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
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

    h * 3_600_000 + m * 60_000 + s * 1000 + ms
}

/// Parse ASS time format (H:MM:SS.cc where cc is centiseconds)
fn parse_ass_time(hours: &str, mins: &str, secs: &str, centis: &str) -> u64 {
    let h: u64 = hours.parse().unwrap_or(0);
    let m: u64 = mins.parse().unwrap_or(0);
    let s: u64 = secs.parse().unwrap_or(0);
    let cs: u64 = centis.parse().unwrap_or(0);

    h * 3_600_000 + m * 60_000 + s * 1000 + cs * 10
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
    if let Some(caps) = HTML_COLOR_PATTERN.captures(text)
        && let Some(color_match) = caps.get(1)
    {
        style.color = Some(color_match.as_str().to_string());
        has_style = true;
    }

    // Strip all HTML-like tags for clean text
    let clean = HTML_TAG_PATTERN.replace_all(text, "").to_string();

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
    if let Some(caps) = ASS_COLOR_PATTERN.captures(text)
        && let Some(color_match) = caps.get(1)
    {
        let bgr = color_match.as_str();
        // Convert BGR to RGB
        if bgr.len() == 6 {
            let r = &bgr[4..6];
            let g = &bgr[2..4];
            let b = &bgr[0..2];
            style.color = Some(format!("#{r}{g}{b}"));
            has_style = true;
        }
    }

    // Strip ASS override tags and convert \N to newlines
    let clean = ASS_TAG_PATTERN.replace_all(text, "");
    let clean = clean.replace(r"\N", "\n").replace(r"\n", "\n");

    (clean, if has_style { Some(style) } else { None })
}

/// Format milliseconds as VTT timestamp (HH:MM:SS.mmm)
fn format_vtt_time(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let mins = (ms % 3_600_000) / 60_000;
    let secs = (ms % 60_000) / 1000;
    let millis = ms % 1000;

    format!("{hours:02}:{mins:02}:{secs:02}.{millis:03}")
}

/// Apply VTT styling tags to text
fn apply_vtt_styling(text: &str, style: Option<&CueStyle>) -> String {
    let mut result = text.to_string();

    if let Some(s) = style {
        // Wrap with styling tags
        if s.bold {
            result = format!("<b>{result}</b>");
        }
        if s.italic {
            result = format!("<i>{result}</i>");
        }
        if s.underline {
            result = format!("<u>{result}</u>");
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
