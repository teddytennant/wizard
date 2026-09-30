//! Manage Wizard's model providers from the GUI: list them, add one from a
//! preset (an API key, a local server, or an account sign-in), pick the
//! active one, and remove one.
//!
//! Everything lands in the same files Wizard's own onboarding writes:
//! `[[providers]]` + `active_provider` in `config.toml`, keys in
//! `credentials.toml` `[keys]` (0600), and account tokens in their own files
//! written by `wizard --login`. Secret values never reach logs or replies.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, BufReader};

use super::{
    CHATGPT_BASE_URL, CHATGPT_KIND, CHATGPT_MODEL, CHATGPT_PROVIDER, CHATGPT_TOKEN_FILE,
    CONFIG_FILE, CREDENTIALS_FILE, CredentialSource, DetectedCredential, Homes, ProviderEntry,
    XAI_BASE_URL, XAI_KIND, XAI_MODEL, XAI_PROVIDER, XAI_TOKEN_FILE, ensure_private_dir, is_local,
    merge_keys, upsert_provider, write_atomic,
};

/// Placeholder Cloudflare's base URL carries until an account id fills it.
const CLOUDFLARE_ACCOUNT: &str = "{account_id}";

/// A way to add a provider, mirroring Wizard's onboarding choices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPreset {
    pub id: String,
    pub label: String,
    pub description: String,
    pub kind: String,
    /// The provider name it is saved under (editable for custom endpoints).
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// `xai` / `chatgpt`: added by signing in through the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Asks for an API key.
    pub needs_key: bool,
    /// The key may be left empty (a local or self-hosted endpoint).
    pub key_optional: bool,
    /// Asks for a Cloudflare account id (fills the base URL).
    pub needs_account_id: bool,
    /// The name and base URL are the user's to set.
    pub custom: bool,
    /// Runs a model on this machine.
    pub local: bool,
}

/// One configured provider, as the settings page shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRow {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub model: String,
    pub active: bool,
    /// Its key or account tokens are present (local providers need none).
    pub signed_in: bool,
    pub local: bool,
}

/// Everything the settings page needs, in one reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardProviders {
    pub providers: Vec<ProviderRow>,
    pub active: Option<String>,
    pub presets: Vec<ProviderPreset>,
    /// Codex / Grok CLI sign-ins on this machine Wizard can reuse.
    pub importable: Vec<DetectedCredential>,
    /// `config.toml` could not be read; the list may be incomplete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
}

/// `AddWizardProvider` params. Unset fields take the preset's defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddProvider {
    pub preset: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// The ways to add a provider, in the order the page offers them.
pub fn presets() -> Vec<ProviderPreset> {
    let preset =
        |id: &str, label: &str, description: &str, kind: &str, name: &str| ProviderPreset {
            id: id.into(),
            label: label.into(),
            description: description.into(),
            kind: kind.into(),
            name: name.into(),
            base_url: String::new(),
            model: String::new(),
            account: None,
            needs_key: false,
            key_optional: false,
            needs_account_id: false,
            custom: false,
            local: false,
        };
    vec![
        ProviderPreset {
            base_url: XAI_BASE_URL.into(),
            model: XAI_MODEL.into(),
            account: Some("xai".into()),
            ..preset(
                "xai-account",
                "xAI account",
                "Sign in with SuperGrok or X Premium — no API key.",
                XAI_KIND,
                XAI_PROVIDER,
            )
        },
        ProviderPreset {
            base_url: CHATGPT_BASE_URL.into(),
            model: CHATGPT_MODEL.into(),
            account: Some("chatgpt".into()),
            ..preset(
                "chatgpt-account",
                "ChatGPT account",
                "Sign in with ChatGPT Plus or Pro — no API key.",
                CHATGPT_KIND,
                CHATGPT_PROVIDER,
            )
        },
        ProviderPreset {
            base_url: XAI_BASE_URL.into(),
            model: XAI_MODEL.into(),
            needs_key: true,
            ..preset(
                "xai",
                "xAI API key",
                "Grok models on your xAI API key.",
                "xai",
                "xai",
            )
        },
        ProviderPreset {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5.6-sol".into(),
            needs_key: true,
            ..preset(
                "openai",
                "OpenAI API key",
                "GPT models on your OpenAI API key.",
                "openai",
                "openai",
            )
        },
        ProviderPreset {
            base_url: "https://api.anthropic.com".into(),
            model: "claude-fable-5".into(),
            needs_key: true,
            ..preset(
                "anthropic",
                "Anthropic API key",
                "Claude models on your Anthropic API key.",
                "anthropic",
                "claude",
            )
        },
        ProviderPreset {
            base_url: "https://openrouter.ai/api/v1".into(),
            model: "anthropic/claude-sonnet-5".into(),
            needs_key: true,
            ..preset(
                "openrouter",
                "OpenRouter",
                "Hundreds of models behind one OpenRouter key.",
                "openrouter",
                "openrouter",
            )
        },
        ProviderPreset {
            base_url: format!(
                "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT}/ai/v1"
            ),
            model: "@cf/zai-org/glm-5.2".into(),
            needs_key: true,
            needs_account_id: true,
            ..preset(
                "cloudflare",
                "Cloudflare Workers AI",
                "Workers AI models with an API token and account id.",
                "cloudflare",
                "cloudflare",
            )
        },
        ProviderPreset {
            base_url: "http://127.0.0.1:11434".into(),
            model: "qwen3.5:9b".into(),
            local: true,
            ..preset(
                "ollama",
                "Ollama",
                "A model served by Ollama on this machine.",
                "ollama",
                "ollama",
            )
        },
        ProviderPreset {
            base_url: "http://127.0.0.1:11435".into(),
            model: "default".into(),
            local: true,
            ..preset(
                "llamacpp",
                "llama.cpp",
                "A running llama-server on this machine.",
                "llamacpp",
                "llamacpp",
            )
        },
        ProviderPreset {
            base_url: "https://".into(),
            needs_key: true,
            key_optional: true,
            custom: true,
            ..preset(
                "custom",
                "OpenAI-compatible",
                "Any endpoint that speaks the OpenAI API: Groq, Together, vLLM, LM Studio…",
                "openai",
                "custom",
            )
        },
    ]
}

