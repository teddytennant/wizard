//! Sign in with an xAI account: OAuth 2.0 Authorization Code + PKCE against
//! `auth.x.ai`, then plain `Bearer` access tokens against the
//! OpenAI-compatible Chat Completions API at `https://api.x.ai/v1`.
//!
//! The wire protocol is handled by [`super::wire::OpenAiProvider`]; this
//! module only supplies the credentials:
//! - [`login`] runs the interactive browser flow (`wizard --login xai` or the
//!   `/login xai` slash command) and stores the tokens in
//!   `~/.wizard/xai_oauth.json` (file 0600, directory 0700). A session already
//!   on disk is left alone; `/login xai force` (or deleting the file) starts a
//!   new one.
//! - [`XaiTokenSource`] implements [`TokenSource`]: it reads the stored
//!   tokens, proactively refreshes the access token when its JWT `exp` is
//!   within 120 s, and force-refreshes once after an API 401. Refresh is
//!   locked across processes so a second Wizard cannot spend a grant the first
//!   one just rotated and then delete the file.
//!
//! Tokens never go into `config.toml`; keys live in env vars or dedicated
//! files only.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::oauth_callback::{self, Callback, Cancel, PasteChannel};
use super::wire::TokenSource;
use crate::config::Config;

/// OpenID Connect discovery document for xAI accounts.
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// Public OAuth client id (the upstream Grok-CLI client; no secret).
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// Scopes: identity, refresh tokens, and API access.
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// The loopback callback registered for [`CLIENT_ID`]. Fixed, not preferred:
/// xAI only redirects the browser to the address it has on file, so this is the
/// single port the flow can work on.
const CALLBACK_PORT: u16 = 56121;
/// The path half of that registered callback.
const CALLBACK_PATH: &str = "/callback";
/// Refresh the access token when it expires within this many seconds.
const EXPIRY_LEEWAY_SECS: i64 = 120;

/// Default Chat Completions base URL for both xAI provider kinds.
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
/// Default model for both xAI provider kinds.
pub const DEFAULT_MODEL: &str = "grok-4.6";
/// Default env var holding a plain xAI API key (`kind = "xai"`).
pub const DEFAULT_KEY_ENV: &str = "XAI_API_KEY";

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// A PKCE verifier/challenge pair (RFC 7636, S256).
#[derive(Debug)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE pair: the verifier is base64url(64 random bytes) without
/// padding, capped at the RFC maximum of 128 chars; the challenge is
/// base64url(sha256(verifier)) without padding.
pub fn generate_pkce() -> Result<Pkce> {
    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("gathering PKCE randomness: {err}"))?;
    let mut verifier = URL_SAFE_NO_PAD.encode(bytes);
    verifier.truncate(128);
    let challenge = pkce_challenge(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// S256 challenge for a verifier: base64url(sha256(verifier)), no padding.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `n` random bytes as lowercase hex (used for `state` and `nonce`).
fn random_hex(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("gathering randomness: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// JWT expiry
// ---------------------------------------------------------------------------

/// The `exp` claim of a JWT, or `None` when the token is not a parseable JWT.
pub fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_i64()
}

/// True when `token` expires at or before `now + EXPIRY_LEEWAY_SECS`.
/// A token without a readable `exp` is treated as live (the API's 401 path
/// then forces a refresh).
fn expires_soon_at(token: &str, now: i64) -> bool {
    match jwt_exp(token) {
        Some(exp) => exp <= now + EXPIRY_LEEWAY_SECS,
        None => false,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Endpoint validation
// ---------------------------------------------------------------------------

/// Require an HTTPS URL on `x.ai` or a subdomain of it. The token endpoint is
/// cached on disk and receives refresh tokens, so it is pinned to xAI hosts
/// both at discovery time and again before every use.
fn validate_xai_https(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid endpoint URL {url}"))?;
    ensure!(
        parsed.scheme() == "https",
        "endpoint {url} is not HTTPS; refusing to send credentials"
    );
    let host = parsed.host_str().unwrap_or_default();
    ensure!(
        host == "x.ai" || host.ends_with(".x.ai"),
        "endpoint {url} is not on x.ai; refusing to send credentials"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

/// Where the OpenID configuration is fetched from: [`DISCOVERY_URL`], always.
#[cfg(not(test))]
fn discovery_url() -> String {
    DISCOVERY_URL.to_string()
}

/// Under test, a caller may point discovery at a loopback stub instead — see
/// [`use_test_discovery_url`]. The endpoints the stub names are still pinned to
/// x.ai by [`validate_xai_https`], so the seam cannot smuggle a token endpoint
/// past the check.
#[cfg(test)]
fn discovery_url() -> String {
    TEST_DISCOVERY_URL
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
        .unwrap_or_else(|| DISCOVERY_URL.to_string())
}

#[cfg(test)]
static TEST_DISCOVERY_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Serve xAI's OpenID discovery from `url` for the rest of the process, so a
/// test can exercise the sign-in flow offline rather than reaching `auth.x.ai`.
///
/// Process-wide, like the flows it stands in for: a test that sets it takes
/// [`oauth_callback::serial_callback_port`] first, which is the same lock the
/// one fixed callback port already forces it to hold.
///
/// The only caller is `src/gui/oauth.rs`, which lives behind `--features
/// native`: a default-feature test build compiles this seam and exercises none
/// of it, which is not the same thing as it being unused.
#[cfg(test)]
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) fn use_test_discovery_url(url: &str) {
    *TEST_DISCOVERY_URL
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(url.to_string());
}

/// Fetch and validate the OpenID configuration.
async fn discover(http: &reqwest::Client) -> Result<Discovery> {
    let url = discovery_url();
    let response = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        let status = response.status();
        bail!("xAI OpenID discovery failed: {url} returned HTTP {status}");
    }
    let discovery: Discovery = response
        .json()
        .await
        .context("parsing the xAI OpenID configuration")?;
    validate_xai_https(&discovery.authorization_endpoint)?;
    validate_xai_https(&discovery.token_endpoint)?;
    Ok(discovery)
}

// ---------------------------------------------------------------------------
// Token storage (~/.wizard/xai_oauth.json)
// ---------------------------------------------------------------------------

/// Persisted OAuth state. Lives in its own 0600 file, never in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub token_type: String,
    /// The discovered token endpoint, cached so refreshes do not depend on
    /// discovery being reachable. Re-validated against x.ai before every use.
    pub token_endpoint: String,
}

/// `~/.wizard/xai_oauth.json`
pub fn token_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("xai_oauth.json"))
}

/// Write tokens atomically, owner-only, through
/// [`crate::platform::secrets::write_private_atomic`].
///
/// This used to be a hand-written copy of that sequence (create the parent,
/// chmod 0700, open 0600, chmod again, write, fsync, rename) with a *fixed*
/// scratch name, `.xai_oauth.json.tmp`. Two Wizards refreshing an expired
/// access token at the same moment, which is ordinary on a machine running two
/// sessions, both truncated that one name and interleaved their writes into a
/// single inode: the first rename published a JSON blob spliced from two token
/// sets and the second failed with ENOENT. The platform primitive owns the
/// scratch name, the modes, and the fsync-before-rename for every secret
/// Wizard stores.
pub fn save_tokens(path: &Path, tokens: &StoredTokens) -> Result<()> {
    let raw = serde_json::to_string_pretty(tokens).context("serializing xAI tokens")?;
    crate::platform::secrets::write_private_atomic(path, raw.as_bytes())
}

/// Read stored tokens; `Ok(None)` when nobody has logged in yet.
pub fn load_tokens(path: &Path) -> Result<Option<StoredTokens>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let tokens: StoredTokens =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(tokens))
}

