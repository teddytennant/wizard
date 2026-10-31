//! The Claude Code settings keys that narrow what Wizard may do or load:
//! MCP allow and deny lists, marketplace lists, the `allowManaged*Only`
//! switches, HTTP hook limits, `sandbox` path lists and the extra directories
//! a permission scope covers.
//!
//! [`merge`] reads one settings document into [`PolicyKeys`]; the loader in
//! [`super::claude`] calls it for every layer. The keys only ever make Wizard
//! stricter or name more of what the user already trusts, so an untrusted
//! project may set the list ones too; `additionalDirectories` is the exception
//! because it widens a scope.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::hooks::spec::{Handler, HookSpec};
use crate::mcp::McpServerConfig;

/// One entry of `allowedMcpServers` or `deniedMcpServers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRule {
    /// `{"serverName": "github"}`.
    Name(String),
    /// `{"serverCommand": ["npx", "-y", "server"]}`: the command and its
    /// arguments, exactly.
    Command(Vec<String>),
    /// `{"serverUrl": "https://*.example.com/*"}`.
    Url(String),
}

impl McpRule {
    fn parse(entry: &Value) -> Option<Self> {
        if let Some(name) = entry.get("serverName").and_then(Value::as_str) {
            return Some(Self::Name(name.to_string()));
        }
        if let Some(command) = entry.get("serverCommand").and_then(Value::as_array) {
            return Some(Self::Command(
                command
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            ));
        }
        entry
            .get("serverUrl")
            .and_then(Value::as_str)
            .map(|url| Self::Url(url.to_string()))
    }

    fn matches(&self, server: &McpServerConfig) -> bool {
        match self {
            Self::Name(name) => *name == server.name,
            Self::Command(wanted) => server.command.as_ref().is_some_and(|command| {
                let mut have = vec![command.clone()];
                have.extend(server.args.iter().cloned());
                have == *wanted
            }),
            Self::Url(pattern) => server
                .url
                .as_deref()
                .is_some_and(|url| wildcard(pattern, url)),
        }
    }
}

/// `*` matches any run of characters; everything else is literal.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some((first, rest)) = parts.split_first() else {
        return false;
    };
    let Some(mut tail) = text.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return tail.is_empty();
    };
    for part in middle {
        match tail.find(part) {
            Some(at) => tail = &tail[at + part.len()..],
            None => return false,
        }
    }
    tail.ends_with(last) && tail.len() >= last.len()
}

/// What the lists in `strictKnownMarketplaces`, `blockedMarketplaces` and
/// `extraKnownMarketplaces` say.
#[derive(Debug, Clone, Default)]
pub struct MarketplaceKeys {
    /// `strictKnownMarketplaces`: when set, only these sources may be added.
    pub strict: Option<Vec<Value>>,
    /// `blockedMarketplaces`: sources that may not be added.
    pub blocked: Vec<Value>,
    /// `extraKnownMarketplaces`: name to `{"source": {...}}`.
    pub extra: BTreeMap<String, Value>,
}

