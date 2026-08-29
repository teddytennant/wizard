//! Best-effort VRAM/RAM detection, ported from `install.sh`'s
//! `detect_memory` / `select_model`. Used to suggest a local model (GGUF for
//! llama.cpp, tag for Ollama) that fits the machine. Everything here is
//! defensive: external commands may be absent or print garbage, so only plain
//! unsigned integers `> 0` are trusted, and total failure yields `None`.
//!
//! Per-OS readings dispatch on [`std::env::consts::OS`] rather than `#[cfg]`
//! (see [`host_ram_gb`]): every reader compiles on every host, so each one is
//! unit-testable from any machine and a new OS is one more match arm.
//!
//! Compiling everywhere is not the same as running everywhere, though: a
//! reader that shells out to `sysctl` still only answers on a Mac. So every
//! decision made from a reading lives in a pure function that takes the
//! reading as a parameter ([`ram_for_os`], [`has_unified_memory_on`],
//! [`detect_memory_from`], [`gguf_suggestion_for`]), with the `std::env`
//! wrappers above them doing nothing but gathering. That is what makes an
//! Apple Silicon path testable on a Linux CI box: without it a swapped match
//! arm passes every test on the host that never takes it.

use std::process::Command;

/// What kind of memory a reading describes. The distinction drives two
/// decisions: whether the spawned `llama-server` should offload to the GPU
/// ([`has_gpu`]), and whether a VRAM figure still has to be capped by system
/// RAM ([`gguf_budget_gb`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// A discrete GPU's own VRAM, separate from system RAM.
    GpuVram,
    /// Apple Silicon: the CPU and the Metal GPU address one pool, so the
    /// reading is the VRAM figure and the RAM figure at once.
    Unified,
    /// Plain system RAM, no usable GPU found.
    SystemRam,
}

/// Detected memory budget and where the number came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Available memory in gibibytes (GPU VRAM, unified memory, or system RAM
    /// as a fallback).
    pub gb: u64,
    /// What the number measures.
    pub kind: MemorySource,
    /// Human description of the source (e.g. `"GPU VRAM (nvidia-smi)"`).
    pub source: String,
}

impl Detected {
    /// A discrete GPU's VRAM, `source` naming the tool that reported it.
    fn gpu_vram(gb: u64, source: &str) -> Self {
        Self {
            gb,
            kind: MemorySource::GpuVram,
            source: source.to_string(),
        }
    }

    /// Whether the spawned `llama-server` should offload layers to a GPU: a
    /// discrete card's VRAM, or Apple Silicon's unified pool (the macOS builds
    /// carry the Metal backend). Plain system RAM means CPU-only.
    pub fn offloads_to_gpu(&self) -> bool {
        matches!(self.kind, MemorySource::GpuVram | MemorySource::Unified)
    }
}

/// Parse a line as a plain unsigned integer `> 0`, ignoring surrounding
/// whitespace. Returns `None` for anything else (headers, units, blanks).
fn parse_positive(line: &str) -> Option<u64> {
    match line.trim().parse::<u64>() {
        Ok(value) if value > 0 => Some(value),
        _ => None,
    }
}

/// Whole gibibytes in a byte count, or `None` when it rounds down to zero.
/// Every byte-reporting source (rocm-smi, sysfs, `sysctl hw.memsize`) funnels
/// through here so they all round the same way.
fn bytes_to_gb(bytes: u64) -> Option<u64> {
    let gb = bytes / (1024 * 1024 * 1024);
    (gb > 0).then_some(gb)
}

/// Run a command and capture stdout as a string, or `None` if it cannot run or
/// exits non-zero.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Largest GPU VRAM in GB via `nvidia-smi` (reports MiB).
fn nvidia_vram_gb() -> Option<u64> {
    let stdout = command_stdout(
        "nvidia-smi",
        &["--query-gpu=memory.total", "--format=csv,noheader,nounits"],
    )?;
    let mib = stdout.lines().filter_map(parse_positive).max()?;
    let gb = mib / 1024;
    (gb > 0).then_some(gb)
}

/// Largest GPU VRAM in GB via `rocm-smi` (reports bytes).
fn rocm_vram_gb() -> Option<u64> {
    let stdout = command_stdout("rocm-smi", &["--showmeminfo", "vram", "--csv"])?;
    // The CSV mixes labels and byte counts; trust the largest plausible
    // integer found in any comma-separated field.
    let bytes = stdout
        .lines()
        .flat_map(|line| line.split(','))
        .filter_map(parse_positive)
        .max()?;
    bytes_to_gb(bytes)
}

/// Largest GPU VRAM in GB from sysfs (`mem_info_vram_total`, bytes).
fn sysfs_vram_gb() -> Option<u64> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    let max_bytes = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("card"))
        })
        .filter_map(|entry| {
            let path = entry.path().join("device/mem_info_vram_total");
            std::fs::read_to_string(path)
                .ok()
                .and_then(|raw| parse_positive(&raw))
        })
        .max()?;
    bytes_to_gb(max_bytes)
}

/// `MemTotal` in GB from `/proc/meminfo` contents.
fn parse_meminfo_total_gb(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    // Format: "MemTotal:       16312456 kB"
    let kb = line.split_whitespace().nth(1).and_then(parse_positive)?;
    let gb = kb / (1024 * 1024);
    (gb > 0).then_some(gb)
}

/// A cgroup memory limit in bytes from the raw file contents. `None` for
/// "no limit": cgroup v2 spells that `max`, cgroup v1 reports
/// `PAGE_COUNTER_MAX` (~`LONG_MAX`, far beyond any real machine).
fn parse_cgroup_limit_bytes(contents: &str) -> Option<u64> {
    /// Anything this large (1 EiB) is a no-limit sentinel, not a limit.
    const NO_LIMIT: u64 = 1 << 60;
    match contents.trim() {
        "max" => None,
        raw => parse_positive(raw).filter(|&bytes| bytes < NO_LIMIT),
    }
}

/// The cgroup memory limit confining this process, in GB. Checks cgroup v2
/// (`memory.max`) then v1 (`memory.limit_in_bytes`); `None` outside
/// containers, or when no readable file carries a real limit.
fn cgroup_limit_gb() -> Option<u64> {
    [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .filter_map(|raw| parse_cgroup_limit_bytes(&raw))
    .min()
    .map(|bytes| bytes / (1024 * 1024 * 1024))
}

/// Cap a `MemTotal` reading with an optional cgroup limit. The bool is true
/// when the limit is what set the number.
fn cap_to_cgroup(total_gb: u64, limit_gb: Option<u64>) -> (u64, bool) {
    match limit_gb {
        Some(limit) if limit < total_gb => (limit, true),
        _ => (total_gb, false),
    }
}

/// Total physical RAM in GB from `/proc/meminfo` (`MemTotal`, kB). Linux and
/// Android (Termux) only; there is no `/proc` on macOS.
fn proc_meminfo_ram_gb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_total_gb(&meminfo)
}

