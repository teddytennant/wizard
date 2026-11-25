// OWNER: renderer
//! What the terminal and the session around it can do, decided from the environment and the
//! startup probe in one place.
//!
//! Ported from Codex 0.174.0-alpha.6 (`openai/codex` 06f334aa), `codex-rs/tui/src/
//! terminal_capabilities.rs`, `terminal_capabilities/terminal_identity.rs`,
//! `terminal_capabilities/tmux_clipboard.rs` and `codex-rs/terminal-detection/src/{lib,color}.rs`,
//! Apache-2.0. Modified: the environment is read through a lookup function so tests can pass
//! their own, and only the decisions codexw acts on are kept (alt-screen probing, clipboard route,
//! colour depth). Upstream's image, bidi, notification and pointer-shape capabilities have no
//! counterpart here.

use std::io::Write;
use std::process::{Command, Stdio};

/// `TERM` Ghostty sets (Codex terminal-detection `GHOSTTY_TERM`).
pub const GHOSTTY_TERM: &str = "xterm-ghostty";

/// An environment lookup: the process environment in the app, a table in tests.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment.
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn non_empty(env: Env, key: &str) -> bool {
    env(key).is_some_and(|v| !v.is_empty())
}

/// A terminal multiplexer between codexw and the terminal (Codex terminal-detection
/// `detect_multiplexer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Multiplexer {
    Tmux,
    Zellij,
}

pub fn multiplexer(env: Env) -> Option<Multiplexer> {
    if non_empty(env, "TMUX") || non_empty(env, "TMUX_PANE") {
        Some(Multiplexer::Tmux)
    } else if non_empty(env, "ZELLIJ")
        || non_empty(env, "ZELLIJ_SESSION_NAME")
        || non_empty(env, "ZELLIJ_VERSION")
    {
        Some(Multiplexer::Zellij)
    } else {
        None
    }
}

/// Codex `is_ssh_session`.
pub fn is_ssh_session(env: Env) -> bool {
    env("SSH_TTY").is_some() || env("SSH_CONNECTION").is_some()
}

/// Codex `tmux_transport`: output goes through a tmux server.
pub fn tmux_transport(env: Env) -> bool {
    env("TMUX").is_some()
}

/// Codex `is_tmux_session`.
pub fn is_tmux_session(env: Env) -> bool {
    tmux_transport(env) || env("TMUX_PANE").is_some()
}

/// Codex `screen_active`: GNU screen sits in the output path.
pub fn screen_active(env: Env) -> bool {
    env("STY").is_some()
}

/// Codex `should_probe_inline_screen`: only a direct SSH session asks the terminal who it is.
/// A multiplexer cannot speak for the client attached to it.
pub fn should_probe_inline_screen(env: Env) -> bool {
    multiplexer(env).is_none() && is_ssh_session(env)
}

/// Codex `terminal_identity.rs` `requires_inline_screen`: Terminal.app answers primary DA with
/// `1;2` and secondary DA with `1;95;0`; that pair asks for the inline screen, which keeps the
/// terminal's own scrollback. Both replies must be complete; a pasted look-alike (inside
/// bracketed paste) does not count.
pub fn requires_inline_screen(input: &[u8]) -> Option<bool> {
    let primary = device_attributes(input, b"[?")?;
    let secondary = device_attributes(input, b"[>")?;
    Some(primary == b"1;2" && secondary == b"1;95;0")
}