/// The keys read by [`merge`].
#[derive(Debug, Clone, Default)]
pub struct PolicyKeys {
    /// `allowedMcpServers` from any layer. `None` means the key was never set.
    pub allowed_mcp: Option<Vec<McpRule>>,
    /// `allowedMcpServers` from managed settings alone, which is all that
    /// counts once `allowManagedMcpServersOnly` is set.
    pub allowed_mcp_managed: Option<Vec<McpRule>>,
    /// `deniedMcpServers`, merged from every layer.
    pub denied_mcp: Vec<McpRule>,
    /// `allowManagedMcpServersOnly`, from managed settings.
    pub managed_mcp_only: bool,
    /// `allowManagedHooksOnly`, from managed settings.
    pub managed_hooks_only: bool,
    /// `allowManagedPermissionRulesOnly`, from managed settings.
    pub managed_permissions_only: bool,
    /// `allowedHttpHookUrls`: when set, the URLs an HTTP hook may call.
    pub http_hook_urls: Option<Vec<String>>,
    /// `httpHookAllowedEnvVars`: when set, the most an HTTP hook may name in
    /// `allowedEnvVars`.
    pub http_hook_env: Option<Vec<String>>,
    /// `sandbox.filesystem.denyRead` when the sandbox is enabled.
    pub sandbox_deny_read: Vec<String>,
    /// `sandbox.filesystem.denyWrite` when the sandbox is enabled.
    pub sandbox_deny_write: Vec<String>,
    /// `sandbox.network.deniedDomains` when the sandbox is enabled.
    pub sandbox_denied_domains: Vec<String>,
    /// `additionalDirectories` and `permissions.additionalDirectories`.
    pub extra_dirs: Vec<String>,
    /// `permissions.disableBypassPermissionsMode` is `"disable"`.
    pub disable_bypass: bool,
    pub marketplaces: MarketplaceKeys,
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn mcp_rules(value: Option<&Value>) -> Option<Vec<McpRule>> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(McpRule::parse).collect())
}

/// Read one settings document. `managed` says it came from a managed settings
/// file; `trusted` says it is not an untrusted project's.
pub fn merge(out: &mut PolicyKeys, value: &Value, managed: bool, trusted: bool) {
    if let Some(rules) = mcp_rules(value.get("allowedMcpServers")) {
        out.allowed_mcp
            .get_or_insert_with(Vec::new)
            .extend(rules.clone());
        if managed {
            out.allowed_mcp_managed
                .get_or_insert_with(Vec::new)
                .extend(rules);
        }
    }
    if let Some(rules) = mcp_rules(value.get("deniedMcpServers")) {
        out.denied_mcp.extend(rules);
    }
    if managed {
        let on = |key: &str| value.get(key).and_then(Value::as_bool) == Some(true);
        out.managed_mcp_only |= on("allowManagedMcpServersOnly");
        out.managed_hooks_only |= on("allowManagedHooksOnly");
        out.managed_permissions_only |= on("allowManagedPermissionRulesOnly");
    }
    for (key, into) in [
        ("allowedHttpHookUrls", &mut out.http_hook_urls),
        ("httpHookAllowedEnvVars", &mut out.http_hook_env),
    ] {
        if let Some(list) = strings(value.get(key)) {
            into.get_or_insert_with(Vec::new).extend(list);
        }
    }
    if let Some(sandbox) = value.get("sandbox")
        && sandbox.get("enabled").and_then(Value::as_bool) == Some(true)
    {
        let files = sandbox.get("filesystem");
        out.sandbox_deny_read
            .extend(strings(files.and_then(|f| f.get("denyRead"))).unwrap_or_default());
        out.sandbox_deny_write
            .extend(strings(files.and_then(|f| f.get("denyWrite"))).unwrap_or_default());
        out.sandbox_denied_domains.extend(
            strings(sandbox.get("network").and_then(|n| n.get("deniedDomains")))
                .unwrap_or_default(),
        );
    }
    let permissions = value.get("permissions");
    if trusted {
        for dirs in [
            strings(value.get("additionalDirectories")),
            strings(permissions.and_then(|p| p.get("additionalDirectories"))),
        ]
        .into_iter()
        .flatten()
        {
            out.extra_dirs.extend(dirs);
        }
    }
    if permissions
        .and_then(|p| p.get("disableBypassPermissionsMode"))
        .and_then(Value::as_str)
        == Some("disable")
    {
        out.disable_bypass = true;
    }
    let m = &mut out.marketplaces;
    if let Some(list) = value
        .get("strictKnownMarketplaces")
        .and_then(Value::as_array)
    {
        m.strict.get_or_insert_with(Vec::new).extend(list.clone());
    }
    if let Some(list) = value.get("blockedMarketplaces").and_then(Value::as_array) {
        m.blocked.extend(list.clone());
    }
    if let Some(map) = value
        .get("extraKnownMarketplaces")
        .and_then(Value::as_object)
    {
        for (name, entry) in map {
            m.extra.insert(name.clone(), entry.clone());
        }
    }
}

