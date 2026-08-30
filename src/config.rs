//! User configuration: `~/.wizard/config.toml` plus env overrides and
//! well-known paths under `~/.wizard/` (see "Data on disk" in
//! `docs/architecture.md`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::llm::provider::LlmProvider;
// `ProviderKind` is re-exported from here (see below) because twenty-three
// modules import it as `crate::config::ProviderKind` and it is, on disk, a
// config field. The type itself lives with the registry that resolves it.
use crate::llm::registry;
// Every directory under `~/.wizard` is private state: session JSONLs carry
// full tool output, `logs/` carries traces, `credentials.toml` carries API
// keys. The mode used to be set only as a side effect of `crate::credentials`
// writing a key, so an install that never stored one (a local-provider-only
// setup) left the whole tree at the user's umask, world-readable on most
// distros. `create_private_dir` creates it private instead, and tightens an
// existing loose directory on the next load. The *how* (0700 on unix, an ACL
// on Windows) and the deliberate warn-rather-than-fail policy for a filesystem
// that cannot express it live in `platform::secrets`, next to the other half
// of the same decision.
use crate::platform::secrets::create_private_dir;

/// Personality mode. Shares tools and model; differs in prompting,
/// temperature, step budget, and confirmation behavior (`docs/modes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Mode {
    /// Interactive TUI. Bypass-permissions: auto-approves tool calls and acts
    /// without per-action prompts.
    #[default]
    Genie,
    /// Autonomous agent. Works continuously without human intervention;
    /// self-directing and self-improving.
    Sovereign,
}

impl Mode {
    /// Sampling temperature for this mode (genie 0.8, sovereign 0.6).
    pub fn temperature(self) -> f32 {
        match self {
            Mode::Genie => 0.8,
            Mode::Sovereign => 0.6,
        }
    }
}

/// How many model → tool → model round trips one turn may take.
///
/// Zero — the default — means *unlimited*: the turn ends when the model stops
/// calling tools, or when something else stops it (interrupt, time limit,
/// circuit breaker, sovereign loop-control file). A positive value re-imposes a
/// ceiling; the turn then ends in [`DoneReason::MaxSteps`] when it is reached.
///
/// [`DoneReason::MaxSteps`]: crate::agent::DoneReason::MaxSteps
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepBudget(u32);

impl StepBudget {
    /// No ceiling: the turn runs until the work is done.
    pub const UNLIMITED: Self = Self(0);

    /// Floor on a *finite* budget in sovereign mode. Nobody is waiting at a
    /// prompt to say "continue", so a hand-set budget below this is raised to
    /// it. An unlimited budget is already more permissive and is left alone.
    const SOVEREIGN_FLOOR: u32 = 100;

    /// A budget of `steps` steps. Zero is [`Self::UNLIMITED`].
    pub const fn new(steps: u32) -> Self {
        Self(steps)
    }

    /// The ceiling, or `None` when unlimited.
    pub const fn cap(self) -> Option<u32> {
        if self.0 == 0 { None } else { Some(self.0) }
    }

    /// The highest step number the loop may run, saturating at [`u32::MAX`] when
    /// unlimited — four billion round trips is a limit no turn ever reaches, so
    /// the loop can stay a plain bounded range.
    pub const fn last_step(self) -> u32 {
        match self.cap() {
            Some(cap) => cap,
            None => u32::MAX,
        }
    }

    /// This budget as `mode` needs it: sovereign lifts a finite budget to
    /// [`Self::SOVEREIGN_FLOOR`]; everything else is returned unchanged.
    pub fn for_mode(self, mode: Mode) -> Self {
        match (mode, self.cap()) {
            (Mode::Sovereign, Some(cap)) if cap < Self::SOVEREIGN_FLOOR => {
                Self(Self::SOVEREIGN_FLOOR)
            }
            _ => self,
        }
    }
}

impl Default for StepBudget {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

impl From<u32> for StepBudget {
    fn from(steps: u32) -> Self {
        Self::new(steps)
    }
}

impl fmt::Display for StepBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.cap() {
            Some(cap) => write!(f, "{cap} steps"),
            None => write!(f, "no step limit"),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Genie => write!(f, "genie"),
            Mode::Sovereign => write!(f, "sovereign"),
        }
    }
}

/// Reasoning effort forwarded as the `reasoning_effort` request field to models
/// that expose the knob (xAI Grok 4.x, OpenAI's o-series and gpt-5). Providers
/// without one ignore it. `None` in [`Config`] leaves the provider default
/// (Grok 4.5, for one, defaults to high).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// Wire value sent as the `reasoning_effort` request field.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};

/// Which messaging gateway, if any, Wizard exposes (`wizard --gateway`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum GatewayKind {
    /// No gateway — terminal only.
    #[default]
    None,
    /// Telegram bot (long-poll `getUpdates` / `sendMessage`).
    Telegram,
}

impl fmt::Display for GatewayKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatewayKind::None => write!(f, "none"),
            GatewayKind::Telegram => write!(f, "telegram"),
        }
    }
}

/// Configuration for the optional messaging gateway. Bot tokens are never
/// stored here — only the name of the environment variable that holds the
/// token (`token_env`). The token itself lives in
/// `~/.wizard/credentials.toml` under `[keys] telegram` (preferred) or in the
/// named env var.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Which gateway to run (default [`GatewayKind::None`]).
    #[serde(default)]
    pub kind: GatewayKind,
    /// Name of the env var holding the bot token (default
    /// `WIZARD_TELEGRAM_TOKEN`); the token itself is never persisted to
    /// config. Consulted only when no `telegram` entry exists in
    /// credentials.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Chat IDs allowed to drive the agent. The list is a closed allow-list:
    /// empty means *nobody* is allowed, so an unconfigured gateway refuses
    /// every message instead of handing a stranger an autonomous agent with
    /// the unrestricted tool set. The gateway plugin's `is_authorized` is what
    /// enforces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chat_ids: Vec<i64>,
}

impl GatewayConfig {
    /// Default name of the env var holding a Telegram bot token.
    pub const DEFAULT_TOKEN_ENV: &'static str = "WIZARD_TELEGRAM_TOKEN";

    /// The env var name to read the bot token from, falling back to
    /// [`Self::DEFAULT_TOKEN_ENV`] when unset.
    pub fn token_env(&self) -> &str {
        self.token_env.as_deref().unwrap_or(Self::DEFAULT_TOKEN_ENV)
    }
}

