//! Native file tools: `read_file`, `write_file`, `edit_file`, `list_files`,
//! `search_files`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::shell::{render_command_result, run_command};
use super::{
    MAX_ERROR_BYTES, MAX_LISTING_BYTES, MAX_OUTPUT_BYTES, MAX_SEARCH_BYTES, Tool, ToolAccess,
    ToolContext, ToolError, ToolOutput, parse_args, resolve_path, truncate_output,
    truncate_output_without_spill,
};

/// Maximum number of lines a single `read_file` call returns.
const MAX_READ_LINES: usize = 2_000;

/// Maximum number of entries `list_files` returns.
const MAX_LIST_ENTRIES: usize = 500;

/// Cap on directory entries visited during a manual `list_files` walk, so a
/// glob over a huge tree cannot spin forever.
const MAX_WALK_VISITS: usize = 100_000;

/// Timeout for `git ls-files` when `list_files` uses it to honour
/// `.gitignore`; the manual walk takes over when it elapses.
const LS_FILES_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest file `read_file` will decode as an image.
///
/// Separate from [`crate::llm::MAX_IMAGE_BYTES`], which caps what may travel
/// to the model: a 40 MB PNG screenshot is over that cap but is worth
/// downscaling, while something the size of a video has nothing in it a
/// decoder should be pointed at.
const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Largest image `read_file` will decode, in pixels. A decoded frame is four
/// bytes a pixel, so this is a 160 MB allocation at the ceiling, and the
/// header carries the dimensions, so an image past it is refused before
/// anything is allocated.
const MAX_IMAGE_PIXELS: u64 = 40_000_000;

/// How many times an oversized image may be halved before `read_file` gives
/// up on getting it under [`crate::llm::MAX_IMAGE_BYTES`]. Four halvings take
/// a 16k-wide screenshot to 1000 px.
const MAX_DOWNSCALE_STEPS: u32 = 4;

/// Timeout for the external search process (`rg`/`grep`).
///
/// Twenty seconds, not the minute it used to be. `rg` walks a large
/// repository in well under a second and the fallback `grep` in a few, so a
/// search still going after twenty is not a slow search — it is a pattern
/// with catastrophic backtracking, or a path that wandered into a network
/// mount or `/proc`. None of those get better with another forty seconds, and
/// the matches found so far come back either way (the `timed_out` branch in
/// [`SearchFilesTool::execute`]), so the only thing the longer budget bought
/// was a longer stall.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Arguments for [`ReadFileTool`].
#[derive(Debug, Deserialize)]
pub struct ReadFileArgs {
    /// Path to read, relative to the project root or absolute.
    pub path: String,
    /// 1-based first line to include (default: start of file).
    #[serde(default)]
    pub start_line: Option<usize>,
    /// 1-based last line to include (default: end of file).
    #[serde(default)]
    pub end_line: Option<usize>,
}

/// `read_file` — read file contents with optional line range.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file, optionally limited to a 1-based line range. \
         An image file (png, jpeg, gif, webp, bmp, pnm/ppm) comes back as the image itself, \
         so look at a screenshot by reading it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to project root or absolute)" },
                "start_line": { "type": "integer", "description": "1-based first line to include" },
                "end_line": { "type": "integer", "description": "1-based last line to include" }
            },
            "required": ["path"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ReadFileArgs = parse_args(self.name(), args)?;
        if matches!(args.start_line, Some(0)) || matches!(args.end_line, Some(0)) {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "start_line and end_line are 1-based; 0 is not a valid line".to_string(),
            });
        }
        if let (Some(start), Some(end)) = (args.start_line, args.end_line)
            && end < start
        {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: format!("end_line ({end}) is before start_line ({start})"),
            });
        }

        let path = resolve_path(ctx, &args.path);
        // An image is answered with the image, not with a decode error. The
        // check is on the first bytes rather than the extension: a QEMU
        // screendump is a `.ppm`, a screenshot saved by hand is often a
        // `.png` that is really a JPEG, and an ASCII PPM would otherwise read
        // back as perfectly valid, perfectly useless text.
        if let Some(kind) = image_kind(&path).await {
            return Ok(read_image(&path, kind, ctx.vision).await);
        }
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(ToolOutput::ok("(empty file)"));
        }

        let start = args.start_line.unwrap_or(1);
        let end = args.end_line.unwrap_or(total).min(total);
        if start > total {
            return Ok(ToolOutput::error(format!(
                "start_line {start} is past the end of {} ({total} lines)",
                path.display()
            )));
        }

        let slice = &lines[start - 1..end];
        let (shown, line_capped) = if slice.len() > MAX_READ_LINES {
            (&slice[..MAX_READ_LINES], true)
        } else {
            (slice, false)
        };

        let mut numbered: String = shown
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6}\t{}", start + offset, line))
            .collect::<Vec<_>>()
            .join("\n");
        if line_capped {
            numbered.push_str(&format!(
                "\n... [showing {MAX_READ_LINES} of {} requested lines; total {total} lines — use start_line/end_line to read more]",
                slice.len()
            ));
        }
        // Never spills: the file itself is the spill file, and `start_line` /
        // `end_line` above are the way back to the rest of it.
        Ok(ToolOutput::ok(truncate_output_without_spill(
            numbered,
            MAX_OUTPUT_BYTES,
        )))
    }
}