/// Delete stored tokens (revoked/expired session). Missing file is fine.
pub fn clear_tokens(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// True when `~/.wizard/xai_oauth.json` holds an access token. The access
/// token may already be stale; a refresh token on the same file is what
/// keeps the session alive, and a missing one is the refresh path's problem
/// rather than a reason to open the browser again.
pub fn signed_in() -> bool {
    matches!(
        token_path()
            .ok()
            .and_then(|path| load_tokens(&path).ok().flatten()),
        Some(tokens) if !tokens.access_token.is_empty()
    )
}

/// Sidecar the refresh lock is taken on. The token file itself is replaced
/// by `rename`, so a lock on that inode would not survive the write it is
/// there to serialize.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// How long a refresh waits for another Wizard that is already rotating the
/// grant. Past this we proceed unlocked and rely on [`forget_spent_grant`]
/// to keep a file the other process already rewrote.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(10);

fn lock_tokens(path: &Path) -> Option<crate::platform::lockfile::Guard> {
    crate::platform::lockfile::exclusive(&lock_path(path), REFRESH_LOCK_WAIT)
}

/// If the file no longer holds the refresh token we have in memory, another
/// Wizard already rotated (or replaced) the grant. Take that session and
/// skip spending ours. Returns true when the cache is now a grant we must
/// not refresh.
fn take_disk_if_rotated(cache: &mut TokenCache, path: &Path) -> bool {
    let Ok(Some(on_disk)) = load_tokens(path) else {
        return false;
    };
    let had_cache = cache.tokens.is_some();
    let cached = cache
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.refresh_token.as_deref());
    let disk = on_disk.refresh_token.as_deref();
    // Only a *different* grant on disk is a rotation. An empty cache just
    // means this process has not loaded the file yet; treating that as
    // rotated would skip the expiry check on every first call.
    let rotated = had_cache && disk.is_some() && disk != cached;
    cache.tokens = Some(on_disk);
    if rotated {
        cache.refreshed_at = Some(Instant::now());
    }
    rotated
}

/// After the token endpoint refused the grant we just sent: keep the file
/// if it is no longer that grant (another Wizard wrote a new session while
/// we were in flight), otherwise delete it. Returns true when the cache
/// now holds the surviving session.
fn forget_spent_grant(cache: &mut TokenCache, path: &Path, spent: &str) -> bool {
    if let Ok(Some(on_disk)) = load_tokens(path)
        && on_disk
            .refresh_token
            .as_deref()
            .is_some_and(|token| token != spent)
    {
        cache.tokens = Some(on_disk);
        cache.refreshed_at = Some(Instant::now());
        return true;
    }
    let _ = clear_tokens(path);
    cache.tokens = None;
    false
}

// ---------------------------------------------------------------------------
// Login flow
// ---------------------------------------------------------------------------

/// Token endpoint response (RFC 6749 section 5.1, subset).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

/// The OAuth half of a sign-in in flight: everything needed to turn the
/// callback's `code` into tokens. Paired with the listener that receives that
/// callback by [`PendingBrowserLogin`].
#[derive(Debug)]
struct PendingLogin {
    /// Send the user here.
    authorize_url: String,
    /// Compare against the `state` on the callback: a mismatch is a forgery.
    state: String,
    /// Must be byte-identical at authorize and at exchange, so it is kept here
    /// rather than rebuilt.
    redirect_uri: String,
    pkce: Pkce,
    token_endpoint: String,
}

