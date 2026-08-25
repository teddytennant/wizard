//! A [`TranscriptModel`] as the blocks the selection layer draws.
//!
//! [`crate::transcript`] is the one reading of a conversation, shared by every
//! surface, and it carries no rendering at all on purpose: a [`TranscriptItem`]
//! says what happened, and each surface decides what that looks like. This is
//! the native GUI's decision, and it is the *whole* of it — nothing below here
//! knows what a tool call is.
//!
//! # Everything is a block, including the chrome
//!
//! A tool row is not a widget with a header and a body. It is two blocks, one
//! after the other, differing in font and fill, and the reason is the
//! acceptance criterion for this workstream: a selection has to run from prose,
//! through a code block, into a tool row. Anything drawn outside
//! [`super::super::select`] is a hole in that drag. So the glyph, the tool name,
//! the summary and the output are all *text*, in one flat vector, and the
//! visual grouping is done with indent, fill and gap.
//!
//! The cost is stated rather than hidden: a tool row cannot be collapsed, and
//! images render as the file they point at rather than as pixels, because
//! neither is text. Both are Phase 2 (`docs/native-gui.md`).
//!
//! # Where the streaming tail goes
//!
//! [`TranscriptModel::streaming`] holds reasoning and text that have not been
//! committed to items yet. They are appended as two more blocks, below
//! everything, which means a token arriving changes the *last* block and no
//! other — so the paragraph cache reshapes one paragraph per frame while the
//! model is writing, rather than the conversation.

use crate::plugins::native::font;
use crate::plugins::native::select::Block;
use crate::plugins::native::theme::Palette;
use crate::theme::Token;
use crate::transcript::{TranscriptItem, TranscriptModel, summarize_tool};

use iced::advanced::text::Span;

use super::markdown::{self, BODY, MONO, bold};

/// How much of a tool's output is drawn.
///
/// A `find /` writes megabytes, and shaping a megabyte-long paragraph blocks
/// the frame that tries. The **head** is kept, unlike the live-output tail in
/// [`crate::transcript::ToolItem::progress`] and for the opposite reason: a
/// finished result is read from the top (the first error, the first match),
/// where a running command's prompt is always the last line.
const OUTPUT_CHARS: usize = 4_000;
/// And a line cap, because 4,000 characters of one-character lines is 4,000
/// visual lines.
const OUTPUT_LINES: usize = 60;

/// How far a tool's body, a thought, or an attachment list is inset under the
/// thing it belongs to.
const INSET: f32 = 18.0;

/// Every block the conversation draws as, in order.
///
/// One pass, no caching: it is a walk over owned data that produces owned data,
/// and the expensive step (shaping) happens above this, in
/// [`super::super::select::cache`], which is keyed by content and so does not
/// care that this vector is rebuilt.
pub fn blocks(model: &TranscriptModel, palette: &Palette) -> Vec<Block> {
    let mut out = Vec::new();
    for item in model.items() {
        item_blocks(item, palette, &mut out);
    }
    let (thinking, text) = model.streaming();
    if !thinking.is_empty() {
        push_thinking(thinking, palette, &mut out);
    }
    if !text.is_empty() {
        markdown::render(text, palette, 0.0, &mut out);
    }
    out
}

