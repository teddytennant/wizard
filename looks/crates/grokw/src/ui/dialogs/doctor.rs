//! `/doctor`: one system block in the transcript, laid out like Grok's (spec 6.15): environment,
//! clipboard, then the findings, each with what to change. Wizard's own `wizard doctor` runs off
//! the UI thread and its checks follow as a second block.

use crate::app::{App, Msg};

/// What the report is made from, so tests can feed it a terminal.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub terminal: String,
    pub multiplexer: &'static str,
    pub ssh: bool,
    pub truecolor: bool,
    pub native_clipboard: bool,
    /// `Some(on)` inside tmux: whether `set-clipboard` is on.
    pub tmux_clipboard: Option<bool>,
    /// `Some(on)` inside tmux: whether `allow-passthrough` is on.
    pub tmux_passthrough: Option<bool>,
    /// `Some(on)` inside tmux: whether `extended-keys` is on.
    pub tmux_extended_keys: Option<bool>,
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn tmux_option(name: &str) -> Option<bool> {
    let o = std::process::Command::new("tmux")
        .args(["show-options", "-gv", name])
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !v.is_empty()).then_some(matches!(v.as_str(), "on" | "always"))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

pub fn detect() -> Env {
    let terminal = match var("TERM_PROGRAM").as_deref() {
        Some("iTerm.app") => "iTerm2".to_string(),
        Some("Apple_Terminal") => "Apple Terminal".to_string(),
        Some("WezTerm") => "WezTerm".to_string(),
        Some("vscode") => "VS Code".to_string(),
        Some("ghostty") => "Ghostty".to_string(),
        Some("zed") => "Zed".to_string(),
        _ if var("KITTY_WINDOW_ID").is_some() => "Kitty".to_string(),
        _ if var("ALACRITTY_LOG").is_some() || var("ALACRITTY_SOCKET").is_some() => {
            "Alacritty".to_string()
        }
        _ if var("WT_SESSION").is_some() => "Windows Terminal".to_string(),
        _ if var("VTE_VERSION").is_some() => "VTE".to_string(),
        _ => "Unknown".to_string(),
    };
    let tmux = var("TMUX").is_some();
    let multiplexer = if tmux {
        "tmux"
    } else if var("ZELLIJ").is_some() {
        "zellij"
    } else if var("STY").is_some() {
        "screen"
    } else {
        "none"
    };
    Env {
        terminal,
        multiplexer,
        ssh: var("SSH_CONNECTION").is_some() || var("SSH_TTY").is_some(),
        truecolor: matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit")),
        native_clipboard: ["wl-copy", "xclip", "xsel", "pbcopy"]
            .iter()
            .any(|b| on_path(b)),
        tmux_clipboard: tmux.then(|| tmux_option("set-clipboard")).flatten(),
        tmux_passthrough: tmux.then(|| tmux_option("allow-passthrough")).flatten(),
        tmux_extended_keys: tmux.then(|| tmux_option("extended-keys")).flatten(),
    }
}

struct Finding {
    id: &'static str,
    message: &'static str,
    /// Lines under it, after the six-space indent.
    fix: Vec<String>,
}

fn findings(e: &Env) -> Vec<Finding> {
    let mut v = Vec::new();
    if e.tmux_passthrough == Some(false) {
        v.push(Finding {
            id: "terminal.dcs-passthrough",
            message: "`allow-passthrough` is off in tmux, which can block clipboard copies in nested sessions",
            fix: vec![
                "Add `set -wg allow-passthrough on` to ~/.tmux.conf".into(),
                "Note: Reload tmux with `tmux source-file ~/.tmux.conf`, or restart the tmux server.".into(),
            ],
        });
    }
    if e.tmux_extended_keys == Some(false) {
        v.push(Finding {
            id: "terminal.tmux-extended-keys",
            message: "tmux `extended-keys` is off, so Shift+Enter and Ctrl+Enter cannot be told apart from Enter",
            fix: vec![
                "Add `set -g extended-keys on` to ~/.tmux.conf".into(),
                "Note: Reload tmux with `tmux source-file ~/.tmux.conf`, or restart the tmux server.".into(),
            ],
        });
    }
    if !e.truecolor {
        v.push(Finding {
            id: "terminal.limited-color",
            message: "this terminal does not report truecolor, so themes are approximated with 256 colors",
            fix: vec!["Set `COLORTERM=truecolor` where your terminal supports it".into()],
        });
    }
    v
}