/// Discover the endpoints, mint PKCE + state, and build the authorize URL for a
/// `redirect_uri` the **caller** serves.
async fn begin_login(redirect_uri: &str) -> Result<PendingLogin> {
    let http = crate::llm::oauth_http_client();
    let discovery = discover(&http).await?;

    let pkce = generate_pkce()?;
    let state = random_hex(16)?;
    let nonce = random_hex(16)?;

    let mut authorize_url = reqwest::Url::parse(&discovery.authorization_endpoint)
        .context("parsing the authorization endpoint")?;
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        // xAI rejects non-allowlisted clients without an explicit plan.
        .append_pair("plan", "generic")
        .append_pair("referrer", "wizard");

    Ok(PendingLogin {
        authorize_url: authorize_url.to_string(),
        state,
        redirect_uri: redirect_uri.to_string(),
        pkce,
        token_endpoint: discovery.token_endpoint,
    })
}

/// Exchange the callback's `code` for tokens and persist them. The `state` from
/// the callback must match the one minted by [`begin_login`].
async fn complete_login(pending: PendingLogin, code: &str, state: &str) -> Result<StoredTokens> {
    anyhow::ensure!(
        state == pending.state,
        "the sign-in state did not match — start again"
    );
    let http = crate::llm::oauth_http_client();
    let token = exchange_code(
        &http,
        &pending.token_endpoint,
        code,
        &pending.redirect_uri,
        &pending.pkce,
    )
    .await?;

    let stored = StoredTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        token_endpoint: pending.token_endpoint,
    };
    save_tokens(&token_path()?, &stored)?;
    Ok(stored)
}

/// The provider entry a completed sign-in earns: no key, no env var — the token
/// store is the credential.
pub fn provider_config() -> crate::config::ProviderConfig {
    crate::config::ProviderConfig {
        name: "xai-oauth".to_string(),
        kind: crate::config::ProviderKind::XaiOauth,
        base_url: DEFAULT_BASE_URL.to_string(),
        model: DEFAULT_MODEL.to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }
}

/// A browser sign-in in flight, holding the listener the redirect will land on.
///
/// xAI sends the browser to the one loopback address registered for
/// [`CLIENT_ID`] and nowhere else, so *every* caller — the terminal flows and
/// the GUI alike — must own that listener rather than serve the redirect on an
/// origin of its own. [`wait_and_complete`] consumes this.
pub struct PendingBrowserLogin {
    /// Send the user here.
    pub authorize_url: String,
    /// The port the redirect will come back on — needed to explain the flow to
    /// a session whose browser is on another machine.
    pub port: u16,
    listener: TcpListener,
    pending: PendingLogin,
}

/// Bind the registered callback port, discover the endpoints, and build the
/// authorize URL. The caller sends the user to `authorize_url`, then hands this
/// back to [`wait_and_complete`].
pub async fn begin_browser_login() -> Result<PendingBrowserLogin> {
    let listener = bind_callback_listener().await?;
    let port = listener
        .local_addr()
        .context("reading listener port")?
        .port();
    // Derived from the port actually bound, so authorize and token-exchange
    // agree byte-for-byte on the redirect_uri.
    let pending = begin_login(&format!("http://127.0.0.1:{port}{CALLBACK_PATH}")).await?;
    Ok(PendingBrowserLogin {
        authorize_url: pending.authorize_url.clone(),
        port,
        listener,
        pending,
    })
}

/// Wait for the browser to hit the callback, exchange the code, and persist the
/// tokens. Consumes the pending login (and its listener).
///
/// `cancel` abandons the wait and gives the port back at once — the GUI fires
/// it when a second sign-in replaces this one. A caller with no competition for
/// the port passes [`Cancel::never`].
pub async fn wait_and_complete(
    pending: PendingBrowserLogin,
    cancel: Cancel,
) -> Result<StoredTokens> {
    wait_and_complete_with_paste(pending, cancel, PasteChannel::Disabled).await
}

/// [`wait_and_complete`], with the option of taking the redirect off stdin as
/// well as off the listener — the only way through for a session whose browser
/// is on another machine and no tunnel between them.
pub async fn wait_and_complete_with_paste(
    pending: PendingBrowserLogin,
    cancel: Cancel,
    paste: PasteChannel,
) -> Result<StoredTokens> {
    let PendingBrowserLogin {
        listener, pending, ..
    } = pending;
    let expected_state = pending.state.clone();
    let spec = matches!(paste, PasteChannel::Stdin).then(|| oauth_callback::PasteSpec {
        callback_path: CALLBACK_PATH,
        state: expected_state.clone(),
    });
    let code = oauth_callback::serve_redirect_or_paste(
        listener,
        cancel,
        |target| parse_callback(target, &expected_state),
        spec,
    )
    .await?;
    let state = pending.state.clone();
    complete_login(pending, &code, &state).await
}