fn item_blocks(item: &TranscriptItem, palette: &Palette, out: &mut Vec<Block>) {
    match item {
        // A turn boundary is a rule with a number on it, not a heading: it
        // marks where a resumed session's turns begin and is chrome, so it
        // stays faint enough to scroll past.
        TranscriptItem::TurnMarker { turn, prompt } => out.push(
            Block::plain_text(
                format!("── turn {turn} · {}", first_line(prompt)),
                MONO,
                font::SANS,
                palette.color(Token::Faint),
            )
            .rule(palette.separator)
            .gap(12.0),
        ),
        // The design spec asks for a right-aligned bubble. A `Block` has no
        // alignment — laying out is the selection widget's job and it stacks
        // left — so what is drawn instead is the same asymmetry expressed the
        // way this layer can: the user's own words get a surface and an accent
        // rule, the agent's get neither. Bubbles are Phase 2 and want an
        // alignment field on `Block`.
        TranscriptItem::User { text, images } => {
            out.push(
                Block::plain_text(text.clone(), BODY, font::SANS, palette.color(Token::Text))
                    .indent(INSET)
                    .rule(palette.color(Token::Accent))
                    .gap(if images.is_empty() { 14.0 } else { 4.0 }),
            );
            push_attachments(
                images.iter().map(|image| image.path.display().to_string()),
                palette,
                out,
            );
        }
        TranscriptItem::Text(text) => markdown::render(text, palette, 0.0, out),
        TranscriptItem::Thinking(text) => push_thinking(text, palette, out),
        TranscriptItem::Tool(tool) => {
            let (glyph, token) = match &tool.output {
                None => ("▸", Token::ToolRunning),
                Some(output) if output.is_error => ("✗", Token::ToolFailed),
                Some(_) => ("✓", Token::ToolDone),
            };
            let summary = tool
                .output
                .as_ref()
                .map(|output| summarize_tool(&tool.name, &tool.args, &output.content))
                .unwrap_or_else(|| summarize_tool(&tool.name, &tool.args, ""));
            // Header: glyph, name, summary. Three spans, one block, so a
            // selection can start halfway through the tool's name.
            out.push(
                Block::new(
                    vec![
                        Span::new(format!("{glyph} "))
                            .font(font::MONO)
                            .size(MONO)
                            .color(palette.color(token)),
                        Span::new(tool.name.clone())
                            .font(bold(font::MONO))
                            .size(MONO)
                            .color(palette.color(Token::Text)),
                        Span::new(format!("  {summary}"))
                            .font(font::MONO)
                            .size(MONO)
                            .color(palette.color(Token::Muted)),
                    ],
                    MONO,
                    font::MONO,
                )
                .gap(4.0),
            );
            // A running command's live output, then (once it lands) the
            // result. Never both: `answer_tool` clears the progress tail when
            // it writes the result, precisely so this does not print twice.
            let body = match &tool.output {
                Some(output) => output.content.as_str(),
                None => tool.progress.as_str(),
            };
            if !body.trim().is_empty() {
                out.push(
                    Block::plain_text(clip(body), MONO, font::MONO, palette.color(Token::Muted))
                        .indent(INSET)
                        .fill(palette.raised)
                        .gap(12.0),
                );
            }
        }
        // Images are files on disk. Drawing them needs a widget that is not
        // text and so cannot be selected across; naming them keeps the path
        // copyable, which is the part a user actually asks for.
        TranscriptItem::Images { source, images } => {
            let from = source.tool().unwrap_or("the model");
            out.push(
                Block::plain_text(
                    format!("{} image(s) from {from}", images.len()),
                    MONO,
                    font::SANS,
                    palette.color(Token::Faint),
                )
                .indent(INSET)
                .gap(2.0),
            );
            push_attachments(
                images.iter().map(|image| image.path.display().to_string()),
                palette,
                out,
            );
        }
        TranscriptItem::Notice(text) => out.push(
            Block::plain_text(text.clone(), MONO, font::SANS, palette.color(Token::Muted))
                .indent(INSET)
                .gap(10.0),
        ),
    }
}

/// Reasoning, dimmed and inset. Not markdown: a model's scratchpad is not
/// authored prose, and rendering its stray backticks as code blocks makes it
/// harder to skim rather than easier.
fn push_thinking(text: &str, palette: &Palette, out: &mut Vec<Block>) {
    out.push(
        Block::new(
            vec![
                Span::new(text.to_string())
                    .font(markdown::italic(font::SANS))
                    .size(BODY)
                    .color(palette.color(Token::Faint)),
            ],
            BODY,
            font::SANS,
        )
        .indent(INSET)
        .gap(12.0),
    );
}

/// One faint mono line per attached file. Mono because a path is a thing you
/// could paste into a terminal (`docs/gui-design-spec.md`, "Sans for prose,
/// mono for literals").
fn push_attachments(paths: impl Iterator<Item = String>, palette: &Palette, out: &mut Vec<Block>) {
    for path in paths {
        out.push(
            Block::plain_text(path, MONO, font::MONO, palette.color(Token::Faint))
                .indent(INSET + 8.0)
                .gap(2.0),
        );
    }
}