/// Share of physical RAM assumed unavailable to user processes, as a percent.
///
/// The two RAM readers are not on the same scale. `/proc/meminfo`'s `MemTotal`
/// already excludes what the kernel keeps for itself (firmware maps, the
/// kernel image, early allocations), which is why an "8 GB" Linux laptop
/// reports about 7.7 GiB and lands on 7 whole gibibytes. `sysctl hw.memsize`
/// does not: it reports the exact bytes installed, so an 8 GB Mac reports a
/// flat 8. The tier boundaries below ([`SMALL_TIER_CEILING_GB`] especially)
/// are calibrated on the `MemTotal` scale, so a raw `hw.memsize` reading walks
/// every 8 GB Mac up one tier into a model macOS will not let it load. This
/// brings physical byte counts onto the same scale before they are rounded.
const OS_RESERVED_PERCENT: u64 = 6;

/// Whole gibibytes of *usable* RAM in a count of physical bytes: the reading
/// minus [`OS_RESERVED_PERCENT`], rounded down. For byte sources that already
/// net out the OS's own reservation, use [`bytes_to_gb`] instead.
///
/// Multiplying after the division keeps the arithmetic away from `u64`'s
/// ceiling at the cost of under a hundred bytes, which cannot move a whole-GB
/// answer.
fn physical_bytes_to_usable_gb(bytes: u64) -> Option<u64> {
    bytes_to_gb(bytes / 100 * (100 - OS_RESERVED_PERCENT))
}

/// Total usable RAM in GB from `sysctl -n hw.memsize`, which prints the exact
/// physical bytes installed. The macOS reader: `install.sh`'s `detect_memory`
/// uses the same sysctl and applies the same [`OS_RESERVED_PERCENT`] haircut,
/// so the installer and the runtime tier the same machine identically.
fn sysctl_memsize_ram_gb() -> Option<u64> {
    let stdout = command_stdout("sysctl", &["-n", "hw.memsize"])?;
    parse_positive(&stdout).and_then(physical_bytes_to_usable_gb)
}

/// Which reader's answer an OS uses for total RAM.
///
/// One arm per OS family. The dispatch is on [`std::env::consts::OS`] rather
/// than `#[cfg]` so every reader is compiled (and unit-testable) everywhere:
/// each one simply fails on the wrong host, `/proc/meminfo` being absent on
/// macOS and `hw.memsize` being an unknown sysctl elsewhere. Adding Windows
/// is one more arm plus its reader, not a restructuring.
///
/// The readers arrive as thunks, and stay lazy, so this choice is pure: a test
/// on any machine can hand the two arms distinguishable answers and catch a
/// swapped arm. Comparing against the live readers cannot do that, because on
/// the host that never takes an arm both answers are `None`.
fn ram_for_os(
    os: &str,
    sysctl: impl FnOnce() -> Option<u64>,
    meminfo: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    match os {
        "macos" => sysctl(),
        // Linux, Android (Termux), and anything else with a Linux-shaped
        // `/proc`; a host without one detects nothing and callers fall back
        // to the smallest tier rather than guessing.
        _ => meminfo(),
    }
}

/// Total RAM in GB usable by user processes as this OS reports it, before any
/// container cap.
fn host_ram_gb() -> Option<u64> {
    ram_for_os(
        std::env::consts::OS,
        sysctl_memsize_ram_gb,
        proc_meminfo_ram_gb,
    )
}

/// Total system RAM in GB ([`host_ram_gb`]), capped by the cgroup memory limit
/// when one is smaller: in a container `MemTotal` reports the host's RAM, not
/// what this process may actually use. The bool is true when the cgroup limit
/// won. Only Linux has cgroups, so elsewhere the cap is always a no-op.
fn system_ram_gb() -> Option<(u64, bool)> {
    let total_gb = host_ram_gb()?;
    let (gb, capped) = cap_to_cgroup(total_gb, cgroup_limit_gb());
    (gb > 0).then_some((gb, capped))
}

/// True when an OS/architecture pair shares one memory pool between CPU and
/// GPU, so a RAM reading is also the VRAM reading: Apple Silicon. Intel Macs
/// have discrete or Intel graphics and no shared pool, so they fall through to
/// plain system RAM.
fn has_unified_memory_on(os: &str, arch: &str) -> bool {
    os == "macos" && arch == "aarch64"
}

/// System RAM usable by this process, in GB ([`system_ram_gb`] including any
/// cgroup cap). This is the binding constraint on GPU machines too: the
/// weights are read and staged through system memory while llama-server
/// loads them, whatever it later offloads to a card.
pub fn usable_ram_gb() -> Option<u64> {
    system_ram_gb().map(|(gb, _)| gb)
}

/// The detection decision with every reading supplied: a discrete GPU's VRAM
/// wins, then Apple Silicon's unified pool, then plain system RAM. `None` only
/// when nothing was read at all.
///
/// Split out of [`detect_memory`] because the readings come from commands and
/// files that only answer on their own OS, while the choice between them is
/// pure and has to be checkable from any host: a swapped arm here is a wrong
/// model tier on someone else's laptop, and a test that runs on the machine
/// that never takes the arm cannot see it.
///
/// `vram` is the largest discrete-GPU reading with the tool that produced it;
/// `ram` is [`system_ram_gb`]'s `(gb, capped_by_cgroup)` pair.
fn detect_memory_from(
    os: &str,
    arch: &str,
    vram: Option<(u64, &str)>,
    ram: Option<(u64, bool)>,
) -> Option<Detected> {
    if let Some((gb, source)) = vram {
        return Some(Detected::gpu_vram(gb, source));
    }
    let (gb, capped) = ram?;
    // Apple Silicon reports no VRAM through any of the probes above (no
    // nvidia-smi, no `/sys/class/drm`), but its Metal GPU addresses the same
    // pool as the CPU. Reporting zero VRAM would send a 64 GB M3 Max to the
    // smallest tier, so report the pool honestly instead.
    if has_unified_memory_on(os, arch) {
        return Some(Detected {
            gb,
            kind: MemorySource::Unified,
            source: "unified memory (Apple Silicon)".to_string(),
        });
    }
    Some(Detected {
        gb,
        kind: MemorySource::SystemRam,
        source: if capped {
            "system RAM (cgroup limit)".to_string()
        } else {
            "system RAM (no GPU detected)".to_string()
        },
    })
}

/// Detect the memory budget, preferring GPU VRAM (nvidia → rocm → sysfs),
/// then Apple Silicon's unified memory, then plain system RAM. `None` only
/// when everything fails. All the judgement lives in [`detect_memory_from`];
/// this gathers the readings for the machine it runs on.
pub fn detect_memory() -> Option<Detected> {
    let vram = nvidia_vram_gb()
        .map(|gb| (gb, "GPU VRAM (nvidia-smi)"))
        .or_else(|| rocm_vram_gb().map(|gb| (gb, "GPU VRAM (rocm-smi)")))
        .or_else(|| sysfs_vram_gb().map(|gb| (gb, "GPU VRAM (sysfs)")));
    detect_memory_from(
        std::env::consts::OS,
        std::env::consts::ARCH,
        vram,
        system_ram_gb(),
    )
}

