//! Hand an existing provider sign-in to Wizard.
//!
//! Wizard keeps its providers in `~/.wizard/config.toml` (`[[providers]]` +
//! `active_provider`) and their secrets beside it: OAuth tokens in their own
//! owner-only JSON files (`xai_oauth.json`, `chatgpt_oauth.json`) and API keys
//! in `credentials.toml` `[keys]`. This module reads sign-ins other CLIs
//! already stored on disk (Codex's `~/.codex/auth.json`, Grok's
//! `~/.grok/auth.json`) or Wizard's own active provider, packs them into a
//! [`WizardAuthBundle`], and writes a bundle into a Wizard home in the same
//! shapes Wizard's own sign-in flows produce — so onboarding can reuse a local
//! login and SSH setup can carry one to another machine.
//!
//! Secret values never reach logs, errors, or [`DetectedCredential`].

pub mod providers;
pub mod usage;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a sign-in comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialSource {
    /// Codex CLI's `auth.json`: a ChatGPT login or an OpenAI API key.
    Codex,
    /// Grok CLI's `auth.json`: an xAI account login.
    Grok,
    /// Wizard's own active provider (for sharing it with another machine).
    Wizard,
}

/// A sign-in found on disk. Carries no secret material.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedCredential {
    pub source: CredentialSource,
    /// e.g. "ChatGPT account from Codex CLI".
    pub title: String,
    /// Non-secret detail: email, plan, provider name.
    pub detail: Option<String>,
    pub path: PathBuf,
}

/// A Wizard provider entry plus the secrets it needs, ready to [`apply`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardAuthBundle {
    source: CredentialSource,
    provider: ProviderEntry,
    /// Owner-only files to write under the Wizard home, by file name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    files: BTreeMap<String, String>,
    /// `credentials.toml` `[keys]` entries, by provider name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    keys: BTreeMap<String, String>,
}

impl WizardAuthBundle {
    /// Name of the provider [`apply`] activates.
    pub fn provider_name(&self) -> &str {
        &self.provider.name
    }
}

/// One `[[providers]]` table of Wizard's config (its `ProviderConfig`, minus
/// `gguf_path`: a local model file never travels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderEntry {
    name: String,
    kind: String,
    base_url: String,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usd_per_mtok_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usd_per_mtok_out: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vision: Option<bool>,
}

// Mirrors of Wizard's own constants (src/llm/xai_oauth.rs,
// src/plugins/chatgpt/oauth.rs, src/onboarding/mod.rs).
const XAI_PROVIDER: &str = "xai-oauth";
const XAI_KIND: &str = "xaioauth";
const XAI_BASE_URL: &str = "https://api.x.ai/v1";
/// Offline floor, not a pin. Matches `llm::xai_oauth::DEFAULT_MODEL`. The GUI
/// crate cannot import the binary crate, so the string is duplicated; a test
/// in that crate is what keeps the two from drifting.
const XAI_MODEL: &str = "grok-4.7";
/// Wizard refreshes with this client id; Grok CLI's login shares it.
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_TOKEN_FILE: &str = "xai_oauth.json";

const CHATGPT_PROVIDER: &str = "chatgpt";
const CHATGPT_KIND: &str = "chatgptoauth";
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CHATGPT_MODEL: &str = "gpt-6-astra";
const CHATGPT_TOKEN_FILE: &str = "chatgpt_oauth.json";

const OPENAI_PROVIDER: &str = "openai";
const OPENAI_KIND: &str = "openai";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_MODEL: &str = "gpt-5.6-sol";

const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_FILE: &str = "credentials.toml";

/// The only files a bundle may write: it can arrive over SSH on stdin.
const SECRET_FILES: [&str; 2] = [XAI_TOKEN_FILE, CHATGPT_TOKEN_FILE];

/// `$WIZARD_HOME`, else `~/.wizard`.
pub fn wizard_home() -> PathBuf {
    Homes::from_env().wizard
}

/// Sign-ins on this machine that Wizard can use: Wizard's own active provider
/// (when its credentials are present), then Codex, then Grok.
pub fn detect() -> Vec<DetectedCredential> {
    Homes::from_env().detect()
}

/// Pack the sign-in from `source` into a bundle.
pub fn bundle_from(source: CredentialSource) -> Result<WizardAuthBundle> {
    Homes::from_env().bundle_from(source)
}

/// Write `bundle` into the Wizard home at `wizard_home`, activating its
/// provider. Returns the provider's name.
pub fn apply(bundle: &WizardAuthBundle, wizard_home: &Path) -> Result<String> {
    for name in bundle.files.keys() {
        if !SECRET_FILES.contains(&name.as_str()) {
            bail!("refusing to write unexpected file '{name}' into the Wizard home");
        }
    }
    let provider = &bundle.provider;
    if provider.name.trim().is_empty() || provider.kind.trim().is_empty() {
        bail!("the credential bundle has no provider name or kind");
    }
    if let Some(tokens) = bundle.files.get(XAI_TOKEN_FILE) {
        let endpoint = serde_json::from_str::<Value>(tokens)
            .ok()
            .and_then(|v| v.get("token_endpoint")?.as_str().map(str::to_string))
            .ok_or_else(|| anyhow!("the xAI sign-in has no token endpoint"))?;
        validate_xai_https(&endpoint)?;
    }

    ensure_private_dir(wizard_home)?;
    for (name, contents) in &bundle.files {
        write_atomic(&wizard_home.join(name), contents.as_bytes(), true)
            .with_context(|| format!("could not save {name}"))?;
    }
    if !bundle.keys.is_empty() {
        merge_keys(&wizard_home.join(CREDENTIALS_FILE), &bundle.keys)?;
    }
    upsert_provider(&wizard_home.join(CONFIG_FILE), provider)?;
    Ok(provider.name.clone())
}

