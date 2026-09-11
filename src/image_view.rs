//! Drawing an [`ImageRef`] inside the ratatui transcript.
//!
//! The agent already wrote every image to disk ([`crate::images::ImageStore`]),
//! so this module never sees base64 — it opens a file, scales it into the cells
//! the transcript gave it, and paints those cells.
//!
//! # Why this is not just "print the escape codes"
//!
//! A ratatui app rewrites its whole buffer every frame and lets the backend
//! diff it against the last one. Terminal graphics that are *out of band* —
//! sixel, iTerm2 — paint pixels the buffer knows nothing about, so a naive
//! blast survives the redraw that was supposed to erase it and smears down the
//! screen on the next scroll. Everything here is arranged so that cannot
//! happen:
//!
//! - An image block is laid out as real transcript rows ([`ImageCache::layout`]),
//!   so it scrolls, wraps and clips like any other content. The renderer is only
//!   ever handed rows that are actually on screen.
//! - A graphics protocol is used **only when the whole block is on screen**, and
//!   the protocol is built at exactly the size of the rect it is rendered into —
//!   it can never overdraw a neighbouring row.
//! - The moment a block straddles the edge of the viewport, the visible slice is
//!   drawn in **half-blocks** instead: `▀`/`▄` cells with 24-bit colour, which
//!   are ordinary buffer cells. They clip for free, they compose with the diff,
//!   and they cannot leave anything behind. This is also the floor for every
//!   terminal that has no graphics protocol at all.
//! - When the terminal has no colour to draw with, [`ImageCache::layout`]
//!   reserves nothing and the image is its caption line — the mime, the size and
//!   the path (see [`crate::ui`]), which is never omitted whatever the terminal.
//!
//! Decoding and scaling happen once per image per block size and are cached; a
//! frame that redraws an unchanged image does no pixel work at all.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::{DynamicImage, ImageBuffer, Rgba, imageops};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::halfblocks::Halfblocks;
use ratatui_image::{FontSize, Image as ImageWidget, Resize};

use crate::images::ImageRef;

/// Widest an image block gets, however wide the terminal is: a thumbnail in the
/// text column, not a wallpaper.
pub const MAX_COLS: u16 = 60;

/// Tallest an image block gets. Callers shrink this further against the
/// viewport (see [`crate::ui`]), so one image can never swallow the screen.
pub const MAX_ROWS: u16 = 16;

/// How many images keep their decoded, scaled and encoded forms in memory. Only
/// what is on screen is ever drawn, so a handful covers the working set;
/// scrolling back to an older image pays for its decode once more.
const CACHE_CAP: usize = 8;

/// Resampling filter for the one scale each image gets. Triangle is the cheapest
/// filter that does not alias a downscaled screenshot into confetti.
const FILTER: FilterType = FilterType::Triangle;

/// Overrides the detected protocol: `off`, `halfblocks`, `kitty`, `sixel`,
/// `iterm2`, or `auto` (the default — ask the terminal).
const PROTOCOL_ENV: &str = "WIZARD_IMAGE_PROTOCOL";

/// How long [`Query::answer`] waits for the terminal, and again for every
/// byte after the first. A terminal answers in microseconds; one that has
/// said nothing after this is not going to.
const PATIENCE: Duration = Duration::from_millis(200);

/// [`PATIENCE`] over SSH, where the reply has a network round trip in it.
const SSH_PATIENCE: Duration = Duration::from_millis(1000);

/// The cells an image block may spend: the column it hangs in, and the tallest
/// it may grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBox {
    pub cols: u16,
    pub rows: u16,
}

/// An image laid out in the transcript: the file, and the cells it was given.
/// Produced by [`ImageCache::layout`] and handed straight back to
/// [`ImageCache::draw`] — the rows in between are the transcript's to scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlock {
    pub path: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

/// Everything one image keeps between frames.
struct Entry {
    /// Pixel size of the file, or `None` when it will not decode — remembered
    /// either way, so a broken file is not re-read frame after frame.
    pixels: Option<(u32, u32)>,
    /// The file scaled into the block it was last given. Rebuilt when the block
    /// changes size (a terminal resize).
    scaled: Option<Scaled>,
}

/// One image, scaled to one block size, plus the encodings made from it.
struct Scaled {
    cols: u16,
    rows: u16,
    /// The file, resized to fit `cols × rows` cells and padded out to exactly
    /// that many pixels. Transparent padding: the terminal's own background
    /// shows through a graphics protocol, and half-blocks read it as black —
    /// at most half a cell of it, since the block was rounded to the image.
    pixels: DynamicImage,
    /// The whole block in the terminal's protocol, built on the first frame it
    /// is fully on screen.
    whole: Option<Protocol>,
    /// Half-blocks for a slice of the block, keyed by `(top, height)` — what a
    /// block straddling the edge of the viewport is drawn with.
    slices: HashMap<(u16, u16), Protocol>,
}

