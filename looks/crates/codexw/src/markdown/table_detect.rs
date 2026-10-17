//! Canonical pipe-table structure detection and fenced-code-block tracking for
//! raw markdown source.
//!
//! Both the streaming controller (`streaming/controller.rs`) and the
//! markdown-fence unwrapper (`markdown.rs`) need to identify pipe-table
//! structure and fenced code blocks in raw markdown source.  This module
//! provides the canonical implementations so fixes only need to happen in one
//! place.
//!
//! ## Concepts
//!
//! A GFM pipe table is a sequence of lines where:
//! - A **header line** contains pipe-separated segments with at least one
//!   non-empty cell.
//! - A **delimiter line** immediately follows the header and contains only
//!   alignment markers (`---`, `:---`, `---:`, `:---:`), each with at least
//!   three dashes.
//! - **Body rows** follow the delimiter.
//!
//! A **fenced code block** starts with 3+ backticks or tildes and ends with a
//! matching close marker.  [`FenceTracker`] classifies each line as
//! [`FenceKind::Outside`], [`FenceKind::Markdown`], or [`FenceKind::Other`]
//! so callers can skip pipe characters that appear inside non-markdown fences.
//!
//! The table functions operate on single lines and do not maintain cross-line
//! state.  Callers (the streaming controller and fence unwrapper) are
//! responsible for pairing consecutive lines to confirm a table.

/// Split a pipe-delimited line into trimmed segments.
///
/// Returns `None` if the line is empty or has no unescaped separator marker.
/// Leading/trailing pipes are stripped before splitting.
///
/// This is intentionally a structural parser, not a renderer. It preserves
/// escaped pipes inside the returned segments because callers only care about
/// whether the line can participate in a table, not how the cell text should
/// finally be displayed.
pub fn parse_table_segments(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let has_outer_pipe = trimmed.starts_with('|') || trimmed.ends_with('|');
    let content = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let content = content.strip_suffix('|').unwrap_or(content);
    let raw_segments = split_unescaped_pipe(content);
    if !has_outer_pipe && raw_segments.len() <= 1 {
        return None;
    }

    let segments: Vec<&str> = raw_segments.into_iter().map(str::trim).collect();
    (!segments.is_empty()).then_some(segments)
}

/// Split `content` on unescaped `|` characters.
///
/// A pipe preceded by `\` is treated as literal text, not a column separator.
/// The backslash remains in the segment (this is structure detection, not
/// rendering).
fn split_unescaped_pipe(content: &str) -> Vec<&str> {
    let mut segments = Vec::with_capacity(8);
    let mut start = 0;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            // Skip the escaped character.
            i += 2;
        } else if bytes[i] == b'|' {
            segments.push(&content[start..i]);
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    segments.push(&content[start..]);
    segments
}

// Small table-detection helpers inlined for the streaming hot path — they are
// called on every source line during incremental holdback scanning.

/// Whether `line` looks like a table header row (has pipe-separated
/// segments with at least one non-empty cell).
#[inline]
pub fn is_table_header_line(line: &str) -> bool {
    parse_table_segments(line).is_some_and(|segments| segments.iter().any(|s| !s.is_empty()))
}

/// Whether a single segment matches the `---`, `:---`, `---:`, or `:---:`
/// alignment-colon syntax used in markdown table delimiter rows.
#[inline]
fn is_table_delimiter_segment(segment: &str) -> bool {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return false;
    }
    let without_leading = trimmed.strip_prefix(':').unwrap_or(trimmed);
    let without_ends = without_leading.strip_suffix(':').unwrap_or(without_leading);
    without_ends.len() >= 3 && without_ends.chars().all(|c| c == '-')
}

/// Whether `line` is a valid table delimiter row (every segment passes
/// [`is_table_delimiter_segment`]).
#[inline]
pub fn is_table_delimiter_line(line: &str) -> bool {
    parse_table_segments(line)
        .is_some_and(|segments| segments.into_iter().all(is_table_delimiter_segment))
}