/// Import the sign-in from `source` into this machine's Wizard home.
pub fn import(source: CredentialSource) -> Result<String> {
    let homes = Homes::from_env();
    let bundle = homes.bundle_from(source)?;
    apply(&bundle, &homes.wizard)
}

/// Pi keeps sign-ins in `<agent-dir>/auth.json`, keyed by provider id. Its
/// `openai-codex` and `xai` subscription logins use the same OAuth clients as
/// the Codex and Grok CLIs (and Wizard's own logins), so those tokens carry
/// over as they are.
pub fn pi_agent_dir() -> PathBuf {
    Homes::from_env().pi
}

/// Provider ids Pi already holds a credential for.
pub fn pi_signed_in() -> Vec<String> {
    Homes::from_env().pi_signed_in()
}

/// Sign-ins on this machine Pi can take over, in [`detect`] order.
pub fn pi_import_candidates() -> Vec<DetectedCredential> {
    let homes = Homes::from_env();
    homes
        .detect()
        .into_iter()
        .filter(|found| homes.pi_credential(found.source).is_ok())
        .collect()
}

/// Hand a sign-in to Pi; returns the Pi provider id it now uses. New Pi
/// sessions start on that provider unless Pi already has a default.
pub fn import_into_pi(source: CredentialSource) -> Result<String> {
    Homes::from_env().import_into_pi(source)
}

/// Environment lookup for API keys held in env vars.
type EnvLookup = Box<dyn Fn(&str) -> Option<String>>;

/// Resolved config roots, injectable so tests never touch the real homes.
struct Homes {
    wizard: PathBuf,
    codex: PathBuf,
    grok: PathBuf,
    /// Pi's agent dir (`PI_CODING_AGENT_DIR`, else `~/.pi/agent`).
    pi: PathBuf,
    env: EnvLookup,
}

impl Homes {
    fn from_env() -> Self {
        Self::resolve(|name| std::env::var_os(name), home_dir())
    }