/// Whether the machine has a GPU the spawned `llama-server` should offload to
/// ([`Detected::offloads_to_gpu`]). The local model tier is sized to the
/// detected budget, so a `true` here means a VRAM-tiered model was picked and
/// the server must offload; left on the CPU it loads entirely into RAM and a
/// large model OOMs the host during startup.
pub fn has_gpu() -> bool {
    detect_memory().is_some_and(|detected| detected.offloads_to_gpu())
}

/// Suggest an Ollama model tag for a given memory budget (GB). Mirrors
/// `install.sh`'s tiers and the boundaries in [`suggest_gguf_model`], down to
/// the 4B tier that keeps 8 GB laptops on a model they can actually load.
pub fn suggest_ollama_model(gb: u64) -> &'static str {
    if gb >= 24 {
        "qwen3.6:35b"
    } else if gb >= 18 {
        "qwen3.6:27b"
    } else if gb >= SMALL_TIER_CEILING_GB {
        "qwen3.5:9b"
    } else {
        "qwen3.5:4b"
    }
}

/// `(tag, explanation)` for Ollama from readings the caller supplies, the
/// counterpart of [`gguf_suggestion_for`] for the other local flavor.
///
/// Everything the GGUF side does applies here: the same [`gguf_budget_gb`] cap
/// (a 24 GB card on an 8 GB host cannot run a 20 GB model through Ollama
/// either, because the weights are staged through system memory whichever
/// runtime loads them) and the same warning when nothing in the table fits,
/// since the tags are the Q4_K_M tiers under different names. Without the cap
/// and the warning the two pickers gave the same machine different advice, and
/// the Ollama one was the wrong half: it read as a recommendation on a machine
/// where the model cannot load.
fn ollama_suggestion_for(
    detected: Option<&Detected>,
    ram_gb: Option<u64>,
) -> (&'static str, String) {
    let Some(detected) = detected else {
        let tag = suggest_ollama_model(0);
        return (
            tag,
            format!("Could not detect GPU VRAM or system RAM; defaulting to {tag}"),
        );
    };
    let (budget, ram_capped) = gguf_budget_gb(detected, ram_gb);
    let tag = suggest_ollama_model(budget);
    let mut explanation = if ram_capped {
        format!(
            "Detected {} GB of {}, capped by {budget} GB of system RAM → {tag}",
            detected.gb, detected.source
        )
    } else {
        format!("Detected {} GB of {} → {tag}", detected.gb, detected.source)
    };
    if largest_tier_fitting(budget, FIT_HEADROOM_GB).is_none() {
        explanation.push_str(&no_local_option_clause(
            suggest_gguf_model(budget).approx_gb,
        ));
    }
    (tag, explanation)
}

/// Run detection and return `(model, explanation)`. Falls back to the smallest
/// model with an explanatory note when nothing can be detected.
pub fn suggest_model() -> (String, String) {
    let (tag, explanation) = ollama_suggestion_for(detect_memory().as_ref(), usable_ram_gb());
    (tag.to_string(), explanation)
}

/// A GGUF model tier for llama.cpp: a display name, the exact filename under
/// `~/.wizard/models/`, and where to download it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GgufModel {
    /// Human-facing name, e.g. `"Qwen3.6 27B"`.
    pub name: &'static str,
    /// Filename under `~/.wizard/models/`, e.g. `"Qwen3.6-27B-Q4_K_M.gguf"`.
    pub file: &'static str,
    /// Hugging Face download URL for [`Self::file`].
    pub url: &'static str,
    /// Approximate file size in GB, used to refuse a model that cannot fit
    /// in RAM before downloading it.
    pub approx_gb: u64,
}

/// GGUF tiers (largest first), the Q4_K_M counterparts of the Ollama tags in
/// [`suggest_ollama_model`]. `install.sh` (WIZARD_LOCAL=1) and
/// [`crate::plugins::llamacpp::setup`] download these exact files.
pub const GGUF_TIERS: &[GgufModel] = &[
    GgufModel {
        name: "Qwen3.6 35B",
        file: "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/main/Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
        approx_gb: 20,
    },
    GgufModel {
        name: "Qwen3.6 27B",
        file: "Qwen3.6-27B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.6-27B-GGUF/resolve/main/Qwen3.6-27B-Q4_K_M.gguf",
        approx_gb: 16,
    },
    GgufModel {
        name: "Qwen3.5 9B",
        file: "Qwen3.5-9B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/Qwen3.5-9B-Q4_K_M.gguf",
        approx_gb: 6,
    },
    // The floor. A 9B needs ~8 GB of RAM once llama-server's KV cache and
    // compute buffers are counted, which an 8 GB laptop (roughly 7 GB usable
    // once the OS's own reservation is netted out, on either reader) does not
    // have; without a 4B-class tier such a machine has no local option at all.
    GgufModel {
        name: "Qwen3.5 4B",
        file: "Qwen3.5-4B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf",
        approx_gb: 3,
    },
];

/// Budget (GB) at and above which the 9B tier is picked over the 4B one.
/// Named because both tier tables and the smallest-tier reasoning share it.
/// It is a figure on the *usable* RAM scale, which is why `hw.memsize` gets
/// the [`OS_RESERVED_PERCENT`] haircut before it is compared against this.
const SMALL_TIER_CEILING_GB: u64 = 8;

/// RAM headroom beyond the raw weights that `llama-server` needs at runtime
/// for its KV cache and compute buffers.
///
/// Lives here, next to the tier table, because the two have to agree: the
/// preflight fit check in [`crate::server`] refuses a model whose weights plus
/// this do not fit, so a tier suggested by this module against a smaller
/// number would be a recommendation the very next step rejects.
pub const FIT_HEADROOM_GB: u64 = 2;

/// The tier whose [`GgufModel::file`] matches `file_name`, if any. Used to
/// decide whether a missing `gguf_path` is one Wizard knows how to download.
pub fn gguf_tier_for_file(file_name: &str) -> Option<&'static GgufModel> {
    GGUF_TIERS.iter().find(|tier| tier.file == file_name)
}

/// Suggest a GGUF tier for a given memory budget (GB). Same boundaries as
/// [`suggest_ollama_model`].
///
/// Down to the smallest tier's own requirement (its `approx_gb` plus
/// [`FIT_HEADROOM_GB`], so 5 GB today) every tier this returns fits its budget
/// and the preflight fit check in [`crate::server`] passes it. Below that
/// nothing in the table fits at all, and this still returns the floor: callers
/// building a picker need a row to point at, and there is no smaller model to
/// name. Whether anything actually fits is [`largest_tier_fitting`]'s
/// question, and [`gguf_suggestion_for`] puts that answer in the explanation
/// so the floor is never offered as if it would run.
pub fn suggest_gguf_model(gb: u64) -> &'static GgufModel {
    if gb >= 24 {
        &GGUF_TIERS[0]
    } else if gb >= 18 {
        &GGUF_TIERS[1]
    } else if gb >= SMALL_TIER_CEILING_GB {
        &GGUF_TIERS[2]
    } else {
        &GGUF_TIERS[3]
    }
}

