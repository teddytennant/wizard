//! Bedrock, Vertex and Foundry through the `claude` provider.
//!
//! The real `claude` decides its route from the environment:
//! `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` or
//! `CLAUDE_CODE_USE_FOUNDRY`, plus the cloud's own variables and credential
//! chain (AWS profiles and SSO, `gcloud` application-default credentials,
//! Azure). The child Wizard starts inherits the process environment, so
//! anything exported in the shell already works. This adds the same names from
//! the `env` block of `~/.wizard/settings.json`, which is where `/setup-bedrock`
//! and `/setup-vertex` write them, and reports the route for `wizard doctor`.
//!
//! Only cloud-route names are taken from the file. The `env` block of a
//! Claude-format settings file can hold anything, and this one starts a
//! subprocess, so the allowlist below is the whole of what crosses.
//!
//! Secrets are never written by the setup commands; keys stay in the shell or
//! the cloud's own credential store.

use std::path::Path;

/// Names (`*` ends a prefix) a settings file may hand the `claude` child.
const ALLOWED: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "ANTHROPIC_BEDROCK_*",
    "ANTHROPIC_VERTEX_*",
    "ANTHROPIC_FOUNDRY_*",
    "ANTHROPIC_AWS_*",
    "ANTHROPIC_GOOGLE_*",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL*",
    "ANTHROPIC_DEFAULT_*_MODEL",
    "AWS_*",
    "AZURE_*",
    "GOOGLE_*",
    "GCLOUD_*",
    "CLOUDSDK_*",
    "CLOUD_ML_REGION",
    "VERTEX_REGION_*",
];

pub fn is_cloud_name(name: &str) -> bool {
    ALLOWED.iter().any(|pattern| match pattern.split_once('*') {
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail)
        }
        None => name == *pattern,
    })
}

/// The cloud-route entries of a settings `env` block that the process
/// environment does not already set (the process wins, as it does for every
/// other variable Wizard reads).
pub fn from_settings(
    block: &[(String, String)],
    process: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    block
        .iter()
        .filter(|(name, value)| is_cloud_name(name) && !value.is_empty() && process(name).is_none())
        .cloned()
        .collect()
}

/// The settings file's cloud variables, less what the process already sets.
fn pending() -> Vec<(String, String)> {
    let block = crate::usersettings::env_block();
    let process = |name: &str| std::env::var(name).ok();
    from_settings(&block, &process)
}

/// Add the settings file's cloud variables to a `claude` child.
pub fn apply(command: &mut tokio::process::Command) {
    for (name, value) in pending() {
        command.env(name, value);
    }
}

