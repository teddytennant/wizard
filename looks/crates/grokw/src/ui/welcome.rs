// OWNER: welcome (sign-in and gate screens are not built)
//! The home screen: header, a hero card (logo, menu, announcement) on wide terminals or a
//! stacked column on narrow ones, a tip row, the composer and a version footer. Row maths follows
//! spec 2.3 to 2.6; the logo shimmers on a wall clock (`anim::logo_color`).

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::width::{display_width, wrap};

use super::{anim, bold, put, put_right, st, Layout};
use crate::app::App;

pub const LOGO07: [&str; 7] = [
    "⠀⠀⠀⠀⠀⠀⣀⣀⡀⠀⠀⠀⢀⠄",
    "⠀⠀⠀⣠⣾⠿⠛⠛⠛⠛⢀⡴⠁⠀",
    "⠀⠀⣼⡟⠁⠀⠀⠀⢀⡴⠻⣿⡀⠀",
    "⠀⠀⣿⡇⠀⠀⠀⠔⠁⠀⠀⣿⡇⠀",
    "⠀⠀⢹⣷⠀⠀⠀⠀⠀⢀⣴⡿⠀⠀",
    "⠀⢀⠞⠁⠠⢶⣶⣶⣶⠿⠋⠀⠀⠀",
    "⠐⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

pub const LOGO05: [&str; 5] = [
    "⠀⠀⠀⣀⣤⣤⣀⠀⠀⡠",
    "⠀⢀⡾⠋⠁⠀⢁⢴⡎⠀",
    "⠀⢸⡇⠀⠀⠐⠁⢀⣿⠀",
    "⠀⢈⠗⢀⣀⣀⣠⡾⠃⠀",
    "⠐⠁⠀⠈⠉⠉⠉⠀⠀⠀",
];

/// grokw's own announcement; Grok's comes from a server.
pub const ANNOUNCEMENT: (&str, &str) = (
    "Welcome to wizard.",
    "This is its grok look. Type /ui to see the others or to switch back to the wizard look.",
);
/// The announcement of this launch: the override a test sets, else grokw's own.
pub fn announcement(app: &App) -> (String, String) {
    app.home
        .announcement
        .clone()
        .unwrap_or_else(|| (ANNOUNCEMENT.0.to_string(), ANNOUNCEMENT.1.to_string()))
}

/// `startup::StartupWarning`: one line of the banner above the card or menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Painted in the warning colour.
    Warning,
    /// Dim: informational, nothing to fix.
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupWarning {
    pub severity: Severity,
    pub message: String,
    pub action: Option<String>,
}

impl StartupWarning {
    pub fn clipboard() -> StartupWarning {
        StartupWarning {
            severity: Severity::Warning,
            message: "Clipboard may be unreachable.".into(),
            action: Some("Run /doctor for details and fixes.".into()),
        }
    }
}

/// The warning the single-slot banner shows: the first `Warning`, else the newest entry.
pub fn banner_warning(list: &[StartupWarning]) -> Option<&StartupWarning> {
    list.iter()
        .find(|w| w.severity == Severity::Warning)
        .or_else(|| list.last())
}

/// Warnings for this launch. Grok shows "Clipboard may be unreachable." only over SSH, inside
/// tmux, with `extended-keys` off or `allow-passthrough` not on; everything else a terminal
/// probe could find stays suppressed. `GROKW_STARTUP_WARNING=clipboard` forces it (screenshots).
pub fn probe_warnings() -> Vec<StartupWarning> {
    if std::env::var("GROKW_STARTUP_WARNING").as_deref() == Ok("clipboard") {
        return vec![StartupWarning::clipboard()];
    }
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let ssh = env("SSH_CONNECTION")
        .or(env("SSH_CLIENT"))
        .or(env("SSH_TTY"));
    if ssh.is_none() || env("TMUX").is_none() {
        return Vec::new();
    }
    let opt = |name: &str| -> Option<String> {
        let out = std::process::Command::new("tmux")
            .args(["show-options", "-gqv", name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let keys_off = opt("extended-keys").as_deref() == Some("off");
    // the option only exists from tmux 3.3; before that passthrough worked unconditionally
    let passthrough_off =
        opt("allow-passthrough").is_some_and(|v| !v.is_empty() && v != "on" && v != "all");
    if keys_off || passthrough_off {
        vec![StartupWarning::clipboard()]
    } else {
        Vec::new()
    }
}

/// Rows the banner takes: its lines, the action line and one spacer.
fn warning_rows(list: &[StartupWarning]) -> u16 {
    banner_warning(list).map_or(0, |w| {
        w.message.lines().count() as u16 + w.action.is_some() as u16 + 1
    })
}

pub const SUBTITLE: &str = "Thanks for trying Wizard, give feedback with /feedback!";
pub const MENU: [(&str, &str); 4] = [
    ("New worktree", "ctrl+w"),
    ("Resume session", "ctrl+r"),
    ("Changelog", ""),
    ("Quit", "ctrl+q"),
];

const TIPS: [&str; 10] = [
    "Use Shift+Tab to cycle between modes like Plan mode.",
    "Use @! for hidden or ignored files: @!.github/workflows.",
    "Use Ctrl+Enter to interject messages. Or just Enter to queue messages.",
    "Press Ctrl+B to background a running terminal command.",
    "Try out workflows using /workflows.",
    "Press Ctrl+O to toggle auto-approve mode.",
    "Run /compact [context] when chat gets long.",
    "Use @ to attach files like @src/main.rs.",
    "Run /dashboard (or Ctrl+\\) to see and manage all your agents in one place.",
    "Start Wizard in a fresh worktree with `-w`; add `-r <session-id>` to resume an existing session there.",
];

/// The tip for this launch: `tips[cursor % len]`, then the cursor moves on.
pub fn next_tip(dir: Option<&Path>) -> String {
    let n = dir
        .and_then(|d| std::fs::read_to_string(d.join("tip_cursor")).ok())
        .and_then(|t| t.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if let Some(d) = dir {
        let _ = std::fs::create_dir_all(d);
        let _ = std::fs::write(d.join("tip_cursor"), (n + 1).to_string());
    }
    TIPS[n % TIPS.len()].to_string()
}

#[derive(Clone, Debug)]
pub struct HomeLayout {
    pub card: Option<Rect>,
    /// Logo top-left and the art (7 or 5 rows), `None` when hidden.
    pub logo: Option<(u16, u16, bool)>,
    pub version_hdr: Option<(u16, u16)>,
    pub subtitle_y: Option<u16>,
    /// Column and width of the menu, and the row of its first item.
    pub menu_x: u16,
    pub menu_w: u16,
    pub menu_y: u16,
    /// Announcement rows: `(x, y, width, message lines)`.
    pub info: Option<(u16, u16, u16, usize)>,
    pub tip_y: u16,
    pub tip_rows: u16,
    pub version_y: u16,
    /// First row of the banner (its gap row sits above it).
    pub warn_y: Option<u16>,
}

fn tip_rows(app: &App, w: u16, hpad: u16) -> u16 {
    let avail = w.saturating_sub(2 * hpad).max(1) as usize;
    let len = display_width(&format!("Tip: {}", app.home.tip));
    len.div_ceil(avail).max(1) as u16
}

pub fn layout(app: &App, lay: &Layout) -> HomeLayout {
    let (w, h) = (lay.w, lay.h);
    let hpad = lay.hpad;
    let content_h = h.saturating_sub(3) as i32;
    let tip_h = tip_rows(app, w, hpad) as i32;
    let prompt_h = 3;
    let fixed_below = tip_h + 1 + prompt_h + 2;
    let menu_h = MENU.len() as i32;
    let tip_y = h.saturating_sub(7 + tip_h as u16);
    let version_y = h.saturating_sub(2);
    let picker = app.home.picker.is_some();
    let warn_h = warning_rows(&app.home.warnings) as i32;
    let warn_gap = (warn_h > 0) as i32;

    // hero card
    let card_w = (w as i32 - 6).min(120);
    if w >= 90 && !picker {
        let right_w = card_w - 2 - 21;
        // announcement lines wrapped to the right column; 0 means the subtitle row
        let msg_lines = wrap(&announcement(app).1, right_w.max(10) as usize)
            .len()
            .min(2) as i32;
        let has_ann = !announcement(app).0.is_empty();
        let options: &[i32] = if has_ann {
            &[1 + msg_lines, 2, 0]
        } else {
            &[0]
        };
        for &info in options {
            let right_col_h = 1 + if info == 0 { 1 } else { 1 + info } + 1 + menu_h;
            let box_h = 4 + 7.max(right_col_h);
            let min_content = warn_gap + warn_h + box_h + 1 + fixed_below;
            if content_h >= min_content {
                // the banner sits in the top padding, the card follows it
                let above = warn_gap + warn_h;
                let pad = ((content_h - above - box_h - 7) / 3).max(0);
                let top = 2 + pad + above;
                let card_x = ((w as i32 - card_w) / 2).max(0) as u16;
                let card = Rect::new(card_x, top as u16, card_w as u16, box_h as u16);
                let inner_x = card.x + 1;
                let rx = inner_x + 19;
                let ver_y = card.y + 2;
                let (info_rows, subtitle_y) = if info == 0 {
                    (None, Some(ver_y + 1))
                } else {
                    (
                        Some((rx, ver_y + 2, right_w as u16, (info - 1) as usize)),
                        None,
                    )
                };
                let menu_y = ver_y + if info == 0 { 3 } else { 3 + info as u16 };
                return HomeLayout {
                    card: Some(card),
                    logo: Some((inner_x + 2, card.y + 2, true)),
                    version_hdr: Some((rx, ver_y)),
                    subtitle_y,
                    menu_x: rx,
                    menu_w: right_w as u16,
                    menu_y,
                    info: info_rows,
                    tip_y,
                    tip_rows: tip_h as u16,
                    version_y,
                    warn_y: (warn_h > 0).then(|| (top - warn_h) as u16),
                };
            }
        }
    }

    // stacked column
    let longest = MENU
        .iter()
        .map(|(l, k)| display_width(l) + display_width(k) + 4)
        .max()
        .unwrap_or(0);
    let menu_w = (longest.max(51).max(30) as u16)
        .min(w.saturating_sub(4))
        .max(10);
    let menu_x = (w.saturating_sub(menu_w)).div_ceil(2);
    let msg_lines = wrap(&announcement(app).1, menu_w as usize).len().min(2) as i32;
    let mut info_h = if announcement(app).0.is_empty() {
        0
    } else {
        1 + msg_lines
    };
    // logo tier by content height
    let mut logo_rows: i32 = if content_h < 22 {
        0
    } else if content_h >= 26 {
        7
    } else {
        5
    };
    let need = |logo: i32, info: i32| {
        logo + 1
            + warn_gap
            + warn_h
            + menu_h
            + if info > 0 { info + 1 } else { 0 }
            + 1
            + fixed_below
    };
    while logo_rows > 0 && need(logo_rows, info_h) > content_h {
        logo_rows = if logo_rows == 7 { 5 } else { 0 };
    }
    // info rows are budgeted to what fits; below that the announcement shrinks
    while info_h > 0 && need(0, info_h) > content_h {
        info_h -= 1;
    }
    let info_gap = if info_h > 0 { 1 } else { 0 };
    let fixed_above = logo_rows + 1 + warn_gap + warn_h;
    let a = (content_h - fixed_above - 4 - info_gap - info_h - fixed_below) / 3;
    let b = content_h - (fixed_above + menu_h + info_gap + info_h + 1 + fixed_below);
    let top_pad = a.min(b).max(0);
    let mut y = 2 + top_pad;
    let logo = (logo_rows > 0).then(|| {
        let cols = if logo_rows == 7 { 14 } else { 10 };
        let x = (w as i32 - cols) / 2;
        (x.max(0) as u16, y as u16, logo_rows == 7)
    });
    y += logo_rows + 1;
    let warn_y = (warn_h > 0).then(|| (y + warn_gap) as u16);
    y += warn_gap + warn_h;
    let menu_y = y as u16;
    y += menu_h + info_gap;
    let info = (info_h > 0).then(|| (menu_x, y as u16, menu_w, (info_h - 1) as usize));
    HomeLayout {
        card: None,
        logo,
        version_hdr: None,
        subtitle_y: None,
        menu_x,
        menu_w,
        menu_y,
        info,
        tip_y,
        tip_rows: tip_h as u16,
        version_y,
        warn_y,
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    super::footer::draw_header(buf, app, lay);
    if app.home.picker.is_some() {
        super::dialogs::draw_home_picker(buf, app, lay);
        return;
    }
    let th = app.theme.clone();
    let hl = layout(app, lay);
    let secs = app.clock.secs();
    // card border
    if let Some(card) = hl.card {
        draw_card_border(buf, card, th.hero_border);
    }
    // logo
    if let Some((lx, ly, full)) = hl.logo {
        let art: &[&str] = if full { &LOGO07 } else { &LOGO05 };
        let rows = art.len();
        let cols = art[0].chars().count();
        for (r, line) in art.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                let color = anim::logo_color(&th, r, c, rows, cols, secs);
                let mut s = [0u8; 4];
                put(
                    buf,
                    lx + c as u16,
                    ly + r as u16,
                    ch.encode_utf8(&mut s),
                    st(color),
                );
            }
        }
    }
    // startup warning, centred over the full width (an odd slack leaves the extra column left)
    if let (Some(y), Some(w)) = (hl.warn_y, banner_warning(&app.home.warnings)) {
        let color = match w.severity {
            Severity::Warning => th.warning,
            Severity::Info => th.gray_dim,
        };
        let lines = w.message.lines().chain(w.action.as_deref());
        for (i, l) in lines.enumerate() {
            let x = (lay.w as usize)
                .saturating_sub(display_width(l))
                .div_ceil(2) as u16;
            put(buf, x, y + i as u16, l, st(color));
        }
    }
    // version line and subtitle (card only)
    if let Some((x, y)) = hl.version_hdr {
        let nx = put(buf, x, y, "Wizard  ", bold(th.text_primary));
        put(buf, nx, y, app.config.version(), st(th.gray));
    }
    if let Some(y) = hl.subtitle_y {
        put(buf, hl.menu_x, y, SUBTITLE, st(th.gray));
    }
    // announcement
    if let Some((x, y, w, n)) = hl.info {
        let (title, msg) = announcement(app);
        put(buf, x, y, &title, bold(th.warning));
        for (i, l) in wrap(&msg, w as usize).into_iter().take(n).enumerate() {
            put(buf, x, y + 1 + i as u16, &l, st(th.gray));
        }
    }
    // menu
    let sel = app.home.menu_sel.or(app.home.hover);
    for (i, (label, key)) in MENU.iter().enumerate() {
        let y = hl.menu_y + i as u16;
        // too small a window: rows that would land on the tip are not drawn
        if y >= hl.tip_y {
            break;
        }
        if sel == Some(i) {
            super::fill(buf, Rect::new(hl.menu_x, y, hl.menu_w, 1), th.bg_light);
        }
        let bg = (sel == Some(i)).then_some(th.bg_light);
        let mut ls = bold(th.text_primary);
        let mut ks = st(th.gray_bright);
        if let Some(b) = bg {
            ls = ls.bg(b);
            ks = ks.bg(b);
        }
        put(buf, hl.menu_x, y, label, ls);
        if !key.is_empty() {
            put_right(buf, hl.menu_x + hl.menu_w, y, key, ks);
        }
    }
    // tip
    let tip_x = if lay.compact { lay.hpad } else { lay.hpad + 1 };
    let tip = format!("Tip: {}", app.home.tip);
    let avail = (lay.w.saturating_sub(2 * lay.hpad)) as usize;
    let rows = wrap(&tip, avail.max(10));
    for (i, l) in rows.iter().enumerate() {
        let y = hl.tip_y + i as u16;
        match l.strip_prefix("Tip: ").filter(|_| i == 0) {
            Some(rest) => {
                let nx = put(buf, tip_x, y, "Tip: ", bold(th.gray));
                put(buf, nx, y, rest, st(th.gray));
            }
            None => {
                put(buf, tip_x, y, l, st(th.gray));
            }
        }
    }
    // version footer; an armed Ctrl+Q turns the row into the confirm hint
    let vy = hl.version_y;
    let end = lay.w.saturating_sub(lay.hpad);
    if let Some(p) = &app.pending {
        let (k, rest) = p.label.split_once(':').unwrap_or((&p.label, ""));
        let x = put(buf, 2, vy, "  ", st(th.gray));
        let x = put(buf, x, vy, k, bold(th.accent_user));
        put(buf, x, vy, &format!(":{rest}"), st(th.gray));
    } else if hl.card.is_some() {
        put_right(buf, end, vy, "[alpha]", st(th.gray));
    } else {
        let tail = format!("{} [alpha]", app.config.version());
        let x = put_right(buf, end, vy, &tail, st(th.gray));
        put_right(buf, x, vy, "Wizard  ", bold(th.text_primary));
    }
}

fn draw_card_border(buf: &mut Buffer, card: Rect, c: ratatui::style::Color) {
    if card.width < 2 || card.height < 2 {
        return;
    }
    let inner = card.width as usize - 2;
    let top = format!("╭{}╮", "─".repeat(inner));
    let bot = format!("╰{}╯", "─".repeat(inner));
    put(buf, card.x, card.y, &top, st(c));
    put(buf, card.x, card.y + card.height - 1, &bot, st(c));
    for y in card.y + 1..card.y + card.height - 1 {
        put(buf, card.x, y, "│", st(c));
        put(buf, card.x + card.width - 1, y, "│", st(c));
    }
}

/// Row of the menu item under `(x, y)`, if any.
pub fn menu_hit(app: &App, lay: &Layout, x: u16, y: u16) -> Option<usize> {
    let hl = layout(app, lay);
    if x < hl.menu_x || x >= hl.menu_x + hl.menu_w {
        return None;
    }
    let i = y.checked_sub(hl.menu_y)? as usize;
    (i < MENU.len()).then_some(i)
}
