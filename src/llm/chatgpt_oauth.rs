//! Sign in with a ChatGPT subscription (Plus/Pro/Team), rather than a
//! pay-as-you-go API key.
//!
//! A subscription is reached exactly the way OpenAI's own Codex CLI reaches it:
//! OAuth 2.0 Authorization Code + PKCE against `auth.openai.com` using Codex's
//! public client id, and then the **Responses** API at
//! `chatgpt.com/backend-api/codex` — not the Chat Completions API, and not
//! `api.openai.com`. That endpoint only answers to the Codex client, so the
//! requests present as it (`originator: codex_cli_rs`, the Codex client id);
//! [`super::chatgpt`] speaks the protocol, this module supplies the credentials.
//!
//! Tokens live in `~/.wizard/chatgpt_oauth.json` (file 0600), never in
//! `config.toml`. The account id needed on every API call is a claim inside the
//! `id_token` and is stored alongside the tokens.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::oauth_callback::{self, Callback, Cancel, PasteChannel};
use super::registry::{Credentials, ProviderDescriptor, ProviderKind};
use super::xai_oauth::{generate_pkce, jwt_exp};
use crate::config::Config;

/// OAuth authorize endpoint.
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// OAuth token + refresh endpoint.
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex CLI's public OAuth client id (no secret). The subscription endpoint
/// only issues tokens to this client, so a third-party sign-in must use it.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Scopes: identity plus a refresh token.
const SCOPE: &str = "openid profile email offline_access";
/// The redirect the client is registered with. Fixed — unlike a floating
/// loopback port, this must match what OpenAI has on file, so both the
/// preferred and the fallback port are registered ones and the path is exact.
const CALLBACK_PORT: u16 = 1455;
const FALLBACK_PORT: u16 = 1457;
const CALLBACK_PATH: &str = "/auth/callback";
/// Identifies the client to both the authorize flow and the API.
const ORIGINATOR: &str = "codex_cli_rs";
/// Refresh the access token when its JWT `exp` is within this many seconds.
const EXPIRY_LEEWAY_SECS: i64 = 300;

/// The subscription API base (Responses API lives under it).
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// A reasonable default model; the real list comes from `GET {BASE_URL}/models`.
pub const DEFAULT_MODEL: &str = "gpt-5.6-sol";
/// Client identifier sent on every API request.
pub const API_ORIGINATOR: &str = ORIGINATOR;

// ---------------------------------------------------------------------------
// Token storage (~/.wizard/chatgpt_oauth.json)
// ---------------------------------------------------------------------------

/// Persisted OAuth state. Its own 0600 file, never config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// The identity token; its claims carry the account id and plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// `chatgpt_account_id` from the id_token — sent on every API call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// `~/.wizard/chatgpt_oauth.json`
pub fn token_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("chatgpt_oauth.json"))
}

/// Write tokens atomically, owner-only, through
/// [`crate::platform::secrets::write_private_atomic`].
///
/// This used to be a hand-written copy of that sequence with a *fixed* scratch
/// name, `.chatgpt_oauth.json.tmp`, opened `truncate(true)` — the same bug
/// `xai_oauth::save_tokens` documents and fixed the same way. Two Wizards
/// refreshing an expired access token at the same moment, ordinary on a
/// machine running two sessions, both truncated that one name and interleaved
/// their writes into a single inode: the first rename published a JSON blob
/// spliced from two token sets and the second failed with ENOENT. The platform
/// primitive owns the scratch name, the modes, and the fsync-before-rename for
/// every secret Wizard stores.
pub fn save_tokens(path: &Path, tokens: &StoredTokens) -> Result<()> {
    let json = serde_json::to_string_pretty(tokens).context("serializing ChatGPT tokens")?;
    crate::platform::secrets::write_private_atomic(path, json.as_bytes())
}

