//! What an install is doing right now, for the GUI's progress bar.
//!
//! An install is a short plan of stages (check, download Node.js, download,
//! install with npm, set up, check). The bar is the finished stages' weight
//! plus the current stage's share of its own. That share is real where a
//! number exists: bytes against Content-Length for downloads the harness does
//! itself, and curl's own meter for downloads a vendor script does (the
//! installer runs with a `curl` on PATH that turns the meter back on). Where
//! nothing reports a number, npm for instance, the share creeps towards 90%
//! of the stage so the bar moves without claiming to be done.
//!
//! The installer's output drives stage changes: "Installing managed Pi
//! dependencies" moves Pi from downloading to npm, "Setting up Claude Code"
//! moves Claude past its download. [`Tracker::line`] is the whole parser.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeron_proto::HarnessId;

/// One snapshot of an install, as the `InstallProgress` RPC returns it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    /// "Downloading Node.js…", "Installing with npm…".
    pub status: String,
    /// The whole install, 0.0 to 1.0. Never goes backwards.
    pub fraction: f32,
    /// "21.4 MB of 54.2 MB", "119 packages", or the installer's last line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Preparing,
    Node,
    Download,
    Npm,
    Verify,
    SetUp,
    Adapter,
    Run,
    Checking,
}

impl Stage {
    /// How long a stage with no numbers of its own takes to cover about 63%
    /// of its span.
    fn pace(self) -> Duration {
        Duration::from_secs(match self {
            Stage::Preparing | Stage::Verify | Stage::Checking => 3,
            Stage::SetUp => 8,
            Stage::Node | Stage::Download | Stage::Adapter => 15,
            Stage::Npm | Stage::Run => 25,
        })
    }
}

/// The stages an install of `harness` goes through, with weights roughly in
/// proportion to how long each takes on a home connection.
pub(crate) fn plan(harness: HarnessId, npm_only: bool, needs_node: bool) -> Vec<(Stage, f32)> {
    use Stage::*;
    let mut plan = vec![(Preparing, 1.0)];
    if needs_node {
        plan.push((Node, 6.0));
    }
    match harness {
        _ if npm_only => plan.push((Npm, 10.0)),
        HarnessId::Pi => plan.extend([(Download, 1.0), (Npm, 6.0), (Verify, 1.0)]),
        HarnessId::ClaudeCode => plan.extend([(Download, 8.0), (SetUp, 3.0)]),
        HarnessId::Wizard | HarnessId::Antigravity => plan.push((Download, 10.0)),
        _ => plan.push((Run, 10.0)),
    }
    plan.push((Checking, 0.5));
    plan
}

fn label(stage: Stage, name: &str) -> String {
    match stage {
        Stage::Preparing | Stage::Checking => "Checking…".into(),
        Stage::Node => "Downloading Node.js…".into(),
        Stage::Download => format!("Downloading {name}…"),
        Stage::Npm => "Installing with npm…".into(),
        Stage::Verify => format!("Verifying {name}…"),
        Stage::SetUp => format!("Setting up {name}…"),
        Stage::Adapter => format!("Installing the {name} adapter…"),
        Stage::Run => format!("Running the {name} installer…"),
    }
}

pub(crate) struct Tracker {
    name: &'static str,
    plan: Vec<(Stage, f32)>,
    at: usize,
    started: Instant,
    /// The current stage's own progress when something measured it.
    measured: Option<f32>,
    detail: Option<String>,
    npm_fetched: u32,
    high_water: f32,
}

impl Tracker {
    pub(crate) fn new(name: &'static str, plan: Vec<(Stage, f32)>, now: Instant) -> Self {
        Self {
            name,
            plan,
            at: 0,
            started: now,
            measured: None,
            detail: None,
            npm_fetched: 0,
            high_water: 0.0,
        }
    }

    fn stage(&self) -> Stage {
        self.plan[self.at].0
    }

    /// Move to `stage` if it is later in the plan. Stages the plan does not
    /// have, or that already passed, are ignored: output can repeat itself.
    pub(crate) fn enter(&mut self, stage: Stage, now: Instant) {
        if let Some(ix) = self.plan.iter().position(|(s, _)| *s == stage)
            && ix > self.at
        {
            self.at = ix;
            self.started = now;
            self.measured = None;
            self.detail = None;
        }
    }

    pub(crate) fn bytes(&mut self, done: u64, total: Option<u64>) {
        match total.filter(|t| *t > 0) {
            Some(total) => {
                self.measured = Some((done as f32 / total as f32).clamp(0.0, 1.0));
                self.detail = Some(format!("{} of {}", megabytes(done), megabytes(total)));
            }
            None => self.detail = Some(megabytes(done)),
        }
    }