// ---------------------------------------------------------------------------
// Fenced code block tracking
// ---------------------------------------------------------------------------

/// Where a source line sits relative to fenced code blocks.
///
/// Table holdback only applies to lines that are `Outside` or inside a
/// `Markdown` fence. Lines inside `Other` fences (e.g. `sh`, `rust`) are
/// ignored by the table scanner because their pipe characters are code, not
/// table syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceKind {
    /// Not inside any fenced code block.
    Outside,
    /// Inside a `` ```md `` or `` ```markdown `` fence.
    Markdown,
    /// Inside a fence with a non-markdown info string.
    Other,
}

/// Incremental tracker for fenced-code-block open/close transitions.
///
/// Feed lines one at a time via [`advance`](Self::advance); query the current
/// context with [`kind`](Self::kind).  The tracker handles leading-whitespace
/// limits (>3 spaces → not a fence), blockquote prefix stripping, and
/// backtick/tilde marker matching.
///
/// The tracker reports the fence context that applies to the current line
/// before that line mutates the state. Callers rely on that when deciding
/// whether the current raw line can open or continue a table.
#[derive(Debug, Default)]
pub struct FenceTracker {
    state: Option<(char, usize, FenceKind)>,
}

impl FenceTracker {
    #[inline]
    pub fn new() -> Self {
        Self { state: None }
    }

    /// Process one raw source line and update fence state.
    ///
    /// Lines with >3 leading spaces are ignored (indented code blocks, not
    /// fences).  Blockquote prefixes (`>`) are stripped before scanning.
    pub fn advance(&mut self, raw_line: &str) {
        let leading_spaces = raw_line
            .as_bytes()
            .iter()
            .take_while(|byte| **byte == b' ')
            .count();
        if leading_spaces > 3 {
            return;
        }

        let trimmed = &raw_line[leading_spaces..];
        let fence_scan_text = strip_blockquote_prefix(trimmed);
        if let Some((marker, len)) = parse_fence_marker(fence_scan_text) {
            if let Some((open_char, open_len, _)) = self.state {
                // Close the current fence if the marker matches.
                if marker == open_char
                    && len >= open_len
                    && fence_scan_text[len..].trim().is_empty()
                {
                    self.state = None;
                }
            } else {
                // Opening a new fence.
                let kind = if is_markdown_fence_info(fence_scan_text, len) {
                    FenceKind::Markdown
                } else {
                    FenceKind::Other
                };
                self.state = Some((marker, len, kind));
            }
        }
    }

    /// Current fence context for the most-recently-advanced line.
    #[inline]
    pub fn kind(&self) -> FenceKind {
        self.state.map_or(FenceKind::Outside, |(_, _, k)| k)
    }
}

/// Return fence marker character and run length for a potential fence line.
///
/// Recognises backtick and tilde fences with a minimum run of 3.
/// The input should already have leading whitespace and blockquote prefixes
/// stripped.
#[inline]
pub fn parse_fence_marker(line: &str) -> Option<(char, usize)> {
    let first = line.as_bytes().first().copied()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|&b| b == first).count();
    if len < 3 {
        return None;
    }
    Some((first as char, len))
}

/// Whether the info string after a fence marker indicates markdown content.
///
/// Matches `md` and `markdown` (case-insensitive).
#[inline]
pub fn is_markdown_fence_info(trimmed_line: &str, marker_len: usize) -> bool {
    let info = trimmed_line[marker_len..]
        .split_whitespace()
        .next()
        .unwrap_or_default();
    info.eq_ignore_ascii_case("md") || info.eq_ignore_ascii_case("markdown")
}

/// Peel all leading `>` blockquote markers from a line.
///
/// Tables can appear inside blockquotes (`> | A | B |`), so the holdback
/// scanner must strip these markers before checking for table syntax.
#[inline]
pub fn strip_blockquote_prefix(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        let Some(stripped) = rest.strip_prefix('>') else {
            return rest;
        };
        rest = stripped.strip_prefix(' ').unwrap_or(stripped).trim_start();
    }
}
