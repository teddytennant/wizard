//! Agent-message markdown entry points: fence unwrapping for tables wrapped in ```md fences and
//! the streaming render that reports the last top-level block.

use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;

use ratatui::text::Line;

use super::table_detect;
use crate::hyperlink::HyperlinkLine;

/// Render an agent message to styled lines at `width` (the caller subtracts its gutter).
pub fn render_markdown_agent(
    markdown_source: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
) -> Vec<Line<'static>> {
    crate::hyperlink::visible_lines(render_markdown_agent_with_links_and_cwd(
        markdown_source,
        width,
        cwd,
    ))
}

pub fn render_markdown_agent_with_links_and_cwd(
    markdown_source: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
) -> Vec<HyperlinkLine> {
    let normalized = unwrap_markdown_fences(markdown_source);
    super::render_markdown_lines_with_width_and_cwd(&normalized, width, cwd)
}

/// If a normalized boundary cannot be expressed as a raw-source suffix, it is discarded so the
/// transformed block remains mutable.
pub fn render_streaming_markdown_agent_with_links_and_cwd(
    markdown_source: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
) -> super::StreamingMarkdownRender {
    let normalized = unwrap_markdown_fences(markdown_source);
    let mut rendered =
        super::render_streaming_markdown_lines_with_width_and_cwd(&normalized, width, cwd);
    if normalized != markdown_source {
        // Fence unwrapping removes opening/closing lines. A normalized tail that is still a raw
        // suffix necessarily begins after those removed lines, so its boundary can safely be
        // mapped back to the raw source; otherwise leave the transformed block mutable.
        rendered.last_top_level_block_start = rendered
            .last_top_level_block_start
            .and_then(|boundary| markdown_source.strip_suffix(&normalized[boundary..]))
            .map(str::len);
    }
    rendered
}