/// What `read_file` found at the head of a file it is about to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    /// A format every vision API takes. The file's own bytes go to the model.
    Native {
        mime: &'static str,
        label: &'static str,
    },
    /// A format none of them take. Decoded here and re-encoded as PNG.
    Transcode { label: &'static str },
}

impl ImageKind {
    fn label(self) -> &'static str {
        match self {
            ImageKind::Native { label, .. } | ImageKind::Transcode { label } => label,
        }
    }
}

/// Whether `path` holds an image, from its first bytes and its length.
///
/// `None` for everything else, including a file that cannot be opened: the
/// text path below reports that failure, with the error the user expects.
async fn image_kind(path: &Path) -> Option<ImageKind> {
    let mut head = [0u8; 64];
    let read = {
        use tokio::io::AsyncReadExt as _;
        let mut file = tokio::fs::File::open(path).await.ok()?;
        file.read(&mut head).await.ok()?
    };
    let head = &head[..read];
    if let Some(mime) = crate::llm::sniff_mime(head) {
        let label = match mime {
            "image/jpeg" => "JPEG",
            "image/gif" => "GIF",
            "image/webp" => "WebP",
            _ => "PNG",
        };
        return Some(ImageKind::Native { mime, label });
    }
    // BMP's magic is two printable letters, so the four-byte file size that
    // follows it is checked too: "BM" at the head of a text file is common,
    // "BM" followed by this file's own length is not.
    if head.len() >= 6 && head.starts_with(b"BM") {
        let declared = u32::from_le_bytes([head[2], head[3], head[4], head[5]]);
        let actual = tokio::fs::metadata(path).await.ok()?.len();
        if u64::from(declared) == actual {
            return Some(ImageKind::Transcode { label: "BMP" });
        }
    }
    if is_pnm(head) {
        return Some(ImageKind::Transcode { label: "PNM" });
    }
    None
}

/// Whether `head` opens a PNM file: `P1`..`P6` (the pbm/pgm/ppm family, what
/// `qemu screendump` writes) or `P7` (PAM).
///
/// The whole preamble is checked, not just the magic. Three of the six
/// subtypes are plain ASCII and the magic is two characters, so "P3 is the
/// third port" would otherwise be read as an image and come back as a decoder
/// error instead of as the line of text it is. After the magic, past
/// whitespace and `#` comments, a P1-P6 header has to start its width.
fn is_pnm(head: &[u8]) -> bool {
    if head.len() < 3 || head[0] != b'P' || !head[2].is_ascii_whitespace() {
        return false;
    }
    match head[1] {
        // PAM names its fields, so there is no digit to look for. Nothing else
        // begins a file with "P7" and a newline.
        b'7' => true,
        b'1'..=b'6' => {
            let mut rest = &head[3..];
            loop {
                let skipped = rest.iter().take_while(|b| b.is_ascii_whitespace()).count();
                rest = &rest[skipped..];
                match rest.first() {
                    Some(b'#') => {
                        let line = rest.iter().take_while(|&&b| b != b'\n').count();
                        rest = &rest[line..];
                    }
                    Some(byte) => return byte.is_ascii_digit(),
                    // The header ran past the bytes we read: a real PNM does
                    // not have 60 bytes of leading comment, so this is not one.
                    None => return false,
                }
            }
        }
        _ => false,
    }
}

