//! `/voice`: dictation into the composer from tools already on the machine.
//!
//! Wizard links no audio library. A recording is made by whichever of
//! `arecord`, `ffmpeg` or `sox` is installed (or by `[voice] recorder`), then
//! handed to a speech-to-text program: `whisper-cli` from whisper.cpp (needs a
//! ggml model file), `vosk-transcriber`, or whatever `[voice] command` names.
//! Nothing is uploaded unless the command you configure uploads it; the
//! built-in ones are local.
//!
//! Push to talk: `/voice` starts recording and returns at once; `/voice stop`
//! (or the time limit, `[voice] seconds`, default 30) ends it, and the text is
//! put in the composer for you to edit and send. When nothing is installed the
//! reply names what to install.
//!
//! ```toml
//! [voice]
//! command  = "my-stt --lang en {wav}"            # prints the text on stdout
//! recorder = "my-rec --out {out} --secs {seconds}" # optional
//! model    = "~/models/ggml-base.en.bin"          # for whisper-cli
//! seconds  = 30
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::process::Command;

/// Longest recording unless `[voice] seconds` says otherwise.
pub const DEFAULT_SECONDS: u64 = 30;
const MAX_SECONDS: u64 = 300;
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceConfig {
    /// Speech-to-text command; `{wav}` is the recording (appended if absent).
    pub command: Option<String>,
    /// Recorder command; `{out}` and `{seconds}` are filled in.
    pub recorder: Option<String>,
    /// whisper.cpp model file.
    pub model: Option<PathBuf>,
    pub seconds: u64,
}

impl VoiceConfig {
    /// `[voice]` from a config.toml's text.
    pub fn parse(raw: &str) -> Self {
        let table = raw.parse::<toml::Table>().unwrap_or_default();
        let voice = table.get("voice").and_then(toml::Value::as_table);
        let text = |key: &str| {
            voice
                .and_then(|v| v.get(key))
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let seconds = voice
            .and_then(|v| v.get("seconds"))
            .and_then(toml::Value::as_integer)
            .map_or(DEFAULT_SECONDS, |s| (s.max(1) as u64).min(MAX_SECONDS));
        Self {
            command: text("command"),
            recorder: text("recorder"),
            model: text("model").map(|m| PathBuf::from(shellexpand::tilde(&m).into_owned())),
            seconds,
        }
    }

    /// `[voice]` from `~/.wizard/config.toml`; defaults when there is none.
    pub fn load() -> Self {
        let raw = crate::config::Config::path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        Self::parse(&raw)
    }
}

fn which(name: &str, path: &OsString) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| crate::platform::host::is_executable(candidate))
}

/// How to make a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorder {
    Arecord(PathBuf),
    Ffmpeg(PathBuf),
    Sox(PathBuf),
    /// `[voice] recorder`, run through the shell.
    Custom(String),
}

impl Recorder {
    pub fn name(&self) -> &str {
        match self {
            Recorder::Arecord(_) => "arecord",
            Recorder::Ffmpeg(_) => "ffmpeg",
            Recorder::Sox(_) => "sox",
            Recorder::Custom(_) => "[voice] recorder",
        }
    }