/// Strip `` ```md ``/`` ```markdown `` fences that contain tables, emitting their content as bare
/// markdown so `pulldown-cmark` parses the tables natively.
///
/// Fences whose info string is not `md` or `markdown` are passed through unchanged.  Markdown
/// fences that do *not* contain a table (detected by checking for a header row + delimiter row)
/// are also passed through so that non-table markdown inside a fence still renders as a code
/// block.
///
/// The fence unwrapping is intentionally conservative: it buffers the entire fence body before
/// deciding, and an unclosed fence at end-of-input is re-emitted with its opening line so partial
/// streams degrade to code display.
fn unwrap_markdown_fences<'a>(markdown_source: &'a str) -> Cow<'a, str> {
    // Zero-copy fast path: most messages contain no fences at all.
    if !markdown_source.contains("```") && !markdown_source.contains("~~~") {
        return Cow::Borrowed(markdown_source);
    }

    #[derive(Clone, Copy)]
    struct Fence {
        marker: u8,
        len: usize,
        is_blockquoted: bool,
    }

    // Strip a trailing newline and up to 3 leading spaces, returning the
    // trimmed slice.  Returns `None` when the line has 4+ leading spaces
    // (which makes it an indented code line per CommonMark).
    fn strip_line_indent(line: &str) -> Option<&str> {
        let without_newline = line.strip_suffix('\n').unwrap_or(line);
        let mut byte_idx = 0usize;
        let mut column = 0usize;
        for b in without_newline.as_bytes() {
            match b {
                b' ' => {
                    byte_idx += 1;
                    column += 1;
                }
                b'\t' => {
                    byte_idx += 1;
                    column += 4;
                }
                _ => break,
            }
            if column >= 4 {
                return None;
            }
        }
        Some(&without_newline[byte_idx..])
    }

    // Parse an opening fence line, returning the fence metadata and whether
    // the fence info string indicates markdown content.
    fn parse_open_fence(line: &str) -> Option<(Fence, bool)> {
        let trimmed = strip_line_indent(line)?;
        let is_blockquoted = trimmed.trim_start().starts_with('>');
        let fence_scan_text = table_detect::strip_blockquote_prefix(trimmed);
        let (marker, len) = table_detect::parse_fence_marker(fence_scan_text)?;
        let is_markdown = table_detect::is_markdown_fence_info(fence_scan_text, len);
        Some((
            Fence {
                marker: marker as u8,
                len,
                is_blockquoted,
            },
            is_markdown,
        ))
    }

    fn is_close_fence(line: &str, fence: Fence) -> bool {
        let Some(trimmed) = strip_line_indent(line) else {
            return false;
        };
        let fence_scan_text = if fence.is_blockquoted {
            if !trimmed.trim_start().starts_with('>') {
                return false;
            }
            table_detect::strip_blockquote_prefix(trimmed)
        } else {
            trimmed
        };
        if let Some((marker, len)) = table_detect::parse_fence_marker(fence_scan_text) {
            marker as u8 == fence.marker
                && len >= fence.len
                && fence_scan_text[len..].trim().is_empty()
        } else {
            false
        }
    }

    fn markdown_fence_contains_table(content: &str, is_blockquoted_fence: bool) -> bool {
        let mut previous_line: Option<&str> = None;
        for line in content.lines() {
            let text = if is_blockquoted_fence {
                table_detect::strip_blockquote_prefix(line)
            } else {
                line
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                previous_line = None;
                continue;
            }

            if let Some(previous) = previous_line
                && table_detect::is_table_header_line(previous)
                && !table_detect::is_table_delimiter_line(previous)
                && table_detect::is_table_delimiter_line(trimmed)
            {
                return true;
            }

            previous_line = Some(trimmed);
        }
        false
    }

    fn content_from_ranges(source: &str, ranges: &[Range<usize>]) -> String {
        let total_len: usize = ranges.iter().map(ExactSizeIterator::len).sum();
        let mut content = String::with_capacity(total_len);
        for range in ranges {
            content.push_str(&source[range.start..range.end]);
        }
        content
    }

    struct MarkdownCandidateData {
        fence: Fence,
        opening_range: Range<usize>,
        content_ranges: Vec<Range<usize>>,
    }

    // Box the large variant to keep ActiveFence small (~pointer-sized).
    enum ActiveFence {
        Passthrough(Fence),
        MarkdownCandidate(Box<MarkdownCandidateData>),
    }

    let mut out = String::with_capacity(markdown_source.len());
    let mut active_fence: Option<ActiveFence> = None;
    let mut source_offset = 0usize;

    let mut push_source_range = |range: Range<usize>| {
        if !range.is_empty() {
            out.push_str(&markdown_source[range]);
        }
    };

    for line in markdown_source.split_inclusive('\n') {
        let line_start = source_offset;
        source_offset += line.len();
        let line_range = line_start..source_offset;

        if let Some(active) = active_fence.take() {
            match active {
                ActiveFence::Passthrough(fence) => {
                    push_source_range(line_range);
                    if !is_close_fence(line, fence) {
                        active_fence = Some(ActiveFence::Passthrough(fence));
                    }
                }
                ActiveFence::MarkdownCandidate(mut data) => {
                    if is_close_fence(line, data.fence) {
                        if markdown_fence_contains_table(
                            &content_from_ranges(markdown_source, &data.content_ranges),
                            data.fence.is_blockquoted,
                        ) {
                            for range in data.content_ranges {
                                push_source_range(range);
                            }
                        } else {
                            push_source_range(data.opening_range);
                            for range in data.content_ranges {
                                push_source_range(range);
                            }
                            push_source_range(line_range);
                        }
                    } else {
                        data.content_ranges.push(line_range);
                        active_fence = Some(ActiveFence::MarkdownCandidate(data));
                    }
                }
            }
            continue;
        }

        if let Some((fence, is_markdown)) = parse_open_fence(line) {
            if is_markdown {
                active_fence = Some(ActiveFence::MarkdownCandidate(Box::new(
                    MarkdownCandidateData {
                        fence,
                        opening_range: line_range,
                        content_ranges: Vec::new(),
                    },
                )));
            } else {
                push_source_range(line_range);
                active_fence = Some(ActiveFence::Passthrough(fence));
            }
            continue;
        }

        push_source_range(line_range);
    }

    if let Some(active) = active_fence {
        match active {
            ActiveFence::Passthrough(_) => {}
            ActiveFence::MarkdownCandidate(data) => {
                push_source_range(data.opening_range);
                for range in data.content_ranges {
                    push_source_range(range);
                }
            }
        }
    }

    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    const LONG_MD: &str = include_str!("../../tests/fixtures/long_md.md");

    pub fn plain(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    #[ignore]
    fn print_long_md() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
        for w in [118usize, 78] {
            for l in render_markdown_agent(LONG_MD, Some(w), None) {
                println!("{}", plain(&l));
            }
            println!("=====");
        }
    }
}