impl PolicyKeys {
    /// Whether `server` may connect under `allowedMcpServers` and
    /// `deniedMcpServers`. A denial wins over an allowance.
    pub fn allows_mcp(&self, server: &McpServerConfig) -> bool {
        if self.denied_mcp.iter().any(|rule| rule.matches(server)) {
            return false;
        }
        let allowed = if self.managed_mcp_only {
            &self.allowed_mcp_managed
        } else {
            &self.allowed_mcp
        };
        allowed
            .as_ref()
            .is_none_or(|rules| rules.iter().any(|rule| rule.matches(server)))
    }

    /// `config` without the servers the lists refuse, and the names removed.
    pub fn filter_mcp(
        &self,
        mut config: crate::mcp::McpConfig,
    ) -> (crate::mcp::McpConfig, Vec<String>) {
        let mut removed = Vec::new();
        config.servers.retain(|server| {
            let keep = self.allows_mcp(server);
            if !keep {
                removed.push(server.name.clone());
            }
            keep
        });
        (config, removed)
    }

    /// Apply `allowedHttpHookUrls` and `httpHookAllowedEnvVars` to a hook.
    /// `None` drops the hook.
    pub fn limit_http_hook(&self, mut spec: HookSpec) -> Option<HookSpec> {
        if let Handler::Http {
            url,
            allowed_env_vars,
            ..
        } = &mut spec.handler
        {
            if let Some(urls) = &self.http_hook_urls
                && !urls.iter().any(|pattern| wildcard(pattern, url))
            {
                tracing::warn!(
                    "{}: http hook to {url} is not in allowedHttpHookUrls",
                    spec.source
                );
                return None;
            }
            if let Some(names) = &self.http_hook_env {
                allowed_env_vars.retain(|name| names.contains(name));
            }
        }
        Some(spec)
    }

    /// Whether a marketplace `source` (a path, `owner/repo` or URL) may be
    /// added; the reason when it may not.
    pub fn refuse_marketplace(&self, source: &str) -> Option<String> {
        let same = |entry: &Value| source_matches(entry, source);
        if self.marketplaces.blocked.iter().any(same) {
            return Some(format!("{source} is in blockedMarketplaces"));
        }
        match &self.marketplaces.strict {
            Some(list) if !list.iter().any(same) => {
                Some(format!("{source} is not in strictKnownMarketplaces"))
            }
            _ => None,
        }
    }
}

/// Whether a marketplace source entry (`{"source": "github", "repo": "a/b"}`,
/// `{"source": "git", "url": "..."}`, `{"source": "directory", "path": "..."}`
/// or a bare string) names `source`.
fn source_matches(entry: &Value, source: &str) -> bool {
    let wanted = source.trim_end_matches('/').trim_end_matches(".git");
    let named = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(|v| v.trim_end_matches('/').trim_end_matches(".git").to_string())
    };
    if let Some(text) = entry.as_str() {
        return wildcard(text, wanted);
    }
    if let Some(repo) = named("repo") {
        return wanted == repo
            || wanted.ends_with(&format!("github.com/{repo}"))
            || wanted.ends_with(&format!("github.com:{repo}"));
    }
    ["url", "path"]
        .iter()
        .filter_map(|key| named(key))
        .any(|pattern| wildcard(&pattern, wanted))
}