/// The warning a gateway allow-list earns when it names a group chat, or
/// [`None`] when every id in it is a one-to-one chat.
///
/// A negative id is a group on every platform wired up so far, and the
/// allow-list authorises a *chat* rather than a person, so a group in it hands
/// full tool access to everyone in that group — including whoever joins it
/// later.
///
/// It lives in core rather than in the gateway plugin for the same reason
/// [`GatewayConfig`] itself does: it is a fact about a value core parses and
/// keeps parsing on a build with no gateway in it, and its callers sit on both
/// sides of the plugin boundary — `wizard doctor` is core, `wizard gateway
/// setup` is the plugin. They have to say the same sentence, or an operator
/// hears the warning once and is reassured by its absence the second time.
/// Same move [`crate::text::is_invisible`] made when its callers ended up on
/// both sides of the mesh.
///
/// It takes a slice rather than a [`GatewayConfig`] because one of those
/// callers is asking about an id that is not in the config yet: `gateway
/// setup` has just discovered a chat id and is about to offer to write it, and
/// the moment to say "that is a group" is before it lands in the file rather
/// than the next time doctor runs.
pub fn group_chat_warning(allowed: &[i64]) -> Option<String> {
    let groups: Vec<i64> = allowed.iter().copied().filter(|id| *id < 0).collect();
    (!groups.is_empty()).then(|| {
        format!(
            "{groups:?} look like group chats. The allow-list authorises a chat, not a \
             person, so every member of those groups — including anyone added later — can \
             run agent turns on this machine with full tool access. Prefer a one-to-one \
             chat id."
        )
    })
}

/// Cosmetic TUI settings (`[ui]` in `config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UiConfig {
    /// Gerund verbs shown next to the busy spinner ("Conjuring…"). A
    /// non-empty list replaces [`UiConfig::DEFAULT_SPINNER_VERBS`]; missing
    /// or empty keeps the defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spinner_verbs: Vec<String>,
    /// Modal (vim-style) editing for the input composer: NORMAL/INSERT modes,
    /// `hjkl`/word motions, `d`/`c`/`y` operators, `x`/`r`/`p`. Off by
    /// default; toggle live with `/vim`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub vim: bool,
    /// Which coding agent's terminal chrome the TUI wears: `wizard` (default),
    /// `codex`, or `grok`. See [`crate::skin`], which owns the
    /// glyphs, and `/ui` to change it live.
    ///
    /// A skin brings its own palette ([`crate::skin::Skin::companion_theme`]),
    /// which is now the only thing that decides the colors. There was once a
    /// separate `theme` key here, above a `WIZARD_THEME` environment variable,
    /// so the two could be set independently; in practice it meant choosing a
    /// skin left the colors of the old one in place for anybody who had ever
    /// set it. Chrome and palette travel together.
    ///
    /// `Option` because every install that predates the key has no key, and an
    /// absent key has to keep meaning "let `WIZARD_SKIN` decide" rather than
    /// "the default, definitively".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin: Option<String>,
}

/// serde `skip_serializing_if` helper: keep `false` flags out of the written
/// config so the file stays minimal.
fn is_false(b: &bool) -> bool {
    !*b
}

impl UiConfig {
    /// Baked-in wizard-flavored spinner verbs, used when `spinner_verbs`
    /// is unset or empty.
    pub const DEFAULT_SPINNER_VERBS: [&'static str; 20] = [
        "Conjuring",
        "Scrying",
        "Brewing",
        "Transmuting",
        "Enchanting",
        "Divining",
        "Summoning",
        "Incanting",
        "Channeling",
        "Bewitching",
        "Alchemizing",
        "Spellweaving",
        "Polymorphing",
        "Wandwaving",
        "Hexing",
        "Levitating",
        "Crystal-gazing",
        "Runereading",
        "Familiar-consulting",
        "Grimoire-flipping",
    ];

    /// Pick a spinner verb for the given seed: deterministic per seed, spread
    /// across the active list (custom when non-empty, defaults otherwise).
    pub fn spinner_verb(&self, seed: u64) -> &str {
        let roll = splitmix64(seed);
        if self.spinner_verbs.is_empty() {
            Self::DEFAULT_SPINNER_VERBS[(roll % Self::DEFAULT_SPINNER_VERBS.len() as u64) as usize]
        } else {
            &self.spinner_verbs[(roll % self.spinner_verbs.len() as u64) as usize]
        }
    }
}

/// SplitMix64: a tiny, well-mixed hash so consecutive seeds do not walk the
/// verb list in order. Not cryptographic — purely cosmetic.
fn splitmix64(seed: u64) -> u64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Settings for the native web tools (`[web]` in `config.toml`): `web_fetch`
/// response caps, the SSRF guard escape hatch, and `web_search` backend
/// selection. Search API keys are never stored here — only the name of the
/// environment variable holding the key (`search_api_key_env`), read at call
/// time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Byte cap on a `web_fetch` response body (default 100_000).
    pub fetch_max_bytes: usize,
    /// Allow fetches that resolve to localhost / private address ranges
    /// (default false). The SSRF guard is on unless this is set.
    pub allow_local: bool,
    /// `web_search` backend: `"duckduckgo"` (default, no key), `"brave"`,
    /// `"tavily"`, `"exa"`, `"serper"` (all key-based), or `"xai"`/`"grok"`
    /// (xAI Grok web search via the Responses API, using the `wizard --login
    /// xai` OAuth session, else a stored key / `XAI_API_KEY`). Configure it
    /// interactively with `/settings` or during onboarding.
    pub search_backend: String,
    /// Optional fallback env var name holding the search API key, used when no
    /// key has been pasted via `/settings` (which stores keys in
    /// `~/.wizard/credentials.toml` under the backend name). Read at call time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_api_key_env: Option<String>,
    /// Model the `xai` search backend runs its server-side search loop on.
    /// Defaults to a fast non-reasoning Grok, which answers a search in a few
    /// seconds where the flagship model spends most of that time thinking.
    /// Ignored by every other backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_model: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            fetch_max_bytes: 100_000,
            allow_local: false,
            search_backend: "duckduckgo".to_string(),
            search_api_key_env: None,
            search_model: None,
        }
    }
}

/// Shell tool settings (`[shell]` in `config.toml`).
///
/// These govern `execute` only. The git tools, `search_files` and scripted
/// tools run short commands whose *result* is the whole point of the call, so
/// they keep their own fixed budgets and are not configurable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// How long an `execute` call that does not name its own `timeout_secs`
    /// waits in the foreground before the command becomes a background task
    /// (default 30 seconds).
    ///
    /// This is short on purpose, and it is short *because* the command is no
    /// longer killed when it runs out. The old budget was two minutes and its
    /// end was a death sentence, so the number had to cover the longest
    /// command anybody might reasonably run — which meant every wedged command
    /// cost two minutes of a turn doing nothing, and every genuinely long one
    /// cost two minutes and then died anyway. With the handover in place the
    /// two questions come apart: this one is only "how long is it worth
    /// blocking the turn for an answer", and half a minute is a generous
    /// answer to that. The command itself gets [`BACKGROUND_TIMEOUT`] either
    /// way.
    ///
    /// [`BACKGROUND_TIMEOUT`]: crate::tools::tasks::BACKGROUND_TIMEOUT
    pub timeout_secs: u64,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            timeout_secs: crate::tools::shell::DEFAULT_FOREGROUND_SECS,
        }
    }
}