/// Read the stored tokens; `Ok(None)` when the file is absent.
pub fn load_tokens(path: &Path) -> Result<Option<StoredTokens>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Forget the stored tokens (a missing file is not an error).
pub fn clear_tokens(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// The id_token's account-id claim
// ---------------------------------------------------------------------------

/// Extract `chatgpt_account_id` from the id_token's `https://api.openai.com/auth`
/// claim. `None` when the token is not a parseable JWT or lacks the claim.
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// The sign-in flow
// ---------------------------------------------------------------------------

/// A sign-in in flight: the URL to send the user to, plus everything needed to
/// finish once the browser comes back. Holds its own bound listener, since the
/// redirect is a fixed address rather than the caller's server.
pub struct PendingLogin {
    pub authorize_url: String,
    state: String,
    redirect_uri: String,
    verifier: String,
    listener: TcpListener,
}

impl PendingLogin {
    /// The port the redirect will come back on — either of the two registered
    /// ones, whichever [`bind_callback_listener`] got. `None` only if the
    /// socket cannot name itself, which costs a hint and nothing more.
    fn callback_port(&self) -> Option<u16> {
        self.listener.local_addr().ok().map(|addr| addr.port())
    }
}

/// Bind the (registered) callback port and build the authorize URL. The
/// listener is held in the returned [`PendingLogin`]; [`wait_and_complete`]
/// consumes it.
pub fn begin_login() -> Result<PendingLogin> {
    let (listener, port) = bind_callback_listener()?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");

    let pkce = generate_pkce()?;
    let state = random_state()?;

    let mut url = reqwest::Url::parse(AUTHORIZE_URL).context("parsing the authorize URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", &state)
        .append_pair("originator", ORIGINATOR);

    Ok(PendingLogin {
        authorize_url: url.to_string(),
        state,
        redirect_uri,
        verifier: pkce.verifier,
        listener,
    })
}

/// Wait for the browser to hit the callback, exchange the code, and persist the
/// tokens. Consumes the pending login (and its listener).
///
/// `cancel` abandons the wait and gives the port back at once — the GUI fires
/// it when a second sign-in replaces this one. A caller with no competition for
/// the port passes [`Cancel::never`].
pub async fn wait_and_complete(pending: PendingLogin, cancel: Cancel) -> Result<StoredTokens> {
    wait_and_complete_with_paste(pending, cancel, PasteChannel::Disabled).await
}

/// [`wait_and_complete`], with the option of taking the redirect off stdin as
/// well as off the listener — the only way through for a session whose browser
/// is on another machine and no tunnel between them.
pub async fn wait_and_complete_with_paste(
    pending: PendingLogin,
    cancel: Cancel,
    paste: PasteChannel,
) -> Result<StoredTokens> {
    let PendingLogin {
        state,
        redirect_uri,
        verifier,
        listener,
        ..
    } = pending;
    let expected = state.clone();
    let spec = matches!(paste, PasteChannel::Stdin).then(|| oauth_callback::PasteSpec {
        callback_path: CALLBACK_PATH,
        state: expected.clone(),
    });
    let code = oauth_callback::serve_redirect_or_paste(
        listener,
        cancel,
        |target| parse_callback(target, &expected),
        spec,
    )
    .await?;

    let token = exchange_code(&code, &redirect_uri, &verifier).await?;
    let account_id = token.id_token.as_deref().and_then(account_id_from_id_token);
    let stored = StoredTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        account_id,
    };
    save_tokens(&token_path()?, &stored)?;
    Ok(stored)
}

/// The provider entry a completed sign-in earns.
pub fn provider_config() -> crate::config::ProviderConfig {
    crate::config::ProviderConfig {
        name: "chatgpt".to_string(),
        kind: crate::config::ProviderKind::CHATGPT_OAUTH,
        base_url: BASE_URL.to_string(),
        model: DEFAULT_MODEL.to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }
}

/// The self-contained terminal flow (`wizard --login chatgpt`): open the
/// browser, wait, exchange. `report` receives progress lines.
///
/// `paste` says whether this caller owns stdin and can therefore offer the
/// remote-session fallback; see [`PasteChannel`].
pub async fn login<F>(report: F, paste: PasteChannel) -> Result<()>
where
    F: Fn(&str) + Send + Sync,
{
    let pending = begin_login()?;
    report(&format!(
        "open this URL to sign in with your ChatGPT account:\n{}",
        pending.authorize_url
    ));
    open_browser(&pending.authorize_url);
    if let Some(hint) = pending
        .callback_port()
        .and_then(oauth_callback::remote_hint)
    {
        report(&hint);
    }
    report("waiting for the browser callback (5 minute timeout)...");
    // Nothing else in a terminal run competes for the callback port.
    wait_and_complete_with_paste(pending, Cancel::never(), paste).await?;
    report(&format!(
        "signed in to ChatGPT; tokens saved to {}",
        token_path()?.display()
    ));
    Ok(())
}