/// [`apply`] for a `std` command (`claude --version`, `claude auth status`).
pub fn apply_std(command: &mut std::process::Command) {
    for (name, value) in pending() {
        command.env(name, value);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cloud {
    Bedrock,
    Vertex,
    Foundry,
}

impl Cloud {
    pub fn name(self) -> &'static str {
        match self {
            Cloud::Bedrock => "Bedrock",
            Cloud::Vertex => "Vertex",
            Cloud::Foundry => "Foundry",
        }
    }
}

fn on(get: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    get(name).is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn set(get: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    get(name).is_some_and(|v| !v.trim().is_empty())
}

/// What the environment routes `claude` to, and what that route still lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub cloud: Cloud,
    pub found: Vec<String>,
    pub missing: Vec<String>,
}

/// Read the route out of `get`, which sees the process environment overlaid by
/// the settings file. `home` is where `~/.aws` and `~/.config/gcloud` live.
pub fn route(get: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<Route> {
    let file = |rel: &str| home.is_some_and(|h| h.join(rel).is_file());
    let mut found = Vec::new();
    let mut missing = Vec::new();
    let mut need = |ok: bool, have: &str, lack: &str| {
        if ok {
            found.push(have.to_string());
        } else {
            missing.push(lack.to_string());
        }
    };
    let cloud = if on(get, "CLAUDE_CODE_USE_BEDROCK") {
        need(
            set(get, "AWS_REGION") || set(get, "AWS_DEFAULT_REGION"),
            "region",
            "AWS_REGION",
        );
        let creds = set(get, "AWS_BEARER_TOKEN_BEDROCK")
            || set(get, "AWS_PROFILE")
            || (set(get, "AWS_ACCESS_KEY_ID") && set(get, "AWS_SECRET_ACCESS_KEY"))
            || set(get, "AWS_WEB_IDENTITY_TOKEN_FILE")
            || on(get, "CLAUDE_CODE_SKIP_BEDROCK_AUTH")
            || file(".aws/credentials")
            || file(".aws/config");
        need(
            creds,
            "AWS credentials",
            "AWS credentials (AWS_PROFILE, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, AWS_BEARER_TOKEN_BEDROCK, or ~/.aws)",
        );
        Cloud::Bedrock
    } else if on(get, "CLAUDE_CODE_USE_VERTEX") {
        need(
            set(get, "ANTHROPIC_VERTEX_PROJECT_ID"),
            "project",
            "ANTHROPIC_VERTEX_PROJECT_ID",
        );
        need(
            set(get, "CLOUD_ML_REGION") || set(get, "ANTHROPIC_VERTEX_BASE_URL"),
            "region",
            "CLOUD_ML_REGION",
        );
        let creds = set(get, "GOOGLE_APPLICATION_CREDENTIALS")
            || on(get, "CLAUDE_CODE_SKIP_VERTEX_AUTH")
            || file(".config/gcloud/application_default_credentials.json");
        need(
            creds,
            "Google credentials",
            "Google credentials (GOOGLE_APPLICATION_CREDENTIALS, or `gcloud auth application-default login`)",
        );
        Cloud::Vertex
    } else if on(get, "CLAUDE_CODE_USE_FOUNDRY") {
        need(
            set(get, "ANTHROPIC_FOUNDRY_RESOURCE") || set(get, "ANTHROPIC_FOUNDRY_BASE_URL"),
            "resource",
            "ANTHROPIC_FOUNDRY_RESOURCE (or ANTHROPIC_FOUNDRY_BASE_URL)",
        );
        need(
            set(get, "ANTHROPIC_FOUNDRY_API_KEY")
                || on(get, "CLAUDE_CODE_SKIP_FOUNDRY_AUTH")
                || file(".azure/azureProfile.json"),
            "Azure credentials",
            "ANTHROPIC_FOUNDRY_API_KEY (or an `az login`)",
        );
        Cloud::Foundry
    } else {
        return None;
    };
    Some(Route {
        cloud,
        found,
        missing,
    })
}

/// The route of this process: environment first, then the settings file.
pub fn current_route() -> Option<Route> {
    route_with(&crate::usersettings::env_block())
}

/// [`current_route`] with the settings `env` block given.
pub fn route_with(block: &[(String, String)]) -> Option<Route> {
    let get = |name: &str| {
        std::env::var(name).ok().or_else(|| {
            block
                .iter()
                .find(|(n, _)| n == name && is_cloud_name(n))
                .map(|(_, v)| v.clone())
        })
    };
    route(&get, dirs::home_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn only_cloud_route_names_cross_from_the_settings_file() {
        for ok in [
            "CLAUDE_CODE_USE_BEDROCK",
            "AWS_REGION",
            "AWS_PROFILE",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "CLOUD_ML_REGION",
            "ANTHROPIC_FOUNDRY_RESOURCE",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(is_cloud_name(ok), "{ok}");
        }
        for no in [
            "LD_PRELOAD",
            "PATH",
            "HOME",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "NODE_OPTIONS",
        ] {
            assert!(!is_cloud_name(no), "{no}");
        }
    }

    #[test]
    fn the_process_environment_beats_the_settings_file() {
        let block = vec![
            ("AWS_REGION".to_string(), "us-west-2".to_string()),
            ("AWS_PROFILE".to_string(), "dev".to_string()),
            ("LD_PRELOAD".to_string(), "/evil.so".to_string()),
            ("AWS_EMPTY".to_string(), String::new()),
        ];
        let out = from_settings(&block, &env(&[("AWS_REGION", "eu-west-1")]));
        assert_eq!(out, vec![("AWS_PROFILE".to_string(), "dev".to_string())]);
    }

    #[test]
    fn a_child_command_gets_the_settings_names() {
        let mut command = tokio::process::Command::new("claude");
        for (name, value) in
            from_settings(&[("CLAUDE_CODE_USE_BEDROCK".into(), "1".into())], &env(&[]))
        {
            command.env(name, value);
        }
        let seen: Vec<_> = command.as_std().get_envs().collect();
        assert!(
            seen.iter()
                .any(|(k, v)| *k == "CLAUDE_CODE_USE_BEDROCK" && v.is_some())
        );
    }

    #[test]
    fn no_flag_means_no_route() {
        assert_eq!(route(&env(&[("AWS_REGION", "x")]), None), None);
        assert_eq!(route(&env(&[("CLAUDE_CODE_USE_BEDROCK", "0")]), None), None);
    }

    #[test]
    fn bedrock_needs_a_region_and_credentials() {
        let r = route(&env(&[("CLAUDE_CODE_USE_BEDROCK", "1")]), None).unwrap();
        assert_eq!(r.cloud, Cloud::Bedrock);
        assert_eq!(r.missing.len(), 2, "{r:?}");
        let r = route(
            &env(&[
                ("CLAUDE_CODE_USE_BEDROCK", "1"),
                ("AWS_REGION", "us-east-1"),
                ("AWS_PROFILE", "p"),
            ]),
            None,
        )
        .unwrap();
        assert!(r.missing.is_empty(), "{r:?}");
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aws")).unwrap();
        std::fs::write(home.path().join(".aws/credentials"), "x").unwrap();
        let r = route(
            &env(&[
                ("CLAUDE_CODE_USE_BEDROCK", "1"),
                ("AWS_DEFAULT_REGION", "us-east-1"),
            ]),
            Some(home.path()),
        )
        .unwrap();
        assert!(r.missing.is_empty(), "{r:?}");
    }

    #[test]
    fn vertex_needs_a_project_a_region_and_application_default_credentials() {
        let r = route(&env(&[("CLAUDE_CODE_USE_VERTEX", "1")]), None).unwrap();
        assert_eq!(r.cloud, Cloud::Vertex);
        assert_eq!(r.missing.len(), 3, "{r:?}");
        let r = route(
            &env(&[
                ("CLAUDE_CODE_USE_VERTEX", "true"),
                ("ANTHROPIC_VERTEX_PROJECT_ID", "p"),
                ("CLOUD_ML_REGION", "us-east5"),
                ("GOOGLE_APPLICATION_CREDENTIALS", "/k.json"),
            ]),
            None,
        )
        .unwrap();
        assert!(r.missing.is_empty(), "{r:?}");
    }

    #[test]
    fn foundry_needs_a_resource_and_a_key() {
        let r = route(&env(&[("CLAUDE_CODE_USE_FOUNDRY", "1")]), None).unwrap();
        assert_eq!(r.cloud, Cloud::Foundry);
        assert_eq!(r.missing.len(), 2);
        let r = route(
            &env(&[
                ("CLAUDE_CODE_USE_FOUNDRY", "1"),
                ("ANTHROPIC_FOUNDRY_RESOURCE", "my-res"),
                ("ANTHROPIC_FOUNDRY_API_KEY", "k"),
            ]),
            None,
        )
        .unwrap();
        assert!(r.missing.is_empty());
    }
}