/// This machine's Wizard providers.
pub fn list() -> WizardProviders {
    Homes::from_env().list_providers()
}

/// Add (or update, by name) a provider from a preset and make it active.
pub fn add(request: AddProvider) -> Result<WizardProviders> {
    let homes = Homes::from_env();
    homes.add_provider(request)?;
    Ok(homes.list_providers())
}

/// Remove a provider and its stored key. The last provider stays.
pub fn remove(name: &str) -> Result<WizardProviders> {
    let homes = Homes::from_env();
    homes.remove_provider(name)?;
    Ok(homes.list_providers())
}

/// Make `name` the provider new Wizard sessions start on.
pub fn set_active(name: &str) -> Result<WizardProviders> {
    let homes = Homes::from_env();
    homes.set_active_provider(name)?;
    Ok(homes.list_providers())
}

/// Reuse a Codex or Grok CLI sign-in on this machine.
pub fn import(source: CredentialSource) -> Result<WizardProviders> {
    if source == CredentialSource::Wizard {
        bail!("that sign-in already belongs to Wizard");
    }
    super::import(source)?;
    Ok(list())
}

impl Homes {
    fn read_config(&self) -> Result<toml_edit::DocumentMut> {
        let path = self.wizard.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        text.parse()
            .with_context(|| format!("Wizard's {} is not valid TOML", path.display()))
    }