/// Return `path` as an image the model can see, with a line naming the file,
/// its pixel size and its format.
///
/// A native format under the transport cap rides as its own bytes. Everything
/// else is decoded and re-encoded as PNG, halving the long edge until it fits
/// [`crate::llm::MAX_IMAGE_BYTES`]; the text says so when that happened, so
/// the model knows it is looking at a reduction.
async fn read_image(path: &Path, kind: ImageKind, vision: bool) -> ToolOutput {
    let name = path.display().to_string();
    let bytes = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(err) => return ToolOutput::error(format!("failed to read {name}: {err}")),
    };
    let owned = path.to_path_buf();
    let dimensions = tokio::task::spawn_blocking(move || {
        image::ImageReader::open(&owned)?
            .with_guessed_format()?
            .into_dimensions()
    })
    .await;
    let (width, height) = match dimensions {
        Ok(Ok(size)) => size,
        Ok(Err(err)) => {
            return ToolOutput::error(format!("{name} is a damaged {}: {err}", kind.label()));
        }
        Err(err) => return ToolOutput::error(format!("reading {name} failed: {err}")),
    };
    let summary = format!("{name}: {width}x{height} {}, {bytes} bytes", kind.label());

    if !vision {
        return ToolOutput::ok(format!(
            "{summary}. The active provider is configured `vision = false`, \
             so the image itself is not attached. Read it another way, or \
             switch to a model that can see."
        ));
    }
    if bytes > MAX_IMAGE_FILE_BYTES {
        return ToolOutput::error(format!(
            "{summary}. That is over the {} MB ceiling for reading an image; \
             shrink it first.",
            MAX_IMAGE_FILE_BYTES / (1024 * 1024)
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return ToolOutput::error(format!(
            "{summary}. That is over the {} megapixel ceiling for reading an \
             image; crop or shrink it first.",
            MAX_IMAGE_PIXELS / 1_000_000
        ));
    }

    if let ImageKind::Native { .. } = kind
        && bytes <= crate::llm::MAX_IMAGE_BYTES as u64
    {
        return match tokio::fs::read(path).await {
            Ok(raw) => match crate::llm::Image::from_bytes(&raw) {
                Ok(image) => ToolOutput::ok_with_images(summary, vec![image]),
                Err(err) => ToolOutput::error(format!("{summary}. Cannot attach it: {err}")),
            },
            Err(err) => ToolOutput::error(format!("failed to read {name}: {err}")),
        };
    }

    let owned = path.to_path_buf();
    let encoded =
        tokio::task::spawn_blocking(move || to_png_under_cap(&owned, crate::llm::MAX_IMAGE_BYTES))
            .await;
    match encoded {
        Ok(Ok((png, shown_width, shown_height))) => {
            let note = if shown_width == width && shown_height == height {
                String::new()
            } else {
                format!(
                    " (shown at {shown_width}x{shown_height} to fit the {} MB image cap)",
                    crate::llm::MAX_IMAGE_BYTES / (1024 * 1024)
                )
            };
            match crate::llm::Image::from_bytes(&png) {
                Ok(image) => ToolOutput::ok_with_images(format!("{summary}{note}"), vec![image]),
                Err(err) => ToolOutput::error(format!("{summary}. Cannot attach it: {err}")),
            }
        }
        Ok(Err(message)) => ToolOutput::error(format!("{summary}. {message}")),
        Err(err) => ToolOutput::error(format!("reading {name} failed: {err}")),
    }
}

/// Decode `path` and encode it as a PNG of at most `cap` bytes, halving it
/// until it fits. Returns the PNG and the size it ended up at.
///
/// `cap` is [`crate::llm::MAX_IMAGE_BYTES`] everywhere but the test that
/// proves the halving, which would otherwise have to build a 10 MB image.
fn to_png_under_cap(path: &Path, cap: usize) -> Result<(Vec<u8>, u32, u32), String> {
    let mut frame = image::ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(|err| format!("cannot open it: {err}"))?
        .decode()
        .map_err(|err| format!("cannot decode it: {err}"))?;
    for _ in 0..=MAX_DOWNSCALE_STEPS {
        let mut png = Vec::new();
        frame
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|err| format!("cannot encode it as PNG: {err}"))?;
        if png.len() <= cap {
            return Ok((png, frame.width(), frame.height()));
        }
        frame = frame.resize(
            (frame.width() / 2).max(1),
            (frame.height() / 2).max(1),
            image::imageops::FilterType::Triangle,
        );
    }
    Err(format!(
        "it is still over the {} MB image cap after {MAX_DOWNSCALE_STEPS} \
         halvings; shrink it first.",
        cap / (1024 * 1024)
    ))
}