/// The smallest tier Wizard knows how to download: the last resort before
/// telling the user local inference cannot work on this machine.
pub fn smallest_gguf_tier() -> &'static GgufModel {
    // The table is ordered largest first, and is never empty.
    GGUF_TIERS.last().expect("GGUF_TIERS is never empty")
}

/// The largest tier whose weights plus `headroom_gb` fit in `ram_gb`, or
/// `None` when not even [`smallest_gguf_tier`] does. Callers use this to name
/// a model that would actually work instead of telling the user to "pick
/// something smaller" when nothing smaller exists.
pub fn largest_tier_fitting(ram_gb: u64, headroom_gb: u64) -> Option<&'static GgufModel> {
    largest_tier_fitting_below(ram_gb, headroom_gb, None)
}

/// [`largest_tier_fitting`], additionally capped to tiers strictly smaller
/// than `below_gb` when a ceiling is given.
///
/// The ceiling is what a model that has already died on this machine buys you.
/// The fit table is a static estimate: it counts the weights and a flat
/// headroom, not the KV cache at this context size, not the compute buffers,
/// and not what the OS and the user's other programs are holding. When
/// `llama-server` is killed loading a model the table said would fit, the
/// table has been proved wrong for that model on that machine, and the only
/// recommendation worth making is a strictly smaller one. Recommending by fit
/// alone would name the model that just died.
pub fn largest_tier_fitting_below(
    ram_gb: u64,
    headroom_gb: u64,
    below_gb: Option<u64>,
) -> Option<&'static GgufModel> {
    GGUF_TIERS.iter().find(|tier| {
        tier.approx_gb + headroom_gb <= ram_gb
            && below_gb.is_none_or(|ceiling| tier.approx_gb < ceiling)
    })
}

/// Memory budget for the GGUF tier choice: a discrete GPU's VRAM reading is
/// capped by system RAM, because the weights are staged through system memory
/// while loading, so a 24 GB card on an 8 GB host still cannot run a 20 GB
/// model. Unified memory needs no cap: the two figures are the same pool. The
/// bool is true when the RAM cap won.
fn gguf_budget_gb(detected: &Detected, ram_gb: Option<u64>) -> (u64, bool) {
    match ram_gb {
        Some(ram) if detected.kind == MemorySource::GpuVram && ram < detected.gb => (ram, true),
        _ => (detected.gb, false),
    }
}

/// `(tier, explanation)` for llama.cpp from readings the caller supplies:
/// `detected` is [`detect_memory`]'s answer, `ram_gb` [`usable_ram_gb`]'s.
///
/// Pure so the interesting machines can be tested from any host, the 4 GB
/// container in particular. On a budget below the smallest tier's requirement
/// there is no tier to recommend; the returned tier is still the floor
/// ([`suggest_gguf_model`], so the picker has a row) but the explanation says
/// outright that it will not run here, because the preflight is about to
/// refuse it and a subtitle reading "recommended for this machine" would be a
/// lie the user pays for with a multi-GB download.
fn gguf_suggestion_for(
    detected: Option<&Detected>,
    ram_gb: Option<u64>,
) -> (&'static GgufModel, String) {
    let Some(detected) = detected else {
        let tier = suggest_gguf_model(0);
        return (
            tier,
            format!(
                "Could not detect GPU VRAM or system RAM; defaulting to {}",
                tier.file
            ),
        );
    };
    let (budget, ram_capped) = gguf_budget_gb(detected, ram_gb);
    let tier = suggest_gguf_model(budget);
    let mut explanation = if ram_capped {
        format!(
            "Detected {} GB of {}, capped by {budget} GB of system RAM → {}",
            detected.gb, detected.source, tier.file
        )
    } else {
        format!(
            "Detected {} GB of {} → {}",
            detected.gb, detected.source, tier.file
        )
    };
    if largest_tier_fitting(budget, FIT_HEADROOM_GB).is_none() {
        explanation.push_str(&no_local_option_clause(tier.approx_gb));
    }
    (tier, explanation)
}

/// The verdict both suggestion helpers append when nothing in the tier table
/// fits the detected budget. Named because three places have to agree on it:
/// the two explanations, and [`suggestion_is_a_warning`], which is how a
/// caller with no picker to put a subtitle on (onboarding's one-click "Local")
/// knows the explanation is a warning rather than a recommendation.
const NO_LOCAL_OPTION: &str = "local inference will not work on this machine";

/// The clause appended to a suggestion that cannot run: what it would have
/// needed, that it will not work here, and the way out.
fn no_local_option_clause(tier_gb: u64) -> String {
    format!(
        ", which needs about {} GB free: {NO_LOCAL_OPTION}, so pick a cloud provider instead",
        tier_gb + FIT_HEADROOM_GB
    )
}

/// Whether an explanation from [`suggest_gguf`] or [`suggest_model`] is
/// telling the user their machine cannot run the model it names.
///
/// The suggestion helpers always return a tier (a picker needs a row to point
/// at), so the explanation is the only thing that separates "recommended" from
/// "this will not load here". A caller that shows the explanation as a picker
/// subtitle needs no such test; one that shows nothing by default does.
pub fn suggestion_is_a_warning(explanation: &str) -> bool {
    explanation.contains(NO_LOCAL_OPTION)
}