/// The block's text.
pub fn report(e: &Env) -> String {
    let mut s = String::new();
    s.push_str("Environment\n");
    s.push_str(&format!("  terminal     {}\n", e.terminal));
    s.push_str(&format!("  multiplexer  {}\n", e.multiplexer));
    s.push_str(&format!(
        "  ssh          {}\n",
        if e.ssh { "yes" } else { "no" }
    ));
    s.push_str(&format!(
        "  color        {}\n",
        if e.truecolor { "truecolor" } else { "256" }
    ));
    s.push_str(&format!(
        "  themes       {}\n",
        if e.truecolor { "all" } else { "approximated" }
    ));
    s.push_str("\nClipboard\n");
    s.push_str(&format!(
        "  native       {}\n",
        if e.native_clipboard {
            "available"
        } else {
            "unavailable"
        }
    ));
    if let Some(on) = e.tmux_clipboard {
        s.push_str(&format!(
            "  tmux         {}\n",
            if on { "on" } else { "off" }
        ));
    }
    s.push_str("  osc 52       unknown\n");
    let f = findings(e);
    if f.is_empty() {
        s.push_str("\nNo issues found.");
    } else {
        s.push_str(&format!("\nIssues ({})\n", f.len()));
        for x in &f {
            s.push_str(&format!("\n  ! {}  {}\n", x.id, x.message));
            for l in &x.fix {
                s.push_str(&format!("      {l}\n"));
            }
        }
        while s.ends_with('\n') {
            s.pop();
        }
    }
    s
}

pub fn run(app: &mut App) {
    app.enter_session();
    let text = report(&detect());
    app.note(text);
    // wizard's own checks, off the UI thread; a test or mock run does not spawn it
    if app.opts.mock {
        return;
    }
    let tx = app.msg_tx.clone();
    std::thread::spawn(move || {
        // from the home directory: `wizard doctor` checks that the project's .wizard is writable
        // and would create one in whatever directory it runs in
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        // the wizard this look talks to, which need not be the one on PATH
        let bin = std::env::var_os("OPENW_WIZARD_BIN")
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "wizard".into());
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("doctor");
        if let Some(h) = home {
            cmd.current_dir(h);
        }
        let text = match cmd.output() {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout).to_string();
                let lines: Vec<String> = out
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| format!("  {l}"))
                    .collect();
                format!("Wizard\n{}", lines.join("\n"))
            }
            Err(e) => format!("Wizard\n  `wizard doctor` could not run: {e}"),
        };
        let _ = tx.send(Msg::Note(text));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux_env() -> Env {
        Env {
            terminal: "Unknown".into(),
            multiplexer: "tmux",
            ssh: false,
            truecolor: true,
            native_clipboard: false,
            tmux_clipboard: Some(true),
            tmux_passthrough: Some(false),
            tmux_extended_keys: Some(true),
        }
    }

    #[test]
    fn tmux_report_has_the_capture_s_sections() {
        let r = report(&tmux_env());
        assert!(r.starts_with("Environment\n  terminal     Unknown\n  multiplexer  tmux\n  ssh          no\n  color        truecolor\n  themes       all\n\nClipboard\n  native       unavailable\n  tmux         on\n  osc 52       unknown\n\nIssues (1)\n\n  ! terminal.dcs-passthrough  "));
        assert!(r.contains("      Add `set -wg allow-passthrough on` to ~/.tmux.conf"));
    }

    #[test]
    fn a_clean_terminal_says_so() {
        let e = Env {
            terminal: "Kitty".into(),
            multiplexer: "none",
            truecolor: true,
            native_clipboard: true,
            ..Default::default()
        };
        assert!(report(&e).ends_with("\n\nNo issues found."));
    }
}