    fn resolve(env: impl Fn(&str) -> Option<OsString> + 'static, home: PathBuf) -> Self {
        let dir = |var: &str, fallback: &str| {
            env(var)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        Self {
            wizard: dir("WIZARD_HOME", ".wizard"),
            codex: dir("CODEX_HOME", ".codex"),
            grok: dir("GROK_HOME", ".grok"),
            pi: dir("PI_CODING_AGENT_DIR", ".pi/agent"),
            env: Box::new(move |name| {
                env(name)
                    .and_then(|value| value.into_string().ok())
                    .filter(|value| !value.trim().is_empty())
            }),
        }
    }

    fn detect(&self) -> Vec<DetectedCredential> {
        let mut found = Vec::new();
        if let Some(wizard) = self.detect_wizard() {
            found.push(wizard);
        }
        if let Ok(codex) = self.read_codex() {
            let path = self.codex.join("auth.json");
            found.push(match codex {
                CodexLogin::ChatGpt { id_token, .. } => {
                    let claims = id_token.as_deref().and_then(jwt_claims);
                    let email = claims
                        .as_ref()
                        .and_then(|c| c.get("email")?.as_str().map(str::to_string));
                    let plan = claims.as_ref().and_then(|c| {
                        c.get("https://api.openai.com/auth")?
                            .get("chatgpt_plan_type")?
                            .as_str()
                            .map(str::to_string)
                    });
                    DetectedCredential {
                        source: CredentialSource::Codex,
                        title: "ChatGPT account from Codex CLI".into(),
                        detail: join_detail([email, plan.map(|p| format!("{p} plan"))]),
                        path,
                    }
                }
                CodexLogin::ApiKey(_) => DetectedCredential {
                    source: CredentialSource::Codex,
                    title: "OpenAI API key from Codex CLI".into(),
                    detail: None,
                    path,
                },
            });
        }
        if let Ok(grok) = self.read_grok() {
            found.push(DetectedCredential {
                source: CredentialSource::Grok,
                title: "xAI account from Grok CLI".into(),
                detail: grok.email,
                path: self.grok.join("auth.json"),
            });
        }
        found
    }

    fn detect_wizard(&self) -> Option<DetectedCredential> {
        let path = self.wizard.join(CONFIG_FILE);
        let provider = self.active_wizard_provider().ok()??;
        let ready = if is_local(&provider) {
            true
        } else {
            match provider.kind.as_str() {
                XAI_KIND => self.wizard.join(XAI_TOKEN_FILE).is_file(),
                CHATGPT_KIND => self.wizard.join(CHATGPT_TOKEN_FILE).is_file(),
                _ => self.wizard_key(&provider).is_some(),
            }
        };
        ready.then(|| DetectedCredential {
            source: CredentialSource::Wizard,
            title: format!("Wizard is already signed in ({})", provider.name),
            detail: Some(format!("{} · {}", provider.kind, provider.model)),
            path,
        })
    }

    fn bundle_from(&self, source: CredentialSource) -> Result<WizardAuthBundle> {
        match source {
            CredentialSource::Codex => self.codex_bundle(),
            CredentialSource::Grok => self.grok_bundle(),
            CredentialSource::Wizard => self.wizard_bundle(),
        }
    }

    fn codex_bundle(&self) -> Result<WizardAuthBundle> {
        match self.read_codex()? {
            CodexLogin::ChatGpt {
                access_token,
                refresh_token,
                id_token,
                account_id,
            } => {
                let account_id = account_id
                    .or_else(|| id_token.as_deref().and_then(chatgpt_account_id))
                    .ok_or_else(|| {
                        anyhow!("Codex's ChatGPT login has no account id; run `codex login` again")
                    })?;
                let mut tokens = serde_json::Map::new();
                tokens.insert("access_token".into(), access_token.into());
                if let Some(refresh) = refresh_token {
                    tokens.insert("refresh_token".into(), refresh.into());
                }
                if let Some(id_token) = id_token {
                    tokens.insert("id_token".into(), id_token.into());
                }
                tokens.insert("account_id".into(), account_id.into());
                Ok(WizardAuthBundle {
                    source: CredentialSource::Codex,
                    provider: ProviderEntry::new(
                        CHATGPT_PROVIDER,
                        CHATGPT_KIND,
                        CHATGPT_BASE_URL,
                        CHATGPT_MODEL,
                    ),
                    files: BTreeMap::from([(
                        CHATGPT_TOKEN_FILE.to_string(),
                        pretty_json(&Value::Object(tokens)),
                    )]),
                    keys: BTreeMap::new(),
                })
            }
            CodexLogin::ApiKey(key) => Ok(WizardAuthBundle {
                source: CredentialSource::Codex,
                provider: ProviderEntry::new(
                    OPENAI_PROVIDER,
                    OPENAI_KIND,
                    OPENAI_BASE_URL,
                    OPENAI_MODEL,
                ),
                files: BTreeMap::new(),
                keys: BTreeMap::from([(OPENAI_PROVIDER.to_string(), key)]),
            }),
        }
    }

    fn grok_bundle(&self) -> Result<WizardAuthBundle> {
        let login = self.read_grok()?;
        let token_endpoint = format!("{}/oauth2/token", login.issuer.trim_end_matches('/'));
        validate_xai_https(&token_endpoint)?;
        let mut tokens = serde_json::Map::new();
        tokens.insert("access_token".into(), login.access_token.into());
        if let Some(refresh) = login.refresh_token {
            tokens.insert("refresh_token".into(), refresh.into());
        }
        tokens.insert("token_type".into(), "Bearer".into());
        tokens.insert("token_endpoint".into(), token_endpoint.into());
        Ok(WizardAuthBundle {
            source: CredentialSource::Grok,
            provider: ProviderEntry::new(XAI_PROVIDER, XAI_KIND, XAI_BASE_URL, XAI_MODEL),
            files: BTreeMap::from([(
                XAI_TOKEN_FILE.to_string(),
                pretty_json(&Value::Object(tokens)),
            )]),
            keys: BTreeMap::new(),
        })
    }

    fn wizard_bundle(&self) -> Result<WizardAuthBundle> {
        let provider = self
            .active_wizard_provider()?
            .ok_or_else(|| anyhow!("Wizard has no provider set up on this machine yet"))?;
        if is_local(&provider) {
            bail!("Wizard's active provider runs locally and can't be shared");
        }
        let mut files = BTreeMap::new();
        let mut keys = BTreeMap::new();
        match provider.kind.as_str() {
            XAI_KIND | CHATGPT_KIND => {
                let file = if provider.kind == XAI_KIND {
                    XAI_TOKEN_FILE
                } else {
                    CHATGPT_TOKEN_FILE
                };
                let contents = std::fs::read_to_string(self.wizard.join(file)).map_err(|_| {
                    anyhow!(
                        "Wizard's {} sign-in is missing; sign in again before sharing it",
                        provider.name
                    )
                })?;
                serde_json::from_str::<Value>(&contents)
                    .map_err(|_| anyhow!("Wizard's {file} is not valid JSON"))?;
                files.insert(file.to_string(), contents);
            }
            _ => {
                let key = self
                    .wizard_key(&provider)
                    .ok_or_else(|| anyhow!("Wizard has no stored API key for {}", provider.name))?;
                keys.insert(provider.name.clone(), key);
            }
        }
        Ok(WizardAuthBundle {
            source: CredentialSource::Wizard,
            provider,
            files,
            keys,
        })
    }

    /// The provider Wizard would run: `active_provider`, else the first.
    fn active_wizard_provider(&self) -> Result<Option<ProviderEntry>> {
        let path = self.wizard.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("{} is not valid TOML", path.display()))?;
        let providers: Vec<ProviderEntry> = doc
            .get("providers")
            .and_then(|item| item.as_array_of_tables())
            .map(|tables| {
                tables
                    .iter()
                    .filter_map(ProviderEntry::from_table)
                    .collect()
            })
            .unwrap_or_default();
        let active = doc.get("active_provider").and_then(|item| item.as_str());
        Ok(active
            .and_then(|name| providers.iter().find(|p| p.name == name).cloned())
            .or_else(|| providers.into_iter().next()))
    }