/// Per-file checkpoint settings (`[checkpoints]` in `config.toml`).
/// Snapshots of files edited by Wizard land under
/// `<project>/.wizard/checkpoints/` and power `/rewind` and the perpetual
/// `rollback_failed_cycles` option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointConfig {
    /// Number of most recent turns whose snapshots are kept; older turns are
    /// garbage-collected at session start (default 50).
    ///
    /// `0` means **keep none** — every snapshot is dropped at session start and
    /// `/rewind` has nothing to restore from. It does not mean "unlimited",
    /// which is the way a reader is most likely to take a zero here, and
    /// getting it wrong is silent: the setting takes effect at session start,
    /// long before anybody wants to rewind. There is no unlimited; a large
    /// number is how you say "effectively never collect".
    pub keep_turns: usize,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self { keep_turns: 50 }
    }
}

/// Fleet-mode settings (`[fleet]` in `config.toml`); see `wizard fleet`
/// and docs/fleet.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Per-worker wall-clock cap in minutes; the coordinator kills a child
    /// past this (default 30).
    pub max_minutes: u64,
    /// Run the synthesis turn (an in-process agent merges the fleet
    /// branches) once all workers finish. `false` skips the merge and just
    /// prints the branch list and results table (default true).
    pub synthesize: bool,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            max_minutes: 30,
            synthesize: true,
        }
    }
}

/// Self-update settings (`[update]` in `config.toml`); see `wizard update`.
/// The passive startup check is a courtesy notice by default and never
/// installs anything unless `auto` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Print a one-line notice at startup when a newer release exists
    /// (default true). Purely informational — no download.
    #[serde(default = "default_update_notify")]
    pub notify: bool,
    /// Download and swap in a newer release on startup, in the background
    /// (default false). Never hot-swaps the running process — the new binary
    /// takes effect on the next launch.
    #[serde(default = "default_update_auto")]
    pub auto: bool,
    /// GitHub `owner/repo` for the passive startup check and auto-update
    /// (default `teddytennant/wizard`). `wizard update` always uses the
    /// default repo.
    #[serde(default = "default_update_repo")]
    pub repo: String,
    /// Hours between startup checks (default 24); a cache under
    /// `~/.wizard/update-check.json` throttles network calls to this cadence.
    #[serde(default = "default_update_interval_hours")]
    pub interval_hours: u64,
}

fn default_update_notify() -> bool {
    true
}

fn default_update_auto() -> bool {
    false
}

fn default_update_repo() -> String {
    "teddytennant/wizard".to_string()
}

fn default_update_interval_hours() -> u64 {
    24
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            notify: default_update_notify(),
            auto: default_update_auto(),
            repo: default_update_repo(),
            interval_hours: default_update_interval_hours(),
        }
    }
}

/// Default port for the mesh listener.
///
/// Only reached when `[mesh] listen` is turned on, and deliberately not a
/// well-known one: 4242 is what the design this transport is modelled on uses,
/// it is outside the registered range, and it is not something else's default.
pub const DEFAULT_MESH_PORT: u16 = 4242;

/// Default bind address for the mesh listener: every interface, on
/// [`DEFAULT_MESH_PORT`].
///
/// Wide, and that is the honest default *for a listener somebody turned on*:
/// a mesh peer is on another machine, so binding loopback would make the
/// feature do nothing and the first thing anyone did would be to change it.
/// The protection is that the listener does not exist unless
/// [`MeshConfig::listen`] is set, not that it exists somewhere hard to reach.
pub const DEFAULT_MESH_LISTEN_ADDR: &str = "0.0.0.0:4242";

/// P2P mesh settings (`[mesh]` in `config.toml`); see `wizard peers` and
/// docs/mesh.md.
///
/// # Every default here is off
///
/// `listen` and `mdns` both default to `false`, and that is the whole point of
/// this struct existing rather than the transport picking its own defaults. A
/// mesh that opened a socket on install would be a security surface nobody
/// asked for, and an mDNS advertisement broadcasts this machine's name and
/// public key to every device on the network. Wizard has shipped fail-open
/// defaults before (the Telegram allowlist that defaulted to allow-all, project
/// hooks that ran themselves on session start), which is why these two are
/// written down and pinned by a test rather than assumed.
///
/// There is deliberately **no key here that offers this machine as compute**.
/// `accepts_work` stays false because nothing sets it: delegated work is tier 3
/// and is cut from this release, so a configuration key for it would be a
/// switch with nothing behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshConfig {
    /// Accept inbound mesh connections (default **false**).
    ///
    /// With this off the node can still dial peers it has routes for; what it
    /// cannot do is be dialled. That asymmetry is the point: reaching out is a
    /// thing this machine chose to do, and listening is a thing other machines
    /// get to do to it.
    pub listen: bool,
    /// Where the listener binds when [`MeshConfig::listen`] is on
    /// (default [`DEFAULT_MESH_LISTEN_ADDR`]). Ignored otherwise.
    pub listen_addr: String,
    /// Announce this node on the local network, and look for peers there
    /// (default **false**). See `crate::plugins::mesh::discovery` for what mDNS does
    /// and, more importantly, what it does not.
    pub mdns: bool,
    /// Where to find peers, as `mesh address -> host:port`.
    ///
    /// Routing, not identity, and it carries no authority whatever: a mesh
    /// address is a public key and does not encode a location, so the location
    /// has to be written down somewhere. A wrong or hostile entry here causes a
    /// refused handshake, never a misdirected stream, because the identity is
    /// checked against the certificate the far end presents and not against
    /// anything in this map.
    ///
    /// Adding a route does **not** add a peer. `wizard peers add` does that,
    /// and it is still a paste and a human decision.
    pub routes: BTreeMap<String, String>,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            listen: false,
            listen_addr: DEFAULT_MESH_LISTEN_ADDR.to_string(),
            mdns: false,
            routes: BTreeMap::new(),
        }
    }
}

impl MeshConfig {
    /// The socket the listener binds to, parsed.
    ///
    /// An error rather than a fallback for a malformed value: silently binding
    /// the default when somebody typed an address they meant is how a node ends
    /// up listening somewhere its operator did not intend.
    pub fn listen_socket(&self) -> Result<std::net::SocketAddr> {
        self.listen_addr.parse().map_err(|_| {
            anyhow!(
                "[mesh] listen_addr = {:?} is not a `host:port` address (for example {DEFAULT_MESH_LISTEN_ADDR:?})",
                self.listen_addr
            )
        })
    }
}

/// Cross-machine sync settings (`[sync]` in `config.toml`); see `wizard sync`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Default source for `wizard sync pull`: a bundle file path or http(s)
    /// URL, used when no positional source is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Model-fusion settings (`[fusion]` in `config.toml`); see `/fusion` and
/// [`crate::llm::fusion`]. Panel and synthesizer reference existing
/// [`ProviderConfig`] entries by name — each provider already binds a model, so
/// a panel member is just a registered provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionConfig {
    /// Provider names forming the debate panel (advisors).
    pub panel: Vec<String>,
    /// Provider name that synthesizes the final answer (the sole tool-caller).
    pub synthesizer: String,
    /// Number of critique rounds (default 1).
    #[serde(default = "default_fusion_rounds")]
    pub rounds: u32,
}