    fn provider_entries(doc: &toml_edit::DocumentMut) -> Vec<ProviderEntry> {
        doc.get("providers")
            .and_then(|item| item.as_array_of_tables())
            .map(|tables| {
                tables
                    .iter()
                    .filter_map(ProviderEntry::from_table)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list_providers(&self) -> WizardProviders {
        let (entries, active, config_error) = match self.read_config() {
            Ok(doc) => {
                let entries = Self::provider_entries(&doc);
                let named = doc
                    .get("active_provider")
                    .and_then(|item| item.as_str())
                    .filter(|name| entries.iter().any(|p| p.name == *name))
                    .map(str::to_string);
                // Wizard runs the first provider when none is named.
                let active = named.or_else(|| entries.first().map(|p| p.name.clone()));
                (entries, active, None)
            }
            Err(err) => (Vec::new(), None, Some(format!("{err:#}"))),
        };
        let providers = entries
            .iter()
            .map(|provider| ProviderRow {
                name: provider.name.clone(),
                kind: provider.kind.clone(),
                base_url: provider.base_url.clone(),
                model: provider.model.clone(),
                active: active.as_deref() == Some(provider.name.as_str()),
                signed_in: self.provider_signed_in(provider),
                local: is_local(provider),
            })
            .collect();
        let importable = self
            .detect()
            .into_iter()
            .filter(|found| found.source != CredentialSource::Wizard)
            .collect();
        WizardProviders {
            providers,
            active,
            presets: presets(),
            importable,
            config_error,
        }
    }

    fn provider_signed_in(&self, provider: &ProviderEntry) -> bool {
        if is_local(provider) {
            return true;
        }
        match provider.kind.as_str() {
            XAI_KIND => self.wizard.join(XAI_TOKEN_FILE).is_file(),
            CHATGPT_KIND => self.wizard.join(CHATGPT_TOKEN_FILE).is_file(),
            _ => self.wizard_key(provider).is_some(),
        }
    }

    fn add_provider(&self, request: AddProvider) -> Result<String> {
        let preset = presets()
            .into_iter()
            .find(|preset| preset.id == request.preset)
            .ok_or_else(|| anyhow!("unknown provider type `{}`", request.preset))?;
        let text = |value: Option<String>| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let name = if preset.custom {
            text(request.name).ok_or_else(|| anyhow!("give the provider a name"))?
        } else {
            text(request.name).unwrap_or_else(|| preset.name.clone())
        };
        if name
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '/')
        {
            bail!("a provider name can't contain spaces, quotes, or slashes");
        }
        let model = text(request.model).unwrap_or_else(|| preset.model.clone());
        if model.is_empty() {
            bail!("choose a model for {}", preset.label);
        }
        let mut base_url = text(request.base_url).unwrap_or_else(|| preset.base_url.clone());
        if preset.needs_account_id {
            let account = text(request.account_id)
                .ok_or_else(|| anyhow!("enter your Cloudflare account id"))?;
            if !account.chars().all(|c| c.is_ascii_alphanumeric()) {
                bail!("a Cloudflare account id is letters and digits only");
            }
            base_url = base_url.replace(CLOUDFLARE_ACCOUNT, &account);
        }
        if base_url.contains(CLOUDFLARE_ACCOUNT) {
            bail!("enter your Cloudflare account id");
        }
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|_| anyhow!("`{base_url}` isn't a valid URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("`{base_url}` isn't an http(s) URL");
        }
        let entry = ProviderEntry::new(&name, &preset.kind, &base_url, &model);
        let key = text(request.api_key);
        if preset.needs_key && !preset.key_optional && key.is_none() {
            // A key already stored (or exported) for this name keeps working.
            if self.wizard_key(&entry).is_none() {
                bail!(
                    "paste your {} API key",
                    preset.label.trim_end_matches(" API key")
                );
            }
        }
        if preset.account.is_some() {
            bail!("{} is added by signing in", preset.label);
        }
        ensure_private_dir(&self.wizard)?;
        if let Some(key) = key {
            merge_keys(
                &self.wizard.join(CREDENTIALS_FILE),
                &BTreeMap::from([(name.clone(), key)]),
            )?;
        }
        upsert_provider(&self.wizard.join(CONFIG_FILE), &entry)?;
        Ok(name)
    }

    fn remove_provider(&self, name: &str) -> Result<()> {
        let path = self.wizard.join(CONFIG_FILE);
        let mut doc = self.read_config()?;
        let entries = Self::provider_entries(&doc);
        if !entries.iter().any(|p| p.name == name) {
            bail!("Wizard has no provider named `{name}`");
        }
        if entries.len() == 1 {
            bail!("Wizard needs at least one provider; add another before removing `{name}`");
        }
        if let Some(tables) = doc
            .get_mut("providers")
            .and_then(|item| item.as_array_of_tables_mut())
        {
            tables.retain(|table| table.get("name").and_then(|n| n.as_str()) != Some(name));
        }
        let was_active = doc.get("active_provider").and_then(|item| item.as_str()) == Some(name);
        if was_active && let Some(next) = entries.iter().find(|p| p.name != name) {
            doc.insert("active_provider", toml_edit::value(next.name.as_str()));
        }
        write_atomic(&path, doc.to_string().as_bytes(), false)
            .with_context(|| format!("could not save {}", path.display()))?;
        self.forget_key(name)
    }

    fn forget_key(&self, name: &str) -> Result<()> {
        let path = self.wizard.join(CREDENTIALS_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(());
        };
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("Wizard's {} is not valid TOML", path.display()))?;
        let removed = doc
            .get_mut("keys")
            .and_then(|item| item.as_table_mut())
            .and_then(|keys| keys.remove(name))
            .is_some();
        if removed {
            write_atomic(&path, doc.to_string().as_bytes(), true)
                .with_context(|| format!("could not save {}", path.display()))?;
        }
        Ok(())
    }

    fn set_active_provider(&self, name: &str) -> Result<()> {
        let path = self.wizard.join(CONFIG_FILE);
        let mut doc = self.read_config()?;
        if !Self::provider_entries(&doc).iter().any(|p| p.name == name) {
            bail!("Wizard has no provider named `{name}`");
        }
        doc.insert("active_provider", toml_edit::value(name));
        write_atomic(&path, doc.to_string().as_bytes(), false)
            .with_context(|| format!("could not save {}", path.display()))
    }

    /// After an account sign-in: keep an existing provider for it (the user
    /// may have picked another model), else add Wizard's default one.
    fn ensure_account_provider(&self, account: Account) -> Result<String> {
        let (name, kind) = match account {
            Account::Xai => (XAI_PROVIDER, XAI_KIND),
            Account::ChatGpt => (CHATGPT_PROVIDER, CHATGPT_KIND),
        };
        let doc = self.read_config()?;
        let existing = Self::provider_entries(&doc)
            .into_iter()
            .find(|p| p.kind == kind);
        match existing {
            Some(provider) => {
                self.set_active_provider(&provider.name)?;
                Ok(provider.name)
            }
            None => {
                let entry = match account {
                    Account::Xai => ProviderEntry::new(name, kind, XAI_BASE_URL, XAI_MODEL),
                    Account::ChatGpt => {
                        ProviderEntry::new(name, kind, CHATGPT_BASE_URL, CHATGPT_MODEL)
                    }
                };
                ensure_private_dir(&self.wizard)?;
                upsert_provider(&self.wizard.join(CONFIG_FILE), &entry)?;
                Ok(name.to_string())
            }
        }
    }
}

/// An account Wizard signs into through the browser (`wizard --login`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Account {
    Xai,
    #[serde(rename = "chatgpt")]
    ChatGpt,
}