    /// A key-based provider's key, resolved the way Wizard does: its
    /// `api_key_env`, then its kind's default variable, then credentials.toml.
    fn wizard_key(&self, provider: &ProviderEntry) -> Option<String> {
        let env_name = match provider.api_key_env.as_deref() {
            Some("") => None,
            Some(name) => Some(name),
            None => default_key_env(&provider.kind),
        };
        env_name.and_then(|name| (self.env)(name)).or_else(|| {
            let text = std::fs::read_to_string(self.wizard.join(CREDENTIALS_FILE)).ok()?;
            let doc: toml_edit::DocumentMut = text.parse().ok()?;
            doc.get("keys")?
                .get(&provider.name)?
                .as_str()
                .map(str::to_string)
                .filter(|key| !key.trim().is_empty())
        })
    }

    fn pi_signed_in(&self) -> Vec<String> {
        let Ok(auth) = read_json(&self.pi.join("auth.json"), "Pi") else {
            return Vec::new();
        };
        auth.as_object()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(_, entry)| entry.is_object())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The `auth.json` entry Pi stores for this sign-in, keyed by its
    /// provider id — the shapes Pi's own `/login` flows write.
    fn pi_credential(&self, source: CredentialSource) -> Result<(String, Value)> {
        let oauth = |access: String, refresh: Option<String>, what: &str| -> Result<Value> {
            let refresh = refresh.ok_or_else(|| {
                anyhow!("{what} has no refresh token, so Pi couldn't keep it signed in")
            })?;
            Ok(serde_json::json!({
                "type": "oauth",
                "expires": jwt_expiry_ms(&access),
                "access": access,
                "refresh": refresh,
            }))
        };
        let codex_oauth = |access: String,
                           refresh: Option<String>,
                           account: Option<String>,
                           what: &str|
         -> Result<Value> {
            // Pi reads the account from the access token; fall back to the
            // one the source stored beside it.
            let account = chatgpt_account_id(&access)
                .or(account)
                .ok_or_else(|| anyhow!("{what} has no ChatGPT account id"))?;
            let mut credential = oauth(access, refresh, what)?;
            credential["accountId"] = account.into();
            Ok(credential)
        };
        match source {
            CredentialSource::Codex => match self.read_codex()? {
                CodexLogin::ChatGpt {
                    access_token,
                    refresh_token,
                    id_token,
                    account_id,
                } => {
                    let account =
                        account_id.or_else(|| id_token.as_deref().and_then(chatgpt_account_id));
                    Ok((
                        "openai-codex".into(),
                        codex_oauth(access_token, refresh_token, account, "Codex's login")?,
                    ))
                }
                CodexLogin::ApiKey(key) => Ok((
                    "openai".into(),
                    serde_json::json!({"type": "api_key", "key": key}),
                )),
            },
            CredentialSource::Grok => {
                let login = self.read_grok()?;
                Ok((
                    "xai".into(),
                    oauth(login.access_token, login.refresh_token, "Grok's login")?,
                ))
            }
            CredentialSource::Wizard => {
                let provider = self
                    .active_wizard_provider()?
                    .ok_or_else(|| anyhow!("Wizard has no provider set up on this machine yet"))?;
                if is_local(&provider) {
                    bail!("Wizard's active provider runs locally, so Pi can't reuse it");
                }
                let token = |file: &str| -> Result<(String, Option<String>, Option<String>)> {
                    let tokens = read_json(&self.wizard.join(file), "Wizard")?;
                    let text = |key: &str| {
                        tokens
                            .get(key)
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string)
                    };
                    let access = text("access_token")
                        .ok_or_else(|| anyhow!("Wizard's {file} has no access token"))?;
                    Ok((access, text("refresh_token"), text("account_id")))
                };
                match provider.kind.as_str() {
                    XAI_KIND => {
                        let (access, refresh, _) = token(XAI_TOKEN_FILE)?;
                        Ok(("xai".into(), oauth(access, refresh, "Wizard's xAI login")?))
                    }
                    CHATGPT_KIND => {
                        let (access, refresh, account) = token(CHATGPT_TOKEN_FILE)?;
                        Ok((
                            "openai-codex".into(),
                            codex_oauth(access, refresh, account, "Wizard's ChatGPT login")?,
                        ))
                    }
                    kind => {
                        let id = match kind {
                            "xai" | "openai" | "anthropic" | "openrouter" => kind,
                            _ => bail!("Pi can't use Wizard's {kind} provider"),
                        };
                        let key = self.wizard_key(&provider).ok_or_else(|| {
                            anyhow!("Wizard has no stored API key for {}", provider.name)
                        })?;
                        Ok((
                            id.into(),
                            serde_json::json!({"type": "api_key", "key": key}),
                        ))
                    }
                }
            }
        }
    }

    fn import_into_pi(&self, source: CredentialSource) -> Result<String> {
        let (id, credential) = self.pi_credential(source)?;
        ensure_private_dir(&self.pi)?;
        let read_object = |path: &Path| -> Result<serde_json::Map<String, Value>> {
            match std::fs::read_to_string(path) {
                Ok(text) if text.trim().is_empty() => Ok(serde_json::Map::new()),
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(Value::Object(map)) => Ok(map),
                    _ => bail!("{} is not a JSON object", path.display()),
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    Ok(serde_json::Map::new())
                }
                Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
            }
        };
        let auth_path = self.pi.join("auth.json");
        let mut auth = read_object(&auth_path)?;
        auth.insert(id.clone(), credential);
        write_atomic(
            &auth_path,
            pretty_json(&Value::Object(auth)).as_bytes(),
            true,
        )?;
        let settings_path = self.pi.join("settings.json");
        let mut settings = read_object(&settings_path)?;
        if !settings.contains_key("defaultProvider") {
            settings.insert("defaultProvider".into(), id.clone().into());
            write_atomic(
                &settings_path,
                pretty_json(&Value::Object(settings)).as_bytes(),
                false,
            )?;
        }
        Ok(id)
    }

    fn read_codex(&self) -> Result<CodexLogin> {
        let path = self.codex.join("auth.json");
        let auth = read_json(&path, "Codex CLI")?;
        if let Some(tokens) = auth.get("tokens").filter(|t| t.is_object()) {
            let text = |key: &str| {
                tokens
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            if let Some(access_token) = text("access_token") {
                return Ok(CodexLogin::ChatGpt {
                    access_token,
                    refresh_token: text("refresh_token"),
                    id_token: text("id_token"),
                    account_id: text("account_id"),
                });
            }
        }
        match auth.get("OPENAI_API_KEY").and_then(Value::as_str) {
            Some(key) if !key.trim().is_empty() => Ok(CodexLogin::ApiKey(key.trim().to_string())),
            _ => bail!("Codex CLI is not signed in (run `codex login`)"),
        }
    }

    fn read_grok(&self) -> Result<GrokLogin> {
        let path = self.grok.join("auth.json");
        let auth = read_json(&path, "Grok CLI")?;
        let entries = auth
            .as_object()
            .ok_or_else(|| anyhow!("Grok CLI's auth.json has an unexpected shape"))?;
        let usable = |entry: &Value| {
            entry
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty())
        };
        let client = |entry: &Value| {
            entry
                .get("oidc_client_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let entry = entries
            .values()
            .filter(|entry| usable(entry))
            .find(|entry| client(entry).as_deref() == Some(XAI_CLIENT_ID));
        let Some(entry) = entry else {
            if entries.values().any(usable) {
                bail!(
                    "Grok CLI's sign-in can't be refreshed by Wizard; sign in with `wizard --login xai` instead"
                );
            }
            bail!("Grok CLI is not signed in (run `grok login`)");
        };
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Ok(GrokLogin {
            access_token: text("key").unwrap_or_default(),
            refresh_token: text("refresh_token"),
            issuer: text("oidc_issuer").unwrap_or_else(|| "https://auth.x.ai".into()),
            email: text("email"),
        })
    }
}

enum CodexLogin {
    ChatGpt {
        access_token: String,
        refresh_token: Option<String>,
        id_token: Option<String>,
        account_id: Option<String>,
    },
    ApiKey(String),
}

struct GrokLogin {
    access_token: String,
    refresh_token: Option<String>,
    issuer: String,
    email: Option<String>,
}

impl ProviderEntry {
    fn new(name: &str, kind: &str, base_url: &str, model: &str) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key_env: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
            vision: None,
        }
    }

    fn from_table(table: &toml_edit::Table) -> Option<Self> {
        let text = |key: &str| table.get(key)?.as_str().map(str::to_string);
        let float = |key: &str| {
            let value = table.get(key)?;
            value
                .as_float()
                .or_else(|| value.as_integer().map(|v| v as f64))
        };
        let mut entry = Self::new(
            &text("name")?,
            &text("kind")?,
            &text("base_url").unwrap_or_default(),
            &text("model").unwrap_or_default(),
        );
        entry.api_key_env = text("api_key_env");
        entry.usd_per_mtok_in = float("usd_per_mtok_in");
        entry.usd_per_mtok_out = float("usd_per_mtok_out");
        entry.vision = table.get("vision").and_then(|v| v.as_bool());
        Some(entry)
    }

    fn to_table(&self) -> toml_edit::Table {
        let mut table = toml_edit::Table::new();
        table.insert("name", toml_edit::value(self.name.as_str()));
        table.insert("kind", toml_edit::value(self.kind.as_str()));
        table.insert("base_url", toml_edit::value(self.base_url.as_str()));
        table.insert("model", toml_edit::value(self.model.as_str()));
        if let Some(env) = &self.api_key_env {
            table.insert("api_key_env", toml_edit::value(env.as_str()));
        }
        if let Some(price) = self.usd_per_mtok_in {
            table.insert("usd_per_mtok_in", toml_edit::value(price));
        }
        if let Some(price) = self.usd_per_mtok_out {
            table.insert("usd_per_mtok_out", toml_edit::value(price));
        }
        if let Some(vision) = self.vision {
            table.insert("vision", toml_edit::value(vision));
        }
        table
    }
}

