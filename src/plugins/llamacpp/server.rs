//! Lifecycle management for a local llama.cpp `llama-server`.
//!
//! When the active provider is `llamacpp` and nothing answers at its
//! `base_url`, Wizard can start `llama-server` itself: detached in its own
//! process group (it keeps serving after Wizard exits), logging to
//! `~/.wizard/llama-server.log`, with the PID recorded in
//! `~/.wizard/llama-server.pid` so `/server stop` kills exactly the process
//! Wizard started and never an unrelated one.
//!
//! This was `src/server.rs` and is now the llama.cpp plugin's, behind
//! `--features provider-llamacpp`. [`crate::server`] has the argument for why
//! it went here rather than into a feature of its own; the short version is
//! that every function below names llama.cpp somewhere — a `/health` whose 503
//! means "still loading the GGUF", a `--n-gpu-layers`, a PID whose process name
//! is checked before it is signalled — so a boundary between "the llama.cpp
//! provider" and "the llama.cpp server" would have been a build flag whose
//! whole content is which half of llama.cpp you get.
//!
//! What core kept is [`crate::server::LocalServer`]: three questions, each answered with a
//! sentence, so that `/server` on the TUI, in the window and in a chat can
//! print what this file says without any of them knowing what it is talking to.
//! The three used to hold that prose themselves, in triplicate.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, ProviderConfig};
use crate::platform::host::{is_executable, local_port};
use crate::progress::Progress;
// RAM headroom beyond the raw weights (KV cache, compute buffers) demanded by
// the preflight fit check in [`ensure_running`]. It lives with the tier table
// because the two have to be sized against the same number.
use crate::hardware::FIT_HEADROOM_GB;

/// Context window passed to a spawned server (`--ctx-size`). Sized so the
/// agent's compaction threshold (48 kB of history ≈ 12k tokens) plus the
/// system prompt and tool specs fit comfortably.
const CTX_SIZE: u32 = 16_384;

/// `--n-gpu-layers` value meaning "offload every layer". llama.cpp clamps it to
/// the model's actual layer count, so this offloads the whole model on a GPU
/// build and is a harmless no-op on a CPU-only build.
const GPU_OFFLOAD_ALL: &str = "99";

/// How long to wait for a spawned server to finish loading its GGUF.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Poll cadence while waiting for readiness.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for a TCP connection on a single health probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many log lines a startup-failure error quotes.
const LOG_TAIL_LINES: usize = 20;

/// What `GET {base_url}/health` says about the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// 200 — model loaded, ready for requests.
    Ready,
    /// 503 — process up, GGUF still loading.
    Loading,
    /// Nothing answering (or an unexpected status).
    Down,
}

/// Probe llama-server's native health endpoint once.
pub async fn probe(base_url: &str) -> Health {
    let Ok(http) = reqwest::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .build()
    else {
        return Health::Down;
    };
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    match http.get(url).timeout(PROBE_TIMEOUT).send().await {
        Ok(response) if response.status().is_success() => Health::Ready,
        Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            Health::Loading
        }
        _ => Health::Down,
    }
}

/// Make sure a ready llama-server answers at `provider.base_url`.
///
/// Already ready: returns immediately. Still loading: waits for readiness.
/// Down: spawns one when possible — the URL points at this machine,
/// `llama-server` is on `PATH`, and the provider has a usable `gguf_path` —
/// and waits for it; anything less is an actionable error telling the user
/// how to start the server themselves.
pub async fn ensure_running(provider: &ProviderConfig, progress: &dyn Progress) -> Result<()> {
    let base_url = provider.base_url.trim_end_matches('/');
    // The model this provider names, when it names one. Carried into every
    // wait so a startup failure can be diagnosed against the model that was
    // actually loading rather than against the tier table alone.
    let configured_gguf = provider
        .gguf_path
        .as_deref()
        .filter(|path| !path.trim().is_empty());
    match probe(base_url).await {
        Health::Ready => return Ok(()),
        Health::Loading => {
            progress.status(&format!(
                "llama-server at {base_url} is loading its model — waiting…"
            ));
            return wait_ready(base_url, configured_gguf, None, progress).await;
        }
        Health::Down => {}
    }

    let Some(port) = local_port(base_url) else {
        bail!(
            "cannot reach llama-server at {base_url} — the host is not this machine, so Wizard \
             cannot start it for you; start it there with {START_HINT}, or fix the provider's \
             `base_url` in ~/.wizard/config.toml"
        );
    };
    // Probe said Down, but if something is already bound to the port it is not a
    // ready llama-server (or `probe` would have seen its /health). Spawning here
    // just loses a race to bind and exits — "couldn't bind HTTP server socket".
    // Bail with the actionable cause instead of looping on doomed spawns.
    if port_in_use(port) {
        bail!(
            "port {port} on this machine is already in use, but the process holding it is not a \
             ready llama-server (it did not answer {base_url}/health). Free the port \
             (e.g. `fuser -k {port}/tcp`), or point the provider's `base_url` at a different \
             port in ~/.wizard/config.toml"
        );
    }
    let Some(gguf) = configured_gguf else {
        bail!(
            "cannot reach llama-server at {base_url} and the provider has no `gguf_path`, so \
             Wizard cannot start it for you — start it with {START_HINT}, or set `gguf_path` \
             in ~/.wizard/config.toml"
        );
    };
    // A model that cannot fit in this machine's RAM is refused up front —
    // before a multi-GB download, and before llama-server gets OOM-killed
    // halfway through loading it. The check is a RAM-only heuristic: on GPU
    // machines [`spawn`] offloads layers to VRAM, but the model is still
    // staged through system memory during load. Undetectable RAM or an
    // unknown model size skips the check.
    if let Some(ram_gb) = crate::hardware::usable_ram_gb()
        && let Some(model_gb) = model_size_gb(Path::new(gguf))
        && !model_fits(model_gb, ram_gb)
    {
        bail!("{}", fit_failure_message(gguf, model_gb, ram_gb));
    }
    // A missing GGUF that names a known tier is downloaded into place (the
    // one-click local onboarding writes exactly such a path); anything else
    // stays an actionable error.
    if !Path::new(gguf).exists() {
        let tier = Path::new(gguf)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::hardware::gguf_tier_for_file);
        match tier {
            Some(tier) => super::setup::download_gguf(tier, Path::new(gguf), progress)
                .await
                .context("downloading the local model")?,
            None => bail!(
                "cannot start llama-server: the model file {gguf} does not exist — fix \
                 `gguf_path` in ~/.wizard/config.toml"
            ),
        }
    }
    let binary = match find_binary() {
        Some(binary) => binary,
        // Not installed anywhere Wizard looks: install it from the official
        // llama.cpp releases, the same way `install.sh` does.
        None => super::setup::install_llama_server(progress)
            .await
            .with_context(|| {
                format!(
                    "cannot reach llama-server at {base_url} and installing llama.cpp failed — \
                     install it yourself (https://github.com/ggml-org/llama.cpp), then start \
                     the server with {START_HINT}"
                )
            })?,
    };

    let pid = spawn(&binary, gguf, port)?;
    progress.status(&format!(
        "started llama-server (PID {pid}, port {port}) — log: {}",
        log_path()?.display()
    ));
    progress.status(&format!(
        "waiting for the model to load (up to {}s)…",
        READY_TIMEOUT.as_secs()
    ));
    wait_ready(base_url, Some(gguf), Some(pid), progress).await
}