    /// 16 kHz mono 16-bit WAV at `out`, for at most `seconds`.
    fn command(&self, out: &Path, seconds: u64) -> Command {
        let secs = seconds.to_string();
        let mut command = match self {
            Recorder::Arecord(bin) => {
                let mut c = Command::new(bin);
                c.args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", &secs])
                    .arg(out);
                c
            }
            Recorder::Ffmpeg(bin) => {
                let mut c = Command::new(bin);
                c.args(["-loglevel", "error", "-y"]);
                if cfg!(target_os = "macos") {
                    c.args(["-f", "avfoundation", "-i", ":0"]);
                } else {
                    c.args(["-f", "pulse", "-i", "default"]);
                }
                c.args(["-ac", "1", "-ar", "16000", "-t", &secs]).arg(out);
                c
            }
            Recorder::Sox(bin) => {
                let mut c = Command::new(bin);
                c.args(["-q", "-d", "-r", "16000", "-c", "1", "-b", "16"])
                    .arg(out)
                    .args(["trim", "0", &secs]);
                c
            }
            Recorder::Custom(template) => {
                let line = template
                    .replace("{out}", &shell_quote(out))
                    .replace("{seconds}", &secs);
                let mut c = Command::new("sh");
                c.arg("-c").arg(line);
                c
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            command.process_group(0);
            // A parent that started us with SIGINT ignored (a backgrounded
            // job, `nohup`) would otherwise hand the recorder an ignored
            // SIGINT, and `/voice stop` could not end it.
            // SAFETY: `signal` is async-signal-safe and runs between fork and
            // exec, touching nothing else.
            unsafe {
                command.pre_exec(|| {
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    Ok(())
                });
            }
        }
        command.kill_on_drop(true);
        command
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// How to turn a recording into text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transcriber {
    WhisperCpp {
        bin: PathBuf,
        model: PathBuf,
    },
    Vosk(PathBuf),
    /// `[voice] command`, run through the shell.
    Custom(String),
}

impl Transcriber {
    pub fn name(&self) -> &str {
        match self {
            Transcriber::WhisperCpp { .. } => "whisper.cpp",
            Transcriber::Vosk(_) => "vosk",
            Transcriber::Custom(_) => "[voice] command",
        }
    }

    fn command(&self, wav: &Path) -> Command {
        let mut command = match self {
            Transcriber::WhisperCpp { bin, model } => {
                let mut c = Command::new(bin);
                c.arg("-m")
                    .arg(model)
                    .arg("-f")
                    .arg(wav)
                    .args(["-nt", "-np"]);
                c
            }
            Transcriber::Vosk(bin) => {
                let mut c = Command::new(bin);
                c.arg("-i").arg(wav).args(["-t", "txt"]);
                c
            }
            Transcriber::Custom(template) => {
                let quoted = shell_quote(wav);
                let line = if template.contains("{wav}") {
                    template.replace("{wav}", &quoted)
                } else {
                    format!("{template} {quoted}")
                };
                let mut c = Command::new("sh");
                c.arg("-c").arg(line);
                c
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

/// The recorder and transcriber this machine can use.
#[derive(Debug, Clone)]
pub struct Plan {
    pub recorder: Recorder,
    pub transcriber: Transcriber,
}

const WHISPER_BINARIES: &[&str] = &["whisper-cli", "whisper-cpp", "whisper.cpp"];

fn model_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for dir in [
        home.join(".wizard/models/whisper"),
        home.join(".cache/whisper"),
        home.join(".local/share/whisper.cpp"),
        home.join("whisper.cpp/models"),
    ] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut bins: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "bin")
                        && p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("ggml-"))
                })
                .collect();
            bins.sort();
            found.extend(bins);
        }
    }
    found
}

/// What is available given `config`, `PATH` and the home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub recorder: Option<Recorder>,
    pub transcriber: Option<Transcriber>,
    /// A transcriber program was found but cannot run yet; says what it lacks.
    pub transcriber_gap: Option<String>,
}

pub fn detect(config: &VoiceConfig, path: &OsString, home: Option<&Path>) -> Found {
    let recorder = match &config.recorder {
        Some(template) => Some(Recorder::Custom(template.clone())),
        None => which("arecord", path)
            .map(Recorder::Arecord)
            .or_else(|| which("ffmpeg", path).map(Recorder::Ffmpeg))
            .or_else(|| which("sox", path).map(Recorder::Sox))
            .or_else(|| which("rec", path).map(Recorder::Sox)),
    };
    let mut gap = None;
    let transcriber = match &config.command {
        Some(command) => Some(Transcriber::Custom(command.clone())),
        None => {
            let whisper = WHISPER_BINARIES.iter().find_map(|n| which(n, path));
            let model = config
                .model
                .clone()
                .or_else(|| std::env::var_os("WHISPER_MODEL").map(PathBuf::from))
                .filter(|m| m.is_file())
                .or_else(|| model_candidates(home).into_iter().next());
            match (whisper, model) {
                (Some(bin), Some(model)) => Some(Transcriber::WhisperCpp { bin, model }),
                (Some(bin), None) => {
                    gap = Some(format!(
                        "{} is installed but has no model. Download a ggml model such as ggml-base.en.bin \
                         (https://huggingface.co/ggerganov/whisper.cpp) and set `model = \"/path/ggml-base.en.bin\"` \
                         under [voice] in ~/.wizard/config.toml, or put it in ~/.wizard/models/whisper/.",
                        bin.display()
                    ));
                    None
                }
                (None, _) => which("vosk-transcriber", path).map(Transcriber::Vosk),
            }
        }
    };
    Found {
        recorder,
        transcriber,
        transcriber_gap: gap,
    }
}

impl Found {
    pub fn plan(&self) -> Option<Plan> {
        Some(Plan {
            recorder: self.recorder.clone()?,
            transcriber: self.transcriber.clone()?,
        })
    }