fn default_fusion_rounds() -> u32 {
    1
}

/// Mixture-of-agents settings (`[ultra]` in `config.toml`); see `/ultra` and
/// [`crate::agent::ultra`]. Unlike [`FusionConfig`], nothing here names a
/// provider: ultra fans its candidates out over whatever model is *already*
/// active — a provider binds exactly one model, so there is nothing else for
/// them to be on. A lens is therefore just a subagent definition, and the
/// number of lenses *is* the candidate count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UltraConfig {
    /// Candidate lenses, in order — one read-only subagent each. Names resolve
    /// against [`crate::agent::ultra::lens_catalog`]: ultra's built-in lenses,
    /// shadowed by anything of the same name in `~/.wizard/subagents/`, so a
    /// lens can be retuned or replaced with a TOML file and any subagent the
    /// user already wrote can serve as one. Duplicates are rejected — the same
    /// prompt twice buys two near-identical drafts and two panes labeled the
    /// same thing.
    pub lenses: Vec<String>,
    /// Judges that compare the drafts head-to-head. `0` skips the compare
    /// phase and hands the raw drafts to the main agent; one is almost always
    /// enough, since verdicts do not vote — the main agent decides.
    pub judges: u8,
    /// Step budget for one candidate's sub-loop. Ultra owns this: a lens
    /// contributes a prompt, never its own budget or tool scope.
    pub candidate_max_steps: u32,
    /// Step budget for one judge's sub-loop.
    pub judge_max_steps: u32,
    /// Wall-clock cap on one candidate or judge. Not optional, and not
    /// allowed to be zero: without it a throttled provider parks a candidate
    /// inside [`crate::agent::subagent::spawn`]'s retry ladder (`retry_base`
    /// doubling to `retry_max`, six attempts — 315s of sleeping at the shipped
    /// 5s/300s defaults) and the turn hangs for five minutes on a spinner.
    pub timeout_secs: u64,
    /// Ceiling on one draft's characters inside the injected guidance, applied
    /// on top of the context-window budget. The middle of an oversized draft is
    /// elided.
    pub max_draft_chars: usize,
}

impl Default for UltraConfig {
    fn default() -> Self {
        Self {
            lenses: crate::agent::ultra::DEFAULT_LENSES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            judges: 1,
            candidate_max_steps: 10,
            judge_max_steps: 6,
            timeout_secs: 300,
            max_draft_chars: 6_000,
        }
    }
}

/// A named LLM provider. Cloud keys are never stored here: the key itself
/// lives in `~/.wizard/credentials.toml` (0600) under the provider's name, and
/// this struct only records the name of the environment variable that
/// overrides it (`api_key_env`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Unique id, e.g. `"local"`, `"openai"`, `"claude"`.
    pub name: String,
    /// Backend kind.
    pub kind: ProviderKind,
    /// Base URL: llamacpp `http://127.0.0.1:11435`; ollama
    /// `http://127.0.0.1:11434`; openai `https://api.openai.com/v1`;
    /// anthropic `https://api.anthropic.com`; openrouter
    /// `https://openrouter.ai/api/v1`; xai / xaioauth
    /// `https://api.x.ai/v1`; cloudflare
    /// `https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1`.
    pub base_url: String,
    /// Model tag.
    pub model: String,
    /// Name of the env var that overrides the stored key (cloud only); the key
    /// itself is never persisted here; see [`Self::resolved_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Path to the GGUF model file (llamacpp only) — used when Wizard spawns
    /// `llama-server` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gguf_path: Option<String>,
    /// Optional input-token price in USD per million tokens, for `/cost`
    /// estimates. Unset = no cost computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_mtok_in: Option<f64>,
    /// Optional output-token price in USD per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_mtok_out: Option<f64>,
}

impl ProviderConfig {
    /// Resolve the API key: the configured (or default) env var first, then
    /// the key stored under this provider's name in
    /// `~/.wizard/credentials.toml` (0600). Empty string when neither is set.
    ///
    /// The env var deliberately wins. Onboarding pastes the key straight into
    /// the credential store, so the variable is the documented override for a
    /// one-off run, a CI job, or a second key: exporting it must not be
    /// silently ignored because something was stored months ago. Values are
    /// trimmed so a key pasted with a trailing newline still works.
    fn resolved_key(&self, default_env: Option<&str>) -> String {
        self.resolved_key_from(
            default_env,
            |name| std::env::var(name).ok(),
            crate::credentials::get,
        )
    }

    /// Testable core of [`resolved_key`](Self::resolved_key): `lookup` supplies
    /// the value of an environment variable and `stored` the key held under a
    /// provider name in `credentials.toml`, both `None` when unset. Mirrors
    /// [`Config::apply_env_from`], so the precedence can be asserted without
    /// mutating the process environment from a test thread and without writing
    /// the one `credentials.toml` this test binary shares.
    fn resolved_key_from(
        &self,
        default_env: Option<&str>,
        lookup: impl Fn(&str) -> Option<String>,
        stored: impl Fn(&str) -> Option<String>,
    ) -> String {
        let env = self.api_key_env.as_deref().or(default_env);
        if let Some(key) = env.and_then(lookup)
            && !key.trim().is_empty()
        {
            return key.trim().to_string();
        }
        stored(&self.name)
            .map(|key| key.trim().to_string())
            .unwrap_or_default()
    }

    /// The registered descriptor for this provider's kind.
    ///
    /// `None` when nothing has registered that kind: a typo in `config.toml`,
    /// or — once providers are plugins — a provider left out of this profile.
    /// Callers that only need one field off it (a display name, whether a key
    /// is needed) treat the absence as "unknown backend" and carry on; only
    /// [`build`](Self::build) turns it into an error, because that is the one
    /// call that cannot degrade.
    pub fn descriptor(&self) -> Option<ProviderDescriptor> {
        registry::installed(&self.kind)
    }

    /// How this provider proves who is asking, per its descriptor.
    ///
    /// An unregistered kind is reported as a keyed cloud backend with no
    /// default variable, which is the safe reading: it keeps a `/settings`
    /// sheet showing "key missing" rather than claiming a backend nothing can
    /// build is free and local.
    pub fn credentials(&self) -> Credentials {
        self.descriptor()
            .map(|descriptor| descriptor.credentials().clone())
            .unwrap_or(Credentials::ApiKey { default_env: None })
    }

    /// The API key this provider would use right now: its configured env var,
    /// then its kind's default env var, then `credentials.toml`.
    ///
    /// The per-kind default used to be a literal in each arm of `build`'s
    /// match — `Some(openrouter::DEFAULT_KEY_ENV)` and friends — sitting
    /// alongside a second, separately maintained copy of the same table in
    /// `gui::settings::default_key_env`. Both now read the one descriptor, so
    /// the settings sheet cannot disagree with the resolver about where a key
    /// comes from, which it previously could and had to be kept from.
    pub fn api_key(&self) -> String {
        self.resolved_key(self.credentials().default_env())
    }

