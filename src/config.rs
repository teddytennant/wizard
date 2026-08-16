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
use crate::llm::anthropic::AnthropicProvider;
use crate::llm::cloudflare::{self, CloudflareProvider};
use crate::llm::llamacpp::LlamaCppProvider;
use crate::llm::ollama::OllamaClient;
use crate::llm::openai::{OpenAiProvider, StaticToken};
use crate::llm::openrouter;
use crate::llm::provider::LlmProvider;
use crate::llm::xai_oauth;
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

/// Which backend a [`ProviderConfig`] talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Local llama.cpp `llama-server` (OpenAI-compatible `/v1` API plus the
    /// native `/health` probe). The default local backend.
    LlamaCpp,
    /// Local Ollama server (native `/api/chat`).
    Ollama,
    /// OpenAI-compatible Chat Completions endpoint (OpenAI, OpenRouter, Groq,
    /// together.ai, vLLM, LM Studio, ...).
    Openai,
    /// Anthropic Messages API.
    Anthropic,
    /// OpenRouter's OpenAI-compatible API at `https://openrouter.ai/api/v1`
    /// with a plain API key (default env var `OPENROUTER_API_KEY`).
    OpenRouter,
    /// xAI (Grok) Chat Completions at `https://api.x.ai/v1` with a plain API
    /// key (default env var `XAI_API_KEY`).
    Xai,
    /// xAI via account sign-in: OAuth tokens from `wizard --login xai`
    /// (stored in `~/.wizard/xai_oauth.json`), no API key needed.
    XaiOauth,
    /// ChatGPT subscription via account sign-in: OAuth tokens from
    /// `wizard --login chatgpt` (stored in `~/.wizard/chatgpt_oauth.json`),
    /// calling the Responses API at `chatgpt.com/backend-api/codex`.
    ChatgptOauth,
    /// Cloudflare Workers AI: serverless open models (GLM, Llama, Qwen, ...)
    /// behind an account-scoped OpenAI-compatible endpoint
    /// (`https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1`) with a
    /// Cloudflare API token (default env var `CLOUDFLARE_API_TOKEN`).
    Cloudflare,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderKind::LlamaCpp => write!(f, "llamacpp"),
            ProviderKind::Ollama => write!(f, "ollama"),
            ProviderKind::Openai => write!(f, "openai"),
            ProviderKind::Anthropic => write!(f, "anthropic"),
            ProviderKind::OpenRouter => write!(f, "openrouter"),
            ProviderKind::Xai => write!(f, "xai"),
            ProviderKind::XaiOauth => write!(f, "xaioauth"),
            ProviderKind::ChatgptOauth => write!(f, "chatgptoauth"),
            ProviderKind::Cloudflare => write!(f, "cloudflare"),
        }
    }
}

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
    /// the unrestricted tool set. See [`crate::gateway::is_authorized`].
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
    /// (default **false**). See [`crate::mesh::discovery`] for what mDNS does
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

    /// Construct the concrete client for this provider. For cloud kinds a
    /// missing key is a soft warning (the client is still built so `health()`
    /// can report the real error).
    pub fn build(&self) -> Result<Arc<dyn LlmProvider>> {
        match self.kind {
            ProviderKind::LlamaCpp => Ok(Arc::new(LlamaCppProvider::new(
                self.base_url.clone(),
                self.model.clone(),
            ))),
            ProviderKind::Ollama => Ok(Arc::new(OllamaClient::new(self.base_url.clone()))),
            ProviderKind::Openai => {
                let key = self.resolved_key(None);
                if key.is_empty() {
                    tracing::warn!(
                        "provider '{}' has no API key (store one via /provider or set {}); requests will likely 401",
                        self.name,
                        self.api_key_env.as_deref().unwrap_or("an env var")
                    );
                }
                Ok(Arc::new(OpenAiProvider::new(
                    self.base_url.clone(),
                    self.model.clone(),
                    key,
                )))
            }
            ProviderKind::Anthropic => {
                let key = self.resolved_key(None);
                if key.is_empty() {
                    tracing::warn!(
                        "provider '{}' has no API key (store one via /provider or set {}); requests will likely 401",
                        self.name,
                        self.api_key_env.as_deref().unwrap_or("an env var")
                    );
                }
                Ok(Arc::new(AnthropicProvider::new(
                    self.base_url.clone(),
                    self.model.clone(),
                    key,
                )))
            }
            // OpenRouter speaks the OpenAI-compatible Chat Completions API;
            // the helper adds the attribution headers.
            ProviderKind::OpenRouter => {
                let key = self.resolved_key(Some(openrouter::DEFAULT_KEY_ENV));
                Ok(Arc::new(openrouter::provider(
                    self.base_url.clone(),
                    self.model.clone(),
                    key,
                )))
            }
            // xAI speaks the OpenAI-compatible Chat Completions API; only the
            // credentials differ between the two kinds.
            ProviderKind::Xai => {
                let key = self.resolved_key(Some(xai_oauth::DEFAULT_KEY_ENV));
                Ok(Arc::new(OpenAiProvider::with_token_source(
                    self.base_url.clone(),
                    self.model.clone(),
                    Arc::new(StaticToken::new(key)),
                    "xai",
                )))
            }
            ProviderKind::XaiOauth => {
                let source = xai_oauth::XaiTokenSource::new()
                    .context("setting up xAI OAuth token storage")?;
                Ok(Arc::new(OpenAiProvider::with_token_source(
                    self.base_url.clone(),
                    self.model.clone(),
                    Arc::new(source),
                    "xai",
                )))
            }
            // A ChatGPT subscription is not the Chat Completions API — it is the
            // Responses API behind account tokens, so it has its own client.
            ProviderKind::ChatgptOauth => Ok(Arc::new(
                crate::llm::chatgpt::ChatgptProvider::new(
                    self.base_url.clone(),
                    self.model.clone(),
                )
                .context("setting up ChatGPT OAuth token storage")?,
            )),
            // Cloudflare Workers AI speaks the OpenAI-compatible Chat
            // Completions API, but has no `/v1/models`, so it needs its own
            // client for health/model-listing (see `CloudflareProvider`).
            ProviderKind::Cloudflare => {
                let key = self.resolved_key(Some(cloudflare::DEFAULT_KEY_ENV));
                if key.is_empty() {
                    tracing::warn!(
                        "provider '{}' has no API token (store one via /provider or set {}); requests will likely 401",
                        self.name,
                        self.api_key_env
                            .as_deref()
                            .unwrap_or(cloudflare::DEFAULT_KEY_ENV)
                    );
                }
                Ok(Arc::new(CloudflareProvider::new(
                    self.base_url.clone(),
                    self.model.clone(),
                    key,
                )))
            }
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
            kind: ProviderKind::LlamaCpp,
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
                    && self.providers[index].kind == ProviderKind::LlamaCpp
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
mod tests {
    use clap::Parser;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
            .expect("valid args")
    }

    #[test]
    fn tests_never_write_to_the_real_wizard_dir() {
        // Regression guard, and not a hypothetical one: the suite exercises
        // code that persists config (the TUI's `/vim` toggle, `/mode`,
        // provider setup, onboarding). When this pointed at $HOME, running
        // `cargo test` silently overwrote the developer's own config.toml —
        // providers and all. It did, once.
        let dir = Config::wizard_dir().expect("a wizard dir");
        let home = dirs::home_dir().expect("a home dir");
        assert_ne!(dir, home.join(".wizard"));
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "tests must use a temp wizard dir, got {}",
            dir.display()
        );
    }

    #[test]
    fn defaults_match_docs() {
        let config = Config::default();
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.llamacpp_host, DEFAULT_LLAMACPP_HOST);
        assert!(config.gguf_path.is_none());
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.max_steps, StepBudget::UNLIMITED);
        assert!(!config.continuous);
        assert!(!config.plan_first);
        assert!(!config.plan_each_cycle);
        assert_eq!(config.retry_base_secs, 5);
        assert_eq!(config.retry_max_secs, 300);
        assert_eq!(config.cycle_pause_secs, 0);
        // No gate unless one is asked for: a gate runs commands unattended.
        assert!(config.gates.is_empty());
        assert_eq!(config.gate_max_attempts, 3);
        assert_eq!(config.gate_timeout_secs, 1_800);
        assert_eq!(config.compact_threshold_bytes, 48_000);
        assert!(!config.rollback_failed_cycles);
        assert_eq!(config.max_consecutive_failures, 5);
        assert_eq!(config.checkpoints.keep_turns, 50);
        assert_eq!(config.fleet.max_minutes, 30);
        assert!(config.fleet.synthesize);
    }

    #[test]
    fn checkpoints_section_parses() {
        let config: Config = toml::from_str("[checkpoints]\nkeep_turns = 7").expect("valid toml");
        assert_eq!(config.checkpoints.keep_turns, 7);
        let config: Config = toml::from_str("rollback_failed_cycles = true").expect("valid toml");
        assert!(config.rollback_failed_cycles);
    }

    /// A config written before the knob existed must keep the documented
    /// default rather than deserializing to `0`, which the loop reads as "no
    /// bound at all" — the exact opposite of the safe reading.
    #[test]
    fn max_consecutive_failures_defaults_when_absent_and_zero_is_explicit() {
        let config: Config = toml::from_str("continuous = true").expect("valid toml");
        assert_eq!(config.max_consecutive_failures, 5);
        let config: Config = toml::from_str("max_consecutive_failures = 0").expect("valid toml");
        assert_eq!(config.max_consecutive_failures, 0);
    }

    #[test]
    fn fleet_section_parses_with_partial_keys() {
        let config: Config =
            toml::from_str("[fleet]\nmax_minutes = 10\nsynthesize = false").expect("valid toml");
        assert_eq!(config.fleet.max_minutes, 10);
        assert!(!config.fleet.synthesize);

        let config: Config = toml::from_str("[fleet]\nmax_minutes = 90").expect("valid toml");
        assert_eq!(config.fleet.max_minutes, 90);
        assert!(config.fleet.synthesize, "missing key takes the default");
    }

    #[test]
    fn update_config_defaults() {
        let update = UpdateConfig::default();
        assert!(update.notify);
        assert!(!update.auto);
        assert_eq!(update.repo, "teddytennant/wizard");
        assert_eq!(update.interval_hours, 24);
    }

    #[test]
    fn config_without_update_table_deserializes_to_defaults() {
        // Configs written before `[update]` existed must still parse.
        let config: Config = toml::from_str("model = \"qwen3.6:27b\"").expect("valid toml");
        assert_eq!(config.update, UpdateConfig::default());
    }

    #[test]
    fn update_section_parses_with_partial_keys() {
        let config: Config =
            toml::from_str("[update]\nauto = true\ninterval_hours = 6").expect("valid toml");
        assert!(config.update.auto);
        assert_eq!(config.update.interval_hours, 6);
        // Unspecified keys take their defaults.
        assert!(config.update.notify);
        assert_eq!(config.update.repo, "teddytennant/wizard");

        let config: Config =
            toml::from_str("[update]\nrepo = \"acme/wizard\"\nnotify = false").expect("valid toml");
        assert_eq!(config.update.repo, "acme/wizard");
        assert!(!config.update.notify);
        assert!(!config.update.auto, "missing key takes the default");
    }

    #[test]
    fn mode_parameters() {
        assert_eq!(Mode::Genie.temperature(), 0.8);
        assert_eq!(Mode::Sovereign.temperature(), 0.6);
        assert_eq!(Mode::Genie.to_string(), "genie");
        assert_eq!(Mode::Sovereign.to_string(), "sovereign");
    }

    #[test]
    fn missing_keys_take_defaults() {
        let config: Config = toml::from_str("model = \"qwen3.5:9b\"").expect("valid toml");
        assert_eq!(config.model, "qwen3.5:9b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.max_steps, StepBudget::UNLIMITED);
    }

    #[test]
    fn the_mesh_listener_and_mdns_are_both_off_by_default() {
        // The one thing about `[mesh]` that must not drift. A mesh that opened
        // a socket on install would be a security surface nobody asked for,
        // and an mDNS advertisement broadcasts this machine's public key to
        // every device on the network. Both are opt-in, and this is the test
        // that says so.
        let mesh = MeshConfig::default();
        assert!(!mesh.listen, "the mesh listener is off until somebody asks");
        assert!(!mesh.mdns, "and so is announcing this machine on the LAN");
        assert!(mesh.routes.is_empty());
        assert_eq!(mesh.listen_addr, DEFAULT_MESH_LISTEN_ADDR);
        assert_eq!(Config::default().mesh, mesh);

        // A config file that says nothing about the mesh reads back as off,
        // rather than as whatever a missing field happens to deserialize to.
        let quiet: Config = toml::from_str("model = \"qwen3.6:27b\"").expect("parse");
        assert!(!quiet.mesh.listen);
        assert!(!quiet.mesh.mdns);
        // And a `[mesh]` section that sets something *else* still leaves the
        // listener off: this is the fail-open shape the module keeps warning
        // about, where a field added later defaults to the permissive side.
        let partial: Config = toml::from_str("[mesh]\nmdns = true\n").expect("parse");
        assert!(partial.mesh.mdns);
        assert!(!partial.mesh.listen);
    }

    #[test]
    fn a_malformed_listen_address_is_an_error_rather_than_a_silent_fallback() {
        // Binding the default when somebody typed an address they meant is how
        // a node ends up listening somewhere its operator did not intend.
        let mesh = MeshConfig::default();
        assert_eq!(
            mesh.listen_socket().expect("the default parses").port(),
            DEFAULT_MESH_PORT
        );
        let broken = MeshConfig {
            listen_addr: "0.0.0.0".to_string(),
            ..MeshConfig::default()
        };
        let err = broken.listen_socket().expect_err("no port");
        assert!(format!("{err:#}").contains("host:port"), "{err:#}");
    }

    /// `WIZARD_CODE_MODE` moves in both directions, and an unrecognised value
    /// moves nothing.
    ///
    /// Both halves matter: an exported `WIZARD_CODE_MODE=maybe` must not
    /// silently arm a model-authored interpreter, and must not silently disarm
    /// one the user turned on in `config.toml` either.
    #[test]
    fn the_code_mode_env_override_moves_in_both_directions() {
        let mut config = Config::default();
        assert!(!config.code_mode, "off by default");

        config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| "1".to_string()));
        assert!(config.code_mode);
        config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| " no ".to_string()));
        assert!(!config.code_mode);
        config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| "true".to_string()));
        assert!(config.code_mode);
        config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| "maybe".to_string()));
        assert!(config.code_mode, "an unrecognised value changes nothing");
        config.apply_env_from(|_| None);
        assert!(config.code_mode, "and an unset variable changes nothing");
    }

    #[test]
    fn full_file_round_trips() {
        let original = Config {
            model: "llama3.3:70b".to_string(),
            ollama_host: "http://10.0.0.5:11434".to_string(),
            llamacpp_host: "http://10.0.0.5:8080".to_string(),
            gguf_path: Some("/models/qwen3-8b-q4_k_m.gguf".to_string()),
            mode: Mode::Sovereign,
            reasoning_effort: Some(ReasoningEffort::High),
            max_steps: StepBudget::new(200),
            continuous: true,
            plan_first: true,
            omakase: true,
            plan_each_cycle: true,
            rollback_failed_cycles: true,
            max_consecutive_failures: 9,
            retry_base_secs: 10,
            retry_max_secs: 600,
            cycle_pause_secs: 30,
            gates: vec!["cargo fmt --check".to_string(), "cargo test".to_string()],
            gate_max_attempts: 4,
            gate_timeout_secs: 600,
            compact_threshold_bytes: 96_000,
            providers: vec![ProviderConfig {
                name: "openai".to_string(),
                kind: ProviderKind::Openai,
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o".to_string(),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }],
            active_provider: Some("openai".to_string()),
            gateway: GatewayConfig {
                kind: GatewayKind::Telegram,
                token_env: Some("MY_BOT_TOKEN".to_string()),
                allowed_chat_ids: vec![42, -100123],
            },
            ui: UiConfig {
                spinner_verbs: vec!["Pondering".to_string(), "Musing".to_string()],
                vim: true,
                skin: Some("codex".to_string()),
            },
            web: WebConfig {
                fetch_max_bytes: 250_000,
                allow_local: true,
                search_backend: "brave".to_string(),
                search_api_key_env: Some("BRAVE_API_KEY".to_string()),
                search_model: Some("grok-4.6".to_string()),
            },
            checkpoints: CheckpointConfig { keep_turns: 12 },
            fleet: FleetConfig {
                max_minutes: 45,
                synthesize: false,
            },
            update: UpdateConfig {
                notify: false,
                auto: true,
                repo: "acme/wizard".to_string(),
                interval_hours: 6,
            },
            sync: SyncConfig {
                source: Some("https://example.com/wizard-sync.tar.gz".to_string()),
            },
            mesh: MeshConfig {
                listen: true,
                listen_addr: "127.0.0.1:4300".to_string(),
                mdns: true,
                routes: BTreeMap::from([(
                    "wiz1AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
                    "10.0.0.9:4242".to_string(),
                )]),
            },
            fusion: Some(FusionConfig {
                panel: vec!["openai".to_string()],
                synthesizer: "openai".to_string(),
                rounds: 2,
            }),
            ultra: Some(UltraConfig {
                lenses: vec!["skeptic".to_string(), "minimalist".to_string()],
                judges: 2,
                candidate_max_steps: 8,
                judge_max_steps: 4,
                timeout_secs: 120,
                max_draft_chars: 4_000,
            }),
            code_mode: true,
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.model, original.model);
        assert_eq!(parsed.ollama_host, original.ollama_host);
        assert_eq!(parsed.llamacpp_host, original.llamacpp_host);
        assert_eq!(parsed.gguf_path, original.gguf_path);
        assert_eq!(parsed.mode, original.mode);
        assert_eq!(parsed.reasoning_effort, original.reasoning_effort);
        assert_eq!(parsed.max_steps, original.max_steps);
        assert_eq!(parsed.continuous, original.continuous);
        assert_eq!(parsed.plan_first, original.plan_first);
        assert_eq!(parsed.plan_each_cycle, original.plan_each_cycle);
        assert_eq!(parsed.retry_base_secs, original.retry_base_secs);
        assert_eq!(parsed.retry_max_secs, original.retry_max_secs);
        assert_eq!(parsed.cycle_pause_secs, original.cycle_pause_secs);
        assert_eq!(parsed.gates, original.gates);
        assert_eq!(parsed.gate_max_attempts, original.gate_max_attempts);
        assert_eq!(parsed.gate_timeout_secs, original.gate_timeout_secs);
        assert_eq!(
            parsed.compact_threshold_bytes,
            original.compact_threshold_bytes
        );
        assert_eq!(parsed.code_mode, original.code_mode);
        assert_eq!(parsed.providers.len(), 1);
        assert_eq!(parsed.providers[0].name, "openai");
        assert_eq!(parsed.providers[0].kind, ProviderKind::Openai);
        assert_eq!(
            parsed.providers[0].api_key_env.as_deref(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(parsed.active_provider.as_deref(), Some("openai"));
        assert_eq!(parsed.gateway.kind, GatewayKind::Telegram);
        assert_eq!(parsed.gateway.token_env.as_deref(), Some("MY_BOT_TOKEN"));
        assert_eq!(parsed.gateway.allowed_chat_ids, vec![42, -100123]);
        assert_eq!(parsed.ui, original.ui);
        assert_eq!(parsed.web, original.web);
        assert_eq!(
            parsed.rollback_failed_cycles,
            original.rollback_failed_cycles
        );
        assert_eq!(
            parsed.max_consecutive_failures,
            original.max_consecutive_failures
        );
        assert_eq!(parsed.checkpoints, original.checkpoints);
        assert_eq!(parsed.fleet, original.fleet);
        assert_eq!(parsed.update, original.update);
        assert_eq!(parsed.sync, original.sync);
        assert_eq!(parsed.fusion, original.fusion);
        assert_eq!(parsed.ultra, original.ultra);
    }

    #[test]
    fn ultra_defaults_when_section_missing() {
        let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
        assert!(config.ultra.is_none());
        assert_eq!(config.effective_ultra(), UltraConfig::default());

        // A partial block fills the rest from the defaults, so adding a knob to
        // `[ultra]` never invalidates a config that predates it.
        let config: Config = toml::from_str("[ultra]\njudges = 0").expect("valid toml");
        let ultra = config.effective_ultra();
        assert_eq!(ultra.judges, 0);
        assert_eq!(ultra.lenses, UltraConfig::default().lenses);
        assert_eq!(
            ultra.candidate_max_steps,
            UltraConfig::default().candidate_max_steps
        );
        assert_eq!(ultra.timeout_secs, UltraConfig::default().timeout_secs);
    }

    #[test]
    fn sync_defaults_when_section_missing() {
        let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
        assert_eq!(config.sync, SyncConfig::default());
        assert!(config.sync.source.is_none());

        let config: Config =
            toml::from_str("[sync]\nsource = \"~/bundles/w.tar.gz\"").expect("valid toml");
        assert_eq!(config.sync.source.as_deref(), Some("~/bundles/w.tar.gz"));
    }

    #[test]
    fn web_defaults_when_section_missing() {
        let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
        assert_eq!(config.web, WebConfig::default());
        assert_eq!(config.web.fetch_max_bytes, 100_000);
        assert!(!config.web.allow_local);
        assert_eq!(config.web.search_backend, "duckduckgo");
        assert!(config.web.search_api_key_env.is_none());
    }

    #[test]
    fn web_section_parses_partial_keys() {
        let config: Config = toml::from_str(
            "[web]\nsearch_backend = \"tavily\"\nsearch_api_key_env = \"TAVILY_API_KEY\"",
        )
        .expect("valid toml");
        assert_eq!(config.web.search_backend, "tavily");
        assert_eq!(
            config.web.search_api_key_env.as_deref(),
            Some("TAVILY_API_KEY")
        );
        assert_eq!(config.web.fetch_max_bytes, 100_000, "missing keys default");
    }

    #[test]
    fn spinner_verbs_default_when_section_missing() {
        let config: Config = toml::from_str("model = \"qwen3.5:9b\"").expect("valid toml");
        assert!(config.ui.spinner_verbs.is_empty());
        for seed in 0..64 {
            let verb = config.ui.spinner_verb(seed);
            assert!(UiConfig::DEFAULT_SPINNER_VERBS.contains(&verb));
        }
    }

    #[test]
    fn spinner_verbs_default_when_list_empty() {
        let config: Config = toml::from_str("[ui]\nspinner_verbs = []").expect("valid toml");
        assert!(config.ui.spinner_verbs.is_empty());
        assert!(UiConfig::DEFAULT_SPINNER_VERBS.contains(&config.ui.spinner_verb(7)));
    }

    #[test]
    fn spinner_verbs_custom_list_replaces_defaults() {
        let config: Config = toml::from_str("[ui]\nspinner_verbs = [\"Pondering\", \"Musing\"]")
            .expect("valid toml");
        assert_eq!(config.ui.spinner_verbs, vec!["Pondering", "Musing"]);
        for seed in 0..64 {
            let verb = config.ui.spinner_verb(seed);
            assert!(verb == "Pondering" || verb == "Musing");
        }
    }

    #[test]
    fn spinner_verb_is_deterministic_per_seed_and_varies_across_seeds() {
        let ui = UiConfig::default();
        assert_eq!(ui.spinner_verb(42), ui.spinner_verb(42));
        // The hash must not collapse every seed onto one verb.
        let first = ui.spinner_verb(0);
        assert!((1..64).any(|seed| ui.spinner_verb(seed) != first));
    }

    #[test]
    fn gateway_defaults_to_none_and_round_trips() {
        // A config without a [gateway] table defaults to None / terminal only.
        let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
        assert_eq!(config.gateway.kind, GatewayKind::None);
        assert!(config.gateway.token_env.is_none());
        assert!(config.gateway.allowed_chat_ids.is_empty());
        assert_eq!(config.gateway.token_env(), GatewayConfig::DEFAULT_TOKEN_ENV);

        // A Telegram gateway round-trips through TOML.
        let raw = toml::to_string_pretty(&Config {
            gateway: GatewayConfig {
                kind: GatewayKind::Telegram,
                token_env: None,
                allowed_chat_ids: vec![7],
            },
            ..Config::default()
        })
        .expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.gateway.kind, GatewayKind::Telegram);
        assert_eq!(parsed.gateway.allowed_chat_ids, vec![7]);
    }

    /// Adversarial: onboarding now pastes the key instead of naming an env
    /// var, so the stored key must be what the provider actually reads back,
    /// and must still lose to an exported variable (the documented override).
    ///
    /// Both sources are injected rather than real. Under `cfg(test)`,
    /// [`Config::wizard_dir`] is one directory for the whole process, so
    /// `credentials.toml` is a single file that several other tests in this
    /// binary (`gui::settings`, `app`) write concurrently
    /// through `credentials::store`, which is a read-modify-write. A test that
    /// stored a key there and read it back could lose its entry to an
    /// interleaved writer and fail for reasons that have nothing to do with
    /// precedence. The on-disk half of the contract is covered where it
    /// belongs and without sharing: the `store_get_remove_round_trip` and
    /// `stored_file_is_0600` tests in `crate::credentials` both run against a
    /// tempdir of their own.
    #[test]
    fn the_env_var_wins_over_a_stored_provider_key() {
        let provider = ProviderConfig {
            name: "test-key-precedence".to_string(),
            kind: ProviderKind::Openai,
            base_url: "https://example.invalid/v1".to_string(),
            model: "m".to_string(),
            api_key_env: Some("WIZARD_TEST_KEY_PRECEDENCE".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        // Stand-ins for the process environment and for credentials.toml:
        // this test neither depends on nor disturbs either.
        fn env_is(value: &'static str) -> impl Fn(&str) -> Option<String> {
            move |name: &str| (name == "WIZARD_TEST_KEY_PRECEDENCE").then(|| value.to_string())
        }
        fn stored_is(value: &'static str) -> impl Fn(&str) -> Option<String> {
            move |name: &str| (name == "test-key-precedence").then(|| value.to_string())
        }
        let nothing = |_: &str| None;

        // Neither stored nor exported: no key at all. This is the state
        // onboarding used to leave behind, and it 401s on the first turn.
        assert_eq!(provider.resolved_key_from(None, nothing, nothing), "");

        // Paste-and-store, exactly as onboarding does: with no variable
        // exported, the stored key is what goes out.
        assert_eq!(
            provider.resolved_key_from(None, nothing, stored_is("sk-pasted\n")),
            "sk-pasted",
            "a key pasted with a trailing newline still works"
        );

        // The env var overrides the stored key, trailing newline and all
        // (`export KEY=$(cat file)`).
        assert_eq!(
            provider.resolved_key_from(None, env_is("sk-exported\n"), stored_is("sk-pasted")),
            "sk-exported"
        );
        // …but an empty or blank export is not an override.
        assert_eq!(
            provider.resolved_key_from(None, env_is("   "), stored_is("sk-pasted")),
            "sk-pasted",
            "a blank env var must not blank out the stored key"
        );
        // A different provider's stored key is not this provider's key.
        assert_eq!(
            provider.resolved_key_from(None, nothing, |name: &str| (name == "someone-else")
                .then(|| "sk-theirs".to_string())),
            ""
        );
        // A provider with no `api_key_env` still honors the backend default.
        let defaulted = ProviderConfig {
            api_key_env: None,
            ..provider.clone()
        };
        assert_eq!(
            defaulted.resolved_key_from(
                Some("WIZARD_TEST_KEY_PRECEDENCE"),
                env_is("sk-default"),
                nothing
            ),
            "sk-default"
        );
    }

    /// `~/.wizard` holds session JSONLs (full tool output), logs and
    /// credentials. Every directory `ensure_dirs` creates must be private the
    /// moment it exists, not only once some credential writer happens to
    /// tighten it.
    ///
    /// The mode itself, and the fact that a pre-existing loose directory is
    /// tightened rather than left alone, belong to
    /// [`crate::platform::secrets`] and are asserted there (exactly 0700, plus
    /// the exFAT/CIFS case where the chmod cannot work at all). What config
    /// owns, and what this covers, is the *set* of directories: a new one
    /// added to `ensure_dirs` and not created privately is the regression.
    #[test]
    fn state_dirs_are_created_private() {
        Config::ensure_dirs().expect("ensure_dirs");
        for dir in [
            Config::wizard_dir().expect("wizard dir"),
            Config::sessions_dir().expect("sessions dir"),
            Config::logs_dir().expect("logs dir"),
            Config::wizard_dir().expect("wizard dir").join("running"),
        ] {
            assert!(
                crate::platform::secrets::is_protected(&dir).expect("stat"),
                "{} must not be readable by other users",
                dir.display()
            );
        }
    }

    #[test]
    fn legacy_ollama_config_synthesizes_llamacpp() {
        // A file with only model/ollama_host (no providers table) still
        // parses, but the synthesized local provider is llama.cpp — Ollama
        // is opt-in via an explicit [[providers]] entry.
        let config =
            Config::from_toml("model = \"qwen3.5:9b\"\nollama_host = \"http://10.0.0.5:11434\"")
                .expect("valid toml");
        assert!(config.providers.is_empty());
        let active = config.active();
        assert_eq!(active.name, "local");
        assert_eq!(active.kind, ProviderKind::LlamaCpp);
        assert_eq!(active.base_url, DEFAULT_LLAMACPP_HOST);
        assert_eq!(active.model, "qwen3.5:9b");
        assert!(active.api_key_env.is_none());
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
    }

    #[test]
    fn fresh_default_synthesizes_llamacpp() {
        // No config file at all: the synthesized provider is llama.cpp.
        let config = Config::default();
        let active = config.active();
        assert_eq!(active.name, "local");
        assert_eq!(active.kind, ProviderKind::LlamaCpp);
        assert_eq!(active.base_url, DEFAULT_LLAMACPP_HOST);
        assert_eq!(active.model, "qwen3.6:27b");
        assert!(active.api_key_env.is_none());
        assert!(active.gguf_path.is_none());

        // An empty file is equivalent to no file.
        let config = Config::from_toml("").expect("valid toml");
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn saved_default_config_stays_llamacpp_on_reload() {
        // save() writes every field, including ollama_host — its presence
        // must not change the synthesized llama.cpp default.
        let raw = toml::to_string_pretty(&Config::default()).expect("serialize");
        assert!(raw.contains("ollama_host"), "save writes legacy fields");
        let config = Config::from_toml(&raw).expect("parse back");
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn llamacpp_provider_round_trips_through_toml() {
        let original = Config {
            providers: vec![ProviderConfig {
                name: "local".to_string(),
                kind: ProviderKind::LlamaCpp,
                base_url: "http://127.0.0.1:8080".to_string(),
                model: "qwen3-8b".to_string(),
                api_key_env: None,
                gguf_path: Some("/home/u/.wizard/models/qwen3-8b-q4_k_m.gguf".to_string()),
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        assert!(raw.contains("kind = \"llamacpp\""), "raw: {raw}");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.providers.len(), 1);
        assert_eq!(parsed.providers[0].kind, ProviderKind::LlamaCpp);
        assert_eq!(
            parsed.providers[0].gguf_path.as_deref(),
            Some("/home/u/.wizard/models/qwen3-8b-q4_k_m.gguf")
        );
        assert!(parsed.providers[0].api_key_env.is_none());
        assert_eq!(parsed.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn xai_kinds_round_trip_through_toml() {
        let original = Config {
            providers: vec![
                ProviderConfig {
                    name: "xai".to_string(),
                    kind: ProviderKind::Xai,
                    base_url: "https://api.x.ai/v1".to_string(),
                    model: "grok-4.3".to_string(),
                    api_key_env: Some("XAI_API_KEY".to_string()),
                    gguf_path: None,
                    usd_per_mtok_in: None,
                    usd_per_mtok_out: None,
                },
                ProviderConfig {
                    name: "xai-account".to_string(),
                    kind: ProviderKind::XaiOauth,
                    base_url: "https://api.x.ai/v1".to_string(),
                    model: "grok-4.3".to_string(),
                    api_key_env: None,
                    gguf_path: None,
                    usd_per_mtok_in: None,
                    usd_per_mtok_out: None,
                },
            ],
            active_provider: Some("xai-account".to_string()),
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        // The serde names are what the /provider parser and Display use.
        assert!(raw.contains("kind = \"xai\""), "raw: {raw}");
        assert!(raw.contains("kind = \"xaioauth\""), "raw: {raw}");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.providers[0].kind, ProviderKind::Xai);
        assert_eq!(
            parsed.providers[0].api_key_env.as_deref(),
            Some("XAI_API_KEY")
        );
        assert_eq!(parsed.providers[1].kind, ProviderKind::XaiOauth);
        assert!(parsed.providers[1].api_key_env.is_none());
        assert_eq!(parsed.active().kind, ProviderKind::XaiOauth);
    }

    #[test]
    fn openrouter_kind_round_trips_through_toml() {
        let original = Config {
            providers: vec![ProviderConfig {
                name: "openrouter".to_string(),
                kind: ProviderKind::OpenRouter,
                base_url: "https://openrouter.ai/api/v1".to_string(),
                model: "openrouter/auto".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }],
            active_provider: Some("openrouter".to_string()),
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        // The serde name is what the /provider parser and Display use.
        assert!(raw.contains("kind = \"openrouter\""), "raw: {raw}");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.providers[0].kind, ProviderKind::OpenRouter);
        assert_eq!(
            parsed.providers[0].api_key_env.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(parsed.active().kind, ProviderKind::OpenRouter);
    }

    #[test]
    fn cloudflare_kind_round_trips_through_toml() {
        let original = Config {
            providers: vec![ProviderConfig {
                name: "cloudflare".to_string(),
                kind: ProviderKind::Cloudflare,
                base_url: "https://api.cloudflare.com/client/v4/accounts/acc123/ai/v1".to_string(),
                model: "@cf/zai-org/glm-5.2".to_string(),
                api_key_env: Some("CLOUDFLARE_API_TOKEN".to_string()),
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }],
            active_provider: Some("cloudflare".to_string()),
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        // The serde name is what the /provider parser and Display use.
        assert!(raw.contains("kind = \"cloudflare\""), "raw: {raw}");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.providers[0].kind, ProviderKind::Cloudflare);
        assert_eq!(parsed.providers[0].model, "@cf/zai-org/glm-5.2");
        assert_eq!(
            parsed.providers[0].api_key_env.as_deref(),
            Some("CLOUDFLARE_API_TOKEN")
        );
        assert_eq!(parsed.active().kind, ProviderKind::Cloudflare);

        // build() dispatches to the Cloudflare client (labeled by vendor+model),
        // proving the wiring from config to provider.
        let client = parsed.active().build().expect("builds a cloudflare client");
        assert_eq!(client.label(), "cloudflare:@cf/zai-org/glm-5.2");
    }

    #[test]
    fn provider_cost_rates_parse_and_round_trip() {
        let raw = "\
[[providers]]
name = \"claude\"
kind = \"anthropic\"
base_url = \"https://api.anthropic.com\"
model = \"claude-fable-5\"
api_key_env = \"ANTHROPIC_API_KEY\"
usd_per_mtok_in = 3.0
usd_per_mtok_out = 15.0
";
        let config: Config = toml::from_str(raw).expect("valid toml");
        let provider = &config.providers[0];
        assert_eq!(provider.usd_per_mtok_in, Some(3.0));
        assert_eq!(provider.usd_per_mtok_out, Some(15.0));

        let serialized = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&serialized).expect("parse back");
        assert_eq!(parsed.providers[0].usd_per_mtok_in, Some(3.0));
        assert_eq!(parsed.providers[0].usd_per_mtok_out, Some(15.0));

        // Unset rates stay absent on the wire.
        let bare: Config = toml::from_str("model = \"m\"").expect("valid toml");
        assert_eq!(bare.active().usd_per_mtok_in, None);
        let serialized = toml::to_string_pretty(&bare).expect("serialize");
        assert!(!serialized.contains("usd_per_mtok"), "{serialized}");
    }

    #[test]
    fn provider_kind_display_matches_serde_names() {
        for (kind, name) in [
            (ProviderKind::LlamaCpp, "llamacpp"),
            (ProviderKind::Ollama, "ollama"),
            (ProviderKind::Openai, "openai"),
            (ProviderKind::Anthropic, "anthropic"),
            (ProviderKind::OpenRouter, "openrouter"),
            (ProviderKind::Xai, "xai"),
            (ProviderKind::XaiOauth, "xaioauth"),
            (ProviderKind::Cloudflare, "cloudflare"),
        ] {
            assert_eq!(kind.to_string(), name);
            let json = serde_json::to_value(kind).expect("serialize kind");
            assert_eq!(
                json,
                serde_json::json!(name),
                "Display and serde must agree"
            );
        }
    }

    #[test]
    fn active_selects_by_name_and_falls_back_to_first() {
        let providers = vec![
            ProviderConfig {
                name: "local".to_string(),
                kind: ProviderKind::Ollama,
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen3.6:27b".to_string(),
                api_key_env: None,
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            },
            ProviderConfig {
                name: "claude".to_string(),
                kind: ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com".to_string(),
                model: "claude-fable-5".to_string(),
                api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            },
        ];

        // Explicit selection by name.
        let config = Config {
            providers: providers.clone(),
            active_provider: Some("claude".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active().name, "claude");
        assert_eq!(config.active().kind, ProviderKind::Anthropic);

        // Unset active_provider falls back to the first.
        let config = Config {
            providers: providers.clone(),
            active_provider: None,
            ..Config::default()
        };
        assert_eq!(config.active().name, "local");

        // Unknown active_provider also falls back to the first.
        let config = Config {
            providers,
            active_provider: Some("missing".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active().name, "local");
    }

    #[test]
    fn active_provider_mismatch_flags_unknown_names_only() {
        let provider = ProviderConfig {
            name: "local".to_string(),
            kind: ProviderKind::LlamaCpp,
            base_url: DEFAULT_LLAMACPP_HOST.to_string(),
            model: "qwen3.6:27b".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };

        // Resolving name / unset name: no mismatch.
        let config = Config {
            providers: vec![provider.clone()],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active_provider_mismatch(), None);
        let config = Config {
            providers: vec![provider.clone()],
            active_provider: None,
            ..Config::default()
        };
        assert_eq!(config.active_provider_mismatch(), None);

        // Unknown name (typo / removed provider): flagged.
        let config = Config {
            providers: vec![provider],
            active_provider: Some("claud".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active_provider_mismatch().as_deref(), Some("claud"));

        // A named provider with no providers configured is also a mismatch —
        // the synthesized local default runs instead.
        let config = Config {
            active_provider: Some("ghost".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active_provider_mismatch().as_deref(), Some("ghost"));
    }

    #[test]
    fn env_model_overrides_active_provider_when_configured() {
        let mut config = Config {
            providers: vec![ProviderConfig {
                name: "openai".to_string(),
                kind: ProviderKind::Openai,
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o".to_string(),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }],
            active_provider: Some("openai".to_string()),
            ..Config::default()
        };
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("gpt-4o-mini".to_string()),
            _ => None,
        });
        assert_eq!(config.active().model, "gpt-4o-mini");
        assert_eq!(config.model, "gpt-4o-mini", "legacy field also updated");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: Config =
            toml::from_str("model = \"m\"\nfuture_option = true").expect("valid toml");
        assert_eq!(config.model, "m");
    }

    #[test]
    fn env_overrides_model_and_host() {
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("  llama3.3:70b  ".to_string()),
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434///".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "llama3.3:70b", "model is trimmed");
        assert_eq!(
            config.ollama_host, "http://10.0.0.5:11434",
            "host trailing slashes are trimmed"
        );
    }

    #[test]
    fn env_ollama_host_does_not_change_synthesized_kind() {
        // The env var updates the field (for explicitly configured Ollama
        // providers) but the synthesized local provider stays llama.cpp.
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
            _ => None,
        });
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn env_llamacpp_host_overrides_synthesized_base_url() {
        let mut config = Config::from_toml("model = \"qwen3.5:9b\"").expect("valid toml");
        config.apply_env_from(|name| match name {
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
            "WIZARD_LLAMACPP_HOST" => Some("http://10.0.0.5:8080///".to_string()),
            _ => None,
        });
        let active = config.active();
        assert_eq!(active.kind, ProviderKind::LlamaCpp);
        assert_eq!(
            active.base_url, "http://10.0.0.5:8080",
            "host trailing slashes are trimmed"
        );
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
    }

    #[test]
    fn env_gguf_path_feeds_synthesized_and_active_llamacpp_provider() {
        // Synthesized provider picks up the path.
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_GGUF_PATH" => Some("  /models/a.gguf  ".to_string()),
            _ => None,
        });
        assert_eq!(config.gguf_path.as_deref(), Some("/models/a.gguf"));
        assert_eq!(config.active().gguf_path.as_deref(), Some("/models/a.gguf"));

        // An explicitly configured active llamacpp provider is updated too;
        // other kinds are left alone.
        let mut config = Config {
            providers: vec![ProviderConfig {
                name: "local".to_string(),
                kind: ProviderKind::LlamaCpp,
                base_url: "http://127.0.0.1:8080".to_string(),
                model: "qwen3-8b".to_string(),
                api_key_env: None,
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        config.apply_env_from(|name| match name {
            "WIZARD_GGUF_PATH" => Some("/models/b.gguf".to_string()),
            _ => None,
        });
        assert_eq!(config.active().gguf_path.as_deref(), Some("/models/b.gguf"));
    }

    #[test]
    fn env_unset_keeps_existing_values() {
        let mut config = Config::default();
        config.apply_env_from(|_| None);
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.llamacpp_host, DEFAULT_LLAMACPP_HOST);
        assert!(config.gguf_path.is_none());
    }

    #[test]
    fn env_empty_values_are_ignored() {
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("   ".to_string()),
            "WIZARD_OLLAMA_HOST" => Some("".to_string()),
            "WIZARD_LLAMACPP_HOST" => Some("  ".to_string()),
            "WIZARD_GGUF_PATH" => Some("".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.llamacpp_host, DEFAULT_LLAMACPP_HOST);
        assert!(config.gguf_path.is_none());
        assert_eq!(
            config.active().kind,
            ProviderKind::LlamaCpp,
            "empty env values do not opt into Ollama"
        );
    }

    #[test]
    fn cli_mode_overrides_config() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.mode, Mode::Sovereign);
        assert_eq!(
            config.max_steps,
            StepBudget::UNLIMITED,
            "sovereign does not cap an unlimited budget"
        );
    }

    #[test]
    fn plan_flag_sets_plan_first() {
        let mut config = Config::default();
        assert!(!config.plan_first);
        assert!(!config.plan_each_cycle);
        config.apply_cli(&cli(&["--plan"]));
        assert!(config.plan_first);
        assert!(!config.plan_each_cycle, "--plan never affects cycles");

        // The flag only sets, never clears, the config value.
        let mut config = Config {
            plan_first: true,
            ..Config::default()
        };
        config.apply_cli(&cli(&[]));
        assert!(config.plan_first);
    }

    #[test]
    fn continuous_flag_forces_sovereign() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--continuous"]));
        assert_eq!(config.mode, Mode::Sovereign);
        assert!(config.continuous);
        assert_eq!(config.max_steps, StepBudget::UNLIMITED);
    }

    #[test]
    fn sovereign_keeps_explicitly_higher_max_steps() {
        let mut config = Config {
            max_steps: StepBudget::new(250),
            ..Config::default()
        };
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.max_steps, StepBudget::new(250));
    }

    #[test]
    fn sovereign_raises_a_capped_budget_to_its_floor() {
        let mut config = Config {
            max_steps: StepBudget::new(25),
            ..Config::default()
        };
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.max_steps, StepBudget::new(100));
    }

    #[test]
    fn step_budget_zero_is_unlimited() {
        let unlimited = StepBudget::new(0);
        assert_eq!(unlimited, StepBudget::UNLIMITED);
        assert_eq!(unlimited, StepBudget::default());
        assert_eq!(unlimited.cap(), None);
        assert_eq!(unlimited.last_step(), u32::MAX);
        assert_eq!(unlimited.to_string(), "no step limit");
        // Unattended posture never shrinks an unlimited budget.
        assert_eq!(unlimited.for_mode(Mode::Sovereign), StepBudget::UNLIMITED);

        let capped = StepBudget::new(25);
        assert_eq!(capped.cap(), Some(25));
        assert_eq!(capped.last_step(), 25);
        assert_eq!(capped.to_string(), "25 steps");
        assert_eq!(capped.for_mode(Mode::Genie), capped);
    }

    #[test]
    fn step_budget_is_a_bare_integer_in_toml() {
        let config: Config = toml::from_str("max_steps = 7").expect("valid toml");
        assert_eq!(config.max_steps, StepBudget::new(7));
        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(raw.contains("max_steps = 7"), "{raw}");

        let config: Config = toml::from_str("max_steps = 0").expect("valid toml");
        assert!(config.max_steps.cap().is_none(), "0 opts out of the limit");
    }

    #[test]
    fn unknown_keys_are_ignored_and_not_written_back() {
        // Old configs carried an `auto_approve` key for the since-removed
        // approval gate. Unknown keys must still load (no `deny_unknown_fields`)
        // and never reappear on re-serialization.
        let config: Config = toml::from_str("auto_approve = false").expect("old key parses");
        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            !raw.contains("auto_approve"),
            "deprecated key is not written back: {raw}"
        );
    }

    #[test]
    fn a_legacy_gui_step_budget_still_loads() {
        // The GUI used to keep a budget of its own (`[gui] max_steps`). It now
        // runs on the shared one like every other surface, and a config still
        // carrying the old section must load — not fail — and not gain it back.
        let config: Config =
            toml::from_str("max_steps = 12\n[gui]\nmax_steps = 250\n").expect("old section parses");
        assert_eq!(config.max_steps, StepBudget::new(12));
        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            !raw.contains("[gui]"),
            "the section is not written back: {raw}"
        );
    }

    #[test]
    fn no_flags_leaves_config_untouched() {
        let mut config = Config::default();
        config.apply_cli(&cli(&[]));
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.max_steps, StepBudget::UNLIMITED);
    }

    #[test]
    fn config_sovereign_mode_raises_a_capped_budget_without_flags() {
        let mut config = Config {
            mode: Mode::Sovereign,
            max_steps: StepBudget::new(10),
            ..Config::default()
        };
        config.apply_cli(&cli(&[]));
        assert_eq!(config.max_steps, StepBudget::new(100));
    }
}
