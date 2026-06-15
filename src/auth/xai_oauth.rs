//! Sign in with an xAI account: OAuth 2.0 Authorization Code + PKCE against
//! `auth.x.ai`, then plain `Bearer` access tokens against the
//! OpenAI-compatible Chat Completions API at `https://api.x.ai/v1`.
//!
//! - [`login`] runs the interactive browser flow (`wizard login xai` or the
//!   `/login xai` slash command) and stores the tokens in
//!   `~/.wizard/xai_oauth.json` (file 0600, directory 0700).
//! - [`XaiTokenSource`] reads the stored tokens and hands out a fresh bearer
//!   via [`XaiTokenSource::bearer`], proactively refreshing the access token
//!   when its JWT `exp` is within 120 s.
//!
//! Tokens never go into `config.toml`; they live only in the dedicated file.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::config::Config;

/// OpenID Connect discovery document for xAI accounts.
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// Public OAuth client id (the upstream Grok-CLI client; no secret).
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// Scopes: identity, refresh tokens, and API access.
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Preferred localhost callback port; an ephemeral port is used when taken.
const CALLBACK_PORT: u16 = 56121;
/// Refresh the access token when it expires within this many seconds.
const EXPIRY_LEEWAY_SECS: i64 = 120;
/// How long the localhost listener waits for the browser callback.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Default Chat Completions base URL for xAI.
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
/// Default model for xAI.
pub const DEFAULT_MODEL: &str = "grok-4.3";

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

/// True when `token` expires at or before `now + EXPIRY_LEEWAY_SECS`. A token
/// without a readable `exp` is treated as live.
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

/// Fetch and validate the OpenID configuration.
async fn discover(http: &reqwest::Client) -> Result<Discovery> {
    let response = http
        .get(DISCOVERY_URL)
        .send()
        .await
        .with_context(|| format!("fetching {DISCOVERY_URL}"))?;
    if !response.status().is_success() {
        let status = response.status();
        bail!("xAI OpenID discovery failed: {DISCOVERY_URL} returned HTTP {status}");
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

/// True when an xAI session is stored on disk.
pub fn is_logged_in() -> bool {
    token_path().ok().map(|path| path.exists()).unwrap_or(false)
}

/// Write tokens atomically: 0600 temp file in the same directory, then rename
/// over the target. The parent directory is created (and tightened to 0700)
/// first.
pub fn save_tokens(path: &Path, tokens: &StoredTokens) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("token path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", dir.display()))?;
    }

    let raw = serde_json::to_string_pretty(tokens).context("serializing xAI tokens")?;
    let tmp = dir.join(".xai_oauth.json.tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        // create(true) keeps the mode of a pre-existing file; enforce 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting permissions on {}", tmp.display()))?;
        }
        file.write_all(raw.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("moving {} into place", path.display()))?;
    Ok(())
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

/// Run the full browser login: discovery, PKCE, localhost callback, code
/// exchange, and token storage. `report` receives human-readable progress
/// lines (stdout for the CLI command, transcript notices for the slash
/// command).
pub async fn login<F>(report: F) -> Result<()>
where
    F: Fn(&str) + Send + Sync,
{
    let http = reqwest::Client::new();
    let discovery = discover(&http).await?;

    let pkce = generate_pkce()?;
    let state = random_hex(16)?;
    let nonce = random_hex(16)?;

    let listener = bind_callback_listener()?;
    let port = listener
        .local_addr()
        .context("reading listener port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut authorize_url = reqwest::Url::parse(&discovery.authorization_endpoint)
        .context("parsing the authorization endpoint")?;
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        // xAI rejects non-allowlisted clients without an explicit plan.
        .append_pair("plan", "generic")
        .append_pair("referrer", "wizard");

    report(&format!(
        "open this URL to sign in with your xAI account:\n{authorize_url}"
    ));
    open_browser(authorize_url.as_str());
    report("waiting for the browser callback (5 minute timeout)...");

    let expected_state = state.clone();
    let code = tokio::task::spawn_blocking(move || wait_for_callback(listener, &expected_state))
        .await
        .context("callback listener task panicked")??;

    report("exchanging the authorization code for tokens...");
    let token = exchange_code(
        &http,
        &discovery.token_endpoint,
        &code,
        &redirect_uri,
        &pkce,
    )
    .await?;

    let stored = StoredTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        token_endpoint: discovery.token_endpoint,
    };
    let path = token_path()?;
    save_tokens(&path, &stored)?;
    report(&format!(
        "signed in to xAI; tokens saved to {}",
        path.display()
    ));
    Ok(())
}

/// Bind the preferred callback port, falling back to an ephemeral one. The
/// redirect_uri is derived from whatever was actually bound, so authorize and
/// token-exchange always agree byte-for-byte.
fn bind_callback_listener() -> Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)) {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(("127.0.0.1", 0))
            .context("binding a localhost port for the OAuth callback"),
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

/// What one callback HTTP request amounted to.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// `/callback` with a code and matching state.
    Code(String),
    /// `/callback` carried an OAuth error or a state mismatch.
    Failed(String),
    /// Some other path (favicon and friends): respond 404 and keep waiting.
    Ignored,
}