impl Account {
    fn arg(self) -> &'static str {
        match self {
            Account::Xai => "xai",
            Account::ChatGpt => "chatgpt",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Account::Xai => "xAI",
            Account::ChatGpt => "ChatGPT",
        }
    }
}

/// Where a browser sign-in stands (`StartWizardLogin` / `PollWizardLogin`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    pub login_id: String,
    pub account: Account,
    pub state: LoginState,
    /// The sign-in page, once Wizard prints it (Wizard also opens it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Failure reason, or the provider name on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The refreshed list once the sign-in finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<WizardProviders>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoginState {
    Running,
    Done,
    Failed,
}

/// Wizard waits five minutes for the browser callback; allow a little more.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(6 * 60);

struct Login {
    status: LoginStatus,
    task: Option<tokio::task::AbortHandle>,
}

fn logins() -> &'static Mutex<HashMap<String, Login>> {
    static LOGINS: OnceLock<Mutex<HashMap<String, Login>>> = OnceLock::new();
    LOGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn update_login(id: &str, change: impl FnOnce(&mut LoginStatus)) {
    if let Some(login) = logins()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(id)
    {
        change(&mut login.status);
    }
}

/// Start `wizard --login <account>` on this machine. Must run on a tokio
/// runtime. Poll with [`poll_login`].
pub fn start_login(account: Account) -> Result<LoginStatus> {
    let wizard = zeron_harness::AcpHarness::wizard()
        .cli_path()
        .ok_or_else(|| anyhow!("Wizard isn't installed on this device"))?;
    let login_id = uuid::Uuid::new_v4().to_string();
    let status = LoginStatus {
        login_id: login_id.clone(),
        account,
        state: LoginState::Running,
        url: None,
        message: None,
        providers: None,
    };
    let mut child = tokio::process::Command::new(&wizard)
        .arg("--login")
        .arg(account.arg())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("couldn't start {}", wizard.display()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let id = login_id.clone();
    let task = tokio::spawn(async move {
        // Wizard prints the sign-in URL on its own line; keep the tail of
        // everything else for a failure message.
        let tail = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let read = |stream: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>| {
            let tail = tail.clone();
            let id = id.clone();
            async move {
                let Some(stream) = stream else { return };
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.starts_with("https://") {
                        update_login(&id, |status| {
                            status.url.get_or_insert(line.clone());
                        });
                    } else if !line.is_empty() {
                        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                        tail.push(line);
                        let excess = tail.len().saturating_sub(4);
                        tail.drain(..excess);
                    }
                }
            }
        };
        let outcome = tokio::time::timeout(LOGIN_TIMEOUT, async {
            let (_, _, status) = tokio::join!(
                read(stdout.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)),
                read(stderr.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)),
                child.wait(),
            );
            status
        })
        .await;
        let failure = |message: String| {
            update_login(&id, |status| {
                status.state = LoginState::Failed;
                status.message = Some(message);
            })
        };
        match outcome {
            Err(_) => failure(format!("The {} sign-in timed out.", account.label())),
            Ok(Err(err)) => failure(format!("The {} sign-in failed: {err}", account.label())),
            Ok(Ok(exit)) if !exit.success() => {
                let tail = tail.lock().unwrap_or_else(|e| e.into_inner()).join(" ");
                failure(if tail.is_empty() {
                    format!("The {} sign-in didn't finish.", account.label())
                } else {
                    tail
                })
            }
            Ok(Ok(_)) => {
                let homes = Homes::from_env();
                match homes.ensure_account_provider(account) {
                    Ok(name) => update_login(&id, |status| {
                        status.state = LoginState::Done;
                        status.message = Some(name);
                        status.providers = Some(homes.list_providers());
                    }),
                    Err(err) => failure(format!("{err:#}")),
                }
            }
        }
    });
    logins().lock().unwrap_or_else(|e| e.into_inner()).insert(
        login_id,
        Login {
            status: status.clone(),
            task: Some(task.abort_handle()),
        },
    );
    Ok(status)
}