/// A provider that talks to a model on this machine: nothing to share.
fn is_local(provider: &ProviderEntry) -> bool {
    if matches!(provider.kind.as_str(), "llamacpp" | "ollama") {
        return true;
    }
    let host = provider
        .base_url
        .split("://")
        .nth(1)
        .unwrap_or(&provider.base_url)
        .split(['/', ':'])
        .next()
        .unwrap_or_default();
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0")
}

/// Mirrors the default key variables Wizard's provider kinds register.
fn default_key_env(kind: &str) -> Option<&'static str> {
    match kind {
        "xai" => Some("XAI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "cloudflare" => Some("CLOUDFLARE_API_TOKEN"),
        _ => None,
    }
}

/// Upsert `provider` by name into `[[providers]]` and make it active,
/// preserving everything else in the file.
fn upsert_provider(path: &Path, provider: &ProviderEntry) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("Wizard's {} is not valid TOML", path.display()))?;
    doc.insert("active_provider", toml_edit::value(provider.name.as_str()));
    let providers = doc
        .entry("providers")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    if providers.is_array() {
        let converted = std::mem::take(providers).into_array_of_tables();
        *providers = toml_edit::Item::ArrayOfTables(
            converted.map_err(|_| anyhow!("Wizard's `providers` list has an unexpected shape"))?,
        );
    }
    let tables = providers
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("Wizard's `providers` is not a list of tables"))?;
    tables.retain(|table| table.get("name").and_then(|n| n.as_str()) != Some(&provider.name));
    tables.push(provider.to_table());
    write_atomic(path, doc.to_string().as_bytes(), false)
        .with_context(|| format!("could not save {}", path.display()))
}