/// The token endpoint's response (from both the code exchange and refresh).
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Exchange the authorization `code` for tokens (form-encoded, per OAuth).
async fn exchange_code(code: &str, redirect_uri: &str, verifier: &str) -> Result<TokenResponse> {
    let http = crate::llm::oauth_http_client();
    let response = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("exchanging the authorization code")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("ChatGPT token exchange failed (HTTP {status}): {body}");
    }
    response
        .json()
        .await
        .context("parsing the ChatGPT token response")
}

/// The token endpoint refused the grant (HTTP 400/401): it is revoked or
/// expired for good, so refreshing again can never succeed. The caller clears
/// the stored tokens so the next run re-prompts for sign-in.
#[derive(Debug, thiserror::Error)]
#[error(
    "the ChatGPT session was revoked or expired (HTTP {status}: {body}); \
     run `wizard --login chatgpt` to sign in again"
)]
pub struct RevokedGrant {
    pub status: u16,
    pub body: String,
}

/// Refresh an access token (JSON body, per Codex). Returns the tokens to
/// persist; the caller merges them (a refresh may omit the refresh token).
///
/// Every failure here is typed, because this runs on the way into a model call
/// and whatever it raises is what the agent's retry ladder has to classify. An
/// untyped error lands on
/// [`error_is_transient`](crate::agent::error_is_transient)'s permissive
/// fallback and is retried, which is the wrong answer for exactly the one case
/// that matters: [`RevokedGrant`] is permanent, and retrying it burns the
/// backoff ladder and a circuit-breaker trip before the user is finally shown
/// the line telling them to sign in again. The reachability failures go the
/// other way and stay retryable: the token host being unreachable for a moment
/// must not end a run that has been going for an hour.
pub async fn refresh(refresh_token: &str) -> Result<TokenResponse> {
    let http = crate::llm::oauth_http_client();
    let response = http
        .post(TOKEN_URL)
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|source| {
            anyhow::Error::new(crate::llm::ProviderError::transport(format!(
                "could not reach {TOKEN_URL} to refresh the ChatGPT token: {source}"
            )))
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED {
        let body = response.text().await.unwrap_or_default();
        let revoked = RevokedGrant {
            status: status.as_u16(),
            body,
        };
        // The typed `ProviderError` rides *under* `RevokedGrant` on the chain,
        // the same arrangement `http_error_with_retry_after` uses: the
        // caller's `err.is::<RevokedGrant>()` still finds it, the message the
        // user sees is still the revoked-grant one, and the retry class is now
        // readable too.
        return Err(
            anyhow::Error::new(crate::llm::ProviderError::http(401, revoked.to_string()))
                .context(revoked),
        );
    }
    if !status.is_success() {
        let retry_after = crate::llm::retry_after_from_headers(response.headers());
        let body = response.text().await.unwrap_or_default();
        return Err(crate::llm::http_error_with_retry_after(
            status.as_u16(),
            format!("ChatGPT token refresh failed (HTTP {status}): {body}"),
            retry_after,
        ));
    }
    response.json().await.map_err(|source| {
        anyhow::Error::new(crate::llm::ProviderError::transport(format!(
            "could not parse the ChatGPT refresh response: {source}"
        )))
    })
}

/// True when `access_token` expires within [`EXPIRY_LEEWAY_SECS`]. A token with
/// no readable `exp` is treated as live; the API's 401 path forces a refresh.
pub fn expires_soon(access_token: &str) -> bool {
    match jwt_exp(access_token) {
        Some(exp) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            exp <= now + EXPIRY_LEEWAY_SECS
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Localhost callback listener
// ---------------------------------------------------------------------------

fn random_state() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("gathering randomness: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The ports the callback listener may bind, in order of preference: the two
/// OpenAI has on file, always. Neither is a preference of ours, so there is
/// nothing to configure and nothing to get wrong.
#[cfg(not(test))]
fn callback_ports() -> [u16; 2] {
    [CALLBACK_PORT, FALLBACK_PORT]
}

/// Under test, two private ports stand in for the registered ones — chosen once,
/// so they behave exactly like the fixed pair they replace (tests that bind them
/// still queue on [`oauth_callback::serial_callback_port`]).
///
/// The registered ports are machine-wide, and a sign-in the user actually has in
/// flight owns them for real; the suite must neither take them from it nor fail
/// because it has them. Production is untouched: OpenAI still redirects only to
/// the registered pair, and the release build has no override to set.
#[cfg(test)]
fn callback_ports() -> [u16; 2] {
    static PORTS: std::sync::OnceLock<[u16; 2]> = std::sync::OnceLock::new();
    *PORTS.get_or_init(|| {
        let ports = [
            oauth_callback::private_test_port(),
            oauth_callback::private_test_port(),
        ];
        for registered in [CALLBACK_PORT, FALLBACK_PORT] {
            assert!(
                !ports.contains(&registered),
                "the suite must never bind a registered callback port"
            );
        }
        ports
    })
}

/// Bind the preferred registered port, then the registered fallback. Both are
/// addresses OpenAI has on file for this client, so either produces a
/// redirect_uri the authorize endpoint will accept.
fn bind_callback_listener() -> Result<(TcpListener, u16)> {
    let ports = callback_ports();
    for port in ports {
        // Wait out a teardown before giving up on this port. Closing a listener
        // does not hand its port back synchronously, so the sign-in that
        // *replaces* a cancelled one — the GUI's retry — can find the port it
        // just released still in the kernel's bind table. Without the grace the
        // preferred port reads as taken and the flow quietly drifts onto the
        // fallback, which is a worse address to be on for no reason at all.
        let deadline = Instant::now() + oauth_callback::REBIND_GRACE;
        loop {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => return Ok((listener, port)),
                Err(err)
                    if err.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    }
    let [preferred, fallback] = ports;
    bail!(
        "could not bind the sign-in callback port ({preferred} or {fallback}); \
         is another Codex/wizard sign-in already running?"
    )
}

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

/// Classify a request target (`/auth/callback?code=…&state=…`).
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignored;
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Ignored;
    }
    let (mut code, mut state, mut error, mut error_desc) = (None, None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_desc = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        let detail = error_desc.unwrap_or(error);
        // OpenAI surfaces a missing Codex entitlement here rather than at the API.
        if detail.contains("missing_codex_entitlement") {
            return Callback::Failed(
                "this ChatGPT plan does not include Codex/API access".to_string(),
            );
        }
        return Callback::Failed(format!("OpenAI returned an error: {detail}"));
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Failed("the sign-in state did not match; aborting".to_string());
    }
    match code {
        Some(code) => Callback::Code(code),
        None => Callback::Failed("the callback carried no authorization code".to_string()),
    }
}

/// How `kind = "chatgptoauth"` is registered.
///
/// A ChatGPT subscription is not the Chat Completions API — it is the
/// Responses API behind account tokens — so it is the one cloud backend with
/// a client of its own rather than a configuration of the shared one.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::CHATGPT_OAUTH,
        "ChatGPT",
        Credentials::Account {
            login: "chatgpt".to_string(),
        },
        |config| {
            Ok(std::sync::Arc::new(
                crate::llm::chatgpt::ChatgptProvider::new(
                    config.base_url.clone(),
                    config.model.clone(),
                )
                .context("setting up ChatGPT OAuth token storage")?,
            ))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn id_token_with(auth_claim: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = URL_SAFE_NO_PAD.encode(
            json!({ "https://api.openai.com/auth": auth_claim })
                .to_string()
                .as_bytes(),
        );
        format!("{header}.{payload}.sig")
    }

    /// The store goes through `platform::secrets`, which owns the scratch name.
    ///
    /// The name this module used to hard-code, `.chatgpt_oauth.json.tmp`, was
    /// opened with `truncate(true)`: anything already at that name was
    /// overwritten. That is both how two concurrent refreshes spliced one token
    /// file — ordinary on a machine running two sessions, since an expired
    /// access token has both of them refreshing at once — and how a name
    /// planted by another local user would have redirected the write. Planting
    /// that exact name is therefore the observation that the old sequence is
    /// gone; `xai_oauth` carries the same test for the same reason.
    #[test]
    fn saving_tokens_never_reuses_the_old_fixed_scratch_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("wizard-home");
        std::fs::create_dir_all(&home).expect("mkdir");
        let planted = home.join(".chatgpt_oauth.json.tmp");
        std::fs::write(&planted, b"not ours").expect("plant the old scratch name");

        let path = home.join("chatgpt_oauth.json");
        save_tokens(
            &path,
            &StoredTokens {
                access_token: "at".to_string(),
                refresh_token: Some("rt".to_string()),
                id_token: None,
                account_id: None,
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

        // The write cleans up after itself: nothing new but the token file.
        let mut left: Vec<String> = std::fs::read_dir(&home)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![".chatgpt_oauth.json.tmp", "chatgpt_oauth.json"],
            "{left:?}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "tokens stay owner-only");
        }
    }

    /// Concurrent refreshes publish one writer's file, never a splice.
    #[test]
    fn concurrent_saves_never_publish_a_spliced_token_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("wizard-home");
        std::fs::create_dir_all(&home).expect("mkdir");
        let path = home.join("chatgpt_oauth.json");

        // Long enough that one write is many syscalls, so a splice shows up as
        // a mixed file rather than needing a lucky interleaving.
        let tokens: Vec<StoredTokens> = (0..8u8)
            .map(|n| StoredTokens {
                access_token: std::iter::repeat_n(char::from(b'a' + n), 200_000).collect(),
                refresh_token: None,
                id_token: None,
                account_id: None,
            })
            .collect();

        std::thread::scope(|scope| {
            for token in &tokens {
                scope.spawn(|| save_tokens(&path, token).expect("save"));
            }
        });

        let landed = load_tokens(&path).expect("parses").expect("present");
        assert!(
            tokens
                .iter()
                .any(|token| token.access_token == landed.access_token),
            "the published file is not any one writer's"
        );
    }

    #[test]
    fn account_id_comes_from_the_auth_claim() {
        let token =
            id_token_with(json!({ "chatgpt_account_id": "acct-123", "chatgpt_plan_type": "pro" }));
        assert_eq!(
            account_id_from_id_token(&token).as_deref(),
            Some("acct-123")
        );
    }

    #[test]
    fn account_id_is_none_without_the_claim() {
        assert_eq!(account_id_from_id_token("not.a.jwt"), None);
        let token = id_token_with(json!({ "chatgpt_plan_type": "pro" }));
        assert_eq!(account_id_from_id_token(&token), None);
    }

    #[test]
    fn clear_tokens_removes_the_file_and_tolerates_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt_oauth.json");
        let tokens = StoredTokens {
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            id_token: None,
            account_id: None,
        };
        save_tokens(&path, &tokens).expect("save");
        assert!(load_tokens(&path).expect("load").is_some());

        clear_tokens(&path).expect("clear");
        assert!(load_tokens(&path).expect("load after clear").is_none());
        clear_tokens(&path).expect("clearing a missing file is fine");
    }

    #[test]
    fn callback_requires_matching_state_and_a_code() {
        assert!(matches!(
            parse_callback("/auth/callback?code=c&state=s", "s"),
            Callback::Code(c) if c == "c"
        ));
        assert!(matches!(
            parse_callback("/auth/callback?code=c&state=wrong", "s"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            parse_callback("/auth/callback?state=s", "s"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            parse_callback("/favicon.ico", "s"),
            Callback::Ignored
        ));
    }

    #[test]
    fn a_denied_callback_reports_the_error() {
        assert!(matches!(
            parse_callback("/auth/callback?error=access_denied&error_description=nope", "s"),
            Callback::Failed(m) if m.contains("nope")
        ));
        assert!(matches!(
            parse_callback("/auth/callback?error=x&error_description=missing_codex_entitlement", "s"),
            Callback::Failed(m) if m.contains("Codex")
        ));
    }

    /// A second sign-in on the same machine takes the registered fallback, and a
    /// third has nowhere left to go: both ports are registered addresses, so
    /// there is no third one to drift onto, and the failure must name the pair
    /// rather than hang on a port OpenAI never redirects to.
    #[test]
    fn the_callback_falls_back_once_and_then_names_both_ports() {
        let _serial = oauth_callback::serial_callback_port();
        let [preferred, fallback] = callback_ports();

        let (first, port) = bind_callback_listener().expect("the preferred port is free");
        assert_eq!(port, preferred);
        let (second, port) = bind_callback_listener().expect("the fallback port is free");
        assert_eq!(port, fallback);

        let err = bind_callback_listener().expect_err("both ports are taken");
        let message = format!("{err:#}");
        assert!(message.contains(&preferred.to_string()), "{message}");
        assert!(message.contains(&fallback.to_string()), "{message}");

        // Leave the shared ports as we found them.
        drop((first, second));
    }
}