pub fn poll_login(login_id: &str) -> Result<LoginStatus> {
    let mut logins = logins().lock().unwrap_or_else(|e| e.into_inner());
    let status = logins
        .get(login_id)
        .map(|login| login.status.clone())
        .ok_or_else(|| anyhow!("that sign-in is no longer running"))?;
    if status.state != LoginState::Running {
        logins.remove(login_id);
    }
    Ok(status)
}

/// Stop a sign-in; its `wizard --login` process is killed with the task.
pub fn cancel_login(login_id: &str) {
    if let Some(login) = logins()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(login_id)
        && let Some(task) = login.task
    {
        task.abort();
    }
}

/// Test seam: [`Homes`] rooted at `root`.
#[cfg(test)]
fn homes(root: &std::path::Path) -> Homes {
    Homes::resolve(|_| None, root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(homes: &Homes, preset: &str, fields: &[(&str, &str)]) -> Result<String> {
        let field = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        };
        homes.add_provider(AddProvider {
            preset: preset.into(),
            name: field("name"),
            base_url: field("baseUrl"),
            model: field("model"),
            api_key: field("apiKey"),
            account_id: field("accountId"),
        })
    }

    #[test]
    fn presets_have_unique_ids_and_valid_defaults() {
        let presets = presets();
        let mut ids: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), presets.len());
        for preset in &presets {
            assert!(!preset.kind.is_empty(), "{}", preset.id);
            assert!(preset.custom || !preset.model.is_empty(), "{}", preset.id);
        }
    }

    #[test]
    fn adding_a_key_provider_saves_its_key_privately_and_activates_it() {
        let dir = tempfile::tempdir().unwrap();
        let homes = homes(dir.path());
        std::fs::create_dir_all(dir.path().join(".wizard")).unwrap();
        std::fs::write(
            dir.path().join(".wizard/config.toml"),
            "theme = \"dark\"\n\n[[providers]]\nname = \"local\"\nkind = \"ollama\"\nbase_url = \"http://127.0.0.1:11434\"\nmodel = \"qwen3.5:9b\"\n",
        )
        .unwrap();
        assert_eq!(
            add(&homes, "anthropic", &[("apiKey", "sk-ant-test")]).unwrap(),
            "claude"
        );
        let listed = homes.list_providers();
        assert_eq!(listed.active.as_deref(), Some("claude"));
        let claude = listed
            .providers
            .iter()
            .find(|p| p.name == "claude")
            .unwrap();
        assert!(claude.signed_in && claude.active && !claude.local);
        assert_eq!(claude.model, "claude-fable-5");
        let config = std::fs::read_to_string(dir.path().join(".wizard/config.toml")).unwrap();
        assert!(config.contains("theme = \"dark\""));
        let keys = std::fs::read_to_string(dir.path().join(".wizard/credentials.toml")).unwrap();
        assert!(keys.contains("claude = \"sk-ant-test\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(".wizard/credentials.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn add_validates_its_fields() {
        let dir = tempfile::tempdir().unwrap();
        let homes = homes(dir.path());
        assert!(add(&homes, "openai", &[]).is_err(), "a key is required");
        assert!(add(&homes, "custom", &[("baseUrl", "https://x.test/v1")]).is_err());
        assert!(add(&homes, "custom", &[("name", "my box"), ("model", "m")]).is_err());
        assert!(add(&homes, "cloudflare", &[("apiKey", "cf")]).is_err());
        assert!(add(&homes, "xai-account", &[]).is_err(), "accounts sign in");
        assert!(add(&homes, "nope", &[]).is_err());
        assert_eq!(
            add(
                &homes,
                "cloudflare",
                &[("apiKey", "cf"), ("accountId", "abc123")]
            )
            .unwrap(),
            "cloudflare"
        );
        let cloudflare = homes.list_providers().providers.remove(0);
        assert!(cloudflare.base_url.contains("/accounts/abc123/ai/v1"));
        // A local server and a keyless custom endpoint need no key.
        add(&homes, "ollama", &[("model", "llama4")]).unwrap();
        add(
            &homes,
            "custom",
            &[
                ("name", "lmstudio"),
                ("baseUrl", "http://127.0.0.1:1234/v1"),
                ("model", "qwen"),
            ],
        )
        .unwrap();
        let listed = homes.list_providers();
        assert_eq!(listed.active.as_deref(), Some("lmstudio"));
        assert!(listed.providers.iter().all(|p| p.signed_in));
    }

    #[test]
    fn remove_keeps_one_provider_and_hands_off_active() {
        let dir = tempfile::tempdir().unwrap();
        let homes = homes(dir.path());
        add(&homes, "xai", &[("apiKey", "xai-key")]).unwrap();
        add(&homes, "openai", &[("apiKey", "sk-openai")]).unwrap();
        assert_eq!(homes.list_providers().active.as_deref(), Some("openai"));
        homes.set_active_provider("xai").unwrap();
        assert_eq!(homes.list_providers().active.as_deref(), Some("xai"));
        assert!(homes.set_active_provider("missing").is_err());
        homes.remove_provider("xai").unwrap();
        let listed = homes.list_providers();
        assert_eq!(listed.active.as_deref(), Some("openai"));
        assert_eq!(listed.providers.len(), 1);
        let keys = std::fs::read_to_string(dir.path().join(".wizard/credentials.toml")).unwrap();
        assert!(!keys.contains("xai-key"));
        assert!(keys.contains("sk-openai"));
        assert!(
            homes.remove_provider("openai").is_err(),
            "the last one stays"
        );
    }

    #[test]
    fn account_sign_in_keeps_an_existing_provider_or_adds_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let homes = homes(dir.path());
        add(&homes, "openai", &[("apiKey", "sk")]).unwrap();
        assert_eq!(
            homes.ensure_account_provider(Account::Xai).unwrap(),
            XAI_PROVIDER
        );
        let xai = homes
            .list_providers()
            .providers
            .into_iter()
            .find(|p| p.kind == XAI_KIND)
            .unwrap();
        assert!(xai.active && !xai.signed_in, "no token file yet");
        // A second sign-in keeps the user's model choice.
        let config = dir.path().join(".wizard/config.toml");
        let edited = std::fs::read_to_string(&config)
            .unwrap()
            .replace("grok-4.7", "grok-4.20");
        std::fs::write(&config, edited).unwrap();
        homes.set_active_provider("openai").unwrap();
        assert_eq!(
            homes.ensure_account_provider(Account::Xai).unwrap(),
            XAI_PROVIDER
        );
        let listed = homes.list_providers();
        assert_eq!(listed.active.as_deref(), Some(XAI_PROVIDER));
        assert!(listed.providers.iter().any(|p| p.model == "grok-4.20"));
    }

    #[test]
    fn listing_marks_the_first_provider_active_when_none_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let homes = homes(dir.path());
        std::fs::create_dir_all(dir.path().join(".wizard")).unwrap();
        std::fs::write(
            dir.path().join(".wizard/config.toml"),
            "[[providers]]\nname = \"a\"\nkind = \"ollama\"\nbase_url = \"http://127.0.0.1:11434\"\nmodel = \"m\"\n\n[[providers]]\nname = \"b\"\nkind = \"xai\"\nbase_url = \"https://api.x.ai/v1\"\nmodel = \"grok-4.6\"\n",
        )
        .unwrap();
        let listed = homes.list_providers();
        assert_eq!(listed.active.as_deref(), Some("a"));
        assert!(!listed.providers[1].signed_in, "no key for b");
    }
}