/// Classify a request line's target (e.g. `/callback?code=...&state=...`).
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let url = match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
        Ok(url) => url,
        Err(_) => return Callback::Ignored,
    };
    if url.path() != "/callback" {
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

/// Accept connections until `/callback` arrives (or the timeout passes),
/// validating `state` and answering each request with a tiny HTML page.
fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("configuring the callback listener")?;
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(outcome) = handle_callback_connection(stream, expected_state) {
                    return outcome;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the browser sign-in (5 minutes)");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("accepting the OAuth callback connection"),
        }
    }
}

/// Serve one connection. `Some(result)` ends the wait; `None` keeps waiting.
fn handle_callback_connection(
    mut stream: TcpStream,
    expected_state: &str,
) -> Option<Result<String>> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // The request line fits well inside 8 KiB; we only need the GET target.
    let mut buf = [0u8; 8192];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&buf[..len]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");

    match parse_callback(target, expected_state) {
        Callback::Ignored => {
            respond(&mut stream, "404 Not Found", "Not found.");
            None
        }
        Callback::Failed(message) => {
            respond(
                &mut stream,
                "200 OK",
                "Sign-in failed. Return to the terminal for details.",
            );
            Some(Err(anyhow!(message)))
        }
        Callback::Code(code) => {
            respond(
                &mut stream,
                "200 OK",
                "Signed in to Wizard. You can close this tab.",
            );
            Some(Ok(code))
        }
    }
}

/// Minimal HTTP response with a one-line HTML body.
fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><title>Wizard</title></head><body style=\"font-family: sans-serif; margin: 4em\"><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
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

/// Reads `~/.wizard/xai_oauth.json`, loads lazily, and refreshes the access
/// token proactively near expiry. [`bearer`](Self::bearer) returns a token
/// good for the next call.
#[derive(Debug)]
pub struct XaiTokenSource {
    http: reqwest::Client,
    path: PathBuf,
    cache: Mutex<Option<StoredTokens>>,
}

impl XaiTokenSource {
    /// Source reading from the default token path.
    pub fn new() -> Result<Self> {
        Ok(Self::with_path(token_path()?))
    }

    /// Source reading from an explicit path (tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            http: reqwest::Client::new(),
            path,
            cache: Mutex::new(None),
        }
    }

    /// A bearer access token valid for the next API call, refreshed first when
    /// it is within the expiry leeway.
    pub async fn bearer(&self) -> Result<String> {
        let mut cache = self.cache.lock().await;
        let tokens = self.ensure_loaded(&mut cache)?;
        if expires_soon_at(&tokens.access_token, unix_now()) {
            self.refresh(&mut cache).await?;
        }
        Ok(cache
            .as_ref()
            .map(|tokens| tokens.access_token.clone())
            .unwrap_or_default())
    }

    /// Ensure the cache holds tokens, loading from disk on first use.
    fn ensure_loaded<'a>(
        &self,
        cache: &'a mut Option<StoredTokens>,
    ) -> Result<&'a mut StoredTokens> {
        if cache.is_none() {
            *cache = load_tokens(&self.path)?;
        }
        cache.as_mut().ok_or_else(|| {
            anyhow!("not signed in to xAI; run `wizard login xai` (or /login xai) first")
        })
    }

    /// Refresh the access token via the stored refresh token. On a 400/401
    /// from the token endpoint the stored tokens are cleared (the grant is
    /// gone for good) and the user is told to log in again.
    async fn refresh(&self, cache: &mut Option<StoredTokens>) -> Result<()> {
        let tokens = self.ensure_loaded(cache)?;
        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            anyhow!("the stored xAI session has no refresh token; run `wizard login xai` again")
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
            .with_context(|| format!("refreshing the xAI access token at {token_endpoint}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED
        {
            let body = response.text().await.unwrap_or_default();
            let _ = clear_tokens(&self.path);
            *cache = None;
            bail!(
                "the xAI session was revoked or expired (HTTP {status}: {body}); \
                 run `wizard login xai` to sign in again"
            );
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("xAI token refresh returned HTTP {status}: {body}");
        }
        let refreshed: TokenResponse = response
            .json()
            .await
            .context("parsing the xAI refresh response")?;

        let updated = StoredTokens {
            access_token: refreshed.access_token,
            // A new refresh token replaces the old one when present.
            refresh_token: refreshed.refresh_token.or(Some(refresh_token)),
            token_type: refreshed.token_type.unwrap_or_else(|| "Bearer".to_string()),
            token_endpoint,
        };
        save_tokens(&self.path, &updated)?;
        *cache = Some(updated);
        Ok(())
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

    #[tokio::test]
    async fn token_source_without_login_says_how_to_sign_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = XaiTokenSource::with_path(dir.path().join("xai_oauth.json"));
        let err = source.bearer().await.expect_err("must fail");
        assert!(
            err.to_string().contains("wizard login xai"),
            "error should name the login command: {err}"
        );
    }
}
