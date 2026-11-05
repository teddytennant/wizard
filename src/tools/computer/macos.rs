//! macOS desktop backend: the CoreGraphics event API (`CGEvent`) for input —
//! this is the system "Accessibility" automation path — and `screencapture`
//! for screen capture.
//!
//! Two permissions must be granted to the terminal (or app bundle) running
//! Wizard, under System Settings → Privacy & Security:
//! - **Accessibility** — lets `CGEvent` posts move the mouse and press keys.
//! - **Screen Recording** — lets `screencapture` read the screen.
//!
//! Without them the OS silently drops the events / returns a blank capture.
//! `wizard desktop-setup` prints the exact steps.
//!
//! Coordinates are reported and consumed in **points** (the `CGEvent`
//! coordinate space). On a Retina display the captured PNG has twice the
//! pixel dimensions, but the model is told the point size so its clicks land
//! correctly without any per-call scaling state.

use anyhow::{Context, Result, anyhow, bail};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::{Backend, MouseButton, Screenshot, ScrollDirection, png_dimensions};

/// macOS backend. Stateless — each method builds and posts fresh events.
pub(crate) struct MacosBackend;

impl MacosBackend {
    pub(crate) fn new() -> Self {
        Self
    }
}

/// A fresh HID event source. CoreGraphics consumes the source per event, so
/// we mint one each time.
fn source() -> Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("could not create a CoreGraphics event source"))
}

/// Current pointer position in points.
fn current_point() -> Result<CGPoint> {
    let event =
        CGEvent::new(source()?).map_err(|_| anyhow!("could not read the pointer position"))?;
    Ok(event.location())
}

/// Post a mouse event of `kind` at `point` for `button`.
fn post_mouse(
    kind: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
    click_state: i64,
) -> Result<()> {
    let event = CGEvent::new_mouse_event(source()?, kind, point, button)
        .map_err(|_| anyhow!("could not create mouse event"))?;
    if click_state > 1 {
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
    }
    event.post(CGEventTapLocation::HID);
    Ok(())
}

impl Backend for MacosBackend {
    fn label(&self) -> String {
        "macos: CoreGraphics (Accessibility) input, screencapture".to_string()
    }

    fn screenshot(&self) -> Result<Screenshot> {
        // Capture the main display to a temp PNG, read it, then remove it.
        // (`screencapture` cannot reliably stream PNG to stdout.)
        let path = std::env::temp_dir().join(format!("wizard-shot-{}.png", std::process::id()));
        let status = std::process::Command::new("screencapture")
            .arg("-x") // silent (no shutter sound)
            .arg("-t")
            .arg("png")
            .arg(&path)
            .status()
            .context("running screencapture (is it on PATH?)")?;
        if !status.success() {
            bail!("screencapture failed — grant Screen Recording permission to the terminal");
        }
        let png = std::fs::read(&path).with_context(|| {
            format!(
                "reading capture {} — grant Screen Recording permission",
                path.display()
            )
        })?;
        let _ = std::fs::remove_file(&path);
        if png_dimensions(&png).is_none() {
            bail!("screencapture did not produce a valid PNG (check Screen Recording permission)");
        }
        // Report the logical point size (the CGEvent coordinate space), not the
        // PNG's pixel size, so model coordinates map 1:1 even on Retina.
        let bounds = CGDisplay::main().bounds();
        Ok(Screenshot {
            png,
            width: bounds.size.width as u32,
            height: bounds.size.height as u32,
        })
    }

    fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        post_mouse(
            CGEventType::MouseMoved,
            CGPoint::new(x as f64, y as f64),
            CGMouseButton::Left,
            0,
        )
    }

    fn click(&self, button: MouseButton, count: u32) -> Result<()> {
        let point = current_point()?;
        let (down, up, cg_button) = match button {
            MouseButton::Left => (
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGMouseButton::Left,
            ),
            MouseButton::Right => (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGMouseButton::Right,
            ),
            MouseButton::Middle => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
            ),
        };
        let count = count.max(1) as i64;
        post_mouse(down, point, cg_button, count)?;
        post_mouse(up, point, cg_button, count)?;
        Ok(())
    }

    fn drag(&self, x: i32, y: i32) -> Result<()> {
        let start = current_point()?;
        let end = CGPoint::new(x as f64, y as f64);
        post_mouse(CGEventType::LeftMouseDown, start, CGMouseButton::Left, 1)?;
        post_mouse(CGEventType::LeftMouseDragged, end, CGMouseButton::Left, 1)?;
        post_mouse(CGEventType::LeftMouseUp, end, CGMouseButton::Left, 1)?;
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<()> {
        // A keyboard event with keycode 0 plus an attached Unicode string
        // injects arbitrary text without per-character keycode mapping.
        let down = CGEvent::new_keyboard_event(source()?, 0, true)
            .map_err(|_| anyhow!("could not create keyboard event"))?;
        down.set_string(text);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(source()?, 0, false)
            .map_err(|_| anyhow!("could not create keyboard event"))?;
        up.set_string(text);
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn key(&self, chord: &str) -> Result<()> {
        let (flags, keycode) = parse_chord(chord)?;
        let down = CGEvent::new_keyboard_event(source()?, keycode, true)
            .map_err(|_| anyhow!("could not create keyboard event"))?;
        down.set_flags(flags);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(source()?, keycode, false)
            .map_err(|_| anyhow!("could not create keyboard event"))?;
        up.set_flags(flags);
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn scroll(&self, direction: ScrollDirection, amount: u32) -> Result<()> {
        // One line per notch; sign sets the direction. wheel1 is vertical,
        // wheel2 is horizontal.
        let step = (amount.max(1) as i32) * 3;
        let (v, h) = match direction {
            ScrollDirection::Up => (step, 0),
            ScrollDirection::Down => (-step, 0),
            ScrollDirection::Left => (0, step),
            ScrollDirection::Right => (0, -step),
        };
        let event = CGEvent::new_scroll_event(source()?, ScrollEventUnit::LINE, 2, v, h, 0)
            .map_err(|_| anyhow!("could not create scroll event"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn cursor_position(&self) -> Result<(i32, i32)> {
        let point = current_point()?;
        Ok((point.x as i32, point.y as i32))
    }
}

/// Parse a key chord into CoreGraphics modifier flags plus the main key's
/// virtual keycode.
fn parse_chord(chord: &str) -> Result<(CGEventFlags, CGKeyCode)> {
    let mut flags = CGEventFlags::empty();
    let mut main = None;
    for token in chord.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        if let Some(flag) = modifier_flag(token) {
            flags |= flag;
        } else if let Some(code) = keycode(token) {
            if main.is_some() {
                bail!("chord '{chord}' has more than one non-modifier key");
            }
            main = Some(code);
        } else {
            bail!("unknown key '{token}' in chord '{chord}'");
        }
    }
    let main = main.ok_or_else(|| anyhow!("chord '{chord}' has no main key"))?;
    Ok((flags, main))
}

/// Map a modifier name to its CoreGraphics flag.
fn modifier_flag(name: &str) -> Option<CGEventFlags> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => CGEventFlags::CGEventFlagControl,
        "shift" => CGEventFlags::CGEventFlagShift,
        "alt" | "option" | "opt" => CGEventFlags::CGEventFlagAlternate,
        "meta" | "super" | "win" | "cmd" | "command" => CGEventFlags::CGEventFlagCommand,
        _ => return None,
    })
}

/// Map a key name to a macOS (Carbon) virtual keycode.
fn keycode(name: &str) -> Option<CGKeyCode> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "return" | "enter" => 36,
        "tab" => 48,
        "space" | "spacebar" => 49,
        "backspace" => 51,
        "delete" | "del" => 117,
        "escape" | "esc" => 53,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "home" => 115,
        "end" => 119,
        "pageup" | "pgup" => 116,
        "pagedown" | "pgdn" => 121,
        "minus" | "-" => 27,
        "equal" | "=" => 24,
        "comma" | "," => 43,
        "period" | "dot" | "." => 47,
        "slash" | "/" => 44,
        "backslash" | "\\" => 42,
        "semicolon" | ";" => 41,
        "apostrophe" | "'" => 39,
        "grave" | "`" => 50,
        "leftbracket" | "[" => 33,
        "rightbracket" | "]" => 30,
        _ => u16::MAX,
    };
    if named != u16::MAX {
        return Some(named);
    }
    if lower.len() == 1 {
        let c = lower.as_bytes()[0];
        if c.is_ascii_lowercase() {
            // a..z in Carbon virtual-keycode order.
            const LETTERS: [u16; 26] = [
                0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13,
                7, 16, 6,
            ];
            return Some(LETTERS[(c - b'a') as usize]);
        }
        if c.is_ascii_digit() {
            const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
            return Some(DIGITS[(c - b'0') as usize]);
        }
    }
    if let Some(rest) = lower.strip_prefix('f')
        && let Ok(n) = rest.parse::<u8>()
    {
        return match n {
            1 => Some(122),
            2 => Some(120),
            3 => Some(99),
            4 => Some(118),
            5 => Some(96),
            6 => Some(97),
            7 => Some(98),
            8 => Some(100),
            9 => Some(101),
            10 => Some(109),
            11 => Some(103),
            12 => Some(111),
            _ => None,
        };
    }
    None
}