/// The terminal's image capability, plus every image it has drawn recently.
///
/// Lives on [`App`](crate::app::App) behind a `RefCell`, because
/// [`crate::ui::draw`] takes `&App` and this is the one thing a frame legitimately
/// mutates: it is a cache, and the alternative is re-decoding a PNG at 60 Hz.
pub struct ImageCache {
    /// How this terminal draws pixels. `None` when it cannot draw any, and an
    /// image is only its caption.
    picker: Option<Picker>,
    entries: HashMap<PathBuf, Entry>,
    /// Paths in least-recently-drawn order — the eviction queue for [`CACHE_CAP`].
    order: Vec<PathBuf>,
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache")
            .field("protocol", &self.protocol())
            .field("cached", &self.entries.len())
            .finish()
    }
}

/// What [`ImageCache::detect`] came back with: an answer, or a question the
/// terminal has been asked and has not answered yet.
pub enum Detection {
    Done(ImageCache),
    Asked(Query),
}

/// An image query written to the terminal, its reply still on the way.
///
/// Sent by [`ImageCache::detect`] after raw mode is on (a cooked tty would
/// echo the reply and hold it for a newline) and before the alternate screen
/// (a terminal that prints what it cannot parse prints it where the switch
/// hides it), and answered by [`Self::answer`] before crossterm's event
/// stream starts draining stdin, because that stream would read the reply
/// as keystrokes.
pub struct Query {
    /// A protocol the user forced through [`PROTOCOL_ENV`]; the query is then
    /// only for the cell size.
    forced: Option<ProtocolType>,
}

impl Query {
    fn send(forced: Option<ProtocolType>) -> Detection {
        let query = Parser::query(false, QueryStdioOptions::default());
        let mut stdout = std::io::stdout().lock();
        if let Err(err) = stdout
            .write_all(query.as_bytes())
            .and_then(|()| stdout.flush())
        {
            tracing::debug!("cannot write the image query ({err}); half-blocks");
            return Detection::Done(ImageCache::fallback());
        }
        Detection::Asked(Self { forced })
    }

    /// Read the terminal's reply, waiting at most [`PATIENCE`] for it, and
    /// build the cache from what it said.
    pub fn answer(self) -> ImageCache {
        let patience = if std::env::var_os("SSH_TTY").is_some() {
            SSH_PATIENCE
        } else {
            PATIENCE
        };
        let responses = collect(&mut read_byte, patience);
        if responses.is_empty() {
            tracing::debug!("terminal did not answer the image query; half-blocks");
        }
        let mut picker = picker_from(&responses, quirky_terminal(), fallback_font());
        if let Some(forced) = self.forced {
            picker.set_protocol_type(forced);
        }
        ImageCache::new(Some(picker))
    }
}

/// Every response up to the terminal's status report, which ends the reply.
///
/// `next` yields one byte of stdin, or `None` once its wait has run out.
/// `patience` is renewed by every byte: a terminal that has begun to answer
/// finishes within microseconds, and one that has not begun by then never
/// will.
fn collect(next: &mut impl FnMut(Duration) -> Option<u8>, patience: Duration) -> Vec<Response> {
    let mut parser = Parser::new();
    let mut responses = Vec::new();
    let mut deadline = Instant::now() + patience;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return responses;
        }
        let Some(byte) = next(wait) else {
            return responses;
        };
        deadline = Instant::now() + patience;
        for response in parser.push(char::from(byte)) {
            if response == Response::Status {
                return responses;
            }
            responses.push(response);
        }
    }
}

/// The picker the terminal's reply describes, the way ratatui-image reads
/// it: kitty over sixel over whatever the environment suggests, and
/// half-blocks when there is no cell size to place pixels with. `quirky`
/// drops kitty and sixel for the terminals that answer yes and draw them
/// wrong (see [`quirky_terminal`]).
fn picker_from(responses: &[Response], quirky: bool, fallback: Option<FontSize>) -> Picker {
    if responses.is_empty() {
        return Picker::halfblocks();
    }
    let mut protocol = None;
    let mut font = None;
    for response in responses {
        match response {
            Response::Kitty if !quirky => protocol = Some(ProtocolType::Kitty),
            Response::Sixel if !quirky => protocol = protocol.or(Some(ProtocolType::Sixel)),
            Response::CellSize(Some(size)) => font = Some(*size),
            _ => {}
        }
    }
    let Some(font) = font.or(fallback) else {
        return Picker::halfblocks();
    };
    // The one constructor that takes a measured cell size. It reads the
    // multiplexer and iTerm2 hints from the environment, as the query path does.
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    if let Some(protocol) = protocol {
        picker.set_protocol_type(protocol);
    }
    picker
}

/// WezTerm and Konsole answer the kitty query but do not place the image
/// where the cells are, and Konsole's sixel is broken; ratatui-image blacklists
/// both and falls back to their iTerm2 support.
fn quirky_terminal() -> bool {
    ["WEZTERM_EXECUTABLE", "KONSOLE_VERSION"]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|value| !value.is_empty()))
}

/// The cell size from the tty's reported pixel dimensions, for a terminal that
/// did not answer `CSI 16 t`.
fn fallback_font() -> Option<FontSize> {
    let size = crossterm::terminal::window_size().ok()?;
    if size.width == 0 || size.height == 0 || size.columns == 0 || size.rows == 0 {
        return None;
    }
    Some((size.width / size.columns, size.height / size.rows))
}