/// Poll `{base_url}/health` until the server reports ready, up to
/// [`READY_TIMEOUT`] (GGUF loads are slow). When `pid` names a server Wizard
/// just spawned, its early death short-circuits the wait. Both failure paths
/// quote the tail of the server log ([`diagnose`]) so the cause is visible.
///
/// `gguf` is the model being loaded, when the caller knows which one it is.
/// Without it a memory-exhaustion diagnosis can only reason from the tier
/// table, and the tier table is what said the model would fit.
pub async fn wait_ready(
    base_url: &str,
    gguf: Option<&str>,
    pid: Option<u32>,
    progress: &dyn Progress,
) -> Result<()> {
    let started = Instant::now();
    let mut reported_loading = false;
    while started.elapsed() < READY_TIMEOUT {
        match probe(base_url).await {
            Health::Ready => return Ok(()),
            Health::Loading if !reported_loading => {
                reported_loading = true;
                progress.status("llama-server is up — loading the model…");
            }
            _ => {}
        }
        if let Some(pid) = pid
            && !process_name(pid).is_some_and(|name| is_llama_server(&name))
        {
            bail!(
                "llama-server (PID {pid}) exited during startup — {}",
                diagnose(&log_path()?, gguf)
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    bail!(
        "llama-server at {base_url} did not become ready within {}s — {}",
        READY_TIMEOUT.as_secs(),
        diagnose(&log_path()?, gguf)
    )
}

/// Command-line arguments for a spawned `llama-server`.
///
/// `--jinja` enables the chat-template engine llama-server needs for
/// OpenAI-style tool calling; without it `/v1/chat/completions` rejects
/// requests that carry tools. When `gpu` is set the model is offloaded to the
/// GPU ([`GPU_OFFLOAD_ALL`]): the local model tier is sized to GPU VRAM
/// ([`crate::hardware::suggest_gguf`]), so a VRAM-tiered model must run on the
/// GPU — left on the CPU it loads entirely into RAM and a large model OOMs the
/// host during startup.
fn server_args(gguf_path: &str, port: u16, gpu: bool) -> Vec<String> {
    let mut args = vec![
        "-m".to_string(),
        gguf_path.to_string(),
        "--port".to_string(),
        port.to_string(),
        "--ctx-size".to_string(),
        CTX_SIZE.to_string(),
        "--jinja".to_string(),
    ];
    if gpu {
        args.push("--n-gpu-layers".to_string());
        args.push(GPU_OFFLOAD_ALL.to_string());
    }
    args
}

/// Whether a `model_gb` model can be loaded on a machine with `ram_gb` of
/// usable RAM, leaving [`FIT_HEADROOM_GB`] for the KV cache and compute
/// buffers llama-server allocates on top of the weights.
fn model_fits(model_gb: u64, ram_gb: u64) -> bool {
    model_gb + FIT_HEADROOM_GB <= ram_gb
}

/// The error for a model that cannot fit in this machine's RAM.
///
/// It names a way out that exists. When a smaller tier still fits, it names
/// that tier; when nothing does, saying "pick a smaller model" would send the
/// user looking for something Wizard does not have, so the message says local
/// inference will not work here and points at the cloud providers onboarding
/// offers instead.
fn fit_failure_message(gguf: &str, model_gb: u64, ram_gb: u64) -> String {
    let head = format!(
        "the model {gguf} is ~{model_gb} GB but this machine has only {ram_gb} GB of usable \
         RAM, so llama-server would be killed loading it"
    );
    match crate::hardware::largest_tier_fitting(ram_gb, FIT_HEADROOM_GB) {
        Some(tier) => format!(
            "{head}; run `wizard --onboard` and pick {} (~{} GB, the largest tier that fits), \
             or point `gguf_path` in ~/.wizard/config.toml at a model that does",
            tier.name, tier.approx_gb
        ),
        None => {
            let smallest = crate::hardware::smallest_gguf_tier();
            format!(
                "{head}. Even the smallest local tier ({}, ~{} GB) needs about {} GB free, so \
                 local inference will not work on this machine: run `wizard --onboard` and pick \
                 a cloud provider (xAI sign-in, Anthropic, OpenAI, OpenRouter), which needs no \
                 local RAM. Nothing smaller to download exists",
                smallest.name,
                smallest.approx_gb,
                smallest.approx_gb + FIT_HEADROOM_GB
            )
        }
    }
}

/// Approximate size of the model at `path` in GB: the file's real size when
/// it exists, else the known tier's [`crate::hardware::GgufModel::approx_gb`]
/// when the file name matches one. `None` for unknown missing files.
fn model_size_gb(path: &Path) -> Option<u64> {
    if let Ok(meta) = std::fs::metadata(path) {
        return Some(meta.len() / (1024 * 1024 * 1024));
    }
    let tier = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(crate::hardware::gguf_tier_for_file)?;
    Some(tier.approx_gb)
}

/// The tail of the llama-server log plus, when it smells like memory
/// exhaustion, a pointer at onboarding. Shared by both startup-failure bails
/// in [`wait_ready`] so the user sees the actual failure, not just a path.
/// `gguf` is the model that was loading, when the caller knows it.
fn diagnose(log: &Path, gguf: Option<&str>) -> String {
    diagnose_with(log, gguf, crate::hardware::usable_ram_gb())
}

/// Testable core of [`diagnose`]: `ram_gb` is this machine's usable RAM.
///
/// It is a parameter for the same reason [`oom_hint`]'s inputs are. Reading it
/// inside meant the one test covering the model → hint wiring could only fail
/// on a host with enough RAM for the largest tier to be a candidate: on a
/// 14 GB dev box or a 16 GB CI runner the advice was the same whether or not
/// the failing model was threaded through, so deleting the wiring left the
/// suite green.
fn diagnose_with(log: &Path, gguf: Option<&str>, ram_gb: Option<u64>) -> String {
    let tail = match std::fs::read_to_string(log) {
        Ok(contents) if !contents.trim().is_empty() => tail_lines(&contents),
        _ => "  (log is empty or unreadable)".to_string(),
    };
    let hint = if looks_like_oom(&tail) {
        format!(
            "\n{}",
            oom_hint(
                gguf.and_then(|path| failing_model_gb(Path::new(path))),
                ram_gb
            )
        )
    } else {
        String::new()
    };
    format!("tail of {}:\n{tail}{hint}", log.display())
}

/// Size of the model that just died, on the scale [`oom_hint`] compares
/// against: a known tier's own `approx_gb` first, then the file on disk.
///
/// [`model_size_gb`] answers the preflight's question ("will this file fit?"),
/// where the real byte count is the honest input. The OOM ceiling is a
/// different question, and mixing the scales made it wrong in the direction
/// that matters: `Qwen3.6-35B-A3B-UD-Q4_K_M.gguf` is 21.4 GiB on disk against
/// an `approx_gb` of 20, so "recommend a tier strictly smaller than 21" let
/// the 35B through and named the model the user had just watched die. Comparing
/// a tier against tiers keeps both sides on the table's own scale; a model that
/// is not in the table has only its file size, and that is fine, because the
/// tiers it is being compared against are then somebody else's numbers anyway.
fn failing_model_gb(path: &Path) -> Option<u64> {
    let tier = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(crate::hardware::gguf_tier_for_file);
    match tier {
        Some(tier) => Some(tier.approx_gb),
        None => model_size_gb(path),
    }
}

/// The advice appended to a log tail that smells of memory exhaustion.
///
/// `failing_gb` is the size of the model that just died, `ram_gb` this
/// machine's usable RAM; both are `None` when unknown. Taking them as
/// parameters is what makes the advice honest *and* testable: asking only
/// "what is the largest tier that fits this much RAM?" answers with the model
/// that was just OOM-killed, because it is the tier the fit table picked for
/// this machine in the first place. Knowing what died lets the recommendation
/// be strictly smaller, and lets the message admit it when nothing smaller
/// exists rather than pointing at a model the user has already watched fail.
/// Same honesty rule as [`fit_failure_message`].
fn oom_hint(failing_gb: Option<u64>, ram_gb: Option<u64>) -> String {
    const LEAD: &str = "the log suggests the model did not fit in memory";
    let Some(ram_gb) = ram_gb else {
        // RAM unknown, so which tiers fit is unknown too; the picker is
        // still the right place to look.
        return format!("{LEAD}; run `wizard --onboard` to pick a smaller model");
    };
    match crate::hardware::largest_tier_fitting_below(ram_gb, FIT_HEADROOM_GB, failing_gb) {
        Some(tier) => {
            let why = match failing_gb {
                Some(gb) => format!("the largest tier smaller than the ~{gb} GB model that failed"),
                None => format!("the largest tier that fits {ram_gb} GB of RAM"),
            };
            format!(
                "{LEAD}; run `wizard --onboard` and pick {} (~{} GB, {why})",
                tier.name, tier.approx_gb
            )
        }
        // Nothing *smaller* fits, which hides two very different machines.
        None => {
            let fits_anything =
                crate::hardware::largest_tier_fitting(ram_gb, FIT_HEADROOM_GB).is_some();
            match failing_gb {
                // Tiers do fit this machine; the model that died was simply at
                // or below the smallest one. Saying "local inference will not
                // work here" would be false (this is reachable on a 64 GB
                // workstation running someone's own 2 GB fine-tune), and
                // naming a tier would mean recommending something *larger*
                // than what already failed. What is left is the honest answer:
                // nothing smaller exists, so the memory has to come from
                // somewhere else.
                Some(gb) if fits_anything => format!(
                    "{LEAD}, and Wizard has no local tier smaller than the ~{gb} GB model that \
                     failed, so there is nothing smaller to fall back to; free memory on this \
                     machine (llama-server needs room for its KV cache and compute buffers on \
                     top of the weights) and try again, or run `wizard --onboard` and pick a \
                     cloud provider (xAI sign-in, Anthropic, OpenAI, OpenRouter), which needs \
                     no local RAM"
                ),
                // Nothing in the table fits this machine at all: the one case
                // where "local inference will not work here" is the truth.
                failing => {
                    let nothing = match failing {
                        Some(gb) => format!(
                            "no local tier fits {ram_gb} GB of RAM, and none is smaller than \
                             the ~{gb} GB model that failed"
                        ),
                        None => format!("no local tier fits {ram_gb} GB of RAM"),
                    };
                    format!(
                        "{LEAD}, and {nothing}; run `wizard --onboard` and pick a cloud provider \
                         (xAI sign-in, Anthropic, OpenAI, OpenRouter) instead"
                    )
                }
            }
        }
    }
}

/// Last [`LOG_TAIL_LINES`] lines of `contents`, indented two spaces, with
/// over-long lines cut so one giant line cannot flood the error.
fn tail_lines(contents: &str) -> String {
    /// Per-line cap; llama-server lines are normally well under this.
    const MAX_LINE_CHARS: usize = 200;
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..]
        .iter()
        .map(|line| {
            if line.chars().count() > MAX_LINE_CHARS {
                let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                format!("  {cut}…")
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a log tail looks like llama-server died of memory exhaustion:
/// allocator failures from llama.cpp/ggml, or the shell/kernel reporting an
/// OOM kill.
fn looks_like_oom(tail: &str) -> bool {
    let lower = tail.to_lowercase();
    [
        "out of memory",
        "failed to allocate",
        "cannot allocate",
        "ggml_aligned_malloc",
        "killed",
    ]
    .iter()
    .any(|signature| lower.contains(signature))
}

/// Start a detached `llama-server` serving `gguf_path` on `port`. The child
/// gets its own process group and appends stdout/stderr to [`log_path`], so
/// it keeps serving after Wizard exits. The PID is recorded in [`pid_path`]
/// for `/server stop` and returned.
pub fn spawn(binary: &Path, gguf_path: &str, port: u16) -> Result<u32> {
    let log_path = log_path()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut command = std::process::Command::new(binary);
    command
        .args(server_args(gguf_path, port, crate::hardware::has_gpu()))
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().context("duplicating log handle")?)
        .stderr(log);
    // Own process group: a Ctrl-C in Wizard's terminal signals the
    // foreground group and must not take the server down with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;
    let pid = child.id();
    // Reap the child if it exits while Wizard is still running, so it never
    // lingers as a zombie. The thread dies with Wizard; the server doesn't.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    write_pid(&pid_path()?, pid)?;
    Ok(pid)
}

/// Outcome of [`stop`].
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// SIGTERM sent to the recorded PID.
    Stopped(u32),
    /// No PID on record — Wizard never started a server.
    NotRecorded,
    /// The recorded PID is gone (the server already exited).
    NotRunning(u32),
    /// The recorded PID now belongs to some other program; refused to kill.
    NotOurs { pid: u32, name: String },
}

/// Stop the llama-server recorded in [`pid_path`]. The PID is verified to
/// still be a running `llama-server` before any signal is sent — a recycled
/// PID must never kill an unrelated process.
pub fn stop() -> Result<StopOutcome> {
    stop_at(&pid_path()?)
}

/// Testable core of [`stop`]: operates on an explicit PID-file path.
fn stop_at(pid_file: &Path) -> Result<StopOutcome> {
    let Some(pid) = read_pid(pid_file) else {
        return Ok(StopOutcome::NotRecorded);
    };
    match process_name(pid) {
        None => {
            let _ = std::fs::remove_file(pid_file);
            Ok(StopOutcome::NotRunning(pid))
        }
        Some(name) if !is_llama_server(&name) => Ok(StopOutcome::NotOurs { pid, name }),
        Some(_) => {
            let status = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .context("running kill")?;
            if !status.success() {
                bail!("kill {pid} exited with {status}");
            }
            let _ = std::fs::remove_file(pid_file);
            Ok(StopOutcome::Stopped(pid))
        }
    }
}

/// PID recorded by [`spawn`], when that process is still a running
/// `llama-server`.
pub fn spawned_pid() -> Option<u32> {
    let pid = read_pid(&pid_path().ok()?)?;
    process_name(pid)
        .is_some_and(|name| is_llama_server(&name))
        .then_some(pid)
}

/// The executable Wizard looks for and verifies PIDs against.
const BINARY_NAME: &str = "llama-server";

/// The "start it yourself" command quoted by every unspawnable-server error.
const START_HINT: &str = "`llama-server -m <model.gguf> --port 11435`";

/// Find `llama-server`: on `PATH`, then in the locations Wizard's own
/// installer uses (`~/.wizard/bin`, `~/.wizard/llama.cpp`) — those are not
/// usually on `PATH`.
pub fn find_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(BINARY_NAME))
            .find(|candidate| is_executable(candidate))
    {
        return Some(found);
    }
    let wizard = Config::wizard_dir().ok()?;
    [wizard.join("bin"), wizard.join("llama.cpp")]
        .into_iter()
        .map(|dir| dir.join(BINARY_NAME))
        .find(|candidate| is_executable(candidate))
}

/// Whether something already accepts TCP connections on `127.0.0.1:port`.
///
/// `llama-server` binds the loopback address, so a successful connect here
/// means a spawn would fail with `couldn't bind HTTP server socket`. A short
/// timeout keeps the check cheap; "connection refused" (nothing listening)
/// returns quickly as `false`.
fn port_in_use(port: u16) -> bool {
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// `~/.wizard/llama-server.log` — stdout/stderr of servers Wizard spawned.
pub fn log_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("llama-server.log"))
}

/// `~/.wizard/llama-server.pid` — PID of the server Wizard spawned.
pub fn pid_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("llama-server.pid"))
}