/// Run detection and return `(tier, explanation)` for llama.cpp. Falls back to
/// the smallest tier with an explanatory note when nothing can be detected.
pub fn suggest_gguf() -> (&'static GgufModel, String) {
    gguf_suggestion_for(detect_memory().as_ref(), usable_ram_gb())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_tier_boundaries() {
        assert_eq!(suggest_ollama_model(0), "qwen3.5:4b");
        assert_eq!(suggest_ollama_model(7), "qwen3.5:4b");
        assert_eq!(suggest_ollama_model(8), "qwen3.5:9b");
        assert_eq!(suggest_ollama_model(17), "qwen3.5:9b");
        assert_eq!(suggest_ollama_model(18), "qwen3.6:27b");
        assert_eq!(suggest_ollama_model(23), "qwen3.6:27b");
        assert_eq!(suggest_ollama_model(24), "qwen3.6:35b");
    }

    #[test]
    fn bytes_to_gb_rounds_down_and_rejects_sub_gigabyte_readings() {
        assert_eq!(bytes_to_gb(68_719_476_736), Some(64), "64 GiB");
        assert_eq!(
            bytes_to_gb(8 * 1024 * 1024 * 1024 - 1),
            Some(7),
            "rounds down"
        );
        assert_eq!(bytes_to_gb(1024 * 1024), None, "1 MiB is not a budget");
        assert_eq!(bytes_to_gb(0), None);
    }

    #[test]
    fn host_ram_reading_comes_from_this_os_reader() {
        // The dispatch itself, with sentinels no reader could produce: on any
        // host, macOS must take the sysctl arm and everything else the
        // /proc arm. Asserting against the live readers cannot show this,
        // because on a Linux box both arms answer `None` for macOS.
        assert_eq!(ram_for_os("macos", || Some(64), || Some(16)), Some(64));
        assert_eq!(ram_for_os("linux", || Some(64), || Some(16)), Some(16));
        assert_eq!(ram_for_os("android", || Some(64), || Some(16)), Some(16));
        assert_eq!(ram_for_os("freebsd", || Some(64), || Some(16)), Some(16));
        // A host whose reader finds nothing detects nothing; callers fall
        // back to the smallest tier rather than guessing.
        assert_eq!(ram_for_os("linux", || Some(64), || None), None);

        // And on this host the reader that belongs to it reports a plausible
        // machine while the other stays silent.
        if cfg!(target_os = "macos") {
            let gb = sysctl_memsize_ram_gb().expect("macOS reports hw.memsize");
            assert!((1..=4096).contains(&gb), "implausible RAM reading: {gb} GB");
            assert_eq!(host_ram_gb(), Some(gb), "macOS dispatches to sysctl");
            assert_eq!(proc_meminfo_ram_gb(), None, "macOS has no /proc/meminfo");
        } else if cfg!(target_os = "linux") {
            let gb = proc_meminfo_ram_gb().expect("Linux reports MemTotal");
            assert!((1..=4096).contains(&gb), "implausible RAM reading: {gb} GB");
            assert_eq!(host_ram_gb(), Some(gb), "Linux dispatches to /proc");
            // `sysctl` may well be installed (procps), but it knows no
            // `hw.memsize`, so the macOS reader stays silent here.
            assert_eq!(sysctl_memsize_ram_gb(), None, "hw.memsize is macOS-only");
        }
    }

    #[test]
    fn unified_memory_is_detected_on_apple_silicon_only() {
        // The property is about OS/arch pairs, not about the host running the
        // test, so drive it with the pairs. An Intel Mac has discrete or Intel
        // graphics and no shared pool; an aarch64 Linux box (a Pi, a Graviton
        // instance) is not a Mac.
        assert!(has_unified_memory_on("macos", "aarch64"));
        assert!(!has_unified_memory_on("macos", "x86_64"));
        assert!(!has_unified_memory_on("linux", "aarch64"));
        assert!(!has_unified_memory_on("windows", "aarch64"));
        // The runtime strings and the compile-time target must name the same
        // machine: the dispatch is on `std::env::consts`, so a typo in either
        // literal would silently send every Mac down the CPU-only path.
        assert_eq!(
            has_unified_memory_on(std::env::consts::OS, std::env::consts::ARCH),
            cfg!(all(target_os = "macos", target_arch = "aarch64")),
            "the live reading follows the same rule"
        );

        // A Mac reports no VRAM through any of the Linux probes, so without
        // the unified-memory branch detection would fall back to plain system
        // RAM and llama-server would never be told to offload to Metal.
        let detected = detect_memory_from("macos", "aarch64", None, Some((16, false)))
            .expect("Apple Silicon detects its pool");
        assert_eq!(detected.kind, MemorySource::Unified);
        assert_eq!(
            detected.gb, 16,
            "one pool: VRAM and RAM are the same number"
        );
        assert!(detected.source.contains("unified memory"));
        assert!(
            detected.offloads_to_gpu(),
            "the Metal build must be offloaded to"
        );
        // The same machine on an Intel Mac is plain system RAM, CPU only.
        let intel = detect_memory_from("macos", "x86_64", None, Some((16, false)))
            .expect("an Intel Mac still reports its RAM");
        assert_eq!(intel.kind, MemorySource::SystemRam);
        assert!(!intel.offloads_to_gpu());
    }

    #[test]
    fn detect_memory_from_prefers_vram_then_unified_then_system_ram() {
        // A discrete card wins wherever it is found, and keeps the name of the
        // tool that found it.
        let gpu = detect_memory_from(
            "linux",
            "x86_64",
            Some((24, "GPU VRAM (rocm-smi)")),
            Some((64, false)),
        )
        .expect("a card is a budget");
        assert_eq!(gpu.kind, MemorySource::GpuVram);
        assert_eq!(gpu.gb, 24, "the card's VRAM, not the host's RAM");
        assert_eq!(gpu.source, "GPU VRAM (rocm-smi)");
        // A discrete card on a Mac is still a discrete card, not unified.
        let egpu = detect_memory_from(
            "macos",
            "aarch64",
            Some((8, "GPU VRAM (nvidia-smi)")),
            Some((16, false)),
        )
        .expect("a card is a budget");
        assert_eq!(egpu.kind, MemorySource::GpuVram);
        // No card: a cgroup-capped container names the cap, an ordinary box
        // names the absence of a GPU.
        let capped = detect_memory_from("linux", "x86_64", None, Some((4, true))).expect("RAM");
        assert_eq!(capped.kind, MemorySource::SystemRam);
        assert_eq!(capped.source, "system RAM (cgroup limit)");
        let plain = detect_memory_from("linux", "x86_64", None, Some((4, false))).expect("RAM");
        assert_eq!(plain.source, "system RAM (no GPU detected)");
        // Nothing read at all is the only `None`.
        assert_eq!(detect_memory_from("linux", "x86_64", None, None), None);
        assert_eq!(detect_memory_from("macos", "aarch64", None, None), None);
    }

    #[test]
    fn parse_positive_rejects_non_positive_integers() {
        assert_eq!(parse_positive("  42 "), Some(42));
        assert_eq!(parse_positive("0"), None);
        assert_eq!(parse_positive("-1"), None);
        assert_eq!(parse_positive("12 kB"), None);
        assert_eq!(parse_positive("MemTotal:"), None);
        assert_eq!(parse_positive(""), None);
    }

    #[test]
    fn parse_meminfo_total_gb_reads_memtotal() {
        let meminfo = "MemTotal:       16312456 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_meminfo_total_gb(meminfo), Some(15));
        assert_eq!(parse_meminfo_total_gb("MemFree: 1234 kB\n"), None);
        assert_eq!(parse_meminfo_total_gb("MemTotal: garbage kB\n"), None);
        assert_eq!(parse_meminfo_total_gb(""), None);
    }

    #[test]
    fn parse_cgroup_limit_rejects_no_limit_sentinels() {
        // cgroup v2 spells "no limit" as the literal string `max`.
        assert_eq!(parse_cgroup_limit_bytes("max\n"), None);
        // cgroup v1 reports PAGE_COUNTER_MAX (~LONG_MAX) when unconfined.
        assert_eq!(parse_cgroup_limit_bytes("9223372036854771712\n"), None);
        assert_eq!(parse_cgroup_limit_bytes(&(1u64 << 60).to_string()), None);
        // A real limit (12 GiB, like a Colab container) is taken at face value.
        assert_eq!(
            parse_cgroup_limit_bytes("12884901888\n"),
            Some(12_884_901_888)
        );
        assert_eq!(parse_cgroup_limit_bytes("0"), None);
        assert_eq!(parse_cgroup_limit_bytes("not a number"), None);
        assert_eq!(parse_cgroup_limit_bytes(""), None);
    }

    #[test]
    fn cap_to_cgroup_only_lowers() {
        assert_eq!(cap_to_cgroup(64, Some(12)), (12, true), "container cap");
        assert_eq!(cap_to_cgroup(16, Some(32)), (16, false), "limit above RAM");
        assert_eq!(cap_to_cgroup(16, Some(16)), (16, false), "equal is no cap");
        assert_eq!(cap_to_cgroup(16, None), (16, false), "no limit");
    }

    #[test]
    fn gguf_budget_caps_vram_by_system_ram() {
        let gpu = Detected::gpu_vram(24, "GPU VRAM (nvidia-smi)");
        // The weights are staged through system memory, so RAM is the
        // binding constraint even with a bigger card.
        assert_eq!(gguf_budget_gb(&gpu, Some(12)), (12, true));
        assert_eq!(gguf_budget_gb(&gpu, Some(64)), (24, false));
        assert_eq!(gguf_budget_gb(&gpu, None), (24, false));
        let ram = Detected {
            gb: 12,
            kind: MemorySource::SystemRam,
            source: "system RAM (cgroup limit)".to_string(),
        };
        assert_eq!(gguf_budget_gb(&ram, Some(12)), (12, false));
        // Unified memory is already the RAM figure, so it is never "capped
        // by system RAM" and never reported as such.
        let unified = Detected {
            gb: 64,
            kind: MemorySource::Unified,
            source: "unified memory (Apple Silicon)".to_string(),
        };
        assert_eq!(gguf_budget_gb(&unified, Some(64)), (64, false));
    }

    #[test]
    fn gguf_tier_boundaries_match_ollama_tiers() {
        assert_eq!(suggest_gguf_model(7).file, "Qwen3.5-4B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(8).file, "Qwen3.5-9B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(17).file, "Qwen3.5-9B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(18).file, "Qwen3.6-27B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(23).file, "Qwen3.6-27B-Q4_K_M.gguf");
        assert_eq!(
            suggest_gguf_model(24).file,
            "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"
        );
        // Four GGUF tiers, four reachable tags. A tier with no tag of its own
        // (or a tag no budget selects) is a machine the llama.cpp flavor can
        // tier and the Ollama flavor cannot, or the reverse.
        let tags: std::collections::BTreeSet<&str> = (0..=64).map(suggest_ollama_model).collect();
        assert_eq!(
            tags.len(),
            GGUF_TIERS.len(),
            "every GGUF tier needs an Ollama tag of its own: {tags:?}"
        );
        // Every boundary picks the same tier in both tables.
        for gb in [0, 7, 8, 17, 18, 23, 24, 48] {
            let gguf = suggest_gguf_model(gb);
            let tag = suggest_ollama_model(gb);
            // "qwen3.6:27b" ↔ "Qwen3.6 27B": compare the size suffix.
            let size = tag.split(':').nth(1).unwrap().to_uppercase();
            assert!(
                gguf.name.ends_with(&size),
                "tier mismatch at {gb} GB: {tag} vs {}",
                gguf.name
            );
        }
    }

    #[test]
    fn gguf_tier_urls_end_with_their_file_names() {
        for tier in GGUF_TIERS {
            assert!(
                tier.url.ends_with(tier.file),
                "URL/file mismatch for {}: {}",
                tier.name,
                tier.url
            );
            assert!(tier.url.starts_with("https://"));
        }
        assert_eq!(
            gguf_tier_for_file("Qwen3.5-9B-Q4_K_M.gguf").map(|t| t.name),
            Some("Qwen3.5 9B")
        );
        assert_eq!(gguf_tier_for_file("other.gguf"), None);
    }

    #[test]
    fn suggest_gguf_returns_a_known_tier() {
        let (tier, explanation) = suggest_gguf();
        assert!(GGUF_TIERS.contains(tier), "unexpected tier {tier:?}");
        assert!(explanation.contains(tier.file));
    }

    #[test]
    fn suggest_model_returns_a_known_tag() {
        let (model, explanation) = suggest_model();
        assert!(
            ["qwen3.6:35b", "qwen3.6:27b", "qwen3.5:9b", "qwen3.5:4b"].contains(&model.as_str()),
            "unexpected model {model}"
        );
        assert!(explanation.contains(&model));
    }

    #[test]
    fn gguf_tiers_are_ordered_largest_first() {
        // largest_tier_fitting walks the table in order and takes the first
        // match, so the ordering is load-bearing, not cosmetic.
        for pair in GGUF_TIERS.windows(2) {
            assert!(
                pair[0].approx_gb > pair[1].approx_gb,
                "{} must be listed before something smaller than {}",
                pair[0].name,
                pair[1].name
            );
        }
        assert_eq!(smallest_gguf_tier().file, "Qwen3.5-4B-Q4_K_M.gguf");
    }

    #[test]
    fn an_8gb_machine_gets_a_real_tier() {
        // An "8 GB" laptop reports ~7 GB of usable RAM: on Linux because
        // MemTotal already nets out the kernel's own reservation, on macOS
        // because `physical_bytes_to_usable_gb` nets out the same share of
        // `hw.memsize`. It must land on a tier that actually fits, with the
        // runtime headroom `crate::server` demands, rather than on a model it
        // cannot load.
        for ram_gb in [5, 6, 7, 8] {
            let tier = suggest_gguf_model(ram_gb);
            assert!(
                tier.approx_gb + FIT_HEADROOM_GB <= ram_gb,
                "{} (~{} GB) does not fit a {ram_gb} GB machine",
                tier.name,
                tier.approx_gb
            );
            assert_eq!(largest_tier_fitting(ram_gb, FIT_HEADROOM_GB), Some(tier));
        }
    }

    #[test]
    fn physical_ram_readings_are_brought_onto_the_memtotal_scale() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // `hw.memsize` reports the exact bytes installed, so the haircut is
        // what makes an 8 GB Mac read the same 7 GB an 8 GB Linux laptop does.
        assert_eq!(physical_bytes_to_usable_gb(8 * GIB), Some(7));
        assert_eq!(bytes_to_gb(8 * GIB), Some(8), "the raw reading was 8");
        assert_eq!(physical_bytes_to_usable_gb(16 * GIB), Some(15));
        assert_eq!(physical_bytes_to_usable_gb(64 * GIB), Some(60));
        // Sub-gigabyte and zero readings are still not budgets.
        assert_eq!(physical_bytes_to_usable_gb(1024 * 1024), None);
        assert_eq!(physical_bytes_to_usable_gb(0), None);
    }

    #[test]
    fn an_8gb_apple_silicon_mac_lands_on_a_tier_it_can_load() {
        // The tier boundaries are on the usable-RAM scale, but `hw.memsize`
        // reports physical bytes: an 8 GB M1 Air read a flat 8, cleared
        // SMALL_TIER_CEILING_GB and was handed the 9B (6 GB of weights plus 2
        // GB of runtime) on a machine macOS itself holds 3-4 GB of, which put
        // the 4B tier out of reach of every Mac Apple sells. Table-driven over
        // the sizes Apple ships, each machine's own physical byte count in.
        const GIB: u64 = 1024 * 1024 * 1024;
        for (installed_gib, expected) in [
            (8, "Qwen3.5-4B-Q4_K_M.gguf"),
            (16, "Qwen3.5-9B-Q4_K_M.gguf"),
            (18, "Qwen3.5-9B-Q4_K_M.gguf"),
            (24, "Qwen3.6-27B-Q4_K_M.gguf"),
            (36, "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"),
        ] {
            let usable = physical_bytes_to_usable_gb(installed_gib * GIB)
                .expect("a Mac that size reports a budget");
            let detected = detect_memory_from("macos", "aarch64", None, Some((usable, false)))
                .expect("Apple Silicon detects its pool");
            let (tier, explanation) = gguf_suggestion_for(Some(&detected), Some(usable));
            assert_eq!(tier.file, expected, "{installed_gib} GB Mac");
            assert!(
                tier.approx_gb + FIT_HEADROOM_GB <= usable,
                "{} (~{} GB) does not fit a {installed_gib} GB Mac ({usable} GB usable)",
                tier.name,
                tier.approx_gb
            );
            assert!(
                !explanation.contains("will not work"),
                "every Mac this size has a local option: {explanation}"
            );
        }
    }

    #[test]
    fn a_budget_below_the_smallest_tier_returns_the_floor_and_says_it_will_not_run() {
        // `docker run --memory=4g`, or a 4 GB SBC. The floor is 3 GB of
        // weights and needs 5 GB with runtime headroom, so nothing in the
        // table fits: the suggestion is still the floor (a picker needs a row,
        // and there is nothing smaller to name) but it must not read as a
        // recommendation, because the preflight is about to refuse it.
        for gb in 0..=4 {
            let tier = suggest_gguf_model(gb);
            assert_eq!(
                tier.file,
                smallest_gguf_tier().file,
                "{gb} GB gets the floor"
            );
            assert_eq!(
                largest_tier_fitting(gb, FIT_HEADROOM_GB),
                None,
                "nothing fits {gb} GB, so nothing may be recommended"
            );
        }
        // The 4 GB container, end to end through the pure core.
        let detected = detect_memory_from("linux", "x86_64", None, Some((4, true)))
            .expect("the cgroup limit is a reading");
        let (tier, explanation) = gguf_suggestion_for(Some(&detected), Some(4));
        assert_eq!(tier.file, "Qwen3.5-4B-Q4_K_M.gguf");
        assert!(
            explanation.contains("local inference will not work on this machine"),
            "the explanation must not read as a recommendation: {explanation}"
        );
        assert!(
            explanation.contains("cloud provider"),
            "and must name the way out: {explanation}"
        );
        assert!(
            explanation.contains("about 5 GB free"),
            "and what it would have taken: {explanation}"
        );
        // One gibibyte more and the floor fits, so the warning disappears.
        let detected = detect_memory_from("linux", "x86_64", None, Some((5, false))).expect("RAM");
        let (tier, explanation) = gguf_suggestion_for(Some(&detected), Some(5));
        assert_eq!(tier.file, "Qwen3.5-4B-Q4_K_M.gguf");
        assert!(!explanation.contains("will not work"), "{explanation}");
    }

    #[test]
    fn largest_tier_fitting_below_skips_the_model_that_just_died() {
        // An 18 GB machine: the 27B (~16 GB) fits by the table, which is
        // exactly why it was started and exactly why recommending it again
        // after it was OOM-killed would be useless.
        assert_eq!(
            largest_tier_fitting(18, FIT_HEADROOM_GB).map(|tier| tier.name),
            Some("Qwen3.6 27B")
        );
        assert_eq!(
            largest_tier_fitting_below(18, FIT_HEADROOM_GB, Some(16)).map(|tier| tier.name),
            Some("Qwen3.5 9B"),
            "the recommendation must be strictly smaller than what died"
        );
        // A ceiling below everything in the table leaves nothing to name.
        assert_eq!(
            largest_tier_fitting_below(64, FIT_HEADROOM_GB, Some(3)),
            None
        );
        // No ceiling is the plain fit question.
        assert_eq!(
            largest_tier_fitting_below(64, FIT_HEADROOM_GB, None),
            largest_tier_fitting(64, FIT_HEADROOM_GB)
        );
    }

    /// The value assigned by the first `MODEL="…"` after `from` in `script`.
    fn model_after(script: &str, from: usize) -> Option<&str> {
        let start = script[from..].find("MODEL=\"")? + from + "MODEL=\"".len();
        let end = start + script[start..].find('"')?;
        Some(&script[start..end])
    }

    /// `install.sh`'s inlined subagents are the ones in `loadout/`.
    ///
    /// The installer writes `~/.wizard/subagents/*.toml` from heredocs, because
    /// it runs before there is a checkout to copy from. So every one of them is
    /// a second copy of a file that also lives in `loadout/subagents/`, and
    /// nothing but this test connects them.
    ///
    /// They had already drifted: the repository's `documenter.toml` grew a
    /// `Voice:` block and three wording changes that the installer's copy never
    /// got, so somebody who ran `curl | bash` had a documenter that wrote
    /// differently from the one a contributor got, with no way to tell and
    /// nothing failing. Prompts drift quietly like that — there is no compile
    /// error for a subagent that is a paragraph out of date.
    #[test]
    fn install_sh_ships_the_same_subagents_as_the_loadout() {
        let root = env!("CARGO_MANIFEST_DIR");
        let script = std::fs::read_to_string(format!("{root}/install.sh"))
            .expect("install.sh sits at the repo root");

        for name in ["documenter", "researcher", "reviewer", "tester"] {
            let repo = std::fs::read_to_string(format!("{root}/loadout/subagents/{name}.toml"))
                .unwrap_or_else(|_| panic!("loadout/subagents/{name}.toml"));

            // The heredoc body: everything between the `loadout_file …` line
            // that names this subagent and the `EOF` that closes it.
            let marker = format!("subagents/{name}.toml");
            let at = script
                .find(&marker)
                .unwrap_or_else(|| panic!("install.sh writes {name}.toml"));
            let body_start = at + script[at..].find('\n').expect("heredoc opener ends") + 1;
            let body_end = body_start
                + script[body_start..]
                    .find("\nEOF\n")
                    .expect("heredoc is terminated");

            assert_eq!(
                script[body_start..body_end].trim_end(),
                repo.trim_end(),
                "install.sh's {name} subagent has drifted from loadout/subagents/{name}.toml; \
                 a curl install and a checkout would disagree about how it behaves"
            );
        }
    }

    #[test]
    fn install_sh_tier_table_matches_the_rust_tier_table() {
        // `install.sh` runs before Wizard exists, so it carries a second copy
        // of this table in shell. The two have to agree: when they drift the
        // installer downloads one model and the runtime refuses it, which is
        // what an 8 GB laptop got. Nothing but this test connects them.
        let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
            .expect("install.sh sits at the repo root");
        for tier in GGUF_TIERS {
            assert!(
                script.contains(&format!("GGUF_FILE=\"{}\"", tier.file)),
                "install.sh cannot download {}",
                tier.file
            );
            let prefix = tier.url.trim_end_matches(tier.file);
            assert!(
                script.contains(&format!("GGUF_URL=\"{prefix}${{GGUF_FILE}}\"")),
                "install.sh downloads {} from somewhere other than {prefix}",
                tier.file
            );
        }

        // The *mapping*, not the presence of the strings. Asserting only that
        // every tag and every boundary appears somewhere passes with the arms
        // swapped (`-ge 24` → the 27B, `-ge 18` → the 35B), which is exactly
        // the drift this test exists to catch: an 18 GB laptop would then be
        // sent the ~20 GB model by the installer and refused by the runtime.
        for boundary in [24, 18, SMALL_TIER_CEILING_GB] {
            let arm = format!("\"$MEM_GB\" -ge {boundary} ]");
            let at = script
                .find(&arm)
                .unwrap_or_else(|| panic!("install.sh's select_model has no {boundary} GB arm"));
            assert_eq!(
                model_after(&script, at),
                Some(suggest_ollama_model(boundary)),
                "install.sh selects a different tag than Wizard does at {boundary} GB"
            );
        }
        // The else-branch below the lowest boundary, which the presence checks
        // could not see either: the same tag also appears in the
        // undetectable-memory fallback higher up the function.
        let lowest = script
            .find(&format!("\"$MEM_GB\" -ge {SMALL_TIER_CEILING_GB} ]"))
            .expect("the lowest boundary");
        let else_branch = lowest
            + script[lowest..]
                .find("else")
                .expect("select_model has an else branch");
        assert_eq!(
            model_after(&script, else_branch),
            Some(suggest_ollama_model(SMALL_TIER_CEILING_GB - 1)),
            "install.sh falls back to a different tag than Wizard does below \
             {SMALL_TIER_CEILING_GB} GB"
        );

        // Every tag the runtime can suggest, including the 0 GB (undetectable)
        // fallback, maps to the GGUF the runtime would have picked for the same
        // budget. `gguf_for_model`'s arms are `<tag>)` … `;;`.
        for gb in [0, 4, 7, 8, 17, 18, 23, 24, 64] {
            let tag = suggest_ollama_model(gb);
            let arm = script
                .find(&format!("{tag})\n"))
                .unwrap_or_else(|| panic!("install.sh's gguf_for_model has no arm for {tag}"));
            let end = arm + script[arm..].find(";;").expect("every case arm ends");
            let file = suggest_gguf_model(gb).file;
            assert!(
                script[arm..end].contains(&format!("GGUF_FILE=\"{file}\"")),
                "install.sh maps {tag} to a different GGUF than Wizard does at {gb} GB \
                 (expected {file})"
            );
        }
        // The haircut that makes a macOS reading comparable to the boundaries.
        // Written multiply-first: dividing by 100 before scaling discards the
        // remainder of a byte count, and shellcheck rejects that form (SC2017),
        // so this assertion has to match the shape CI will actually accept.
        assert!(
            script.contains(&format!("* {} / 100 /", 100 - OS_RESERVED_PERCENT)),
            "install.sh must net the same {OS_RESERVED_PERCENT}% off hw.memsize"
        );
    }

    /// Adversarial: only half of the sub-5 GB honesty fix had landed. The GGUF
    /// picker said outright that nothing would run; the Ollama picker, fed by
    /// the same detection in the same file, still returned a plain
    /// recommendation for a budget nothing can load, and the user found out
    /// after `ollama pull` failed.
    #[test]
    fn the_ollama_suggestion_is_as_honest_as_the_gguf_one() {
        // 4 GB container: the floor needs 5 GB, so nothing fits.
        let tight = detect_memory_from("linux", "x86_64", None, Some((4, true)))
            .expect("the cgroup limit is a reading");
        let (tag, explanation) = ollama_suggestion_for(Some(&tight), Some(4));
        assert_eq!(tag, "qwen3.5:4b", "the floor is still the row to point at");
        assert!(suggestion_is_a_warning(&explanation), "{explanation}");
        assert!(explanation.contains("about 5 GB free"), "{explanation}");
        assert!(explanation.contains("cloud provider"), "{explanation}");
        // And the GGUF half says the same thing about the same machine.
        let (_, gguf) = gguf_suggestion_for(Some(&tight), Some(4));
        assert!(suggestion_is_a_warning(&gguf), "{gguf}");

        // One gibibyte more and the floor fits: a recommendation again.
        let ok = detect_memory_from("linux", "x86_64", None, Some((5, false))).expect("RAM");
        let (tag, explanation) = ollama_suggestion_for(Some(&ok), Some(5));
        assert_eq!(tag, "qwen3.5:4b");
        assert!(!suggestion_is_a_warning(&explanation), "{explanation}");

        // A big card on a small host: the weights are staged through system
        // memory whichever runtime loads them, so the Ollama budget is capped
        // by RAM exactly as the GGUF one is. Uncapped this recommended the
        // 35B tag on an 8 GB machine.
        let card = Detected::gpu_vram(24, "GPU VRAM (nvidia-smi)");
        let (tag, explanation) = ollama_suggestion_for(Some(&card), Some(8));
        assert_eq!(tag, "qwen3.5:9b", "capped by system RAM: {explanation}");
        assert!(explanation.contains("capped by 8 GB"), "{explanation}");
        assert_eq!(
            suggest_gguf_model(8).file,
            gguf_suggestion_for(Some(&card), Some(8)).0.file,
            "both flavors tier the same machine identically"
        );

        // Nothing detected at all: the floor, and no false warning.
        let (tag, explanation) = ollama_suggestion_for(None, None);
        assert_eq!(tag, "qwen3.5:4b");
        assert!(explanation.contains("Could not detect"), "{explanation}");
        assert!(!suggestion_is_a_warning(&explanation), "{explanation}");
    }

    #[test]
    fn largest_tier_fitting_finds_nothing_for_a_2gb_machine() {
        // Below the smallest tier there is no local answer at all; callers
        // must say so instead of pointing at a model that does not exist.
        assert_eq!(largest_tier_fitting(2, 2), None);
        assert_eq!(largest_tier_fitting(4, 2), None);
        assert_eq!(
            largest_tier_fitting(8, 2).map(|tier| tier.name),
            Some("Qwen3.5 9B")
        );
        assert_eq!(
            largest_tier_fitting(64, 2).map(|tier| tier.name),
            Some("Qwen3.6 35B")
        );
    }
}