/// One byte of stdin, or `None` when `wait` passes without one.
///
/// A byte at a time, straight from the descriptor: a buffered read would
/// swallow keystrokes typed behind the reply, which crossterm then never sees.
#[cfg(unix)]
fn read_byte(wait: Duration) -> Option<u8> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut fds = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `fds` is one valid pollfd and the count says so.
        let ready =
            unsafe { libc::poll(&mut fds, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if ready <= 0 {
            return None;
        }
        let mut byte = 0u8;
        // SAFETY: `byte` is a valid one-byte buffer for the length passed.
        let read = unsafe { libc::read(libc::STDIN_FILENO, (&raw mut byte).cast(), 1) };
        return (read == 1).then_some(byte);
    }
}

/// Without `poll`, the reply is read the way ratatui-image reads it, which
/// blocks: never called on the first-frame path, see [`ImageCache::detect`].
#[cfg(not(unix))]
fn read_byte(_wait: Duration) -> Option<u8> {
    None
}

impl ImageCache {
    /// Ask the terminal what it can draw.
    ///
    /// Runs after raw mode is on, before the alternate screen, and returns at
    /// once: the query goes out, the reply is read by [`Query::answer`] once
    /// the first frame is painted and before the event stream starts. A terminal that does not answer is not
    /// assumed to be capable — it gets half-blocks, which every terminal can
    /// draw.
    pub fn detect() -> Detection {
        match std::env::var(PROTOCOL_ENV).ok().as_deref() {
            // The user has told us what they have (or that they want none of
            // it). A lying terminal, a multiplexer that eats the query, a
            // recording session that must not have pixels in it.
            Some("off") => return Detection::Done(Self::new(None)),
            Some(forced) if forced != "auto" => {
                let Some(protocol) = parse_protocol(forced) else {
                    // Unreadable value: fall through to detection rather than
                    // guess at what they meant and corrupt the screen.
                    return Self::query(None);
                };
                // Still ask, for the terminal's real cell size — an image only
                // lands on a whole number of rows if we know how tall one is.
                // Except under a multiplexer, where asking costs the keyboard;
                // see `multiplexer`.
                if multiplexer().is_some() {
                    let mut picker = Picker::halfblocks();
                    picker.set_protocol_type(protocol);
                    return Detection::Done(Self::new(Some(picker)));
                }
                return Self::query(Some(protocol));
            }
            _ => {}
        }
        // A multiplexer is not asked. This is the whole reason `multiplexer`
        // exists — see its comment. Half-blocks, which need no query and which
        // tmux passes through fine.
        if let Some(which) = multiplexer() {
            tracing::debug!("{which} detected: skipping the image query, using half-blocks");
            return Detection::Done(Self::fallback());
        }
        // Nothing to draw with: half-blocks are 24-bit colour, and without
        // colour there is nothing between them and noise.
        if std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term.is_empty() || term == "dumb")
        {
            return Detection::Done(Self::new(None));
        }
        Self::query(None)
    }

    /// Detection proper: the terminal is asked, and its silence is taken for
    /// a no. Where stdin cannot be polled the query is the blocking one.
    fn query(forced: Option<ProtocolType>) -> Detection {
        if cfg!(unix) {
            return Query::send(forced);
        }
        let mut picker = Picker::from_query_stdio().unwrap_or_else(|err| {
            tracing::debug!("terminal did not answer the image query ({err}); half-blocks");
            Picker::halfblocks()
        });
        if let Some(forced) = forced {
            picker.set_protocol_type(forced);
        }
        Detection::Done(Self::new(Some(picker)))
    }

    /// Half-blocks against an assumed cell size — no I/O, no terminal query.
    ///
    /// Also what a multiplexed terminal gets, deliberately: see [`multiplexer`].
    /// What [`App`](crate::app::App) starts with (and what tests draw against),
    /// so a frame can be rendered before, or entirely without, [`Self::detect`].
    pub fn fallback() -> Self {
        Self::new(Some(Picker::halfblocks()))
    }

    /// The protocol images are drawn with, or `None` when they are not drawn.
    pub fn protocol(&self) -> Option<ProtocolType> {
        self.picker.as_ref().map(Picker::protocol_type)
    }

    /// The cells `image` gets in the transcript, or `None` when it has none to
    /// give — the terminal cannot draw, the budget is empty, or the file will
    /// not decode. Its caption is printed either way; this is only the pixels.
    ///
    /// Cheap enough to call for every image in the transcript on every frame:
    /// the file's size is read from its header once and then remembered.
    pub fn layout(&mut self, image: &ImageRef, budget: ImageBox) -> Option<ImageBlock> {
        let font = self.font()?;
        if budget.cols == 0 || budget.rows == 0 {
            return None;
        }
        let pixels = self.entry(&image.path).pixels?;
        let (cols, rows) = fit(pixels, font, budget);
        Some(ImageBlock {
            path: image.path.clone(),
            cols,
            rows,
        })
    }

    /// Paint rows `top .. top + at.height` of `block` into `at`.
    ///
    /// `at` is exactly the rows of the block the transcript kept on screen, so
    /// the whole block being visible means `top == 0` and `at.height ==
    /// block.rows` — the one case a graphics protocol is trusted with (see the
    /// module docs). Anything else is drawn in half-blocks.
    pub fn draw(&mut self, buf: &mut Buffer, at: Rect, block: &ImageBlock, top: u16) {
        let Some(font) = self.font() else {
            return;
        };
        if at.width == 0 || at.height == 0 {
            return;
        }
        // Scale first, while `self` is free: this is the one call that may go to
        // the disk, and it is a no-op after the first frame at this size.
        if self.scaled(block, font).is_none() {
            return;
        }
        // Now split the borrow — the picker is read while the image's cache line
        // is written — and encode. Both branches cache, so a redrawn frame only
        // clones an already-encoded protocol.
        let Self {
            picker, entries, ..
        } = self;
        let (Some(picker), Some(scaled)) = (
            picker.as_ref(),
            entries
                .get_mut(&block.path)
                .and_then(|entry| entry.scaled.as_mut()),
        ) else {
            return;
        };

        let protocol = if top == 0 && at.height == block.rows {
            // The whole block is on screen: the terminal's own protocol, built
            // at exactly the size of the rect it goes into. When that protocol
            // *is* half-blocks the two branches agree, so a block scrolling into
            // full view never visibly switches representation.
            if scaled.whole.is_none() {
                let area = Rect::new(0, 0, scaled.cols, scaled.rows);
                // The pixels already measure exactly `area`, so `Crop` resizes
                // nothing — it only takes the size we chose.
                scaled.whole = picker
                    .new_protocol(scaled.pixels.clone(), area, Resize::Crop(None))
                    .inspect_err(|err| tracing::warn!("cannot encode image: {err}"))
                    .ok();
            }
            scaled.whole.clone()
        } else if let Some(cached) = scaled.slices.get(&(top, at.height)) {
            Some(cached.clone())
        } else {
            // Straddling the edge of the viewport. Half-blocks of just the
            // visible slice: ordinary cells, so they clip exactly and leave
            // nothing on the screen behind them.
            let slice = crop_rows(&scaled.pixels, scaled.rows, top, at.height);
            let area = Rect::new(0, 0, scaled.cols, at.height);
            match Halfblocks::new(slice, area) {
                Ok(encoded) => {
                    let encoded = Protocol::Halfblocks(encoded);
                    scaled.slices.insert((top, at.height), encoded.clone());
                    Some(encoded)
                }
                Err(err) => {
                    tracing::warn!("cannot encode image slice: {err}");
                    None
                }
            }
        };
        if let Some(protocol) = protocol {
            ImageWidget::new(&protocol).render(at, buf);
        }
    }

    fn new(picker: Option<Picker>) -> Self {
        Self {
            picker,
            entries: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// The terminal's cell size, and `None` when there is no terminal support to
    /// have one — which is also when nothing is measured or drawn.
    fn font(&self) -> Option<FontSize> {
        self.picker.as_ref().map(Picker::font_size)
    }

    /// This image's cache line, reading the file's header on first sight (and
    /// its failure to read, so a broken file costs one attempt, not one a frame).
    fn entry(&mut self, path: &Path) -> &mut Entry {
        self.touch(path);
        self.entries.entry(path.to_path_buf()).or_insert_with(|| {
            let pixels = match dimensions(path) {
                Ok(pixels) => Some(pixels),
                Err(err) => {
                    tracing::warn!("cannot read image {}: {err}", path.display());
                    None
                }
            };
            Entry {
                pixels,
                scaled: None,
            }
        })
    }

    /// The image scaled to `block`, decoding and resizing it if this is the
    /// first frame at this size. `None` when the file will not decode.
    fn scaled(&mut self, block: &ImageBlock, font: FontSize) -> Option<&mut Scaled> {
        let entry = self.entry(&block.path);
        let stale = entry
            .scaled
            .as_ref()
            .is_none_or(|scaled| scaled.cols != block.cols || scaled.rows != block.rows);
        if stale {
            let pixels = match decode(&block.path) {
                Ok(pixels) => pixels,
                Err(err) => {
                    tracing::warn!("cannot decode image {}: {err}", block.path.display());
                    entry.pixels = None;
                    entry.scaled = None;
                    return None;
                }
            };
            entry.scaled = Some(Scaled {
                cols: block.cols,
                rows: block.rows,
                pixels: scale_into(&pixels, block.cols, block.rows, font),
                whole: None,
                slices: HashMap::new(),
            });
        }
        entry.scaled.as_mut()
    }

    /// Move `path` to the back of the eviction queue, dropping the oldest images
    /// once more than [`CACHE_CAP`] are held.
    fn touch(&mut self, path: &Path) {
        if let Some(at) = self.order.iter().position(|held| held == path) {
            let held = self.order.remove(at);
            self.order.push(held);
            return;
        }
        self.order.push(path.to_path_buf());
        while self.order.len() > CACHE_CAP {
            let evicted = self.order.remove(0);
            self.entries.remove(&evicted);
        }
    }
}

/// Read an image's pixel size from its header, without decoding it.
fn dimensions(path: &Path) -> Result<(u32, u32), image::ImageError> {
    image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()
}

/// Decode an image file.
fn decode(path: &Path) -> Result<DynamicImage, image::ImageError> {
    image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
}

/// The name for a [`ProtocolType`] in [`PROTOCOL_ENV`].
/// The terminal multiplexer we are running under, if any.
///
/// # Why this costs the keyboard, and not just an image
///
/// `Picker::from_query_stdio` writes a DSR query and reads the reply. To do
/// that, ratatui-image spawns a **detached** thread that loops on
/// `io::stdin().read()` until it parses an answer. Its timeout only makes the
/// *caller* give up: the thread is never joined and never cancelled, so if no
/// answer ever comes it sits on stdin for the life of the process.
///
/// Under tmux or screen no answer ever comes. The query goes out wrapped in
/// multiplexer passthrough, and tmux's `allow-passthrough` is **off** by
/// default, so the terminal never sees it and nothing replies. From then on
/// that thread races crossterm's `EventStream` for every byte the user types
/// and wins most of them.
///
/// Measured on this tree before the fix: twenty keystrokes sent into a tmux
/// pane, four tenths of a second apart, produced exactly **one** character in
/// the composer. Arrow keys came through shredded — the ESC went to the thief
/// and the rest landed as literal `[[D`. With the query skipped, all twenty
/// land. The symptom is a terminal agent that ignores you, which reads as a
/// hang rather than as a missing feature, and nothing on screen points at
/// images.
///
/// So a multiplexer is never asked. It costs nothing real: tmux does not pass
/// graphics protocols through by default either, so the honest answer there is
/// half-blocks anyway. Somebody who has turned passthrough on can still force
/// a protocol with `WIZARD_IMAGE_PROTOCOL`, which skips the query too.
fn multiplexer() -> Option<&'static str> {
    if std::env::var_os("TMUX").is_some() {
        return Some("tmux");
    }
    // GNU screen sets STY; both set TERM to a screen*/tmux* family, which
    // catches the case where the variables were scrubbed but TERM was not.
    if std::env::var_os("STY").is_some() {
        return Some("screen");
    }
    match std::env::var("TERM") {
        Ok(term) if term.starts_with("tmux") => Some("tmux"),
        Ok(term) if term.starts_with("screen") => Some("screen"),
        _ => None,
    }
}