fn write_pid(path: &Path, pid: u32) -> Result<()> {
    std::fs::write(path, format!("{pid}\n")).with_context(|| format!("writing {}", path.display()))
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a process name belongs to llama-server. Distro wrappers rename
/// the real executable (NixOS execs `.llama-server-wrapped`, which `comm`
/// truncates to 15 bytes), so this matches on substring rather than
/// equality — a recycled PID with an unrelated name still never matches.
fn is_llama_server(name: &str) -> bool {
    name.contains(BINARY_NAME)
}

/// Name of the running process `pid`, or `None` when no such process.
fn process_name(pid: u32) -> Option<String> {
    // /proc is authoritative on Linux; `ps` covers macOS and the BSDs.
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        return Some(comm.trim().to_string());
    }
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        return None;
    }
    // Some platforms print the full path; PID checks compare the file name.
    Some(name.rsplit('/').next().unwrap_or(&name).to_string())
}

// ---------------------------------------------------------------------------
// The `/server` seam
// ---------------------------------------------------------------------------

/// llama.cpp's answer to [`crate::server::LocalServer`].
///
/// A unit struct because a process manager has no state: everything it knows it
/// reads back off the disk (`~/.wizard/llama-server.pid`) or off the wire
/// (`GET /health`). Holding a handle to a spawned child instead was the obvious
/// alternative and is wrong for the reason the PID file exists — the server
/// outlives the wizard that started it, on purpose, so the next wizard has to
/// be able to find it.
///
/// Every string below used to live three times over, in `app/command.rs`, in
/// the window's `command.rs` and in the gateway's. They were the same sentences
/// with three different error prefixes, which is the shape a boundary drawn in
/// the wrong place leaves behind.
pub struct LlamaCppServer;