/// The self-contained terminal flow (`wizard --login xai`, `/login xai`): open
/// the browser, wait, exchange. `report` receives human-readable progress lines
/// (stdout for the CLI flag, transcript notices for the slash command).
///
/// A session already on disk is left alone unless `force` is set. Opening the
/// browser every time is how a user who is already signed in gets asked again,
/// and a second grant written over a live one is what a sibling Wizard then
/// treats as a revoked refresh and deletes.
///
/// `paste` says whether this caller owns stdin and can therefore offer the
/// remote-session fallback; see [`PasteChannel`].
pub async fn login<F>(report: F, paste: PasteChannel, force: bool) -> Result<()>
where
    F: Fn(&str) + Send + Sync,
{
    if !force && signed_in() {
        report(&format!(
            "already signed in to xAI ({}); run `/login xai force` to replace the session",
            token_path()?.display()
        ));
        return Ok(());
    }
    let pending = begin_browser_login().await?;
    report(&format!(
        "open this URL to sign in with your xAI account:\n{}",
        pending.authorize_url
    ));
    open_browser(&pending.authorize_url);
    if let Some(hint) = oauth_callback::remote_hint(pending.port) {
        report(&hint);
    }
    report("waiting for the browser callback (5 minute timeout)...");
    // Nothing else in a terminal run competes for the callback port.
    wait_and_complete_with_paste(pending, Cancel::never(), paste).await?;
    report(&format!(
        "signed in to xAI; tokens saved to {}",
        token_path()?.display()
    ));
    Ok(())
}

/// The port the callback listener binds: [`CALLBACK_PORT`], always. It is not a
/// preference, so there is nothing to configure and nothing to get wrong.
#[cfg(not(test))]
fn callback_port() -> u16 {
    CALLBACK_PORT
}

/// Under test, a private port stands in for the registered one — chosen once,
/// so it behaves exactly like the fixed port it replaces (tests that bind it
/// still queue on [`oauth_callback::serial_callback_port`]).
///
/// [`CALLBACK_PORT`] is machine-wide, and a sign-in the user actually has in
/// flight — a `wizard gui` holding it for its five-minute callback window — owns
/// it for real. The suite must neither take it from them nor fail because they
/// have it, and it has no business binding a port a browser might be redirected
/// to. Nothing here changes production: xAI still redirects only to
/// `CALLBACK_PORT`, and the release build has no override to set.
#[cfg(test)]
fn callback_port() -> u16 {
    static PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *PORT.get_or_init(|| {
        let port = oauth_callback::private_test_port();
        assert_ne!(
            port, CALLBACK_PORT,
            "the suite must never bind the registered callback port"
        );
        port
    })
}

/// Bind [`callback_port`], or fail. There is nothing to fall back to: an
/// ephemeral port yields a redirect_uri xAI has never heard of, so it would
/// authorize against an address the browser is never sent to — a sign-in that
/// can only hang. Better to name the conflict — after [`REBIND_GRACE`] has ruled
/// out the one conflict that is not one.
async fn bind_callback_listener() -> Result<TcpListener> {
    let port = callback_port();
    let deadline = Instant::now() + oauth_callback::REBIND_GRACE;
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "could not bind 127.0.0.1:{port} for the xAI sign-in callback; \
                         it is the only redirect xAI accepts for this client, so the sign-in \
                         cannot run elsewhere — close whatever is using it (another wizard \
                         sign-in?) and retry"
                    )
                });
            }
        }
    }
}

/// Best-effort browser launch; the URL is always printed as a fallback.
fn open_browser(url: &str) {
    for opener in ["xdg-open", "open"] {
        if std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}

/// Classify a request line's target (e.g. `/callback?code=...&state=...`).
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let url = match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
        Ok(url) => url,
        Err(_) => return Callback::Ignored,
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Ignored;
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Callback::Failed(format!("xAI returned an OAuth error: {error}"));
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Failed("OAuth state mismatch; aborting the login".to_string());
    }
    match code {
        Some(code) => Callback::Code(code),
        None => Callback::Failed("the OAuth callback carried no code".to_string()),
    }
}

/// Exchange the authorization code for tokens. xAI quirk: the token POST must
/// echo `code_challenge` and `code_challenge_method` alongside the standard
/// `code_verifier`, or it rejects with "code_challenge is required".
async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    pkce: &Pkce,
) -> Result<TokenResponse> {
    validate_xai_https(token_endpoint)?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", pkce.verifier.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    let response = http
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| format!("token exchange with {token_endpoint} failed"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("xAI token exchange returned HTTP {status}: {body}");
    }
    response
        .json()
        .await
        .context("parsing the xAI token response")
}

// ---------------------------------------------------------------------------
// Refreshing token source
// ---------------------------------------------------------------------------

/// [`TokenSource`] backed by `~/.wizard/xai_oauth.json`. Loads lazily,
/// refreshes proactively near expiry, and once more after an API 401.
#[derive(Debug)]
pub struct XaiTokenSource {
    http: reqwest::Client,
    path: PathBuf,
    /// The tokens, and when they were last replaced. Both live under the one
    /// lock, which is held *across* the refresh HTTP call: a refresh token is
    /// single-use, so two concurrent refreshes would spend the same one and
    /// the loser would be told its grant was revoked — and then delete the
    /// token file. `/ultra` fans several candidates at one endpoint per turn,
    /// so concurrent is the ordinary case, not the exotic one.
    cache: Mutex<TokenCache>,
}

/// The cached tokens plus the moment they were last refreshed.
#[derive(Debug, Default)]
struct TokenCache {
    tokens: Option<StoredTokens>,
    /// When [`XaiTokenSource::refresh`] last succeeded. `None` before the
    /// first one. See [`XaiTokenSource::refresh_after_unauthorized`], which
    /// compares it against the moment a caller *entered* to decide whether the
    /// 401 it is reacting to has already been answered by somebody else.
    refreshed_at: Option<Instant>,
}