/// The first line of `text`, for a one-line label.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Clip a tool's output to something a paragraph can shape, saying so when it
/// had to. Silently truncating output would make "the build printed nothing
/// after line 60" indistinguishable from "the build stopped at line 60".
fn clip(text: &str) -> String {
    let mut end = text.len().min(OUTPUT_CHARS);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let (head, dropped_lines) = match head.match_indices('\n').nth(OUTPUT_LINES) {
        Some((at, _)) => (&head[..at], true),
        None => (head, false),
    };
    if dropped_lines || end < text.len() {
        let remaining = text.len() - head.len();
        format!("{}\n… {remaining} more bytes", head.trim_end())
    } else {
        head.to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::{AgentEvent, ImageSource};
    use crate::images::ImageRef;
    use crate::tools::ToolOutput;

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    fn texts(model: &TranscriptModel) -> Vec<String> {
        blocks(model, &palette())
            .into_iter()
            .map(|block| block.plain().to_string())
            .collect()
    }

    /// A whole turn, in order: the prompt, the reply's prose, its code block,
    /// the tool row and the tool's body. This is the shape the selection tests
    /// drag across, so if this order ever changes they change with it.
    #[test]
    fn a_turn_renders_prompt_prose_code_and_tool_rows() {
        let mut model = TranscriptModel::new();
        model.user("fix the lock".to_string(), Vec::new());
        model.apply(&AgentEvent::TextDelta(
            "Found a stale lock file.\n\n```rust\nfn main() {}\n```\n".to_string(),
        ));
        model.apply(&AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: json!({ "command": "rm -f .lock" }),
        });
        model.apply(&AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::ok("removed .lock"),
        });

        assert_eq!(
            texts(&model),
            [
                "fix the lock",
                "Found a stale lock file.",
                "fn main() {}",
                "✓ execute  rm -f .lock",
                "removed .lock",
            ]
        );
    }

    /// The glyph and its colour are how a failure is legible. Both change, and
    /// the token is the theme's, not a literal.
    #[test]
    fn a_failed_tool_row_is_marked_and_colored() {
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "gone.rs" }),
        });
        model.apply(&AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: ToolOutput::error("error: no such file"),
        });

        let out = blocks(&model, &palette());
        assert!(out[0].plain().starts_with("✗ read_file"));
        assert_eq!(
            out[0].spans[0].color,
            Some(palette().color(Token::ToolFailed))
        );
    }

    /// The uncommitted tail draws below the items, so a reply appears as it is
    /// written rather than when it is finished.
    #[test]
    fn the_streaming_tail_draws_last() {
        let mut model = TranscriptModel::new();
        model.user("hi".to_string(), Vec::new());
        model.apply(&AgentEvent::ThinkingDelta("weighing it up".to_string()));
        model.apply(&AgentEvent::TextDelta("half a rep".to_string()));
        assert_eq!(texts(&model), ["hi", "weighing it up", "half a rep"]);
    }

    /// A running command's live output shows on its own row while the call is
    /// still open — that is the whole point of `ToolItem::progress` — and is
    /// replaced, not doubled, when the result lands.
    #[test]
    fn live_command_output_shows_and_is_then_replaced() {
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: json!({ "command": "npm init" }),
        });
        model.apply(&AgentEvent::ConsoleOutput {
            gate: serde_json::from_str("1").expect("a ticket is a number"),
            chunk: "package name: ".to_string(),
        });
        assert_eq!(texts(&model), ["▸ execute  npm init", "package name: "]);

        model.apply(&AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::ok("package name: wizard\ndone"),
        });
        assert_eq!(
            texts(&model),
            ["✓ execute  npm init", "package name: wizard\ndone"]
        );
    }

    /// Enormous output is clipped, and says that it was. A silent truncation
    /// makes "printed nothing more" and "we stopped showing you" identical.
    #[test]
    fn huge_output_is_clipped_and_admits_it() {
        let body = "line\n".repeat(5_000);
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: json!({ "command": "find /" }),
        });
        model.apply(&AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::ok(body.clone()),
        });

        let out = blocks(&model, &palette());
        let shown = out[1].plain();
        assert!(shown.len() < body.len());
        assert!(
            shown.lines().count() <= OUTPUT_LINES + 2,
            "{}",
            shown.lines().count()
        );
        assert!(shown.ends_with("more bytes"), "{shown:?}");
    }

    /// An image is named rather than drawn, and the name is the path, in mono,
    /// so it can be copied out. Losing the path would make the feature
    /// unusable rather than merely unfinished.
    #[test]
    fn images_are_named_with_their_paths() {
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::Images {
            source: ImageSource::Tool("render".to_string()),
            images: vec![ImageRef {
                path: "/img/hat.png".into(),
                mime: "image/png".to_string(),
                bytes: 2,
            }],
        });
        assert_eq!(texts(&model), ["1 image(s) from render", "/img/hat.png"]);
    }

    /// An empty conversation draws nothing. A window that opened onto one
    /// placeholder block would put a selectable phantom row in every new chat.
    #[test]
    fn an_empty_transcript_has_no_blocks() {
        assert!(blocks(&TranscriptModel::new(), &palette()).is_empty());
    }
}