#[async_trait::async_trait]
impl crate::server::LocalServer for LlamaCppServer {
    async fn status(&self, provider: &ProviderConfig) -> String {
        // Whether *this* wizard started it is worth saying: it decides whether
        // `/server stop` will do anything, and a server somebody started by
        // hand is one Wizard deliberately will not touch.
        let spawned = spawned_pid()
            .map(|pid| format!(" (PID {pid}, started by wizard)"))
            .unwrap_or_default();
        match probe(&provider.base_url).await {
            Health::Ready => format!("llama-server at {}: ready{spawned}", provider.base_url),
            Health::Loading => format!(
                "llama-server at {}: loading its model{spawned}",
                provider.base_url
            ),
            Health::Down => format!(
                "llama-server at {}: not running — start it with /server start",
                provider.base_url
            ),
        }
    }

    async fn is_down(&self, provider: &ProviderConfig) -> bool {
        probe(&provider.base_url).await == Health::Down
    }

    async fn start(
        &self,
        provider: ProviderConfig,
        progress: Box<dyn crate::progress::Progress>,
    ) -> Result<String, String> {
        if probe(&provider.base_url).await == Health::Ready {
            return Ok(format!(
                "llama-server at {} is already running",
                provider.base_url
            ));
        }
        // Said through the sink rather than returned, because everything after
        // it can take minutes — a binary to install, several GB of weights to
        // fetch — and a surface that only sees the return value would show
        // nothing at all until it was over.
        progress.status(&format!("starting llama-server at {}…", provider.base_url));
        match ensure_running(&provider, &*progress).await {
            Ok(()) => Ok(format!("llama-server at {} is ready", provider.base_url)),
            Err(err) => Err(format!("could not start llama-server: {err:#}")),
        }
    }