    /// Warn that this provider has no credential, in the one wording every
    /// backend that warns has always used.
    ///
    /// `label` is what the backend calls the secret ("API key", "API token")
    /// and `fallback` names the variable to export when the config names
    /// none. Only the backends that warned before call this: OpenRouter and
    /// xAI deliberately do not, and preserving that asymmetry exactly is
    /// worth more here than tidying it, because tidying it is a separate
    /// change with its own argument.
    pub fn warn_missing_key(&self, label: &str, fallback: &str) {
        tracing::warn!(
            "provider '{}' has no {label} (store one via /provider or set {}); requests will likely 401",
            self.name,
            self.api_key_env.as_deref().unwrap_or(fallback)
        );
    }

    /// Construct the client for this provider.
    ///
    /// Was a nine-arm `match self.kind` that imported nine concrete provider
    /// types. It is now one lookup, and the nine constructors live in the
    /// modules that own them — which is the whole point of the change: a
    /// tenth provider is a `register` call from anywhere, not an edit here.
    ///
    /// A missing key stays a soft warning inside the backend's own
    /// constructor rather than an error, so `health()` can report the real
    /// failure against the real endpoint.
    pub fn build(&self) -> Result<Arc<dyn LlmProvider>> {
        let descriptor = self
            .descriptor()
            .ok_or_else(|| registry::unknown(&self.kind))?;
        descriptor.build(self)
    }

    /// Get the backend ready to answer: spawn `llama-server`, pull an Ollama
    /// tag, or — for every hosted backend — nothing at all.
    ///
    /// `model` is the tag the caller will actually ask for, which is not
    /// always [`Self::model`]: an agent built with a `/model` override has to
    /// pull the model it is about to use, not the one in the file.
    ///
    /// An unregistered kind is *not* an error here. Preparation is best
    /// effort by construction — every cloud backend has none — and the
    /// caller's next step is `build`, which is where an unknown kind is
    /// supposed to be reported.
    pub async fn prepare(&self, model: &str) -> Result<()> {
        match self.descriptor() {
            Some(descriptor) => descriptor.prepare(self, model).await,
            None => Ok(()),
        }
    }
}

/// Contents of `~/.wizard/config.toml`. Unknown keys are ignored; missing
/// keys take the documented defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model tag for the synthesized local provider (default `qwen3.6:27b`).
    pub model: String,
    /// Base URL of the Ollama server — only used by explicitly configured
    /// Ollama providers; the synthesized local default is always llama.cpp.
    pub ollama_host: String,
    /// Base URL of the local llama.cpp `llama-server` — feeds the synthesized
    /// default provider when `providers` is empty.
    pub llamacpp_host: String,
    /// Path to the GGUF model file for the synthesized llama.cpp provider —
    /// used when Wizard spawns `llama-server` itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gguf_path: Option<String>,
    /// Default personality mode.
    pub mode: Mode,
    /// Reasoning effort forwarded to models that support a `reasoning_effort`
    /// request field (xAI Grok 4.x, OpenAI o-series / gpt-5); set with
    /// `/effort`. `None` leaves the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Agent loop limit per turn, on every surface — TUI, headless, gateway,
    /// GUI. `0` (the default) is unlimited: a turn runs until the model stops
    /// calling tools. A positive value caps the turn; sovereign raises a cap
    /// below its floor, since no one is at the prompt.
    pub max_steps: StepBudget,
    /// Perpetual sovereign operation: keep working/self-directing/self-improving
    /// until stopped.
    pub continuous: bool,
    /// Start every session in plan mode (the `--plan` flag sets this for one
    /// run): the agent investigates with read-only tools and presents a plan
    /// via `exit_plan` before executing. Headless runs auto-approve the plan.
    pub plan_first: bool,
    /// Start every session in omakase (chef's-choice) mode (the `--omakase`
    /// flag sets this for one run): plan mode where the agent decides the
    /// approach itself and auto-approves its own plan. Implies `plan_first`.
    pub omakase: bool,
    /// Continuous mode: re-enter plan mode at the top of every cycle, so each
    /// cycle plans read-only before acting.
    pub plan_each_cycle: bool,
    /// Continuous mode: when a cycle ends in a circuit breaker or a hard
    /// error, restore that cycle's file checkpoints before moving on (the
    /// rollback is noted in `mission.toml`). Default false.
    pub rollback_failed_cycles: bool,
    /// Continuous mode: how many cycles in a row may end badly — a hard error
    /// or a tripped circuit breaker — before the perpetual run gives up.
    ///
    /// A perpetual run is supposed to outlive its own mistakes. One malformed
    /// tool call, one unreadable file, one provider that blinked is not a
    /// reason to end a mission that was asked to run forever; the loop rolls
    /// the cycle back, tells the next cycle what went wrong, backs off, and
    /// tries again. What that alone cannot survive is a setup that is broken
    /// rather than unlucky — no disk space, a model that cannot emit a tool
    /// call this schema will accept — where retrying is an infinite loop that
    /// burns tokens and never lands. This is the line between the two: the
    /// streak is *consecutive*, so any cycle that finishes resets it to zero
    /// and only a genuinely stuck agent ever reaches the limit.
    ///
    /// `0` disables the bound entirely — the run then only ever ends on
    /// `.wizard/loop-control`, `--max-hours`, or a signal. It does not mean
    /// "give up immediately".
    pub max_consecutive_failures: u32,
    /// Base seconds for exponential backoff when the LLM server is unreachable
    /// or rate-limited.
    pub retry_base_secs: u64,
    /// Cap on backoff sleep in seconds.
    pub retry_max_secs: u64,
    /// Pause between continuous cycles (0 = none).
    pub cycle_pause_secs: u64,
    /// Quality gates: commands that must exit zero before a sovereign or
    /// continuous run is allowed to finish (see [`crate::gates`] and
    /// `docs/modes.md`). Merged with `--gate` flags and the project's own
    /// `.wizard/gates.toml`; never applied in genie mode.
    ///
    /// Here as well as on the command line because a gate is a standing rule,
    /// not a per-invocation one. A user who has to remember `--gate 'cargo
    /// test'` on every run is a user whose unattended runs are ungated on the
    /// day it matters.
    #[serde(default)]
    pub gates: Vec<String>,
    /// How many consecutive gate checks may fail before the run gives up and
    /// reports the gates as failing. `0` is unlimited, leaving only
    /// `--max-hours` and `.wizard/loop-control` to end it.
    ///
    /// The bound exists because a model that cannot fix the problem does not
    /// stop trying: it says "fixed it" again, the workspace has not changed,
    /// and the loop would hand it the same failure forever. Consecutive, so a
    /// run that gets the gates green and later breaks them starts over.
    pub gate_max_attempts: u32,
    /// Wall clock for a single gate command, also clamped to the run's own
    /// `--max-hours`. A gate that hangs must not be the thing that outlives
    /// the deadline the whole run is being judged against.
    pub gate_timeout_secs: u64,
    /// When the provider's context window is unknown and the serialized chat
    /// history exceeds this many bytes, compact older messages into a summary.
    /// With a known window, the reported prompt size governs instead.
    pub compact_threshold_bytes: usize,
    /// Configured LLM providers. Empty means "use the legacy `model` /
    /// `ollama_host` fields as a single local Ollama provider".
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Name of the active provider in [`providers`](Self::providers). `None`
    /// (or an unknown name) selects the first configured provider.
    #[serde(default)]
    pub active_provider: Option<String>,
    /// Optional messaging gateway (`wizard --gateway`). Defaults to
    /// [`GatewayKind::None`] — terminal only.
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// Cosmetic TUI settings (spinner verbs).
    #[serde(default)]
    pub ui: UiConfig,
    /// Native web tool settings (`web_fetch` / `web_search`).
    #[serde(default)]
    pub web: WebConfig,
    /// `execute` budgets: how long a command runs in the foreground before it
    /// is handed to the background task registry.
    #[serde(default)]
    pub shell: ShellConfig,
    /// Per-file checkpoint settings (snapshots powering `/rewind`).
    #[serde(default)]
    pub checkpoints: CheckpointConfig,
    /// Fleet-mode settings (`wizard fleet`).
    #[serde(default)]
    pub fleet: FleetConfig,
    /// Self-update settings (`wizard update` + the passive startup check).
    #[serde(default)]
    pub update: UpdateConfig,
    /// Cross-machine sync settings (`wizard sync`).
    #[serde(default)]
    pub sync: SyncConfig,
    /// P2P mesh settings (`wizard peers`). Every default is off; see
    /// [`MeshConfig`].
    #[serde(default)]
    pub mesh: MeshConfig,
    /// Model-fusion settings (`/fusion`). Absent until configured; the toggle
    /// falls back to a default panel derived from `providers` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fusion: Option<FusionConfig>,
    /// Mixture-of-agents settings (`/ultra`). Absent until configured; the
    /// toggle falls back to [`UltraConfig::default`], which needs nothing from
    /// disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ultra: Option<UltraConfig>,
    /// Whether the model may run Lua programs that call Wizard's own tools
    /// (`run_code`).
    ///
    /// Off by default. A program is model-authored code running in-process with
    /// the user's privileges and the full standard library, and a model that
    /// cannot write Lua spends a turn debugging a language nobody asked about.
    /// One flag and no knobs: the compute, memory and call budgets are
    /// constants until there is evidence a user needs to move one. Never
    /// registered on the JSON tool protocol whatever this says, because a
    /// multi-line program inside a JSON string is a stalled turn rather than a
    /// loud failure. See docs/code-mode.md.
    #[serde(default)]
    pub code_mode: bool,
}