/// Merge API keys into `credentials.toml` `[keys]`.
fn merge_keys(path: &Path, keys: &BTreeMap<String, String>) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("Wizard's {} is not valid TOML", path.display()))?;
    let table = doc
        .entry("keys")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("Wizard's credentials.toml `keys` is not a table"))?;
    for (name, key) in keys {
        table.insert(name, toml_edit::value(key.as_str()));
    }
    write_atomic(path, doc.to_string().as_bytes(), true)
        .with_context(|| format!("could not save {}", path.display()))
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", dir.display()))?;
    }
    Ok(())
}

/// Write through a scratch file and a rename. `private` files are 0600;
/// others keep the existing file's mode (0644 when new).
fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut scratch = tempfile::Builder::new()
        .prefix(".zeron-")
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if private {
            0o600
        } else {
            std::fs::metadata(path)
                .map(|meta| meta.permissions().mode() & 0o777)
                .unwrap_or(0o644)
        };
        scratch
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = private;
    scratch.write_all(data)?;
    scratch.as_file().sync_all()?;
    scratch.persist(path).map_err(|err| err.error)?;
    Ok(())
}

fn read_json(path: &Path, tool: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            anyhow!("{tool} is not signed in on this machine")
        } else {
            anyhow!("could not read {}: {err}", path.display())
        }
    })?;
    serde_json::from_str(&text).map_err(|_| anyhow!("{} is not valid JSON", path.display()))
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Require an HTTPS URL on `x.ai` or a subdomain of it, as Wizard does before
/// every refresh.
fn validate_xai_https(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("invalid xAI endpoint {url}"))?;
    let host = parsed.host_str().unwrap_or_default();
    if parsed.scheme() != "https" || !(host == "x.ai" || host.ends_with(".x.ai")) {
        bail!("the xAI sign-in points at {url}, which is not an x.ai HTTPS endpoint");
    }
    Ok(())
}