    fn stop(&self) -> String {
        match stop() {
            Ok(StopOutcome::Stopped(pid)) => format!("stopped llama-server (PID {pid})"),
            Ok(StopOutcome::NotRecorded) => {
                "wizard has not started a llama-server — nothing to stop".to_string()
            }
            Ok(StopOutcome::NotRunning(pid)) => format!("llama-server (PID {pid}) already exited"),
            Ok(StopOutcome::NotOurs { pid, name }) => {
                format!("refusing to stop PID {pid}: it is '{name}', not llama-server")
            }
            Err(err) => format!("could not stop llama-server: {err:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_round_trips_and_rejects_garbage() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("llama-server.pid");

        assert_eq!(read_pid(&path), None, "missing file");
        write_pid(&path, 4242).expect("write pid");
        assert_eq!(read_pid(&path), Some(4242));

        std::fs::write(&path, "not a pid\n").expect("write garbage");
        assert_eq!(read_pid(&path), None, "garbage is not a pid");
    }

    #[test]
    fn is_llama_server_tolerates_wrapper_names() {
        assert!(is_llama_server("llama-server"));
        // NixOS wraps the binary as `.llama-server-wrapped`; /proc comm
        // additionally truncates names to 15 bytes.
        assert!(is_llama_server(".llama-server-wrapped"));
        assert!(is_llama_server(".llama-server-w"));
        assert!(!is_llama_server("wizard"));
        assert!(!is_llama_server("llama-cli"));
    }

    #[test]
    fn process_name_resolves_live_and_dead_pids() {
        let me = process_name(std::process::id()).expect("own process exists");
        assert!(!me.is_empty());
        // PIDs are capped well below this on every supported platform.
        assert_eq!(process_name(u32::MAX - 1), None);
    }

    #[test]
    fn stop_refuses_to_kill_a_process_that_is_not_llama_server() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("llama-server.pid");
        // Record this test process: alive, but definitely not llama-server.
        write_pid(&path, std::process::id()).expect("write pid");

        match stop_at(&path).expect("stop runs") {
            StopOutcome::NotOurs { pid, name } => {
                assert_eq!(pid, std::process::id());
                assert_ne!(name, BINARY_NAME);
            }
            other => panic!("expected NotOurs, got {other:?}"),
        }
        assert!(path.exists(), "a refused stop keeps the record");
    }

    #[test]
    fn stop_clears_a_stale_pid_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("llama-server.pid");
        write_pid(&path, u32::MAX - 1).expect("write pid");

        assert_eq!(
            stop_at(&path).expect("stop runs"),
            StopOutcome::NotRunning(u32::MAX - 1)
        );
        assert!(!path.exists(), "stale record is removed");
    }