/// `extraKnownMarketplaces` as sources `/plugin marketplace add` understands.
pub fn extra_marketplace_sources(keys: &MarketplaceKeys) -> Vec<(String, String)> {
    keys.extra
        .iter()
        .filter_map(|(name, entry)| {
            let source = entry.get("source")?;
            let text = if let Some(repo) = source.get("repo").and_then(Value::as_str) {
                repo.to_string()
            } else if let Some(url) = source.get("url").and_then(Value::as_str) {
                url.to_string()
            } else {
                source.get("path").and_then(Value::as_str)?.to_string()
            };
            Some((name.clone(), text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server(
        name: &str,
        command: Option<&str>,
        args: &[&str],
        url: Option<&str>,
    ) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: if url.is_some() {
                crate::mcp::McpTransport::Http
            } else {
                crate::mcp::McpTransport::Stdio
            },
            command: command.map(str::to_string),
            args: args.iter().map(|a| a.to_string()).collect(),
            url: url.map(str::to_string),
            env: Default::default(),
            headers: Default::default(),
        }
    }

    #[test]
    fn wildcard_matches_the_way_the_schema_says() {
        assert!(wildcard(
            "https://*.example.com/*",
            "https://api.example.com/mcp"
        ));
        assert!(!wildcard(
            "https://*.example.com/*",
            "https://evil.test/example.com/"
        ));
        assert!(wildcard("exact", "exact"));
        assert!(!wildcard("exact", "exact2"));
        assert!(wildcard("*", ""));
    }

    #[test]
    fn denial_beats_allowance_and_empty_allowlist_blocks_all() {
        let mut keys = PolicyKeys::default();
        let doc = json!({
            "allowedMcpServers": [{"serverName": "gh"}, {"serverName": "fs"}],
            "deniedMcpServers": [{"serverName": "fs"}],
        });
        merge(&mut keys, &doc, false, true);
        assert!(keys.allows_mcp(&server("gh", Some("x"), &[], None)));
        assert!(!keys.allows_mcp(&server("fs", Some("x"), &[], None)));
        assert!(!keys.allows_mcp(&server("other", Some("x"), &[], None)));

        let mut empty = PolicyKeys::default();
        merge(&mut empty, &json!({"allowedMcpServers": []}), false, true);
        assert!(!empty.allows_mcp(&server("gh", Some("x"), &[], None)));
        assert!(PolicyKeys::default().allows_mcp(&server("gh", Some("x"), &[], None)));
    }

    #[test]
    fn command_and_url_entries_match_exactly_and_by_pattern() {
        let mut keys = PolicyKeys::default();
        let doc = json!({"allowedMcpServers": [
            {"serverCommand": ["npx", "-y", "srv"]},
            {"serverUrl": "https://*.corp.test/*"},
        ]});
        merge(&mut keys, &doc, false, true);
        assert!(keys.allows_mcp(&server("a", Some("npx"), &["-y", "srv"], None)));
        assert!(!keys.allows_mcp(&server("a", Some("npx"), &["-y", "other"], None)));
        assert!(keys.allows_mcp(&server("b", None, &[], Some("https://x.corp.test/mcp"))));
        assert!(!keys.allows_mcp(&server("b", None, &[], Some("https://x.test/mcp"))));
    }

    #[test]
    fn managed_only_ignores_the_users_allowlist() {
        let mut keys = PolicyKeys::default();
        merge(
            &mut keys,
            &json!({"allowedMcpServers": [{"serverName": "mine"}]}),
            false,
            true,
        );
        merge(
            &mut keys,
            &json!({"allowManagedMcpServersOnly": true, "allowedMcpServers": [{"serverName": "corp"}]}),
            true,
            true,
        );
        assert!(keys.allows_mcp(&server("corp", Some("x"), &[], None)));
        assert!(!keys.allows_mcp(&server("mine", Some("x"), &[], None)));
        let mut user_only = PolicyKeys::default();
        merge(
            &mut user_only,
            &json!({"allowManagedMcpServersOnly": true}),
            false,
            true,
        );
        assert!(
            !user_only.managed_mcp_only,
            "only managed settings can set it"
        );
    }

    #[test]
    fn http_hooks_are_limited_to_listed_urls_and_env_names() {
        let mut keys = PolicyKeys::default();
        merge(
            &mut keys,
            &json!({"allowedHttpHookUrls": ["https://hooks.corp.test/*"], "httpHookAllowedEnvVars": ["TOKEN"]}),
            false,
            true,
        );
        let http = |url: &str| HookSpec {
            event: crate::hooks::event::HookEvent::Stop,
            matcher: None,
            handler: Handler::Http {
                url: url.to_string(),
                headers: Default::default(),
                allowed_env_vars: vec!["TOKEN".into(), "SECRET".into()],
            },
            timeout_secs: None,
            origin: crate::hooks::spec::Origin::Claude,
            json: true,
            condition: None,
            is_async: false,
            block_on_failure: false,
            source: "t".into(),
        };
        assert!(keys.limit_http_hook(http("https://evil.test/x")).is_none());
        let kept = keys
            .limit_http_hook(http("https://hooks.corp.test/x"))
            .unwrap();
        match kept.handler {
            Handler::Http {
                allowed_env_vars, ..
            } => assert_eq!(allowed_env_vars, vec!["TOKEN"]),
            _ => panic!("still http"),
        }
    }

    #[test]
    fn sandbox_lists_count_only_when_enabled() {
        let mut keys = PolicyKeys::default();
        let off = json!({"sandbox": {"filesystem": {"denyRead": ["~/.ssh"]}}});
        merge(&mut keys, &off, false, true);
        assert!(keys.sandbox_deny_read.is_empty());
        let on = json!({"sandbox": {"enabled": true,
            "filesystem": {"denyRead": ["~/.ssh"], "denyWrite": ["/etc"]},
            "network": {"deniedDomains": ["evil.test"]}}});
        merge(&mut keys, &on, false, true);
        assert_eq!(keys.sandbox_deny_read, vec!["~/.ssh"]);
        assert_eq!(keys.sandbox_deny_write, vec!["/etc"]);
        assert_eq!(keys.sandbox_denied_domains, vec!["evil.test"]);
    }

    #[test]
    fn untrusted_projects_cannot_widen_the_scope() {
        let mut keys = PolicyKeys::default();
        let doc = json!({"additionalDirectories": ["/data"], "permissions": {"additionalDirectories": ["/more"]}});
        merge(&mut keys, &doc, false, false);
        assert!(keys.extra_dirs.is_empty());
        merge(&mut keys, &doc, false, true);
        assert_eq!(keys.extra_dirs, vec!["/data", "/more"]);
    }

    #[test]
    fn marketplace_lists_gate_sources() {
        let mut keys = PolicyKeys::default();
        merge(
            &mut keys,
            &json!({"blockedMarketplaces": [{"source": "github", "repo": "bad/market"}],
                    "strictKnownMarketplaces": [{"source": "github", "repo": "good/market"},
                                                {"source": "git", "url": "https://git.corp.test/*"}]}),
            false,
            true,
        );
        assert!(keys.refuse_marketplace("good/market").is_none());
        assert!(
            keys.refuse_marketplace("https://git.corp.test/team/m.git")
                .is_none()
        );
        assert!(
            keys.refuse_marketplace("bad/market")
                .unwrap()
                .contains("blockedMarketplaces")
        );
        assert!(
            keys.refuse_marketplace("other/market")
                .unwrap()
                .contains("strictKnownMarketplaces")
        );
    }

    #[test]
    fn extra_marketplaces_become_sources() {
        let mut keys = PolicyKeys::default();
        merge(
            &mut keys,
            &json!({"extraKnownMarketplaces": {"team": {"source": {"source": "github", "repo": "co/plugins"}}}}),
            false,
            true,
        );
        assert_eq!(
            extra_marketplace_sources(&keys.marketplaces),
            vec![("team".to_string(), "co/plugins".to_string())]
        );
    }

    #[test]
    fn disable_bypass_reads_the_permissions_key() {
        let mut keys = PolicyKeys::default();
        merge(
            &mut keys,
            &json!({"permissions": {"disableBypassPermissionsMode": "disable"}}),
            false,
            true,
        );
        assert!(keys.disable_bypass);
    }
}