/// Arguments for [`WriteFileTool`].
#[derive(Debug, Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    /// Full contents to write (creates or overwrites; parents created).
    pub content: String,
}

/// `write_file` — create or overwrite a file.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        r#"Create or overwrite a file with the given content, creating parent directories as needed.

Tips:
- Use for new files or full rewrites; prefer `edit_file` for surgical changes.
- Write required deliverables as soon as you know the path and a schema-valid payload — do not defer the only required output to a narration-only final turn.
- When a verification script already found the answer, write the file in that same step.
- For JSONL/CWE reports, copy the demonstration schema exactly (key names, types; `cwe_id` is a **list** of lowercase `cwe-N` strings). Only use IDs from the task's candidate list.
- When multiple answers/IDs/moves are required, write them all."#
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to create or overwrite" },
                "content": { "type": "string", "description": "Full file contents" }
            },
            "required": ["path", "content"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: WriteFileArgs = parse_args(self.name(), args)?;
        let path = resolve_path(ctx, &args.path);

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(ToolOutput::error(format!(
                "failed to create parent directory {}: {err}",
                parent.display()
            )));
        }

        let existed = path.exists();
        if let Err(err) = tokio::fs::write(&path, &args.content).await {
            return Ok(ToolOutput::error(format!(
                "failed to write {}: {err}",
                path.display()
            )));
        }

        let verb = if existed { "Overwrote" } else { "Created" };
        Ok(ToolOutput::ok(format!(
            "{verb} {} ({} bytes)",
            path.display(),
            args.content.len()
        )))
    }
}

/// Arguments for [`EditFileTool`].
#[derive(Debug, Deserialize)]
pub struct EditFileArgs {
    pub path: String,
    /// Exact text to find. Must match exactly once unless `replace_all`.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// `edit_file` — exact search-and-replace edit.
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by exact search-and-replace. old_string must match exactly once unless replace_all is true."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit" },
                "old_string": { "type": "string", "description": "Exact text to replace" },
                "new_string": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Edit
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: EditFileArgs = parse_args(self.name(), args)?;
        if args.old_string.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "old_string must not be empty".to_string(),
            });
        }
        if args.old_string == args.new_string {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "old_string and new_string are identical".to_string(),
            });
        }

        let path = resolve_path(ctx, &args.path);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };

        let count = content.matches(&args.old_string).count();
        if count == 0 {
            return Ok(ToolOutput::error(format!(
                "old_string not found in {}",
                path.display()
            )));
        }
        if count > 1 && !args.replace_all {
            return Ok(ToolOutput::error(format!(
                "old_string matches {count} times in {}; provide more surrounding context to make it unique, or set replace_all",
                path.display()
            )));
        }

        // Line of the first match, for the confirmation message.
        let first_line = content
            .find(&args.old_string)
            .map(|idx| content[..idx].matches('\n').count() + 1)
            .unwrap_or(1);

        let updated = if args.replace_all {
            content.replace(&args.old_string, &args.new_string)
        } else {
            content.replacen(&args.old_string, &args.new_string, 1)
        };

        if let Err(err) = tokio::fs::write(&path, &updated).await {
            return Ok(ToolOutput::error(format!(
                "failed to write {}: {err}",
                path.display()
            )));
        }

        let message = if count == 1 {
            format!(
                "Edited {}: replaced 1 occurrence (line {first_line})",
                path.display()
            )
        } else {
            format!(
                "Edited {}: replaced {count} occurrences (first at line {first_line})",
                path.display()
            )
        };
        Ok(ToolOutput::ok(message))
    }
}