    #[test]
    fn stop_without_a_record_is_a_noop() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            stop_at(&dir.path().join("llama-server.pid")).expect("stop runs"),
            StopOutcome::NotRecorded
        );
    }

    #[test]
    fn server_args_always_carry_model_context_and_jinja() {
        let args = server_args("/models/m.gguf", 8080, false);
        // -m <path>, --port 8080, --ctx-size, --jinja, all present and paired.
        let pos = args.iter().position(|a| a == "-m").expect("-m present");
        assert_eq!(args[pos + 1], "/models/m.gguf");
        let port = args.iter().position(|a| a == "--port").expect("--port");
        assert_eq!(args[port + 1], "8080");
        assert!(args.iter().any(|a| a == "--ctx-size"));
        assert!(args.iter().any(|a| a == "--jinja"));
    }

    #[test]
    fn server_args_offload_to_gpu_only_when_a_gpu_is_present() {
        let cpu = server_args("/models/m.gguf", 8080, false);
        assert!(
            !cpu.iter().any(|a| a == "--n-gpu-layers"),
            "CPU-only spawn must not request GPU offload"
        );

        let gpu = server_args("/models/m.gguf", 8080, true);
        let pos = gpu
            .iter()
            .position(|a| a == "--n-gpu-layers")
            .expect("GPU spawn offloads layers");
        assert_eq!(gpu[pos + 1], GPU_OFFLOAD_ALL, "offload every layer");
    }

    #[test]
    fn port_in_use_detects_a_bound_socket() {
        // Bind an ephemeral loopback port: nothing else can be on it, and it is
        // definitively in use while the listener lives.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("addr").port();
        assert!(port_in_use(port), "a bound port reads as in use");

        drop(listener);
        // The OS may hold the port briefly in TIME_WAIT, but a listener-less
        // port refuses connections; allow a couple of retries to avoid flaking.
        let freed = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(25));
            !port_in_use(port)
        });
        assert!(freed, "a released port eventually reads as free");
    }

    #[test]
    fn every_suggested_tier_passes_the_fit_check_at_its_boundary() {
        // The tier table and the preflight check must agree: a machine with
        // exactly the RAM that selects a tier must also be allowed to run it.
        // Swept from zero rather than from the first tier's boundary, because
        // the disagreement can only live below it: under 5 GB nothing in the
        // table fits, and there the contract is not "the suggestion runs" but
        // "the suggestion is the floor and nothing is claimed to fit".
        let floor = crate::hardware::smallest_gguf_tier();
        for gb in 0..=64 {
            let tier = crate::hardware::suggest_gguf_model(gb);
            let anything_fits = crate::hardware::largest_tier_fitting(gb, FIT_HEADROOM_GB);
            if anything_fits.is_none() {
                assert!(
                    gb < floor.approx_gb + FIT_HEADROOM_GB,
                    "{gb} GB fits nothing but is not below the floor's requirement"
                );
                assert_eq!(tier.file, floor.file, "{gb} GB must get the floor");
                continue;
            }
            assert!(
                model_fits(tier.approx_gb, gb),
                "{} (~{} GB) does not fit the {gb} GB budget that selects it",
                tier.name,
                tier.approx_gb
            );
        }
    }

    #[test]
    fn an_8gb_machine_starts_the_tier_it_is_given() {
        // An "8 GB" laptop reports ~7 GB of usable RAM. Before the 4B tier
        // existed the smallest model was 6 GB, so 6 + 2 > 7 refused the only
        // thing on offer and told the user to pick something smaller than the
        // smallest thing. It now resolves to a model it can actually load.
        let tier = crate::hardware::suggest_gguf_model(7);
        assert!(
            model_fits(tier.approx_gb, 7),
            "{} (~{} GB) still does not fit an 8 GB laptop",
            tier.name,
            tier.approx_gb
        );
        // And the file it names is one Wizard knows how to download.
        assert!(crate::hardware::gguf_tier_for_file(tier.file).is_some());
    }

    #[test]
    fn a_model_that_does_not_fit_names_a_tier_that_does() {
        let message = fit_failure_message("/m/Qwen3.6-27B-Q4_K_M.gguf", 16, 8);
        assert!(
            message.contains("16 GB"),
            "states the model size: {message}"
        );
        assert!(
            message.contains("8 GB of usable"),
            "states the RAM: {message}"
        );
        assert!(
            message.contains("Qwen3.5 9B"),
            "names the tier that fits: {message}"
        );
        assert!(message.contains("wizard --onboard"));
    }

    #[test]
    fn a_machine_below_every_tier_is_pointed_at_the_cloud() {
        // 2 GB fits nothing, not even the smallest tier, so the message must
        // not send the user hunting for a smaller model that does not exist.
        let message = fit_failure_message("/m/Qwen3.5-4B-Q4_K_M.gguf", 3, 2);
        assert!(
            message.contains("local inference will not work"),
            "says so plainly: {message}"
        );
        assert!(
            message.contains("cloud provider") && message.contains("Anthropic"),
            "names the cloud path: {message}"
        );
        assert!(
            !message.contains("the largest tier that fits"),
            "no tier can be recommended here: {message}"
        );
        assert!(
            message.contains("Qwen3.5 4B"),
            "names the smallest tier it already ruled out: {message}"
        );
    }

    #[test]
    fn oom_hint_never_recommends_the_model_that_just_died() {
        // An 18 GB machine loading the 27B (~16 GB): the fit table admits it
        // (16 + 2 <= 18), which is why it was downloaded and started, and why
        // llama-server being OOM-killed at ctx 16384 must not be answered with
        // "pick Qwen3.6 27B, the largest tier that fits 18 GB of RAM".
        let hint = oom_hint(Some(16), Some(18));
        assert!(
            !hint.contains("Qwen3.6 27B"),
            "recommends the model that just died: {hint}"
        );
        assert!(
            hint.contains("Qwen3.5 9B"),
            "must name the next tier down: {hint}"
        );
        assert!(hint.contains("wizard --onboard"));

        // Same shape one tier lower: an 8 GB laptop that OOMs on the 9B is
        // sent to the 4B, not back to the 9B.
        let hint = oom_hint(Some(6), Some(8));
        assert!(!hint.contains("Qwen3.5 9B"), "{hint}");
        assert!(hint.contains("Qwen3.5 4B"), "{hint}");
    }

    #[test]
    fn oom_hint_admits_when_nothing_smaller_exists() {
        // The floor itself died on an 8 GB machine: by fit alone the 4B still
        // "fits" (3 + 2 <= 8), but recommending it again is recommending the
        // failure. There is nothing smaller to download, so say so and name
        // the cloud, the same way `fit_failure_message` does.
        let hint = oom_hint(Some(3), Some(8));
        assert!(
            !hint.contains("Qwen3.5 4B (~3 GB"),
            "the model that died is not the advice: {hint}"
        );
        assert!(
            hint.contains("no local tier smaller than"),
            "says nothing smaller exists: {hint}"
        );
        assert!(
            hint.contains("cloud provider") && hint.contains("Anthropic"),
            "names the way out: {hint}"
        );

        // A machine under every tier, with no idea what was loading: still the
        // cloud, and no tier named.
        let hint = oom_hint(None, Some(2));
        assert!(hint.contains("no local tier fits 2 GB"), "{hint}");
        assert!(hint.contains("cloud provider"), "{hint}");

        // Adversarial: "nothing smaller fits" is not "nothing fits". A 64 GB
        // workstation whose own 2 GB fine-tune died has every tier available
        // to it, so telling that user local inference cannot work here (and
        // sending them to a paid API) is simply false. It used to say exactly
        // that, because the branch conflated the two questions.
        let hint = oom_hint(Some(2), Some(64));
        assert!(
            !hint.contains("no local tier fits"),
            "every tier fits 64 GB: {hint}"
        );
        assert!(
            hint.contains("nothing smaller to fall back to"),
            "the true statement is that nothing *smaller* exists: {hint}"
        );
        assert!(
            hint.contains("free memory"),
            "and the way out is memory, not a smaller model: {hint}"
        );

        // RAM unknown: which tiers fit is unknown too, so the hint stays
        // vague on purpose rather than inventing a recommendation.
        let hint = oom_hint(Some(16), None);
        assert_eq!(
            hint,
            "the log suggests the model did not fit in memory; run `wizard --onboard` to pick a \
             smaller model"
        );
    }

    #[test]
    fn oom_hint_without_a_model_falls_back_to_the_fit_table() {
        // Nothing knows which model a foreign llama-server was loading, so the
        // best available answer is the largest tier this machine can hold.
        let hint = oom_hint(None, Some(18));
        assert!(hint.contains("Qwen3.6 27B"), "{hint}");
        assert!(hint.contains("fits 18 GB of RAM"), "{hint}");
    }

    #[test]
    fn model_size_gb_prefers_the_real_file_then_known_tiers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tiny.gguf");
        std::fs::write(&path, b"stub").expect("write stub");
        assert_eq!(model_size_gb(&path), Some(0), "real file: real size");
        // Missing but a known tier: the tier's approximate size.
        let missing = dir.path().join("Qwen3.5-9B-Q4_K_M.gguf");
        assert_eq!(model_size_gb(&missing), Some(6));
        // Missing and unknown: no size, the preflight check is skipped.
        assert_eq!(model_size_gb(&dir.path().join("custom.gguf")), None);
    }

    #[test]
    fn tail_lines_keeps_the_last_lines_and_cuts_long_ones() {
        let contents: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let tail = tail_lines(&contents);
        assert!(!tail.contains("line 10"), "older lines dropped");
        assert!(tail.starts_with("  line 11"));
        assert!(tail.ends_with("  line 30"));
        assert_eq!(tail.lines().count(), LOG_TAIL_LINES);

        let long = "x".repeat(500);
        let cut = tail_lines(&long);
        assert!(cut.ends_with('…'), "over-long line is cut");
        assert!(cut.chars().count() < 250);
    }

    #[test]
    fn looks_like_oom_matches_llama_cpp_failure_signatures() {
        assert!(looks_like_oom(
            "ggml_backend_cpu_buffer_type_alloc_buffer: failed to allocate"
        ));
        assert!(looks_like_oom("llama_model_load: error: Out of memory"));
        assert!(looks_like_oom("mmap: Cannot allocate memory"));
        assert!(looks_like_oom("Killed"));
        assert!(!looks_like_oom("main: server is listening on 127.0.0.1"));
        assert!(!looks_like_oom(""));
    }

    #[test]
    fn diagnose_quotes_the_log_and_hints_at_onboarding_on_oom() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = dir.path().join("llama-server.log");

        // Missing log: still a readable message, never an error.
        assert!(diagnose(&log, None).contains("(log is empty or unreadable)"));

        std::fs::write(&log, "loading model\nfailed to allocate 20 GiB\n").expect("write log");
        let message = diagnose(&log, None);
        assert!(message.contains("failed to allocate 20 GiB"));
        assert!(message.contains("wizard --onboard"), "OOM hint present");

        std::fs::write(&log, "wrong chat template\n").expect("write log");
        let message = diagnose(&log, None);
        assert!(message.contains("wrong chat template"));
        assert!(!message.contains("wizard --onboard"), "no hint without OOM");
    }

    /// Adversarial: the model → hint wiring, on a *stated* machine.
    ///
    /// This used to read the host's own RAM, so it could only fail where the
    /// largest tier was a candidate at all: on the 14 GB box this was written
    /// on (and on 16 GB CI runners) the advice was identical whether or not
    /// the failing model was passed, and deleting the whole threading left the
    /// suite green. With the reading as a parameter the two answers differ on
    /// every host, and the control below is what proves it.
    #[test]
    fn the_model_that_died_is_excluded_from_the_advice_on_any_host() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = dir.path().join("llama-server.log");
        std::fs::write(&log, "loading model\nfailed to allocate 20 GiB\n").expect("write log");

        // A 24 GB machine that was loading the largest tier. The fit table
        // admits that tier here (20 + 2 <= 24), which is exactly why it was
        // downloaded and started and exactly why it must not be the advice.
        let biggest = crate::hardware::GGUF_TIERS[0];
        let path = dir.path().join(biggest.file);
        // Deliberately a stub: the real file is ~21 GiB and the ceiling must
        // not depend on the bytes being there. It also pins the scale fix:
        // by file size this stub is 0 GB, which would exclude every tier.
        std::fs::write(&path, b"stub").expect("write stub");
        let message = diagnose_with(&log, Some(&path.display().to_string()), Some(24));
        assert!(
            !message.contains(biggest.name),
            "recommends the model that just died: {message}"
        );
        assert!(
            message.contains("Qwen3.6 27B"),
            "must name the next tier down: {message}"
        );

        // The control that makes the assertion above mean something: without
        // the model, the fit table alone names the largest tier on this same
        // machine. If the threading is deleted, this is what the first
        // assertion gets, and it fails.
        let blind = diagnose_with(&log, None, Some(24));
        assert!(
            blind.contains(biggest.name),
            "control: the fit table alone recommends the largest tier: {blind}"
        );
    }

    #[test]
    fn the_oom_ceiling_is_measured_on_the_tier_tables_own_scale() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A known tier answers with the table's `approx_gb`, whatever the file
        // on disk says. The 35B's real download is ~21.4 GiB against an
        // `approx_gb` of 20, and comparing a measured 21 against the table's
        // 20 let the tier that just died back into the recommendation.
        let biggest = crate::hardware::GGUF_TIERS[0];
        let known = dir.path().join(biggest.file);
        std::fs::write(&known, b"stub").expect("write stub");
        assert_eq!(failing_model_gb(&known), Some(biggest.approx_gb));
        // Missing, but a known tier: same answer.
        assert_eq!(
            failing_model_gb(&dir.path().join("Qwen3.5-9B-Q4_K_M.gguf")),
            Some(6)
        );
        // A model Wizard does not ship has only its own size to go on.
        let foreign = dir.path().join("my-tune-3b-q4.gguf");
        std::fs::write(&foreign, b"stub").expect("write stub");
        assert_eq!(failing_model_gb(&foreign), Some(0));
        assert_eq!(failing_model_gb(&dir.path().join("gone.gguf")), None);
    }

    #[tokio::test]
    async fn probe_reports_down_for_an_unreachable_server() {
        // Port 1 on localhost: connection refused immediately.
        assert_eq!(probe("http://127.0.0.1:1").await, Health::Down);
    }
}