fn parse_protocol(name: &str) -> Option<ProtocolType> {
    match name {
        "halfblocks" => Some(ProtocolType::Halfblocks),
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" => Some(ProtocolType::Iterm2),
        _ => None,
    }
}

/// The cell size an image of `pixels` gets inside `budget`.
///
/// The image's aspect ratio is preserved (in *cell* space — a cell is roughly
/// twice as tall as it is wide, so the same picture is about half as many rows
/// as columns), it is fitted to whichever side of the budget binds first, and it
/// is never enlarged past its natural size: a 16×16 icon stays a 16×16 icon
/// instead of being blown up to fill the column.
pub fn fit(
    (width, height): (u32, u32),
    (cell_w, cell_h): FontSize,
    budget: ImageBox,
) -> (u16, u16) {
    // Natural size in cells. Rounded up, so a picture never loses its last row
    // or column of pixels to the floor.
    let natural_cols = width.div_ceil(cell_w as u32).max(1) as f64;
    let natural_rows = height.div_ceil(cell_h as u32).max(1) as f64;
    let scale = (budget.cols as f64 / natural_cols)
        .min(budget.rows as f64 / natural_rows)
        .min(1.0);
    let cols = (natural_cols * scale)
        .round()
        .clamp(1.0, budget.cols as f64) as u16;
    let rows = (natural_rows * scale)
        .round()
        .clamp(1.0, budget.rows as f64) as u16;
    (cols, rows)
}