/// Default port for the local llama.cpp `llama-server`. Deliberately not 8080:
/// that is a very common dev-server/proxy port (Jupyter and friends on hosted
/// notebooks, local proxies, other web apps), and a collision there makes
/// `llama-server` fail to bind and exit during startup.
pub const DEFAULT_LLAMACPP_PORT: u16 = 11435;

/// Default base URL for the local llama.cpp `llama-server` ([`DEFAULT_LLAMACPP_PORT`]).
pub const DEFAULT_LLAMACPP_HOST: &str = "http://127.0.0.1:11435";

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "qwen3.6:27b".to_string(),
            ollama_host: "http://127.0.0.1:11434".to_string(),
            llamacpp_host: DEFAULT_LLAMACPP_HOST.to_string(),
            gguf_path: None,
            mode: Mode::Genie,
            reasoning_effort: None,
            max_steps: StepBudget::UNLIMITED,
            continuous: false,
            plan_first: false,
            omakase: false,
            plan_each_cycle: false,
            rollback_failed_cycles: false,
            max_consecutive_failures: 5,
            retry_base_secs: 5,
            retry_max_secs: 300,
            cycle_pause_secs: 0,
            gates: Vec::new(),
            gate_max_attempts: 3,
            gate_timeout_secs: 1_800,
            compact_threshold_bytes: 48_000,
            providers: Vec::new(),
            active_provider: None,
            gateway: GatewayConfig::default(),
            ui: UiConfig::default(),
            web: WebConfig::default(),
            shell: ShellConfig::default(),
            checkpoints: CheckpointConfig::default(),
            fleet: FleetConfig::default(),
            update: UpdateConfig::default(),
            sync: SyncConfig::default(),
            mesh: MeshConfig::default(),
            fusion: None,
            ultra: None,
            code_mode: false,
        }
    }
}

/// Relocates `~/.wizard` for this process: set from `WIZARD_HOME`, or by
/// [`use_wizard_dir`] — which the test suite calls, so that a test exercising
/// something that persists config (the TUI's `/vim` toggle, say) writes to a
/// temp directory instead of the developer's real config file.
static WIZARD_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Point this process at `dir` instead of `~/.wizard`. First call wins, so
/// parallel tests all land in the same temp dir.
pub fn use_wizard_dir(dir: PathBuf) {
    let _ = WIZARD_DIR.set(dir);
}