/// Turn a refresh failure into an error the retry ladder can classify.
///
/// This is the whole point of the function. A refresh runs on the way into
/// every model call, so whatever it raises is what the agent sees, and an
/// untyped `anyhow` error reaches
/// [`error_is_transient`](crate::agent::error_is_transient)'s permissive
/// fallback and is retried — including the one case that must never be
/// retried, a grant the user has to sign in again to replace. Naming a status
/// makes each outcome say what it is: `None` for a transport failure that a
/// retry may well fix, 401 for a session that is gone, the endpoint's own
/// status for anything it answered with.
fn refresh_error(status: Option<u16>, message: String) -> anyhow::Error {
    anyhow::Error::new(match status {
        Some(status) => crate::llm::ProviderError::http(status, message),
        None => crate::llm::ProviderError::transport(message),
    })
}

impl XaiTokenSource {
    /// Source reading from the default token path.
    pub fn new() -> Result<Self> {
        Ok(Self::with_path(token_path()?))
    }

    /// Source reading from an explicit path (tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            http: crate::llm::oauth_http_client(),
            path,
            cache: Mutex::new(TokenCache::default()),
        }
    }

    /// Ensure the cache holds tokens, loading from disk on first use.
    ///
    /// "Not signed in" is a permanent 401 rather than a bare message: there is
    /// no token file, so no amount of backing off produces one, and letting it
    /// climb the ladder spends seven retries and a circuit-breaker trip before
    /// showing the user the one line that would have helped them.
    fn ensure_loaded<'a>(&self, cache: &'a mut TokenCache) -> Result<&'a mut StoredTokens> {
        if cache.tokens.is_none() {
            cache.tokens = load_tokens(&self.path)?;
        }
        cache.tokens.as_mut().ok_or_else(|| {
            refresh_error(
                Some(401),
                "not signed in to xAI; run `wizard --login xai` (or /login xai) first".to_string(),
            )
        })
    }

    /// Refresh the access token via the stored refresh token. On a 400/401
    /// from the token endpoint the stored tokens are cleared (the grant is
    /// gone for good) and the user is told to log in again — unless the file
    /// on disk is already a different grant, which is a sibling Wizard that
    /// signed in or refreshed while this one was holding a spent token.
    async fn refresh(&self, cache: &mut TokenCache) -> Result<()> {
        let _lock = lock_tokens(&self.path);
        if take_disk_if_rotated(cache, &self.path) {
            return Ok(());
        }
        let tokens = self.ensure_loaded(cache)?;
        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            refresh_error(
                Some(401),
                "the stored xAI session has no refresh token; run `wizard --login xai` again"
                    .to_string(),
            )
        })?;
        let token_endpoint = tokens.token_endpoint.clone();
        validate_xai_https(&token_endpoint)?;

        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token.as_str()),
        ];
        let response = self
            .http
            .post(&token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            // A DNS failure, a refused connect, a dead TLS handshake or the
            // client's own request timeout: auth.x.ai being unreachable for a
            // moment is not a reason to end a run that has been going for an
            // hour, so it carries no status and is therefore transient.
            .map_err(|source| {
                refresh_error(
                    None,
                    format!(
                        "could not reach {token_endpoint} to refresh the xAI access token: {source}"
                    ),
                )
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED
        {
            let body = response.text().await.unwrap_or_default();
            if forget_spent_grant(cache, &self.path, &refresh_token) {
                return Ok(());
            }
            return Err(refresh_error(
                Some(401),
                format!(
                    "the xAI session was revoked or expired (HTTP {status}: {body}); \
                     run `wizard --login xai` to sign in again"
                ),
            ));
        }
        if !status.is_success() {
            let retry_after = crate::llm::retry_after_from_headers(response.headers());
            let body = response.text().await.unwrap_or_default();
            // The endpoint's own status, so a 429 or a 503 from auth.x.ai
            // backs off and comes back rather than ending the run.
            return Err(crate::llm::http_error_with_retry_after(
                status.as_u16(),
                format!("xAI token refresh returned HTTP {status}: {body}"),
                retry_after,
            ));
        }
        let refreshed: TokenResponse = response.json().await.map_err(|source| {
            // A truncated or non-JSON body from the token endpoint means the
            // response was mangled in transit, not that the grant is bad.
            refresh_error(
                None,
                format!("could not parse the xAI refresh response: {source}"),
            )
        })?;

        let updated = StoredTokens {
            access_token: refreshed.access_token,
            // A new refresh token replaces the old one when present.
            refresh_token: refreshed.refresh_token.or(Some(refresh_token)),
            token_type: refreshed.token_type.unwrap_or_else(|| "Bearer".to_string()),
            token_endpoint,
        };
        // Persisted before it is cached, so a re-exec or a second Wizard picks
        // up the token this one just minted instead of replaying the spent
        // refresh token it still has on disk.
        save_tokens(&self.path, &updated)?;
        cache.tokens = Some(updated);
        cache.refreshed_at = Some(Instant::now());
        Ok(())
    }
}

#[async_trait]
impl TokenSource for XaiTokenSource {
    async fn bearer(&self) -> Result<Option<String>> {
        let mut cache = self.cache.lock().await;
        let tokens = self.ensure_loaded(&mut cache)?;
        if expires_soon_at(&tokens.access_token, unix_now()) {
            self.refresh(&mut cache).await?;
        }
        Ok(cache
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.clone()))
    }

    /// Force a refresh after the API answered 401 — unless somebody already
    /// did.
    ///
    /// The 401 this reacts to happened strictly before this call started, so a
    /// refresh that *completed* after it started has already replaced the
    /// token that was rejected: refreshing again would spend a second
    /// single-use grant to obtain a token no better than the one now in hand.
    /// That is not merely wasteful. xAI rotates the refresh token, and a burst
    /// of N parallel subagents all hitting one expiry would queue N refreshes
    /// on this mutex, each one racing the server's tolerance for a grant it has
    /// just superseded; the first to be judged stale takes the whole session
    /// down, because a rejected refresh deletes the token file.
    ///
    /// Timing is read before the lock deliberately — the wait *is* the window
    /// in which someone else's refresh lands.
    async fn refresh_after_unauthorized(&self) -> Result<bool> {
        let entered = Instant::now();
        let mut cache = self.cache.lock().await;
        if cache
            .refreshed_at
            .is_some_and(|refreshed| refreshed >= entered)
        {
            return Ok(true);
        }
        self.refresh(&mut cache).await?;
        Ok(true)
    }

    fn unauthorized_hint(&self) -> &str {
        "run `wizard --login xai` to sign in again"
    }

    fn forbidden_hint(&self) -> Option<&str> {
        // Do not loop refreshes on 403: the token is valid but the account's
        // plan does not include OAuth API access.
        Some(
            "xAI gates OAuth API access to certain SuperGrok plans; if your plan lacks it, \
             set XAI_API_KEY and use a provider with kind \"xai\" instead",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_well_formed() {
        let pkce = generate_pkce().expect("pkce");
        // 64 random bytes encode to 86 base64url chars, inside RFC bounds.
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "len {}",
            pkce.verifier.len()
        );
        assert!(
            pkce.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier {} has invalid chars",
            pkce.verifier
        );
        assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
        // A second pair must differ (randomness).
        assert_ne!(generate_pkce().expect("pkce").verifier, pkce.verifier);
    }

    /// Unsigned JWT with the given JSON payload.
    fn jwt_with_payload(payload: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn jwt_exp_reads_the_exp_claim() {
        let token = jwt_with_payload(r#"{"sub":"u1","exp":1234567890}"#);
        assert_eq!(jwt_exp(&token), Some(1_234_567_890));
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp(&jwt_with_payload(r#"{"sub":"u1"}"#)), None);
    }

    #[test]
    fn expiry_uses_a_two_minute_leeway() {
        let token = jwt_with_payload(r#"{"exp":1000}"#);
        assert!(expires_soon_at(&token, 1000), "already expired");
        assert!(expires_soon_at(&token, 880), "exactly at the leeway edge");
        assert!(!expires_soon_at(&token, 879), "just outside the leeway");
        // Unreadable exp counts as live; the 401 path handles it.
        assert!(!expires_soon_at("opaque-token", 0));
    }

    #[test]
    fn token_store_round_trips_with_tight_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wizard-home").join("xai_oauth.json");
        let tokens = StoredTokens {
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            token_type: "Bearer".to_string(),
            token_endpoint: "https://auth.x.ai/oauth/token".to_string(),
        };
        save_tokens(&path, &tokens).expect("save");

        let loaded = load_tokens(&path).expect("load").expect("present");
        assert_eq!(loaded.access_token, "at");
        assert_eq!(loaded.refresh_token.as_deref(), Some("rt"));
        assert_eq!(loaded.token_type, "Bearer");
        assert_eq!(loaded.token_endpoint, "https://auth.x.ai/oauth/token");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&path)
                .expect("file meta")
                .permissions()
                .mode();
            assert_eq!(file_mode & 0o777, 0o600, "token file must be 0600");
            let dir_mode = std::fs::metadata(path.parent().expect("parent"))
                .expect("dir meta")
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "token dir must be 0700");
        }

        clear_tokens(&path).expect("clear");
        assert!(load_tokens(&path).expect("load after clear").is_none());
        clear_tokens(&path).expect("clearing a missing file is fine");
    }

    #[test]
    fn saving_tokens_leaves_no_scratch_file_and_never_reuses_a_fixed_name() {
        // The store went through `platform::secrets`, which owns the scratch
        // name. The name this module used to hard-code, `.xai_oauth.json.tmp`,
        // was opened with `truncate(true)`: anything already sitting there was
        // overwritten, which is both how two concurrent refreshes spliced one
        // token file and how a name planted by another local user would have
        // redirected the write. Planting that exact name is therefore the
        // observation that the old sequence is gone.
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("wizard-home");
        std::fs::create_dir_all(&home).expect("mkdir");
        let planted = home.join(".xai_oauth.json.tmp");
        std::fs::write(&planted, b"not ours").expect("plant the old scratch name");

        let path = home.join("xai_oauth.json");
        save_tokens(
            &path,
            &StoredTokens {
                access_token: "at".to_string(),
                refresh_token: None,
                token_type: "Bearer".to_string(),
                token_endpoint: "https://auth.x.ai/oauth/token".to_string(),
            },
        )
        .expect("save");

        assert_eq!(
            std::fs::read(&planted).expect("read the planted file"),
            b"not ours",
            "the old fixed scratch name is still written through"
        );
        assert_eq!(
            load_tokens(&path)
                .expect("load")
                .expect("present")
                .access_token,
            "at"
        );

        // And the write cleans up after itself: the only files this call may
        // add are the token file and nothing else.
        let mut left: Vec<String> = std::fs::read_dir(&home)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![".xai_oauth.json.tmp", "xai_oauth.json"],
            "{left:?}"
        );
    }

    #[test]
    fn missing_token_file_reads_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.json");
        assert!(load_tokens(&path).expect("load").is_none());
    }

    #[test]
    fn endpoint_pinning_rejects_non_xai_hosts() {
        validate_xai_https("https://auth.x.ai/oauth/token").expect("subdomain ok");
        validate_xai_https("https://x.ai/oauth/token").expect("apex ok");
        assert!(
            validate_xai_https("http://auth.x.ai/oauth/token").is_err(),
            "plain http"
        );
        assert!(
            validate_xai_https("https://evil.example/token").is_err(),
            "other host"
        );
        // String suffix is not enough: notx.ai is a different domain.
        assert!(validate_xai_https("https://notx.ai/token").is_err());
        assert!(validate_xai_https("https://x.ai.evil.example/token").is_err());
    }

    #[test]
    fn callback_parsing_validates_state_and_path() {
        assert_eq!(
            parse_callback("/callback?code=abc&state=s1", "s1"),
            Callback::Code("abc".to_string())
        );
        assert!(matches!(
            parse_callback("/callback?code=abc&state=wrong", "s1"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            parse_callback("/callback?error=access_denied&state=s1", "s1"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            parse_callback("/callback?state=s1", "s1"),
            Callback::Failed(_)
        ));
        assert_eq!(parse_callback("/favicon.ico", "s1"), Callback::Ignored);
    }

    /// A runtime for a test that holds the one callback port. Not
    /// `#[tokio::test]`: the guard for that port is a plain lock, and it has to
    /// be held across the very binds it is there to serialize.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// A port somebody else owns is still owned when [`REBIND_GRACE`] runs out.
    #[test]
    fn an_occupied_callback_port_fails_instead_of_falling_back() {
        // xAI redirects only to the callback port, so an ephemeral fallback
        // could never receive the browser — it would hang until the timeout.
        // The bind must fail loudly, naming the port that is in the way.
        let _serial = oauth_callback::serial_callback_port();
        let port = callback_port();
        let _held = oauth_callback::take_test_port(port);
        let err = runtime()
            .block_on(bind_callback_listener())
            .expect_err("the port is taken");
        assert!(
            format!("{err:#}").contains(&port.to_string()),
            "the error should name the port: {err:#}"
        );
    }

    /// The grace exists for one thing: the port this process has just given
    /// back. Dropping a listener does not free its port synchronously, so the
    /// bind that follows a cancelled sign-in must be able to wait out the
    /// kernel's teardown rather than report a conflict that is already over.
    #[test]
    fn a_port_released_a_moment_ago_is_bound_not_reported_as_a_conflict() {
        let _serial = oauth_callback::serial_callback_port();
        let held = oauth_callback::take_test_port(callback_port());
        // Released from another thread, so the bind races the close exactly as
        // the GUI's retry races the flow it has just cancelled.
        std::thread::spawn(move || drop(held));
        runtime()
            .block_on(bind_callback_listener())
            .expect("the port comes back well inside the grace");
    }

    #[tokio::test]
    async fn token_source_without_login_says_how_to_sign_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = XaiTokenSource::with_path(dir.path().join("xai_oauth.json"));
        let err = source.bearer().await.expect_err("must fail");
        assert!(
            err.to_string().contains("wizard --login xai"),
            "error should name the login command: {err}"
        );
    }

    /// Every way a refresh can fail has to reach the retry ladder as a
    /// classifiable error, and the two directions matter for opposite reasons.
    ///
    /// A refresh runs on the way into every model call, so whatever it raises
    /// is what ends the turn. Untyped, all of them took
    /// `error_is_transient`'s permissive fallback and were retried: a session
    /// the user has to sign in again to replace burned the whole ladder and a
    /// circuit-breaker trip before finally showing them the line that would
    /// have helped. Typed, "the token host was briefly unreachable" retries
    /// (which is the failure that silently ends a long run) and "the grant is
    /// gone" stops at once with the instruction.
    #[test]
    fn refresh_failures_are_typed_so_the_ladder_can_tell_them_apart() {
        let unreachable = refresh_error(None, "dns lookup failed".to_string());
        let provider = unreachable
            .downcast_ref::<crate::llm::ProviderError>()
            .expect("typed");
        assert_eq!(provider.status, None);
        assert!(
            provider.is_transient(),
            "auth.x.ai being unreachable for a moment must not end an hour-long run"
        );

        let revoked = refresh_error(Some(401), "sign in again".to_string());
        assert!(
            !revoked
                .downcast_ref::<crate::llm::ProviderError>()
                .expect("typed")
                .is_transient(),
            "a revoked grant never refreshes, however long we wait"
        );

        // The token endpoint's own transient statuses stay transient.
        for status in [429, 500, 503] {
            assert!(
                refresh_error(Some(status), "later".to_string())
                    .downcast_ref::<crate::llm::ProviderError>()
                    .expect("typed")
                    .is_transient(),
                "HTTP {status} from the token endpoint"
            );
        }
    }

    /// Not being signed in is permanent, not a thing to back off from.
    #[tokio::test]
    async fn not_being_signed_in_ends_the_call_instead_of_climbing_the_ladder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = XaiTokenSource::with_path(dir.path().join("xai_oauth.json"));
        let err = source.bearer().await.expect_err("must fail");
        let provider = err
            .downcast_ref::<crate::llm::ProviderError>()
            .expect("typed");
        assert_eq!(provider.status, Some(401));
        assert!(
            !provider.is_transient(),
            "no amount of backoff produces a token file that is not there"
        );

        // A session stored without a refresh token is the same kind of dead
        // end: there is nothing to refresh with.
        let path = dir.path().join("no-refresh.json");
        save_tokens(
            &path,
            &StoredTokens {
                access_token: "at".to_string(),
                refresh_token: None,
                token_type: "Bearer".to_string(),
                token_endpoint: "https://auth.x.ai/oauth/token".to_string(),
            },
        )
        .expect("save");
        let source = XaiTokenSource::with_path(path);
        let err = source
            .refresh_after_unauthorized()
            .await
            .expect_err("nothing to refresh with");
        assert!(
            !err.downcast_ref::<crate::llm::ProviderError>()
                .expect("typed")
                .is_transient()
        );
        assert!(format!("{err:#}").contains("wizard --login xai"));
    }

    /// A 401 that another task's refresh has already answered must not spend a
    /// second single-use grant.
    ///
    /// xAI rotates the refresh token, so N parallel subagents meeting one
    /// expiry queue N refreshes on this mutex, each spending a grant the
    /// previous one just superseded. The first the server judges stale comes
    /// back as "revoked" — and a revoked refresh *deletes the token file*, so
    /// a burst of concurrency signs the user out of a session that was fine.
    ///
    /// The stored endpoint is deliberately not an x.ai host: a refresh that
    /// actually ran would fail at [`validate_xai_https`], with no network
    /// call. So the `Ok` below can only mean the refresh was skipped.
    #[tokio::test]
    async fn a_401_already_answered_while_waiting_for_the_lock_spends_no_second_grant() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("xai_oauth.json");
        save_tokens(
            &path,
            &StoredTokens {
                access_token: "at".to_string(),
                refresh_token: Some("rt".to_string()),
                token_type: "Bearer".to_string(),
                token_endpoint: "https://not-xai.example/token".to_string(),
            },
        )
        .expect("save");
        let source = Arc::new(XaiTokenSource::with_path(path));

        // The control: a refresh that does run against this store fails.
        {
            let mut cache = source.cache.lock().await;
            source
                .refresh(&mut cache)
                .await
                .expect_err("the pinning check refuses a non-x.ai endpoint");
        }

        // Hold the lock, as the task doing the real refresh would.
        let mut held = source.cache.lock().await;
        let waiter = tokio::spawn({
            let source = Arc::clone(&source);
            async move { source.refresh_after_unauthorized().await }
        });
        // Let it reach the lock and park there; its own "entered" stamp is
        // taken before that, which is the whole point.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The sibling's refresh lands and the lock is handed over — the exact
        // sequence a real one goes through.
        held.refreshed_at = Some(Instant::now());
        drop(held);

        assert!(
            waiter
                .await
                .expect("the task ran")
                .expect("a refresh that already happened is not a failure"),
            "the caller is still told to retry its request, with the new token"
        );
    }

    /// The opposite case, so the skip above cannot be a refusal to ever
    /// refresh: a 401 nobody has answered still drives a real refresh.
    #[tokio::test]
    async fn a_401_nobody_has_answered_still_refreshes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("xai_oauth.json");
        save_tokens(
            &path,
            &StoredTokens {
                access_token: "at".to_string(),
                refresh_token: Some("rt".to_string()),
                token_type: "Bearer".to_string(),
                token_endpoint: "https://not-xai.example/token".to_string(),
            },
        )
        .expect("save");
        let source = XaiTokenSource::with_path(path);
        source
            .refresh_after_unauthorized()
            .await
            .expect_err("the refresh ran, and this store cannot satisfy it");
    }

    fn sample_tokens(access: &str, refresh: Option<&str>) -> StoredTokens {
        StoredTokens {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            token_type: "Bearer".to_string(),
            token_endpoint: "https://auth.x.ai/oauth/token".to_string(),
        }
    }

    #[test]
    fn a_spent_refresh_does_not_delete_a_newer_session_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("xai_oauth.json");
        save_tokens(&path, &sample_tokens("new-at", Some("new-rt"))).expect("save");
        let mut cache = TokenCache {
            tokens: Some(sample_tokens("old-at", Some("old-rt"))),
            refreshed_at: None,
        };
        assert!(
            forget_spent_grant(&mut cache, &path, "old-rt"),
            "the file is a different grant and must survive"
        );
        assert_eq!(
            cache.tokens.as_ref().map(|t| t.access_token.as_str()),
            Some("new-at")
        );
        assert!(path.exists(), "the surviving session stays on disk");

        save_tokens(&path, &sample_tokens("same-at", Some("old-rt"))).expect("save");
        cache.tokens = Some(sample_tokens("old-at", Some("old-rt")));
        assert!(
            !forget_spent_grant(&mut cache, &path, "old-rt"),
            "the file still holds the grant that was refused"
        );
        assert!(!path.exists(), "a truly spent grant is forgotten");
        assert!(cache.tokens.is_none());
    }

    #[test]
    fn a_refresh_adopts_a_grant_another_wizard_already_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("xai_oauth.json");
        save_tokens(&path, &sample_tokens("new-at", Some("new-rt"))).expect("save");
        let mut cache = TokenCache {
            tokens: Some(sample_tokens("old-at", Some("old-rt"))),
            refreshed_at: None,
        };
        assert!(take_disk_if_rotated(&mut cache, &path));
        assert_eq!(
            cache
                .tokens
                .as_ref()
                .and_then(|t| t.refresh_token.as_deref()),
            Some("new-rt")
        );

        // First load of a matching file is not a rotation: refresh still has
        // to run if the access token is stale.
        cache.tokens = None;
        cache.refreshed_at = None;
        assert!(!take_disk_if_rotated(&mut cache, &path));
        assert_eq!(
            cache.tokens.as_ref().map(|t| t.access_token.as_str()),
            Some("new-at")
        );
    }
}