    /// Exactly what to install, or nothing when the plan is complete.
    pub fn missing(&self) -> Option<String> {
        let mut needs = Vec::new();
        if self.recorder.is_none() {
            needs.push(
                "a recorder: `arecord` (package alsa-utils), `ffmpeg`, or `sox` (brew install sox, \
                 sudo apt-get install sox, sudo dnf install sox, sudo pacman -S sox); or set \
                 `recorder = \"cmd --out {out} --secs {seconds}\"` under [voice] to write a 16 kHz mono WAV to {out}"
                    .to_string(),
            );
        }
        if self.transcriber.is_none() {
            needs.push(match &self.transcriber_gap {
                Some(gap) => gap.clone(),
                None => "a speech-to-text program: whisper.cpp (`whisper-cli` plus a ggml model \
                         file, https://github.com/ggml-org/whisper.cpp), or vosk (`pip install vosk` \
                         for `vosk-transcriber`); or set `command = \"my-stt {wav}\"` under [voice] \
                         in ~/.wizard/config.toml, any program that prints the text on stdout"
                    .to_string(),
            });
        }
        if needs.is_empty() {
            None
        } else {
            Some(format!("voice dictation needs {}.", needs.join("; and ")))
        }
    }

    pub fn report(&self) -> String {
        format!(
            "recorder: {}\ntranscriber: {}",
            self.recorder
                .as_ref()
                .map_or("none".to_string(), |r| r.name().to_string()),
            self.transcriber
                .as_ref()
                .map_or("none".to_string(), |t| t.name().to_string()),
        )
    }
}

/// The pid of the recorder that is running, so `/voice stop` can end it.
static RECORDING: Mutex<Option<u32>> = Mutex::new(None);

pub fn is_recording() -> bool {
    RECORDING.lock().is_ok_and(|g| g.is_some())
}

/// Ask the running recording to finish now. `false` when none is running.
pub fn stop() -> bool {
    let Some(pid) = RECORDING.lock().ok().and_then(|g| *g) else {
        return false;
    };
    #[cfg(unix)]
    {
        // SAFETY: SIGINT to the recorder's own process group, which it made
        // with process_group(0); recorders finalize their WAV on SIGINT.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGINT) };
    }
    #[cfg(not(unix))]
    let _ = pid;
    true
}

struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        if let Ok(mut g) = RECORDING.lock() {
            *g = None;
        }
    }
}

/// Clean a transcriber's stdout into one line of text.
pub fn clean(text: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || matches!(line, "[BLANK_AUDIO]" | "(silence)" | "[silence]") {
            continue;
        }
        parts.push(line);
    }
    parts.join(" ")
}

/// Record up to `seconds`, transcribe, and return the text. Returns early
/// with what was said so far when [`stop`] is called.
pub async fn dictate(plan: &Plan, seconds: u64) -> Result<String, String> {
    if is_recording() {
        return Err("already recording; /voice stop ends it".into());
    }
    let seconds = seconds.clamp(1, MAX_SECONDS);
    let wav = std::env::temp_dir().join(format!(
        "wizard-voice-{}.wav",
        uuid::Uuid::new_v4().simple()
    ));
    let result = dictate_to(plan, seconds, &wav).await;
    let _ = std::fs::remove_file(&wav);
    result
}

async fn dictate_to(plan: &Plan, seconds: u64, wav: &Path) -> Result<String, String> {
    let mut child = plan
        .recorder
        .command(wav, seconds)
        .spawn()
        .map_err(|e| format!("starting {}: {e}", plan.recorder.name()))?;
    let _release = Release;
    if let Ok(mut g) = RECORDING.lock() {
        *g = child.id();
    }
    let waited = tokio::time::timeout(Duration::from_secs(seconds + 10), child.wait()).await;
    if waited.is_err() {
        let _ = child.kill().await;
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        // Not until EOF: a custom recorder can leave a child holding the pipe.
        let _ = tokio::time::timeout(Duration::from_millis(500), pipe.read_to_string(&mut stderr))
            .await;
    }
    drop(_release);
    let size = std::fs::metadata(wav).map_or(0, |m| m.len());
    if size <= 44 {
        let why = stderr.trim();
        return Err(format!(
            "{} recorded no audio{}",
            plan.recorder.name(),
            if why.is_empty() {
                String::new()
            } else {
                format!(": {why}")
            }
        ));
    }
    let output = tokio::time::timeout(TRANSCRIBE_TIMEOUT, plan.transcriber.command(wav).output())
        .await
        .map_err(|_| {
            format!(
                "{} did not finish within {}s",
                plan.transcriber.name(),
                TRANSCRIBE_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("starting {}: {e}", plan.transcriber.name()))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{} failed: {}",
            plan.transcriber.name(),
            why.trim()
        ));
    }
    let text = clean(&String::from_utf8_lossy(&output.stdout));
    if text.is_empty() {
        return Err("heard nothing".into());
    }
    Ok(text)
}

#[cfg(all(test, unix))]
mod tests;