    /// Read one line of installer output.
    pub(crate) fn line(&mut self, raw: &str, now: Instant) {
        let line = strip_ansi(raw);
        let line = line.trim();
        // Blank lines, and art like pi's block-letter logo, say nothing.
        if !line.chars().any(char::is_alphanumeric) {
            return;
        }
        if let Some(meter) = CurlMeter::parse(line) {
            // The version file, manifests and checksums finish in one tick;
            // only a real payload moves the bar.
            if meter.total >= 256 * 1024 {
                self.bytes(meter.received, Some(meter.total));
            }
            return;
        }
        if CurlMeter::is_header(line) {
            return;
        }
        let stage = match () {
            _ if line.starts_with("Resolving Node.js")
                || line.starts_with("Downloading Node.js") =>
            {
                Some(Stage::Node)
            }
            _ if line.starts_with("Downloading managed installer")
                || line.starts_with("Downloading wizard binary")
                || line.starts_with("==> Downloading wizard") =>
            {
                Some(Stage::Download)
            }
            _ if line.starts_with("Installing managed Pi dependencies")
                || line.starts_with("Installing Pi...") =>
            {
                Some(Stage::Npm)
            }
            _ if line.starts_with("Verifying managed Pi")
                || line.starts_with("Activating managed Pi") =>
            {
                Some(Stage::Verify)
            }
            _ if line.starts_with("Setting up Claude Code") => Some(Stage::SetUp),
            _ => None,
        };
        if let Some(stage) = stage {
            self.enter(stage, now);
        }
        if line.starts_with("npm http fetch") {
            self.npm_fetched += 1;
            self.detail = Some(format!("{} packages fetched", self.npm_fetched));
        } else if let Some(count) = npm_added(line) {
            if self.stage() == Stage::Npm || self.stage() == Stage::Adapter {
                self.measured = Some(1.0);
            }
            self.detail = Some(match count {
                1 => "1 package".into(),
                n => format!("{n} packages"),
            });
        } else if stage.is_none() {
            // Anything else is the installer talking; the latest line is the
            // most useful thing to show under the bar.
            let text = line.trim_start_matches("==> ");
            self.detail = Some(text.chars().take(120).collect());
        }
    }

    pub(crate) fn snapshot(&mut self, now: Instant) -> InstallProgress {
        let total: f32 = self
            .plan
            .iter()
            .map(|(_, w)| w)
            .sum::<f32>()
            .max(f32::EPSILON);
        let done: f32 = self.plan[..self.at].iter().map(|(_, w)| w).sum();
        let (stage, weight) = self.plan[self.at];
        let share = self.measured.unwrap_or_else(|| {
            let t = now.saturating_duration_since(self.started).as_secs_f32();
            0.9 * (1.0 - (-t / stage.pace().as_secs_f32()).exp())
        });
        let fraction = ((done + weight * share) / total).clamp(0.0, 1.0);
        self.high_water = self.high_water.max(fraction);
        InstallProgress {
            status: label(stage, self.name),
            fraction: self.high_water,
            detail: self.detail.clone(),
        }
    }
}

/// A shared handle the installer writes and the RPC reads.
#[derive(Clone)]
pub struct Progress(Arc<Mutex<Option<Tracker>>>);

impl Default for Progress {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl Progress {
    fn with<R>(&self, f: impl FnOnce(&mut Tracker) -> R) -> Option<R> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .map(f)
    }

    pub(crate) fn start(&self, name: &'static str, plan: Vec<(Stage, f32)>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Tracker::new(name, plan, Instant::now()));
    }

    pub(crate) fn enter(&self, stage: Stage) {
        self.with(|t| t.enter(stage, Instant::now()));
    }

    pub(crate) fn bytes(&self, done: u64, total: Option<u64>) {
        self.with(|t| t.bytes(done, total));
    }

    pub(crate) fn line(&self, line: &str) {
        self.with(|t| t.line(line, Instant::now()));
    }

    /// `None` until an install has started.
    pub fn snapshot(&self) -> Option<InstallProgress> {
        self.with(|t| t.snapshot(Instant::now()))
    }
}

/// One update of curl's default progress meter:
/// ` 68 54.16M  68 37.21M   0      0 37.23M      0   00:01 ...`
#[derive(Debug, PartialEq)]
pub(crate) struct CurlMeter {
    pub received: u64,
    pub total: u64,
}

impl CurlMeter {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let mut tokens = line.split_whitespace();
        let percent: u32 = tokens.next()?.parse().ok()?;
        let total = size(tokens.next()?)?;
        let received_percent: u32 = tokens.next()?.parse().ok()?;
        let received = size(tokens.next()?)?;
        // Upload columns follow; a line with fewer columns is not the meter.
        let _xferd_percent: u32 = tokens.next()?.parse().ok()?;
        if percent > 100 || received_percent > 100 {
            return None;
        }
        Some(Self { received, total })
    }

    pub(crate) fn is_header(line: &str) -> bool {
        line.starts_with("% Total") || line.starts_with("Dload")
    }
}