/// Codex `terminal_identity.rs` `device_attributes`.
fn device_attributes<'a>(input: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let mut inside_paste = false;
    for sequence in input.split(|byte| *byte == b'\x1b').skip(1) {
        match sequence {
            [b'[', b'2', b'0', b'0', b'~', ..] => inside_paste = true,
            [b'[', b'2', b'0', b'1', b'~', ..] => inside_paste = false,
            _ if !inside_paste => {
                if let Some(payload) = sequence.strip_prefix(prefix) {
                    if let Some(end) = payload
                        .iter()
                        .take(64)
                        .position(|byte| (0x40..=0x7e).contains(byte))
                    {
                        if payload[end] == b'c' {
                            return Some(&payload[..end]);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Codex terminal-detection `color.rs` `ansi_color_level`: SSH can forward `TERM` without
/// `COLORTERM`. Ghostty's own `TERM` still means truecolor, unless colour is being forced, since a
/// multiplexer can inherit Ghostty's identity but advertise another palette.
pub fn ghostty_truecolor(env: Env) -> bool {
    env("TERM").as_deref() == Some(GHOSTTY_TERM)
        && env("FORCE_COLOR").is_none()
        && env("CLICOLOR_FORCE").is_none_or(|v| v == "0")
}

/// How a clipboard write reaches the terminal (Codex `ClipboardCopyMode`, the cases codexw has).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardCopyMode {
    /// The platform tool, then OSC 52 when there is none.
    FallbackOsc52,
    /// OSC 52 only: the platform clipboard is on another machine.
    Osc52,
    /// tmux's own clipboard (`load-buffer -w`), then OSC 52.
    ClientBuffer,
}

/// Codex `ClipboardCapabilities::detect`, without the VS Code dev-container case.
pub fn clipboard_copy_mode(env: Env) -> ClipboardCopyMode {
    if is_tmux_session(env) {
        ClipboardCopyMode::ClientBuffer
    } else if is_ssh_session(env) || env("HERDR_ENV").is_some_and(|v| v == "1") {
        ClipboardCopyMode::Osc52
    } else {
        ClipboardCopyMode::FallbackOsc52
    }
}

/// How an OSC 52 write gets through what sits between codexw and the terminal (Codex
/// `Osc52Target`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Osc52Target {
    Direct,
    /// Wrapped in tmux's DCS passthrough.
    EscapedDcs,
    /// Wrapped in GNU screen's DCS, in chunks screen accepts.
    ChunkedDcs,
}

impl Osc52Target {
    pub fn detect(env: Env) -> Self {
        if tmux_transport(env) {
            Self::EscapedDcs
        } else if screen_active(env) {
            Self::ChunkedDcs
        } else {
            Self::Direct
        }
    }
}

/// Codex `escaped_dcs`: embed a command in tmux's DCS passthrough.
pub fn escaped_dcs(command: &str) -> String {
    let escaped = command.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// Codex `clipboard_copy.rs` `osc52_sequence`, the `ChunkedDcs` arm: screen buffers each DCS
/// separately, so the OSC goes out in pieces below its older 256-byte string limit.
pub fn chunked_dcs(osc: &str) -> String {
    const CHUNK_SIZE: usize = 252;
    let mut wrapped = String::new();
    for start in (0..osc.len()).step_by(CHUNK_SIZE) {
        let end = (start + CHUNK_SIZE).min(osc.len());
        // OSC 52 contains only ASCII, so every byte offset is a character boundary.
        wrapped.push_str("\x1bP");
        wrapped.push_str(&osc[start..end]);
        wrapped.push_str("\x1b\\");
    }
    wrapped
}

/// Codex `tmux_clipboard.rs` `copy`: hand the text to tmux, which keeps a paste buffer and sends
/// it to the attached client's clipboard itself (no DCS passthrough needed). Modified: `tmux` is
/// looked up on `PATH` rather than through Codex's trusted-executable helper.
pub fn tmux_client_buffer_copy(text: &str) -> Result<(), String> {
    let pane = std::env::var("TMUX_PANE")
        .map_err(|_| "tmux clipboard forwarding is unavailable: no current pane".to_string())?;
    let client = clipboard_target(
        || command_output(["show-options", "-gv", "set-clipboard"]),
        || {
            let session = command_output(["display-message", "-p", "-t", &pane, "#{session_id}"])?;
            command_output([
                "list-clients",
                "-t",
                session.trim(),
                "-F",
                "#{client_activity} #{client_name}",
            ])
        },
        |client| command_output(["show-messages", "-T", "-t", client]),
    )?;
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-t", &client, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn tmux: {e}"))?;
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("failed to open tmux stdin".to_string());
    };
    if let Err(err) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("failed to write to tmux: {err}"));
    }
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for tmux: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("tmux exited with status {}", output.status)
        } else {
            format!("tmux failed: {stderr}")
        })
    }
}

/// Codex `tmux_clipboard.rs` `clipboard_target`: the most recently active client of this pane's
/// session, provided tmux forwards clipboard writes and that client's terminal takes them (`Ms`).
pub fn clipboard_target(
    set_clipboard_fn: impl FnOnce() -> Result<String, String>,
    list_clients_fn: impl FnOnce() -> Result<String, String>,
    tmux_info_fn: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String, String> {
    let set_clipboard = set_clipboard_fn()?;
    if set_clipboard.trim() == "off" {
        return Err("tmux clipboard forwarding is disabled".to_string());
    }
    let clients = list_clients_fn()?;
    let client = clients
        .lines()
        .filter_map(|line| {
            let (activity, client) = line.split_once(' ')?;
            Some((activity.parse::<u64>().ok()?, client))
        })
        .filter(|(_, client)| !client.is_empty())
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, client)| client.to_string())
        .ok_or_else(|| {
            "tmux clipboard forwarding is unavailable: no attached client".to_string()
        })?;
    let tmux_info = tmux_info_fn(&client)?;
    if !tmux_info.lines().any(|line| {
        line.split_once("Ms: (string) ")
            .is_some_and(|(_, sequence)| !sequence.trim().is_empty())
    }) {
        return Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string());
    }
    Ok(client)
}