/// Resize `image` to fit `cols × rows` cells and centre it on a transparent
/// canvas of exactly that many pixels, so every row of the block maps to a whole
/// number of pixel rows and a slice of it can be cut on a cell boundary.
fn scale_into(
    image: &DynamicImage,
    cols: u16,
    rows: u16,
    (cell_w, cell_h): FontSize,
) -> DynamicImage {
    let width = cols as u32 * cell_w as u32;
    let height = rows as u32 * cell_h as u32;
    let fitted = image.resize(width, height, FILTER);
    let mut canvas: DynamicImage =
        ImageBuffer::from_pixel(width, height, Rgba([0u8, 0, 0, 0])).into();
    let x = (width.saturating_sub(fitted.width()) / 2) as i64;
    let y = (height.saturating_sub(fitted.height()) / 2) as i64;
    imageops::overlay(&mut canvas, &fitted, x, y);
    canvas
}

/// Rows `top .. top + height` of a block, in pixels. `pixels` measures exactly
/// `rows` cells tall (see [`scale_into`]), so the cut lands on a cell boundary.
fn crop_rows(pixels: &DynamicImage, rows: u16, top: u16, height: u16) -> DynamicImage {
    let cell_h = pixels.height() / rows.max(1) as u32;
    pixels.crop_imm(
        0,
        top as u32 * cell_h,
        pixels.width(),
        height as u32 * cell_h,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const FONT: FontSize = (10, 20);

    fn budget(cols: u16, rows: u16) -> ImageBox {
        ImageBox { cols, rows }
    }

    /// A red/green/blue/black PNG on disk, `size × size` pixels, quartered.
    fn quartered_png(dir: &Path, size: u32) -> ImageRef {
        let mut pixels = image::RgbaImage::new(size, size);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = match (x < size / 2, y < size / 2) {
                (true, true) => Rgba([255, 0, 0, 255]),
                (false, true) => Rgba([0, 255, 0, 255]),
                (true, false) => Rgba([0, 0, 255, 255]),
                (false, false) => Rgba([0, 0, 0, 255]),
            };
        }
        let path = dir.join(format!("q{size}.png"));
        pixels.save(&path).expect("wrote the png");
        let bytes = std::fs::metadata(&path).unwrap().len() as usize;
        ImageRef {
            path,
            mime: "image/png".to_string(),
            bytes,
        }
    }

    /// A multiplexer is recognised from any of the three things that mark one.
    ///
    /// This is the guard on `Picker::from_query_stdio`, and what it prevents is
    /// not a missing image — it is a terminal agent that ignores the keyboard.
    /// The query's reader thread is detached and never joined, so under tmux
    /// (where `allow-passthrough` is off by default and no reply can arrive) it
    /// sits on stdin forever and races crossterm for the user's keystrokes.
    /// Measured before the fix: twenty keypresses into a tmux pane produced one
    /// character. See [`multiplexer`].
    ///
    /// Serial, and it restores what it finds: these are process-wide.
    #[test]
    fn a_multiplexer_is_recognised_however_it_announces_itself() {
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = ["TMUX", "STY", "TERM"]
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        let restore = || {
            for (key, value) in &saved {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        };
        let clear = || unsafe {
            std::env::remove_var("TMUX");
            std::env::remove_var("STY");
            std::env::remove_var("TERM");
        };

        clear();
        unsafe { std::env::set_var("TERM", "xterm-256color") };
        assert_eq!(multiplexer(), None, "a plain terminal is asked");

        clear();
        unsafe { std::env::set_var("TMUX", "/tmp/tmux-1000/default,123,0") };
        assert_eq!(multiplexer(), Some("tmux"), "$TMUX is the direct signal");

        clear();
        unsafe { std::env::set_var("STY", "1234.pts-0.host") };
        assert_eq!(multiplexer(), Some("screen"), "GNU screen sets $STY");

        // TERM alone, for the case where the variables were scrubbed — a
        // sudo, a `env -i`, a service manager — but TERM survived.
        clear();
        unsafe { std::env::set_var("TERM", "screen-256color") };
        assert_eq!(multiplexer(), Some("screen"));

        clear();
        unsafe { std::env::set_var("TERM", "tmux-256color") };
        assert_eq!(multiplexer(), Some("tmux"));

        restore();
    }

    #[test]
    fn a_big_image_is_shrunk_to_the_budget_and_keeps_its_shape() {
        // 1920×1080 is 192×54 cells; the rows bind first, so it lands 16 rows
        // tall and the columns follow the 16:9.
        let (cols, rows) = fit((1920, 1080), FONT, budget(60, 16));
        assert_eq!(rows, 16);
        assert_eq!(cols, 57, "16:9 in cell space, to the nearest column");

        // Now let it be as tall as it likes: the columns bind instead.
        let (cols, rows) = fit((1920, 1080), FONT, budget(60, 99));
        assert_eq!(cols, 60);
        assert_eq!(rows, 17);
    }

    #[test]
    fn a_small_image_is_never_blown_up() {
        // 16×16 px is under two cells; it stays that way in a huge budget.
        assert_eq!(fit((16, 16), FONT, budget(60, 16)), (2, 1));
        // And nothing ever rounds away to nothing.
        assert_eq!(fit((1, 1), FONT, budget(60, 16)), (1, 1));
    }

    #[test]
    fn the_budget_is_never_exceeded() {
        for pixels in [(4000, 10), (10, 4000), (1, 3000), (3000, 1), (640, 480)] {
            let (cols, rows) = fit(pixels, FONT, budget(20, 6));
            assert!((1..=20).contains(&cols), "{pixels:?} → {cols} cols");
            assert!((1..=6).contains(&rows), "{pixels:?} → {rows} rows");
        }
    }

    #[test]
    fn no_terminal_support_means_no_rows_are_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageCache::new(None);
        assert_eq!(cache.protocol(), None);
        assert_eq!(
            cache.layout(&quartered_png(dir.path(), 64), budget(60, 16)),
            None,
            "the caption is all it gets — the transcript reserves nothing"
        );
    }

    #[test]
    fn an_unreadable_file_is_read_once_and_then_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n not really").unwrap();
        let broken = ImageRef {
            path: path.clone(),
            mime: "image/png".to_string(),
            bytes: 20,
        };

        let mut cache = ImageCache::fallback();
        assert_eq!(cache.layout(&broken, budget(60, 16)), None);

        // Deleting it changes nothing: the failure was remembered, so no frame
        // ever goes back to the disk for it.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache.layout(&broken, budget(60, 16)), None);
    }

    /// The colour a half-block cell reads as: the crate puts the brighter of the
    /// cell's two halves in the foreground, and that is the quadrant's colour.
    /// Compared loosely, because resampling bleeds the quadrant boundaries by a
    /// few units — "recognisably red" is the claim, not "bit-for-bit red".
    #[track_caller]
    fn assert_colour(buf: &Buffer, (x, y): (u16, u16), want: (u8, u8, u8), what: &str) {
        let got = buf.cell((x, y)).expect("a cell in the buffer").fg;
        let Color::Rgb(r, g, b) = got else {
            panic!("{what} at ({x},{y}) is not 24-bit colour: {got:?}");
        };
        let near = |got: u8, want: u8| got.abs_diff(want) <= 24;
        assert!(
            near(r, want.0) && near(g, want.1) && near(b, want.2),
            "{what} at ({x},{y}) is {got:?}, not {want:?}"
        );
    }

    #[test]
    fn the_fallback_draws_a_recognisable_image_in_ordinary_cells() {
        let dir = tempfile::tempdir().unwrap();
        let image = quartered_png(dir.path(), 64);
        let mut cache = ImageCache::fallback();
        assert_eq!(cache.protocol(), Some(ProtocolType::Halfblocks));

        let block = cache.layout(&image, budget(60, 16)).expect("a block");
        assert_eq!((block.cols, block.rows), (7, 4), "square, in 1:2 cells");

        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        cache.draw(&mut buf, Rect::new(0, 0, block.cols, block.rows), &block, 0);

        // Every cell is a half-block (or a space, where the two halves agree),
        // and the four quadrants land in the four corners. That is a picture,
        // not noise.
        for y in 0..block.rows {
            for x in 0..block.cols {
                let symbol = buf.cell((x, y)).unwrap().symbol();
                assert!(
                    matches!(symbol, "▀" | "▄" | " "),
                    "({x},{y}) is {symbol:?}, not a half-block"
                );
            }
        }
        let (right, bottom) = (block.cols - 1, block.rows - 1);
        assert_colour(&buf, (0, 0), (255, 0, 0), "the top-left quadrant");
        assert_colour(&buf, (right, 0), (0, 255, 0), "the top-right quadrant");
        assert_colour(&buf, (0, bottom), (0, 0, 255), "the bottom-left quadrant");

        // Nothing was painted outside the block.
        assert_eq!(buf.cell((block.cols, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((0, block.rows)).unwrap().symbol(), " ");
    }

    #[test]
    fn a_block_straddling_the_viewport_is_clipped_to_the_rows_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let image = quartered_png(dir.path(), 64);
        let mut cache = ImageCache::fallback();
        let block = cache.layout(&image, budget(60, 16)).expect("a block");

        // The top two rows of the block have scrolled off the viewport: only the
        // bottom two are drawn, and what they hold is the *bottom* of the
        // picture — blue on the left — not the top of it drawn again.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        cache.draw(&mut buf, Rect::new(0, 0, block.cols, 2), &block, 2);
        assert_colour(&buf, (0, 0), (0, 0, 255), "the bottom-left quadrant");

        // The rows the scroll took are not painted — this is what keeps an image
        // from smearing over the transcript around it.
        for y in 2..10 {
            assert_eq!(
                buf.cell((0, y)).unwrap().symbol(),
                " ",
                "row {y} is outside the slice and was left alone"
            );
        }

        // And the top slice of the same block is the *top* of the picture, so the
        // two are genuinely different views of it.
        let mut top = Buffer::empty(Rect::new(0, 0, 20, 10));
        cache.draw(&mut top, Rect::new(0, 0, block.cols, 2), &block, 0);
        assert_colour(&top, (0, 0), (255, 0, 0), "the top-left quadrant");
    }

    #[test]
    fn the_pixel_work_is_done_once_per_size() {
        let dir = tempfile::tempdir().unwrap();
        let image = quartered_png(dir.path(), 64);
        let mut cache = ImageCache::fallback();
        let block = cache.layout(&image, budget(60, 16)).expect("a block");
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        let area = Rect::new(0, 0, block.cols, block.rows);
        cache.draw(&mut buf, area, &block, 0);

        // The file is gone, but the frame after it still draws: the scaled
        // pixels and the encoding are held, so a redraw touches no disk and does
        // no work.
        std::fs::remove_file(&image.path).unwrap();
        let mut again = Buffer::empty(Rect::new(0, 0, 20, 10));
        cache.draw(&mut again, area, &block, 0);
        assert_eq!(buf, again, "the same frame, from cache");

        // A slice of the same block reuses the same scaled pixels, so it draws
        // from the cache too.
        cache.draw(&mut again, Rect::new(0, 0, block.cols, 2), &block, 1);
        assert_ne!(buf, again, "and it drew something");
    }

    #[test]
    fn the_cache_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageCache::fallback();
        let images: Vec<ImageRef> = (1..=(CACHE_CAP as u32 + 2))
            .map(|n| quartered_png(dir.path(), 8 * n))
            .collect();
        for image in &images {
            assert!(cache.layout(image, budget(60, 16)).is_some());
        }
        assert_eq!(cache.entries.len(), CACHE_CAP, "the oldest were dropped");
        assert!(
            !cache.entries.contains_key(&images[0].path),
            "and the oldest is the first one drawn"
        );
        assert!(cache.entries.contains_key(&images.last().unwrap().path));
    }

    #[test]
    fn the_protocol_can_be_forced_by_name() {
        assert_eq!(parse_protocol("kitty"), Some(ProtocolType::Kitty));
        assert_eq!(parse_protocol("sixel"), Some(ProtocolType::Sixel));
        assert_eq!(parse_protocol("iterm2"), Some(ProtocolType::Iterm2));
        assert_eq!(parse_protocol("halfblocks"), Some(ProtocolType::Halfblocks));
        assert_eq!(parse_protocol("KITTY"), None, "exact names only");
        assert_eq!(parse_protocol("auto"), None, "handled before it gets here");
    }

    #[test]
    fn a_forced_protocol_encodes_with_that_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let image = quartered_png(dir.path(), 64);
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut cache = ImageCache::new(Some(picker));

        let block = cache.layout(&image, budget(60, 16)).expect("a block");
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        cache.draw(&mut buf, Rect::new(0, 0, block.cols, block.rows), &block, 0);

        // Kitty's unicode-placeholder form: the image is transmitted in an APC
        // escape and *placed* by ordinary cells, which is exactly why it can be
        // scrolled and diffed like text.
        let first = buf.cell((0, 0)).unwrap().symbol().to_string();
        assert!(
            first.contains("\x1b_G"),
            "an APC graphics escape: {first:?}"
        );
        assert!(
            first.contains('\u{10EEEE}'),
            "and a unicode placeholder: {first:?}"
        );

        // Straddling, it falls back to half-blocks — which is the whole point:
        // a partly-scrolled image is never a graphics escape.
        let mut sliced = Buffer::empty(Rect::new(0, 0, 20, 10));
        cache.draw(&mut sliced, Rect::new(0, 0, block.cols, 2), &block, 1);
        let symbol = sliced.cell((0, 0)).unwrap().symbol().to_string();
        assert!(
            matches!(symbol.as_str(), "▀" | "▄" | " "),
            "clipped rows are half-blocks, got {symbol:?}"
        );
    }

    /// Bytes handed out one at a time, the way `read_byte` hands out stdin.
    fn scripted(bytes: &'static [u8]) -> impl FnMut(Duration) -> Option<u8> {
        let mut at = 0;
        move |_wait| {
            let byte = bytes.get(at).copied();
            at += 1;
            byte
        }
    }

    /// The reply the benchmark pty gives: kitty, sixel, a 10×20 cell, DSR.
    const XTERM_LIKE: &[u8] = b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c\x1b[6;20;10t\x1b[0n";

    /// A terminal that says nothing costs one poll that comes back empty:
    /// the reply is read after the first frame, and there is no thread left
    /// behind on stdin to steal keystrokes.
    #[test]
    fn a_silent_terminal_is_halfblocks_and_nothing_waits_on_it() {
        let mut polls = 0;
        let started = Instant::now();
        let responses = collect(
            &mut |_wait| {
                polls += 1;
                None
            },
            PATIENCE,
        );
        assert!(responses.is_empty());
        assert_eq!(polls, 1, "silence is one poll, not a retry loop");
        assert!(
            started.elapsed() < PATIENCE,
            "the wait is the poll's, not ours"
        );
        let picker = picker_from(&responses, false, Some((9, 18)));
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);
    }

    /// A terminal that answers gets its own protocol at its own cell size,
    /// and the reply is read up to the status report and not one byte past
    /// it: the keystroke behind it stays on stdin for crossterm.
    #[test]
    fn a_reply_picks_the_real_protocol_and_leaves_the_next_byte_alone() {
        let mut next = scripted(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c\x1b[6;20;10t\x1b[0nq");
        let responses = collect(&mut next, PATIENCE);
        assert_eq!(
            responses,
            vec![
                Response::Kitty,
                Response::Sixel,
                Response::CellSize(Some((10, 20)))
            ]
        );
        assert_eq!(
            next(PATIENCE),
            Some(b'q'),
            "typed behind the reply, still there"
        );

        let picker = picker_from(&responses, false, None);
        assert_eq!(picker.protocol_type(), ProtocolType::Kitty);
        assert_eq!(picker.font_size(), (10, 20));
    }

    /// Sixel alone is sixel; the tty's pixel size stands in for a missing
    /// `CSI 16 t`; and without any cell size there is nothing to place pixels
    /// with, so the answer is half-blocks whatever the terminal claimed.
    #[test]
    fn the_cell_size_decides_whether_a_protocol_is_usable() {
        let mut next = scripted(b"\x1b[?62;4;22c\x1b[0n");
        let responses = collect(&mut next, PATIENCE);
        let picker = picker_from(&responses, false, Some((8, 16)));
        assert_eq!(picker.protocol_type(), ProtocolType::Sixel);
        assert_eq!(picker.font_size(), (8, 16));

        let picker = picker_from(&[Response::Kitty], false, None);
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);
    }

    /// WezTerm and Konsole say yes to kitty and draw it wrong, so their yes
    /// is not taken; the cell size still is.
    #[test]
    fn a_quirky_terminal_keeps_its_cell_size_but_not_its_kitty() {
        let responses = collect(&mut scripted(XTERM_LIKE), PATIENCE);
        let picker = picker_from(&responses, true, None);
        assert_ne!(picker.protocol_type(), ProtocolType::Kitty);
        assert_ne!(picker.protocol_type(), ProtocolType::Sixel);
        assert_eq!(picker.font_size(), (10, 20));
    }

    /// A reply cut off before the status report keeps what did arrive.
    #[test]
    fn a_reply_that_never_finishes_still_counts_what_it_said() {
        let responses = collect(&mut scripted(b"\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t"), PATIENCE);
        assert_eq!(
            responses,
            vec![Response::Kitty, Response::CellSize(Some((10, 20)))]
        );
    }
}