/// curl's sizes: `3981`, `12k`, `54.16M`, `1.2G`.
fn size(token: &str) -> Option<u64> {
    let (number, scale) = match token.chars().last()? {
        'k' | 'K' => (&token[..token.len() - 1], 1024.0),
        'M' => (&token[..token.len() - 1], 1024.0 * 1024.0),
        'G' => (&token[..token.len() - 1], 1024.0 * 1024.0 * 1024.0),
        'T' => (&token[..token.len() - 1], 1024.0_f64.powi(4)),
        c if c.is_ascii_digit() => (token, 1.0),
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    (value >= 0.0).then_some((value * scale) as u64)
}

/// `added 119 packages in 6s` / `added 1 package, and audited 2 packages`.
fn npm_added(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("added ")?;
    let (count, rest) = rest.split_once(' ')?;
    rest.starts_with("package").then_some(())?;
    count.parse().ok()
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Drop terminal escape sequences (pi's installer colors its menu even when
/// piped).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A directory holding a `curl` that runs the real one with its progress
/// meter on. The installer gets it first on PATH, so a vendor script's
/// `curl -fsSL -o file url` reports bytes to us on stderr. Removed on drop.
#[cfg(unix)]
pub(crate) struct CurlMeterShim {
    dir: std::path::PathBuf,
}

#[cfg(unix)]
impl CurlMeterShim {
    pub(crate) fn new(real_curl: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "zeron-install-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir)?;
        let script = format!(
            "#!/bin/sh\nexec '{}' \"$@\" --no-silent\n",
            real_curl.display().to_string().replace('\'', r"'\''")
        );
        let path = dir.join("curl");
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self { dir })
    }

    pub(crate) fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

#[cfg(unix)]
impl Drop for CurlMeterShim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(harness: HarnessId, needs_node: bool) -> (Tracker, Instant) {
        let now = Instant::now();
        (
            Tracker::new("Pi", plan(harness, false, needs_node), now),
            now,
        )
    }

    #[test]
    fn curl_meter_lines_parse_on_old_and_new_curl() {
        // curl 8.18 (Linux)
        assert_eq!(
            CurlMeter::parse(
                " 68 54.16M  68 37.21M   0      0 37.23M      0   00:01           00:01 37.21M"
            ),
            Some(CurlMeter {
                received: (37.21 * 1048576.0) as u64,
                total: (54.16 * 1048576.0) as u64
            })
        );
        // curl 8.7 (macOS 15)
        assert_eq!(
            CurlMeter::parse(
                "100  3981  100  3981    0     0   8892      0 --:--:-- --:--:-- --:--:--  8892"
            ),
            Some(CurlMeter {
                received: 3981,
                total: 3981
            })
        );
        assert_eq!(
            CurlMeter::parse(
                "  0      0   0      0   0      0      0      0                              0"
            ),
            Some(CurlMeter {
                received: 0,
                total: 0
            })
        );
        for not_a_meter in [
            "added 119 packages in 6s",
            "100 packages",
            "Downloading Node.js v22.23.3",
            "  % Total    % Received % Xferd  Average Speed",
        ] {
            assert_eq!(CurlMeter::parse(not_a_meter), None, "{not_a_meter}");
        }
        assert!(CurlMeter::is_header(
            "% Total    % Received % Xferd  Average Speed"
        ));
        assert!(CurlMeter::is_header("Dload  Upload  Total   Spent"));
    }

    #[test]
    fn a_big_download_moves_the_bar_and_small_ones_do_not() {
        let (mut t, now) = tracker(HarnessId::ClaudeCode, false);
        t.enter(Stage::Download, now);
        // the version file and the manifest
        t.line(
            "100    8  100    8    0     0     40      0 --:--:-- --:--:-- --:--:--    40",
            now,
        );
        t.line(
            "100 4102  100 4102    0     0  20000      0 --:--:-- --:--:-- --:--:-- 20000",
            now,
        );
        let before = t.snapshot(now);
        assert_eq!(before.status, "Downloading Pi…");
        assert!(before.fraction < 0.1, "{before:?}");
        t.line(
            " 50  200M   50  100M    0     0  50.0M      0  0:00:04  0:00:02  0:00:02 50.0M",
            now,
        );
        let half = t.snapshot(now);
        // Preparing (1) + half of Download (8) out of 12.5.
        assert!((half.fraction - 5.0 / 12.5).abs() < 0.01, "{half:?}");
        assert_eq!(half.detail.as_deref(), Some("104.9 MB of 209.7 MB"));
    }

    #[test]
    fn pi_installer_output_walks_the_plan() {
        let (mut t, now) = tracker(HarnessId::Pi, false);
        let steps = [
            ("\u{1b}[1m  Pi Installer\u{1b}[0m", "Checking…"),
            (
                "Downloading managed installer release metadata",
                "Downloading Pi…",
            ),
            ("Installing managed Pi dependencies", "Installing with npm…"),
            ("added 119 packages in 6s", "Installing with npm…"),
            ("Verifying managed Pi 0.87.1", "Verifying Pi…"),
            ("Activating managed Pi 0.87.1", "Verifying Pi…"),
        ];
        let mut last = 0.0;
        for (line, status) in steps {
            t.line(line, now);
            let snap = t.snapshot(now);
            assert_eq!(snap.status, status, "after {line:?}");
            assert!(snap.fraction >= last, "went backwards after {line:?}");
            last = snap.fraction;
        }
        assert_eq!(t.snapshot(now).detail, None);
        // Output that repeats an earlier stage does not rewind.
        t.line("Installing Pi...", now);
        assert_eq!(t.snapshot(now).status, "Verifying Pi…");
    }

    #[test]
    fn npm_counts_packages_and_finishes_its_stage() {
        let now = Instant::now();
        let mut t = Tracker::new("Codex", plan(HarnessId::Codex, true, false), now);
        t.enter(Stage::Npm, now);
        t.line(
            "npm http fetch GET 200 https://registry.npmjs.org/a 55ms (cache miss)",
            now,
        );
        t.line(
            "npm http fetch GET 200 https://registry.npmjs.org/b 51ms (cache miss)",
            now,
        );
        assert_eq!(
            t.snapshot(now).detail.as_deref(),
            Some("2 packages fetched")
        );
        t.line("added 1 package in 2s", now);
        let snap = t.snapshot(now);
        assert_eq!(snap.detail.as_deref(), Some("1 package"));
        // Preparing (1) + all of npm (10) out of 11.5.
        assert!((snap.fraction - 11.0 / 11.5).abs() < 0.01, "{snap:?}");
    }

    #[test]
    fn an_unmeasured_stage_creeps_but_never_finishes() {
        let (mut t, now) = tracker(HarnessId::Pi, true);
        t.enter(Stage::Node, now);
        let early = t.snapshot(now + Duration::from_secs(1)).fraction;
        let late = t.snapshot(now + Duration::from_secs(600)).fraction;
        assert!(late > early);
        // Node is weight 6 starting after Preparing (1), total 15.5.
        assert!(late <= (1.0 + 6.0 * 0.9) / 15.5 + 1e-4, "{late}");
        // Measured bytes override the creep but the bar never goes back.
        t.bytes(1, Some(100));
        assert_eq!(t.snapshot(now + Duration::from_secs(601)).fraction, late);
    }

    #[test]
    fn plans_match_the_installers() {
        let stages = |p: Vec<(Stage, f32)>| p.into_iter().map(|(s, _)| s).collect::<Vec<_>>();
        use Stage::*;
        assert_eq!(
            stages(plan(HarnessId::Pi, false, true)),
            [Preparing, Node, Download, Npm, Verify, Checking]
        );
        assert_eq!(
            stages(plan(HarnessId::ClaudeCode, false, false)),
            [Preparing, Download, SetUp, Checking]
        );
        assert_eq!(
            stages(plan(HarnessId::Pi, true, false)),
            [Preparing, Npm, Checking]
        );
    }

    #[test]
    fn installer_chatter_becomes_the_detail_line() {
        let (mut t, now) = tracker(HarnessId::Wizard, false);
        t.line("==> Platform: macos/aarch64", now);
        assert_eq!(
            t.snapshot(now).detail.as_deref(),
            Some("Platform: macos/aarch64")
        );
        t.line("  ██  ██", now);
        assert_eq!(
            t.snapshot(now).detail.as_deref(),
            Some("Platform: macos/aarch64")
        );
        t.line("\u{1b}[2m  There are many agent harnesses\u{1b}[0m", now);
        assert_eq!(
            t.snapshot(now).detail.as_deref(),
            Some("There are many agent harnesses")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_curl_shim_runs_the_real_curl_with_the_meter_on() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("real curl");
        std::fs::write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = CurlMeterShim::new(&fake).unwrap();
        let out = std::process::Command::new(shim.dir().join("curl"))
            .args(["-fsSL", "-o", "x", "https://example.com"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "-fsSL\n-o\nx\nhttps://example.com\n--no-silent\n"
        );
        let path = shim.dir().to_path_buf();
        drop(shim);
        assert!(!path.exists());
    }
}