/// Codex `tmux_clipboard.rs` `command_output`.
fn command_output<const N: usize>(args: [&str; N]) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to spawn tmux: {e}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|e| format!("tmux output was not UTF-8: {e}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("tmux exited with status {}", output.status)
        } else {
            format!("tmux failed: {stderr}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    // Codex `terminal_identity_tests.rs`.
    #[test]
    fn distinguishes_the_native_terminal_replies() {
        for (primary, secondary, expected) in [
            ("1;2", "1;95;0", true),
            ("62;22;52", "1;10;0", false),
            ("64;1;2;4;6;17;18;21;22;52", "64;2500;0", false),
            ("62;52;", "1;4000;46", false),
            ("65;4;6;18;22", "1;277;0", false),
            ("6", "0;2600;1", false),
            ("1;2", "0;95;0", false),
            ("6", "1;95;0", false),
            ("1;2", "1;95;00", false),
            ("1;2", "1;95;1", false),
        ] {
            let primary = format!("\x1b[?{primary}c");
            let secondary = format!("\x1b[>{secondary}c");
            for input in [
                format!("{primary}{secondary}"),
                format!("{secondary}{primary}"),
            ] {
                assert_eq!(
                    requires_inline_screen(input.as_bytes()),
                    Some(expected),
                    "{input:?}"
                );
            }
        }
    }

    #[test]
    fn unrelated_csi_replies_cannot_consume_typed_text_as_device_attributes() {
        for reply in [b"\x1b[?7u".as_slice(), b"\x1b[>4;2m".as_slice()] {
            let mut input = reply.to_vec();
            input.extend_from_slice(b"c\x1b[>1;95;0c");
            assert_eq!(requires_inline_screen(&input), None);
            input.extend_from_slice(b"\x1b[?1;2c");
            assert_eq!(requires_inline_screen(&input), Some(true));
        }
    }

    #[test]
    fn identity_waits_for_complete_replies_and_ignores_pasted_signatures() {
        let signature = b"\x1b[?1;2c\x1b[>1;95;0c";
        for end in 0..signature.len() {
            assert_eq!(requires_inline_screen(&signature[..end]), None);
        }
        let mut input = b"typed\x1b[200~".to_vec();
        input.extend_from_slice(signature);
        assert_eq!(requires_inline_screen(&input), None);
        input.extend_from_slice(b"\x1b[201~more typed");
        assert_eq!(requires_inline_screen(&input), None);
        input.extend_from_slice(signature);
        assert_eq!(requires_inline_screen(&input), Some(true));
    }

    #[test]
    fn only_a_direct_ssh_session_probes_identity() {
        assert!(should_probe_inline_screen(&env_of(&[(
            "SSH_TTY",
            "/dev/pts/1"
        )])));
        assert!(!should_probe_inline_screen(&env_of(&[])));
        assert!(!should_probe_inline_screen(&env_of(&[
            ("SSH_CONNECTION", "a b c d"),
            ("TMUX", "/tmp/tmux-1/default,1,0"),
        ])));
        assert!(!should_probe_inline_screen(&env_of(&[
            ("SSH_TTY", "/dev/pts/1"),
            ("ZELLIJ", "0"),
        ])));
    }

    #[test]
    fn ghostty_over_ssh_is_truecolor_unless_forced() {
        assert!(ghostty_truecolor(&env_of(&[("TERM", "xterm-ghostty")])));
        assert!(ghostty_truecolor(&env_of(&[
            ("TERM", "xterm-ghostty"),
            ("CLICOLOR_FORCE", "0"),
        ])));
        assert!(!ghostty_truecolor(&env_of(&[
            ("TERM", "xterm-ghostty"),
            ("FORCE_COLOR", "1"),
        ])));
        assert!(!ghostty_truecolor(&env_of(&[
            ("TERM", "xterm-ghostty"),
            ("CLICOLOR_FORCE", "1"),
        ])));
        assert!(!ghostty_truecolor(&env_of(&[("TERM", "xterm")])));
    }

    #[test]
    fn clipboard_route_follows_the_session() {
        assert_eq!(
            clipboard_copy_mode(&env_of(&[("TMUX", "x"), ("SSH_TTY", "y")])),
            ClipboardCopyMode::ClientBuffer
        );
        assert_eq!(
            clipboard_copy_mode(&env_of(&[("SSH_CONNECTION", "y")])),
            ClipboardCopyMode::Osc52
        );
        assert_eq!(
            clipboard_copy_mode(&env_of(&[])),
            ClipboardCopyMode::FallbackOsc52
        );
        assert_eq!(
            Osc52Target::detect(&env_of(&[("STY", "1.pts")])),
            Osc52Target::ChunkedDcs
        );
        assert_eq!(
            escaped_dcs("\x1b]52;c;aGk=\x07"),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }

    // Codex `tmux_clipboard_tests.rs`, the client choice.
    #[test]
    fn the_most_recent_client_with_ms_is_chosen() {
        let ok = clipboard_target(
            || Ok("on\n".into()),
            || Ok("10 /dev/ttys001\n30 /dev/ttys002\n20 /dev/ttys003\n".into()),
            |c| {
                assert_eq!(c, "/dev/ttys002");
                Ok("Ms: (string) \\E]52;%p1%s;%p2%s\\007\n".into())
            },
        );
        assert_eq!(ok.as_deref(), Ok("/dev/ttys002"));
        assert!(
            clipboard_target(|| Ok("off".into()), || unreachable!(), |_| unreachable!()).is_err()
        );
        assert!(
            clipboard_target(|| Ok("on".into()), || Ok(String::new()), |_| unreachable!()).is_err()
        );
        assert!(
            clipboard_target(
                || Ok("external".into()),
                || Ok("1 c".into()),
                |_| Ok("Ss: (string) x".into())
            )
            .is_err()
        );
    }
}