/// The payload of a JWT, unverified — only ever used for display and the
/// ChatGPT account id Codex itself read the same way.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn chatgpt_account_id(id_token: &str) -> Option<String> {
    jwt_claims(id_token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// A JWT's `exp` in epoch milliseconds; 0 (refresh on first use) when the
/// token carries none.
fn jwt_expiry_ms(token: &str) -> i64 {
    jwt_claims(token)
        .and_then(|claims| claims.get("exp")?.as_i64())
        .map_or(0, |exp| exp.saturating_mul(1000))
}

fn join_detail(parts: [Option<String>; 2]) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn homes(root: &Path) -> Homes {
        let root = root.to_path_buf();
        Homes::resolve(|_| None, root)
    }

    fn jwt(claims: Value) -> String {
        let encode = |value: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.sig",
            encode(&serde_json::json!({"alg": "none"})),
            encode(&claims)
        )
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn grok_auth(client: &str) -> String {
        serde_json::json!({
            format!("https://auth.x.ai::{client}"): {
                "key": "xai-access",
                "auth_mode": "oidc",
                "email": "me@example.com",
                "refresh_token": "xai-refresh",
                "expires_at": "2026-09-26T22:28:54Z",
                "oidc_issuer": "https://auth.x.ai",
                "oidc_client_id": client,
            }
        })
        .to_string()
    }

    #[test]
    fn nothing_on_disk_detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(homes(dir.path()).detect().is_empty());
        assert!(
            homes(dir.path())
                .bundle_from(CredentialSource::Codex)
                .is_err()
        );
        assert!(
            homes(dir.path())
                .bundle_from(CredentialSource::Grok)
                .is_err()
        );
        assert!(
            homes(dir.path())
                .bundle_from(CredentialSource::Wizard)
                .is_err()
        );
    }

    #[test]
    fn codex_chatgpt_login_becomes_a_chatgpt_provider() {
        let dir = tempfile::tempdir().unwrap();
        let id_token = jwt(serde_json::json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-123",
                "chatgpt_plan_type": "pro",
            }
        }));
        write(
            &dir.path().join(".codex/auth.json"),
            &serde_json::json!({
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": id_token,
                    "access_token": "chatgpt-access",
                    "refresh_token": "chatgpt-refresh",
                }
            })
            .to_string(),
        );
        let homes = homes(dir.path());
        let found = homes.detect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, CredentialSource::Codex);
        assert_eq!(
            found[0].detail.as_deref(),
            Some("me@example.com · pro plan")
        );

        let bundle = homes.bundle_from(CredentialSource::Codex).unwrap();
        assert_eq!(apply(&bundle, &homes.wizard).unwrap(), "chatgpt");
        let tokens: Value = serde_json::from_str(
            &std::fs::read_to_string(homes.wizard.join(CHATGPT_TOKEN_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(tokens["access_token"], "chatgpt-access");
        assert_eq!(tokens["refresh_token"], "chatgpt-refresh");
        assert_eq!(tokens["account_id"], "acct-123");
        let config = std::fs::read_to_string(homes.wizard.join(CONFIG_FILE)).unwrap();
        let doc: toml_edit::DocumentMut = config.parse().unwrap();
        assert_eq!(doc["active_provider"].as_str(), Some("chatgpt"));
        let provider = &doc["providers"]
            .as_array_of_tables()
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(provider["kind"].as_str(), Some("chatgptoauth"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("https://chatgpt.com/backend-api/codex")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(homes.wizard.join(CHATGPT_TOKEN_FILE)), 0o600);
            assert_eq!(mode(homes.wizard.clone()), 0o700);
        }
    }

    #[test]
    fn codex_api_key_becomes_an_openai_key() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".codex/auth.json"),
            r#"{"OPENAI_API_KEY": "sk-test", "tokens": null}"#,
        );
        let homes = homes(dir.path());
        assert_eq!(homes.detect()[0].title, "OpenAI API key from Codex CLI");
        let bundle = homes.bundle_from(CredentialSource::Codex).unwrap();
        assert_eq!(apply(&bundle, &homes.wizard).unwrap(), "openai");
        let creds = std::fs::read_to_string(homes.wizard.join(CREDENTIALS_FILE)).unwrap();
        let doc: toml_edit::DocumentMut = creds.parse().unwrap();
        assert_eq!(doc["keys"]["openai"].as_str(), Some("sk-test"));
    }

    #[test]
    fn grok_login_becomes_an_xai_oauth_provider_and_preserves_config() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".grok/auth.json"),
            &grok_auth(XAI_CLIENT_ID),
        );
        let existing = "# my settings\nmodel = \"x\"\nactive_provider = \"old\"\n\n[ui]\nvim = true\n\n[[providers]]\nname = \"old\"\nkind = \"openai\"\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\n\n[[providers]]\nname = \"xai-oauth\"\nkind = \"xaioauth\"\nbase_url = \"https://api.x.ai/v1\"\nmodel = \"grok-4.5\"\n";
        write(&dir.path().join(".wizard/config.toml"), existing);
        let homes = homes(dir.path());
        let grok = homes
            .detect()
            .into_iter()
            .find(|c| c.source == CredentialSource::Grok)
            .unwrap();
        assert_eq!(grok.detail.as_deref(), Some("me@example.com"));

        let bundle = homes.bundle_from(CredentialSource::Grok).unwrap();
        let json = serde_json::to_string(&bundle).unwrap();
        let bundle: WizardAuthBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(apply(&bundle, &homes.wizard).unwrap(), "xai-oauth");

        let tokens: Value = serde_json::from_str(
            &std::fs::read_to_string(homes.wizard.join(XAI_TOKEN_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(tokens["access_token"], "xai-access");
        assert_eq!(tokens["refresh_token"], "xai-refresh");
        assert_eq!(tokens["token_type"], "Bearer");
        assert_eq!(tokens["token_endpoint"], "https://auth.x.ai/oauth2/token");

        let config = std::fs::read_to_string(homes.wizard.join(CONFIG_FILE)).unwrap();
        assert!(config.starts_with("# my settings\n"));
        assert!(config.contains("[ui]\nvim = true"));
        let doc: toml_edit::DocumentMut = config.parse().unwrap();
        assert_eq!(doc["active_provider"].as_str(), Some("xai-oauth"));
        let providers = doc["providers"].as_array_of_tables().unwrap();
        assert_eq!(providers.len(), 2);
        let xai: Vec<_> = providers
            .iter()
            .filter(|t| t["name"].as_str() == Some("xai-oauth"))
            .collect();
        assert_eq!(xai.len(), 1);
        assert_eq!(xai[0]["model"].as_str(), Some("grok-4.7"));
    }

    #[test]
    fn active_provider_lands_at_the_root_of_a_config_with_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "[ui]\nvim = true\n").unwrap();
        upsert_provider(
            &path,
            &ProviderEntry::new(XAI_PROVIDER, XAI_KIND, XAI_BASE_URL, XAI_MODEL),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(doc["active_provider"].as_str(), Some("xai-oauth"));
        assert_eq!(doc["ui"]["vim"].as_bool(), Some(true));
        assert!(doc["ui"].get("active_provider").is_none());
    }

    #[test]
    fn grok_login_for_another_client_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".grok/auth.json"),
            &grok_auth("other-client"),
        );
        let err = homes(dir.path())
            .bundle_from(CredentialSource::Grok)
            .unwrap_err();
        assert!(err.to_string().contains("wizard --login xai"));
    }

    #[test]
    fn wizard_export_round_trips_to_another_home() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".grok/auth.json"),
            &grok_auth(XAI_CLIENT_ID),
        );
        let local = homes(dir.path());
        apply(
            &local.bundle_from(CredentialSource::Grok).unwrap(),
            &local.wizard,
        )
        .unwrap();
        let detected = local.detect();
        assert_eq!(detected[0].source, CredentialSource::Wizard);
        assert_eq!(detected[0].title, "Wizard is already signed in (xai-oauth)");

        let bundle = local.bundle_from(CredentialSource::Wizard).unwrap();
        let remote = tempfile::tempdir().unwrap();
        let remote_home = remote.path().join("wizard");
        assert_eq!(apply(&bundle, &remote_home).unwrap(), "xai-oauth");
        assert_eq!(
            std::fs::read_to_string(remote_home.join(XAI_TOKEN_FILE)).unwrap(),
            std::fs::read_to_string(local.wizard.join(XAI_TOKEN_FILE)).unwrap(),
        );
    }

    #[test]
    fn wizard_export_carries_a_stored_key_and_refuses_local_models() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".wizard/config.toml"),
            "active_provider = \"fw\"\n\n[[providers]]\nname = \"fw\"\nkind = \"openai\"\nbase_url = \"https://api.fireworks.ai/inference/v1\"\nmodel = \"m\"\n\n[[providers]]\nname = \"local\"\nkind = \"llamacpp\"\nbase_url = \"http://127.0.0.1:8080\"\nmodel = \"q\"\ngguf_path = \"/models/q.gguf\"\n",
        );
        write(
            &dir.path().join(".wizard/credentials.toml"),
            "[keys]\nfw = \"fw-key\"\n",
        );
        let local = homes(dir.path());
        let bundle = local.bundle_from(CredentialSource::Wizard).unwrap();
        assert_eq!(bundle.keys.get("fw").map(String::as_str), Some("fw-key"));
        assert!(bundle.files.is_empty());

        let config = std::fs::read_to_string(local.wizard.join(CONFIG_FILE))
            .unwrap()
            .replace("active_provider = \"fw\"", "active_provider = \"local\"");
        std::fs::write(local.wizard.join(CONFIG_FILE), config).unwrap();
        let err = local.bundle_from(CredentialSource::Wizard).unwrap_err();
        assert!(err.to_string().contains("runs locally"));
    }

    #[test]
    fn apply_rejects_unexpected_files_and_foreign_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = WizardAuthBundle {
            source: CredentialSource::Wizard,
            provider: ProviderEntry::new(XAI_PROVIDER, XAI_KIND, XAI_BASE_URL, XAI_MODEL),
            files: BTreeMap::from([("../evil".to_string(), "{}".to_string())]),
            keys: BTreeMap::new(),
        };
        assert!(apply(&bundle, dir.path()).is_err());
        bundle.files = BTreeMap::from([(
            XAI_TOKEN_FILE.to_string(),
            r#"{"access_token":"a","token_type":"Bearer","token_endpoint":"https://evil.example/token"}"#
                .to_string(),
        )]);
        assert!(apply(&bundle, dir.path()).is_err());
        assert!(!dir.path().join(CONFIG_FILE).exists());
    }

    #[test]
    fn env_overrides_pick_the_homes() {
        let dir = tempfile::tempdir().unwrap();
        let wizard = dir.path().join("w");
        let wizard_os = wizard.clone().into_os_string();
        let homes = Homes::resolve(
            move |name| (name == "WIZARD_HOME").then(|| wizard_os.clone()),
            dir.path().to_path_buf(),
        );
        assert_eq!(homes.wizard, wizard);
        assert_eq!(homes.codex, dir.path().join(".codex"));
        assert_eq!(homes.grok, dir.path().join(".grok"));
    }

    #[test]
    fn codex_and_grok_logins_carry_over_to_pi() {
        let dir = tempfile::tempdir().unwrap();
        let access = jwt(serde_json::json!({
            "exp": 1_900_000_000,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123"},
        }));
        write(
            &dir.path().join(".codex/auth.json"),
            &serde_json::json!({
                "tokens": {"access_token": access, "refresh_token": "chatgpt-refresh"}
            })
            .to_string(),
        );
        write(
            &dir.path().join(".grok/auth.json"),
            &grok_auth(XAI_CLIENT_ID),
        );
        write(
            &dir.path().join(".pi/agent/settings.json"),
            r#"{"packages": ["npm:pi-tools"]}"#,
        );
        let homes = homes(dir.path());
        assert!(homes.pi_signed_in().is_empty());
        assert_eq!(
            homes.import_into_pi(CredentialSource::Codex).unwrap(),
            "openai-codex"
        );
        assert_eq!(homes.import_into_pi(CredentialSource::Grok).unwrap(), "xai");
        let auth: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".pi/agent/auth.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(auth["openai-codex"]["type"], "oauth");
        assert_eq!(auth["openai-codex"]["accountId"], "acct-123");
        assert_eq!(auth["openai-codex"]["refresh"], "chatgpt-refresh");
        assert_eq!(auth["openai-codex"]["expires"], 1_900_000_000_000i64);
        assert_eq!(auth["xai"]["access"], "xai-access");
        assert_eq!(auth["xai"]["refresh"], "xai-refresh");
        assert_eq!(auth["xai"]["expires"], 0);
        let mut ids = homes.pi_signed_in();
        ids.sort();
        assert_eq!(ids, ["openai-codex", "xai"]);
        // The first import picks Pi's default provider; existing settings stay.
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".pi/agent/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["defaultProvider"], "openai-codex");
        assert_eq!(settings["packages"][0], "npm:pi-tools");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(".pi/agent/auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