/// Arguments for [`ListFilesTool`].
#[derive(Debug, Deserialize)]
pub struct ListFilesArgs {
    /// Directory to list (default: project root).
    #[serde(default)]
    pub path: Option<String>,
    /// Glob filter, e.g. `**/*.rs` (default: all entries).
    #[serde(default)]
    pub glob: Option<String>,
}

/// `list_files` — directory listing with optional glob filter.
pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List files and directories under a path, optionally filtered by a glob pattern. Respects .gitignore."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: project root)" },
                "glob": { "type": "string", "description": "Glob filter, e.g. **/*.rs" }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ListFilesArgs = parse_args(self.name(), args)?;
        let dir = resolve_path(ctx, args.path.as_deref().unwrap_or("."));
        if !dir.is_dir() {
            return Ok(ToolOutput::error(format!(
                "{} is not a directory",
                dir.display()
            )));
        }

        match args.glob {
            None => list_single_level(self.name(), &dir),
            Some(glob) => {
                let matcher = Glob::new(&glob)
                    .map_err(|err| ToolError::InvalidArgs {
                        tool: self.name().to_string(),
                        message: format!("invalid glob '{glob}': {err}"),
                    })?
                    .compile_matcher();
                list_recursive(self.name(), &dir, &glob, &matcher).await
            }
        }
    }
}

/// Plain single-level listing (no glob): directories first with a trailing
/// `/`, then files, both alphabetical.
fn list_single_level(tool: &str, dir: &Path) -> Result<ToolOutput, ToolError> {
    let entries = std::fs::read_dir(dir).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::Error::new(err).context(format!("reading {}", dir.display())),
    })?;

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();

    let mut listing: Vec<String> = dirs;
    listing.extend(files);
    if listing.is_empty() {
        return Ok(ToolOutput::ok("(empty directory)"));
    }

    let truncated = listing.len() > MAX_LIST_ENTRIES;
    listing.truncate(MAX_LIST_ENTRIES);
    let mut content = listing.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n... [listing truncated at {MAX_LIST_ENTRIES} entries]"
        ));
    }
    Ok(ToolOutput::ok(truncate_output(content, MAX_LISTING_BYTES)))
}

/// Recursive glob listing. Prefers `git ls-files` (which respects
/// `.gitignore`); falls back to a manual walk that skips `.git` when the
/// directory is not inside a git repository.
async fn list_recursive(
    tool: &str,
    dir: &Path,
    glob: &str,
    matcher: &GlobMatcher,
) -> Result<ToolOutput, ToolError> {
    let mut matches = match git_tracked_files(tool, dir).await {
        Some(files) => files
            .into_iter()
            .filter(|file| matcher.is_match(Path::new(file)))
            .collect(),
        None => walk_matching(dir, matcher),
    };
    matches.sort();

    if matches.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "No files matching '{glob}' under {}",
            dir.display()
        )));
    }

    let truncated = matches.len() > MAX_LIST_ENTRIES;
    matches.truncate(MAX_LIST_ENTRIES);
    let mut content = matches.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n... [listing truncated at {MAX_LIST_ENTRIES} entries]"
        ));
    }
    Ok(ToolOutput::ok(truncate_output(content, MAX_LISTING_BYTES)))
}

