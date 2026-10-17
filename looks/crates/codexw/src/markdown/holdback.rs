//! Pipe-table holdback scanner for source-backed agent streams.
//!
//! Agent streams with markdown tables keep the active table as mutable tail so
//! adding a row can reflow earlier table rows instead of committing a stale
//! render to scrollback.
//!
//! The scanner is intentionally conservative: it only looks for enough
//! structure to decide where the mutable tail should start. It does not try to
//! validate an entire table or predict final layout. Rendering remains the job
//! of the markdown renderer.

use super::table_detect::FenceKind;
use super::table_detect::FenceTracker;
use super::table_detect::is_table_delimiter_line;
use super::table_detect::is_table_header_line;
use super::table_detect::parse_table_segments;
use super::table_detect::strip_blockquote_prefix;

/// Result of scanning accumulated raw source for pipe-table patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum TableHoldbackState {
    /// No table detected -- all rendered lines can flow into the stable queue.
    None,
    /// The last non-blank line looks like a table header row but no delimiter
    /// row has followed yet. Hold back in case the next delta is a delimiter.
    PendingHeader { header_start: usize },
    /// A header + delimiter pair was found -- the source contains a confirmed
    /// table. Content from the table header onward stays mutable.
    Confirmed { table_start: usize },
}

/// Facts remembered about the previous committed source line.
///
/// The scanner only needs one-line lookbehind because a table is confirmed by
/// a header row immediately followed by a delimiter row.
#[derive(Clone, Copy, Debug)]
struct PreviousLineState {
    source_start: usize,
    fence_kind: FenceKind,
    is_header: bool,
}

/// Incremental scanner for table holdback state on append-only source streams.
///
/// `push_source_chunk` must receive source in the same order it will be
/// appended to the stream. The scanner stores byte offsets into that logical
/// source buffer, so feeding chunks out of order would make later tail
/// boundaries point at the wrong rendered region.
#[derive(Debug, Default)]
pub struct TableHoldbackScanner {
    source_offset: usize,
    fence_tracker: FenceTracker,
    previous_line: Option<PreviousLineState>,
    pending_header_start: Option<usize>,
    confirmed_table_start: Option<usize>,
}

impl TableHoldbackScanner {
    pub fn new() -> Self {
        Self {
            source_offset: 0,
            fence_tracker: FenceTracker::new(),
            previous_line: None,
            pending_header_start: None,
            confirmed_table_start: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Return the current holdback decision for the committed source prefix.
    ///
    /// `PendingHeader` means the newest non-blank line looks like a table
    /// header but the delimiter row has not arrived yet, so callers should
    /// optimistically keep that region mutable. `Confirmed` means the header
    /// and delimiter pair has been seen and all subsequent body rows remain in
    /// the live tail until finalization.
    pub fn state(&self) -> TableHoldbackState {
        if let Some(table_start) = self.confirmed_table_start {
            TableHoldbackState::Confirmed { table_start }
        } else if let Some(header_start) = self.pending_header_start {
            TableHoldbackState::PendingHeader { header_start }
        } else {
            TableHoldbackState::None
        }
    }

    /// Advance the scanner with newly committed source.
    ///
    /// Chunks are expected to contain only source that is now safe to commit
    /// into `raw_source`, typically newline-terminated lines from the
    /// streaming collector. Partial rows are intentionally excluded so the
    /// scanner never treats an unfinished table row as a stable structural
    /// signal.
    pub fn push_source_chunk(&mut self, source_chunk: &str) {
        if source_chunk.is_empty() {
            return;
        }

        for source_line in source_chunk.split_inclusive('\n') {
            self.push_line(source_line);
        }
    }

    /// Fold one committed source line into the scanner state machine.
    fn push_line(&mut self, source_line: &str) {
        let line = source_line.strip_suffix('\n').unwrap_or(source_line);
        let source_start = self.source_offset;
        let fence_kind = self.fence_tracker.kind();

        let candidate_text = if fence_kind == FenceKind::Other {
            None
        } else {
            table_candidate_text(line)
        };
        let is_header = candidate_text.is_some_and(is_table_header_line);
        let is_delimiter = candidate_text.is_some_and(is_table_delimiter_line);

        if self.confirmed_table_start.is_none()
            && let Some(previous_line) = self.previous_line
            && previous_line.fence_kind != FenceKind::Other
            && fence_kind != FenceKind::Other
            && previous_line.is_header
            && is_delimiter
        {
            self.confirmed_table_start = Some(previous_line.source_start);
            self.pending_header_start = None;
        }

        if self.confirmed_table_start.is_none() && !line.trim().is_empty() {
            if fence_kind != FenceKind::Other && is_header {
                self.pending_header_start = Some(source_start);
            } else {
                self.pending_header_start = None;
            }
        }

        self.previous_line = Some(PreviousLineState {
            source_start,
            fence_kind,
            is_header,
        });

        self.fence_tracker.advance(line);
        self.source_offset = self.source_offset.saturating_add(source_line.len());
    }
}

/// Strip blockquote prefixes and return the trimmed text if it contains
/// pipe-table segments, or `None` otherwise.
fn table_candidate_text(line: &str) -> Option<&str> {
    let stripped = strip_blockquote_prefix(line).trim();
    parse_table_segments(stripped).map(|_| stripped)
}