/// Send this test binary's `~/.wizard` to a temp directory of its own.
#[cfg(test)]
fn use_temp_wizard_dir() {
    if WIZARD_DIR.get().is_some() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("wizard-test-home-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    use_wizard_dir(dir);
}

impl Config {
    /// `~/.wizard` — root of all Wizard state on disk. `WIZARD_HOME` (or a
    /// [`use_wizard_dir`] override) relocates it wholesale (sandboxes, CI, a
    /// second install).
    pub fn wizard_dir() -> Result<PathBuf> {
        // Every path into `~/.wizard` — config, credentials, sessions — comes
        // through here, so redirecting it under `cfg(test)` is what keeps the
        // suite off the developer's real state. It is not hypothetical: the
        // TUI's `/vim` toggle persists config, and the vim tests exercise it,
        // which used to overwrite the real config.toml with a default one —
        // silently deleting the developer's providers on every `cargo test`.
        #[cfg(test)]
        use_temp_wizard_dir();
        if let Some(dir) = WIZARD_DIR.get() {
            return Ok(dir.clone());
        }
        if let Some(dir) = std::env::var_os("WIZARD_HOME").filter(|dir| !dir.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let home = dirs::home_dir().context("could not determine home directory")?;
        Ok(home.join(".wizard"))
    }

    /// `~/.wizard/config.toml`
    pub fn path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("config.toml"))
    }

    /// `~/.wizard/mcp.toml` — MCP server declarations.
    pub fn mcp_config_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("mcp.toml"))
    }

    /// `~/.wizard/sessions/` — JSONL chat history.
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("sessions"))
    }

    /// `~/.wizard/images/` — images produced during a session, one directory
    /// per session id (`crate::images::ImageStore`).
    pub fn images_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("images"))
    }

    /// `~/.wizard/attachments/` — non-image files a user attached, one
    /// directory per session id. Images do not land here: they belong to the
    /// content-addressed [`images_dir`](Self::images_dir), which is the only
    /// directory the GUI will serve an image back out of.
    pub fn attachments_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("attachments"))
    }

    /// `~/.wizard/tools/` — agent-authored scripted tools.
    pub fn scripted_tools_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("tools"))
    }

    /// `~/.wizard/skills/` — user/evolved skills (in addition to bundled ones).
    pub fn skills_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("skills"))
    }

    /// `~/.wizard/subagents/` — user-defined subagent definitions (TOML).
    pub fn subagents_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("subagents"))
    }

    /// `~/.wizard/src/` — source checkout for deep evolve.
    pub fn source_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("src"))
    }

    /// `~/.wizard/memory/` — persistent per-project memory
    /// (`crate::memory::MemoryStore`).
    pub fn memory_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("memory"))
    }

    /// `~/.wizard/evolution.jsonl` — self-extension log.
    pub fn evolution_log_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("evolution.jsonl"))
    }

    /// `~/.wizard/logs/` — debug traces.
    pub fn logs_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("logs"))
    }

    /// `~/.wizard/schedule.toml` — cron schedule entries
    /// (`crate::schedule`).
    pub fn schedule_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("schedule.toml"))
    }

    /// `~/.wizard/system_prompt.md` — optional override for the baked base
    /// personality prompt. When this file (or the path in `$WIZARD_SYSTEM_PROMPT`)
    /// exists and is non-empty, its contents replace the compiled prompt; this
    /// is the surface external harness-evolution tools mutate. Absent → baked
    /// default, so behavior is unchanged on a normal install.
    pub fn system_prompt_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("system_prompt.md"))
    }

    /// The active harness bundle directory, if any: `$WIZARD_HARNESS_DIR`
    /// (set directly or via `--harness-dir`). A bundle shadows the compiled
    /// harness defaults per component — `system_prompt.md`,
    /// `tool_descriptions/<tool>.md`, `skills/<name>/SKILL.md`,
    /// `subagents/<name>.toml` — and any missing file falls back to the
    /// default, so a partial or broken bundle degrades gracefully. This is
    /// the surface external harness-evolution tools (e.g. AHE) mutate;
    /// `wizard harness export` produces a bundle of the current defaults.
    pub fn harness_dir() -> Option<PathBuf> {
        let raw = std::env::var_os("WIZARD_HARNESS_DIR")?;
        if raw.is_empty() {
            return None;
        }
        Some(PathBuf::from(raw))
    }

    /// Create the `~/.wizard` directory tree (sessions, tools, skills, logs)
    /// if it does not exist yet. Idempotent; called on every load so a fresh
    /// install is usable without running the installer.
    pub fn ensure_dirs() -> Result<()> {
        for dir in [
            Self::wizard_dir()?,
            Self::sessions_dir()?,
            Self::scripted_tools_dir()?,
            Self::skills_dir()?,
            Self::subagents_dir()?,
            Self::memory_dir()?,
            Self::logs_dir()?,
            Self::wizard_dir()?.join("running"),
        ] {
            create_private_dir(&dir)?;
        }
        Ok(())
    }

    /// Load config from disk, falling back to defaults when the file is
    /// missing, then apply env overrides (`WIZARD_MODEL`,
    /// `WIZARD_OLLAMA_HOST`, `WIZARD_LLAMACPP_HOST`, `WIZARD_GGUF_PATH`).
    /// Creates the `~/.wizard` directory tree on first run.
    pub fn load() -> Result<Self> {
        Self::ensure_dirs()?;

        let path = Self::path()?;
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Self::from_toml(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        config.apply_env();

        if let Some(name) = config.active_provider_mismatch() {
            tracing::warn!(
                "active_provider '{name}' does not match any configured provider; \
                 using '{}' instead",
                config.active().name
            );
        }

        Ok(config)
    }

    /// Parse a config file. Unknown keys are ignored; missing keys take the
    /// documented defaults. Ollama-era files (`model` / `ollama_host` only)
    /// parse fine but synthesize a llama.cpp provider like everything else —
    /// Ollama is opt-in via an explicit `[[providers]]` entry.
    fn from_toml(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }

    /// The effective active provider. When [`providers`](Self::providers) is
    /// non-empty, returns the one named by
    /// [`active_provider`](Self::active_provider) (or the first if unset or
    /// unknown). Otherwise synthesizes a local llama.cpp provider from
    /// `model` / `llamacpp_host` / `gguf_path`.
    pub fn active(&self) -> ProviderConfig {
        if !self.providers.is_empty() {
            let chosen = self
                .active_provider
                .as_ref()
                .and_then(|name| self.providers.iter().find(|p| &p.name == name))
                .or_else(|| self.providers.first());
            if let Some(provider) = chosen {
                return provider.clone();
            }
        }
        ProviderConfig {
            name: "local".to_string(),
            kind: ProviderKind::LLAMACPP,
            base_url: self.llamacpp_host.clone(),
            model: self.model.clone(),
            api_key_env: None,
            gguf_path: self.gguf_path.clone(),
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }
    }

    /// `Some(name)` when [`active_provider`](Self::active_provider) names no
    /// configured provider (typo, or the provider was removed) — in that case
    /// [`active`](Self::active) silently falls back to the first provider (or
    /// the synthesized local one). `None` when the selection resolves or no
    /// provider is named. Surfaced as a warning on load and by `wizard doctor`.
    pub fn active_provider_mismatch(&self) -> Option<String> {
        let name = self.active_provider.as_ref()?;
        if self.providers.iter().any(|p| &p.name == name) {
            None
        } else {
            Some(name.clone())
        }
    }

    /// The effective fusion config: the explicit `[fusion]` block if set,
    /// otherwise a default derived from `providers` (panel = first two, the
    /// first as synthesizer). `None` when no providers are configured.
    pub fn effective_fusion(&self) -> Option<FusionConfig> {
        if let Some(fusion) = &self.fusion {
            return Some(fusion.clone());
        }
        if self.providers.is_empty() {
            return None;
        }
        let panel: Vec<String> = self
            .providers
            .iter()
            .take(2)
            .map(|p| p.name.clone())
            .collect();
        let synthesizer = panel[0].clone();
        Some(FusionConfig {
            panel,
            synthesizer,
            rounds: default_fusion_rounds(),
        })
    }

    /// Build a [`FusionProvider`](crate::llm::fusion::FusionProvider) from the
    /// effective fusion config, resolving panel/synthesizer names against
    /// `providers`. Errors when no fusion config is resolvable or a referenced
    /// provider name is unknown.
    pub fn build_fusion(&self) -> Result<crate::llm::fusion::FusionProvider> {
        let fusion = self
            .effective_fusion()
            .context("no [fusion] config and no providers to derive one from")?;
        self.build_fusion_from(&fusion)
    }

    /// Build a [`FusionProvider`](crate::llm::fusion::FusionProvider) from a
    /// specific fusion config (used by `/fusion config` before persisting).
    pub fn build_fusion_from(
        &self,
        fusion: &FusionConfig,
    ) -> Result<crate::llm::fusion::FusionProvider> {
        use crate::llm::fusion::{FusionProvider, PanelMember};

        let find = |name: &str| -> Result<&ProviderConfig> {
            self.providers
                .iter()
                .find(|p| p.name == name)
                .with_context(|| format!("fusion references unknown provider '{name}'"))
        };

        let mut panel = Vec::new();
        for name in &fusion.panel {
            let pc = find(name)?;
            panel.push(PanelMember {
                name: pc.name.clone(),
                provider: pc.build()?,
                model: pc.model.clone(),
            });
        }

        let synth_pc = find(&fusion.synthesizer)?;
        let synthesizer = synth_pc.build()?;

        let label = format!(
            "fusion: {} \u{00d7}{}",
            fusion.panel.join("+"),
            fusion.rounds
        );
        FusionProvider::new(
            panel,
            synthesizer,
            synth_pc.model.clone(),
            fusion.rounds,
            label,
        )
    }

    /// The effective ultra config: the explicit `[ultra]` block if set,
    /// otherwise the defaults. Never `None` — unlike
    /// [`effective_fusion`](Self::effective_fusion), ultra resolves no provider
    /// names, it just reuses the active one, so there is nothing to derive and
    /// nothing to fail on.
    pub fn effective_ultra(&self) -> UltraConfig {
        self.ultra.clone().unwrap_or_default()
    }

    /// Build the `/ultra` engine from the effective config.
    pub fn build_ultra(&self) -> Result<crate::agent::ultra::UltraEngine> {
        self.build_ultra_from(&self.effective_ultra())
    }

    /// Build the engine from a specific config — used by `/ultra config` to
    /// validate a selection *before* persisting it, and by `restore_ultra` to
    /// re-arm a rebuilt agent.
    pub fn build_ultra_from(
        &self,
        ultra: &UltraConfig,
    ) -> Result<crate::agent::ultra::UltraEngine> {
        crate::agent::ultra::UltraEngine::build(ultra, &Self::subagents_dir()?)
    }

    /// Index of the effective active provider in [`providers`](Self::providers),
    /// when any are configured.
    fn active_index(&self) -> Option<usize> {
        if self.providers.is_empty() {
            return None;
        }
        Some(
            self.active_provider
                .as_ref()
                .and_then(|name| self.providers.iter().position(|p| &p.name == name))
                .unwrap_or(0),
        )
    }

    /// Apply environment-variable overrides on top of file/default config.
    /// Empty values are ignored.
    fn apply_env(&mut self) {
        self.apply_env_from(|name| std::env::var(name).ok());
    }

    /// Testable core of [`apply_env`]: `lookup` supplies the value of an
    /// environment variable, or `None` when unset.
    ///
    /// `WIZARD_MODEL` overrides the legacy `model` field and, when providers
    /// are explicitly configured, the active provider's model too;
    /// `WIZARD_OLLAMA_HOST` overrides `ollama_host` (used by explicitly
    /// configured Ollama providers); `WIZARD_LLAMACPP_HOST` overrides
    /// `llamacpp_host`; `WIZARD_GGUF_PATH` overrides `gguf_path` and, when
    /// the active provider is llamacpp, its `gguf_path` too;
    /// `WIZARD_CODE_MODE` turns `run_code` on or off for one process.
    fn apply_env_from(&mut self, lookup: impl Fn(&str) -> Option<String>) {
        if let Some(model) = lookup("WIZARD_MODEL")
            && !model.trim().is_empty()
        {
            let model = model.trim().to_string();
            self.model = model.clone();
            if let Some(index) = self.active_index() {
                self.providers[index].model = model;
            }
        }
        if let Some(host) = lookup("WIZARD_OLLAMA_HOST") {
            let host = host.trim().trim_end_matches('/');
            if !host.is_empty() {
                self.ollama_host = host.to_string();
            }
        }
        if let Some(host) = lookup("WIZARD_LLAMACPP_HOST") {
            let host = host.trim().trim_end_matches('/');
            if !host.is_empty() {
                self.llamacpp_host = host.to_string();
            }
        }
        if let Some(raw) = lookup("WIZARD_CODE_MODE") {
            // Both directions, and an unrecognised value changes nothing: an
            // exported `WIZARD_CODE_MODE=maybe` must not silently arm a
            // model-authored interpreter, and must not silently disarm one the
            // user configured on either.
            match raw.trim() {
                "1" | "true" | "yes" => self.code_mode = true,
                "0" | "false" | "no" => self.code_mode = false,
                _ => {}
            }
        }
        if let Some(path) = lookup("WIZARD_GGUF_PATH") {
            let path = path.trim();
            if !path.is_empty() {
                self.gguf_path = Some(path.to_string());
                if let Some(index) = self.active_index()
                    && self.providers[index].kind == ProviderKind::LLAMACPP
                {
                    self.providers[index].gguf_path = Some(path.to_string());
                }
            }
        }
    }

    /// Persist config to `~/.wizard/config.toml`, creating the directory if
    /// needed.
    ///
    /// Through the scratch-file-and-rename primitive, not `fs::write`, which
    /// truncates the target and then fills it. That window is short but this
    /// runs constantly — `/settings`, `/mode`, `/vim`, every provider change,
    /// every onboarding step — and what it truncates is the file wizard needs
    /// to start. A crash, a full disk or a `kill -9` inside it left a
    /// config.toml that does not parse, and the next launch refused to run
    /// until the user found and deleted it by hand. A rename is atomic, so a
    /// reader sees the whole old file or the whole new one.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        crate::platform::secrets::write_atomic(&path, raw.as_bytes())
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Apply CLI flag overrides on top of file/env config for this run.
    /// CLI mode wins; sovereign mode raises a capped `max_steps` to its floor
    /// (an unlimited budget stays unlimited).
    pub fn apply_cli(&mut self, cli: &Cli) {
        if let Some(mode) = cli.mode {
            self.mode = mode;
        }
        if cli.continuous {
            self.mode = Mode::Sovereign;
            self.continuous = true;
        }
        if cli.plan {
            self.plan_first = true;
        }
        if cli.omakase {
            self.omakase = true;
        }
        // Omakase is a flavor of plan mode, so it implies `plan_first` — and it
        // implies it however it was asked for. Doing this after the flag rather
        // than inside it makes `omakase = true` in config.toml, with no
        // `--omakase` and no `--plan`, mean the same thing as the flag: every
        // surface that starts a session in plan mode from `plan_first` starts
        // this one in plan mode too.
        if self.omakase {
            self.plan_first = true;
        }
        self.max_steps = self.max_steps.for_mode(self.mode);
    }
}

#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