/// `git ls-files --cached --others --exclude-standard` relative to `dir`.
/// Returns `None` when git is unavailable or `dir` is not in a repository.
///
/// The budget is short on purpose: this is an index read, which is fast even
/// on a huge repository, and the fallback when it does not answer is a manual
/// walk that produces the same listing. Waiting half a minute to find out that
/// git is wedged, when there is a working answer on the other side of giving
/// up, is time spent for nothing.
async fn git_tracked_files(tool: &str, dir: &Path) -> Option<Vec<String>> {
    let mut command = Command::new("git");
    command
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(dir);
    match run_command(tool, command, LS_FILES_TIMEOUT).await {
        Ok(result) if result.code == Some(0) => Some(
            result
                .stdout
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

/// Manual recursive walk used outside git repositories. Skips `.git` and
/// stops after [`MAX_WALK_VISITS`] entries.
fn walk_matching(root: &Path, matcher: &GlobMatcher) -> Vec<String> {
    let mut results = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut visited = 0usize;

    'walk: while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_WALK_VISITS || results.len() > MAX_LIST_ENTRIES {
                break 'walk;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
            } else {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if matcher.is_match(rel) {
                    results.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    results
}

/// Arguments for [`SearchFilesTool`].
#[derive(Debug, Deserialize)]
pub struct SearchFilesArgs {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Directory to search (default: project root).
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict to files matching this glob, e.g. `*.rs`.
    #[serde(default)]
    pub glob: Option<String>,
}

/// `search_files` — content search via ripgrep, falling back to grep.
pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search file contents for a regex pattern (ripgrep if available, grep otherwise). Returns matching lines with file and line number."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern" },
                "path": { "type": "string", "description": "Directory to search (default: project root)" },
                "glob": { "type": "string", "description": "Restrict to files matching this glob, e.g. *.rs" }
            },
            "required": ["pattern"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SearchFilesArgs = parse_args(self.name(), args)?;
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "pattern must not be empty".to_string(),
            });
        }

        // Keep the path relative when possible so match locations come back
        // relative to the project root.
        let search_path = shellexpand::tilde(args.path.as_deref().unwrap_or(".")).into_owned();

        let mut rg = Command::new("rg");
        rg.args([
            "--line-number",
            "--no-heading",
            "--color",
            "never",
            "--max-columns",
            "500",
        ]);
        if let Some(glob) = &args.glob {
            rg.arg("--glob").arg(glob);
        }
        rg.arg("--regexp")
            .arg(&args.pattern)
            .arg("--")
            .arg(&search_path)
            .current_dir(&ctx.cwd);

        let result = match run_command(self.name(), rg, SEARCH_TIMEOUT).await {
            Ok(result) => result,
            // rg missing or unspawnable: fall back to grep.
            Err(ToolError::Execution { .. }) => {
                let mut grep = Command::new("grep");
                grep.args(["-r", "-n", "-I", "-E"]);
                if let Some(glob) = &args.glob {
                    grep.arg(format!("--include={glob}"));
                }
                grep.arg("-e")
                    .arg(&args.pattern)
                    .arg("--")
                    .arg(&search_path)
                    .current_dir(&ctx.cwd);
                run_command(self.name(), grep, SEARCH_TIMEOUT).await?
            }
            Err(err) => return Err(err),
        };

        // A timed-out search still reports the matches found so far.
        if result.timed_out.is_some() {
            return Ok(render_command_result(&result));
        }

        // Both rg and grep: 0 = matches, 1 = no matches, >1 = error.
        match result.code {
            Some(0) => Ok(ToolOutput::ok(truncate_output(
                result.stdout.trim_end().to_string(),
                MAX_SEARCH_BYTES,
            ))),
            Some(1) => Ok(ToolOutput::ok(format!(
                "No matches for pattern '{}'.",
                args.pattern
            ))),
            _ => {
                let stderr = result.stderr.trim_end();
                let detail = if stderr.is_empty() {
                    "search failed"
                } else {
                    stderr
                };
                Ok(ToolOutput::error(truncate_output(
                    detail.to_string(),
                    MAX_ERROR_BYTES,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn read_file_line_range() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        let out = ReadFileTool
            .execute(
                json!({ "path": "f.txt", "start_line": 2, "end_line": 3 }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("two"));
        assert!(out.content.contains("three"));
        assert!(!out.content.contains("one"));
        assert!(!out.content.contains("four"));
    }

    #[tokio::test]
    async fn read_file_missing_is_tool_output_error() {
        let tmp = TempDir::new();
        let out = ReadFileTool
            .execute(json!({ "path": "nope.txt" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_file_rejects_bad_range() {
        let tmp = TempDir::new();
        let err = ReadFileTool
            .execute(
                json!({ "path": "f.txt", "start_line": 5, "end_line": 2 }),
                &tmp.ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn write_file_creates_parents() {
        let tmp = TempDir::new();
        let out = WriteFileTool
            .execute(json!({ "path": "a/b/c.txt", "content": "hi" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("a/b/c.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn edit_file_requires_unique_match() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "foo bar foo").unwrap();
        let ctx = tmp.ctx();

        // Ambiguous match is reported as a tool-level error.
        let out = EditFileTool
            .execute(
                json!({ "path": "f.txt", "old_string": "foo", "new_string": "baz" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("2 times"));

        // replace_all succeeds.
        let out = EditFileTool
            .execute(
                json!({ "path": "f.txt", "old_string": "foo", "new_string": "baz", "replace_all": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("f.txt")).unwrap(),
            "baz bar baz"
        );
    }

    #[tokio::test]
    async fn edit_file_missing_old_string_errors() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "hello").unwrap();
        let out = EditFileTool
            .execute(
                json!({ "path": "f.txt", "old_string": "absent", "new_string": "x" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
    }

    #[tokio::test]
    async fn list_files_glob_filters() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("src")).unwrap();
        std::fs::write(tmp.0.join("src/main.rs"), "").unwrap();
        std::fs::write(tmp.0.join("notes.md"), "").unwrap();

        let out = ListFilesTool
            .execute(json!({ "glob": "**/*.rs" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/main.rs"));
        assert!(!out.content.contains("notes.md"));
    }

    #[tokio::test]
    async fn list_files_single_level_marks_dirs() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("sub")).unwrap();
        std::fs::write(tmp.0.join("file.txt"), "").unwrap();

        let out = ListFilesTool.execute(json!({}), &tmp.ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("sub/"));
        assert!(out.content.contains("file.txt"));
    }

    #[tokio::test]
    async fn search_files_finds_pattern() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("f.txt"), "alpha\nneedle here\nomega\n").unwrap();

        let out = SearchFilesTool
            .execute(json!({ "pattern": "needle" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("needle here"));

        let out = SearchFilesTool
            .execute(json!({ "pattern": "zzz_absent" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("No matches"));
    }

    /// A 12x8 test image written in `format`, as file bytes.
    fn encode(format: image::ImageFormat) -> Vec<u8> {
        let mut frame = image::RgbImage::new(12, 8);
        for (x, y, pixel) in frame.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 20) as u8, (y * 30) as u8, 40]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(frame)
            .write_to(&mut std::io::Cursor::new(&mut bytes), format)
            .expect("encoded");
        bytes
    }

    /// A binary P6 PPM written by hand, the shape `qemu screendump` produces.
    fn qemu_ppm(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[(x * 3) as u8, (y * 5) as u8, 90]);
            }
        }
        bytes
    }

    /// Every format `read_file` claims comes back as an image, with a line
    /// naming the file, its size and its format. The four the vision APIs take
    /// keep their own media type; BMP and PNM arrive as PNG, since no API
    /// takes either.
    #[tokio::test]
    async fn read_file_returns_images_as_images() {
        let tmp = TempDir::new();
        let cases = [
            ("shot.png", image::ImageFormat::Png, "PNG", "image/png"),
            ("shot.jpg", image::ImageFormat::Jpeg, "JPEG", "image/jpeg"),
            ("shot.gif", image::ImageFormat::Gif, "GIF", "image/gif"),
            ("shot.webp", image::ImageFormat::WebP, "WebP", "image/webp"),
            ("shot.bmp", image::ImageFormat::Bmp, "BMP", "image/png"),
            ("shot.ppm", image::ImageFormat::Pnm, "PNM", "image/png"),
        ];
        for (name, format, label, mime) in cases {
            let bytes = if format == image::ImageFormat::Pnm {
                qemu_ppm(12, 8)
            } else {
                encode(format)
            };
            std::fs::write(tmp.0.join(name), bytes).unwrap();
            let out = ReadFileTool
                .execute(json!({ "path": name }), &tmp.ctx())
                .await
                .unwrap();
            assert!(!out.is_error, "{name}: {}", out.content);
            assert_eq!(out.images.len(), 1, "{name} returned no image");
            assert_eq!(out.images[0].mime, mime, "{name}");
            assert!(out.content.contains("12x8"), "{name}: {}", out.content);
            assert!(out.content.contains(label), "{name}: {}", out.content);
            assert!(out.content.contains(name), "{name}: {}", out.content);
            assert!(
                !out.images[0].b64.is_empty() && out.images[0].decode().is_ok(),
                "{name} carried no decodable bytes"
            );
        }
    }

    /// The extension is not what decides. A screendump named `.dat` is still
    /// an image, and a `.png` holding text is still text.
    #[tokio::test]
    async fn read_file_goes_by_the_bytes_not_the_extension() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("screendump.dat"), qemu_ppm(12, 8)).unwrap();
        let out = ReadFileTool
            .execute(json!({ "path": "screendump.dat" }), &tmp.ctx())
            .await
            .unwrap();
        assert_eq!(out.images.len(), 1, "{}", out.content);
        assert_eq!(out.images[0].mime, "image/png");
        assert!(out.content.contains("12x8 PNM"), "{}", out.content);

        std::fs::write(tmp.0.join("notes.png"), "plain text\n").unwrap();
        let out = ReadFileTool
            .execute(json!({ "path": "notes.png" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.images.is_empty());
        assert!(out.content.contains("plain text"), "{}", out.content);
    }

    /// Text files are untouched by the image branch, including the two whose
    /// first bytes come close to a magic number: `BM...` and a `P3` line.
    #[tokio::test]
    async fn read_file_leaves_text_alone() {
        let tmp = TempDir::new();
        for (name, body) in [
            ("plain.txt", "one\ntwo\n"),
            ("bmw.md", "BMW cars are made in Bavaria.\n"),
            ("port.txt", "P3 is the third port on the switch.\n"),
        ] {
            std::fs::write(tmp.0.join(name), body).unwrap();
            let out = ReadFileTool
                .execute(json!({ "path": name }), &tmp.ctx())
                .await
                .unwrap();
            assert!(!out.is_error, "{name}: {}", out.content);
            assert!(out.images.is_empty(), "{name} was taken for an image");
            assert!(out.content.contains("     1\t"), "{name}: {}", out.content);
        }
    }

    /// An image with more pixels than the decode ceiling is refused by name
    /// and size rather than decoded. The header says how big it is, so nothing
    /// is allocated to find out.
    #[tokio::test]
    async fn read_file_refuses_an_oversized_image() {
        let tmp = TempDir::new();
        // A 9000x9000 PNG of one flat colour: 81 Mpx, well past the ceiling,
        // and a few KB on disk because it compresses to nothing.
        let huge = image::DynamicImage::ImageLuma8(image::GrayImage::new(9000, 9000));
        huge.save(tmp.0.join("huge.png")).unwrap();

        let out = ReadFileTool
            .execute(json!({ "path": "huge.png" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.images.is_empty());
        assert!(out.content.contains("9000x9000"), "{}", out.content);
        assert!(out.content.contains("megapixel ceiling"), "{}", out.content);
    }

    /// An image over the transport cap is halved until it fits, and the text
    /// says at what size the model is looking at it.
    #[test]
    fn oversized_images_are_halved_until_they_fit() {
        let tmp = TempDir::new();
        let path = tmp.0.join("noise.png");
        // Incompressible, so the PNG cannot shrink its way under the cap
        // without losing pixels.
        let mut frame = image::RgbImage::new(256, 256);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for pixel in frame.pixels_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            *pixel = image::Rgb([bytes[0], bytes[1], bytes[2]]);
        }
        image::DynamicImage::ImageRgb8(frame).save(&path).unwrap();

        let (png, width, height) = to_png_under_cap(&path, 20_000).expect("fits after halving");
        assert!(png.len() <= 20_000, "{} bytes", png.len());
        assert!(
            width < 256 && height < 256,
            "{width}x{height} was not reduced"
        );

        // A cap nothing can reach says so instead of returning a broken image.
        let err = to_png_under_cap(&path, 16).expect_err("16 bytes is unreachable");
        assert!(err.contains("halvings"), "{err}");
    }

    /// With `vision = false` on the provider, the image is described and not
    /// attached: a text-only model gets a sentence it can act on instead of a
    /// megabyte it will throw away.
    #[tokio::test]
    async fn read_file_describes_an_image_when_the_model_cannot_see() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("shot.png"), encode(image::ImageFormat::Png)).unwrap();

        let out = ReadFileTool
            .execute(json!({ "path": "shot.png" }), &tmp.ctx().with_vision(false))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.images.is_empty());
        assert!(out.content.contains("12x8 PNG"), "{}", out.content);
        assert!(out.content.contains("vision = false"), "{}", out.content);
    }
}
