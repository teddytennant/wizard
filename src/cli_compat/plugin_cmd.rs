//! `wizard plugin ...`: the verbs of `claude plugin`, over the Claude-format
//! plugin store the `/plugin` slash command already manages
//! ([`crate::claude_dir::plugins`]).
//!
//! `--scope` is accepted and ignored: Wizard keeps one store, in
//! `~/.wizard/claude-plugins`. `--json` prints one result object.

use std::path::Path;

use anyhow::Result;
use serde_json::json;

use super::{MarketplaceCmd, PluginCmd};
use crate::claude_dir::plugins;

/// Print a result the way Claude Code does, or as one JSON line, and give the
/// exit code.
fn emit(result: Result<String, String>, json_out: bool) -> i32 {
    match result {
        Ok(message) => {
            if json_out {
                println!("{}", json!({"ok": true, "message": message}));
            } else {
                println!("\u{2714} {message}");
            }
            0
        }
        Err(error) => {
            if json_out {
                println!("{}", json!({"ok": false, "error": error}));
            } else {
                eprintln!("\u{2718} {error}");
            }
            1
        }
    }
}

fn warn_scope(scope: &Option<String>) {
    if let Some(scope) = scope.as_deref().filter(|s| *s != "user") {
        eprintln!("wizard: plugins have one store, so --scope {scope} is treated as user");
    }
}

/// A marketplace directory is validated as one; anything else as a plugin.
fn validate(path: &Path, strict: bool) -> Result<String, String> {
    let marketplace = path.join(".claude-plugin").join("marketplace.json");
    if marketplace.is_file() {
        let (name, listed) = plugins::read_marketplace(path)?;
        return Ok(format!(
            "{name}: valid marketplace ({} plugins)",
            listed.len()
        ));
    }
    let report = plugins::validate(path)?;
    if strict && !path.join("README.md").is_file() {
        return Err(format!("{report}; --strict: README.md is missing"));
    }
    Ok(report)
}

pub fn run(cmd: PluginCmd) -> Result<i32> {
    let root = std::env::current_dir()?;
    Ok(match cmd {
        PluginCmd::List { json: as_json, .. } => {
            let active = plugins::enabled(&root);
            if as_json {
                let installed = plugins::installed_names();
                let rows: Vec<_> = active
                    .iter()
                    .map(|p| {
                        json!({
                            "id": p.name,
                            "version": p.version,
                            "enabled": true,
                            "origin": p.origin,
                            "installPath": p.root,
                        })
                    })
                    .chain(
                        installed
                            .iter()
                            .filter(|(name, on)| !on && !active.iter().any(|p| &p.name == name))
                            .map(|(name, _)| json!({"id": name, "enabled": false})),
                    )
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if active.is_empty() && plugins::installed_names().is_empty() {
                println!("No plugins installed. Use `wizard plugin install` to install a plugin.");
            } else {
                print!("{}", plugins::report(&root));
                println!();
                for (name, on) in plugins::installed_names() {
                    if !on {
                        println!("{name:<22} disabled");
                    }
                }
            }
            0
        }
        PluginCmd::Install {
            plugin,
            marketplace,
            scope,
            json: as_json,
            ..
        } => {
            warn_scope(&scope);
            let spec = match marketplace {
                Some(source) => {
                    let (market, _) =
                        plugins::marketplace_add_named(&source).map_err(anyhow::Error::msg)?;
                    format!("{plugin}@{market}")
                }
                None => plugin,
            };
            emit(plugins::install(&spec), as_json)
        }
        PluginCmd::Uninstall {
            plugin,
            scope,
            json: as_json,
            ..
        } => {
            warn_scope(&scope);
            emit(plugins::uninstall(&plugin), as_json)
        }
        PluginCmd::Enable {
            plugin,
            scope,
            json: as_json,
        } => {
            warn_scope(&scope);
            emit(plugins::set_enabled(&plugin, true), as_json)
        }
        PluginCmd::Disable {
            plugin,
            all,
            scope,
            json: as_json,
        } => {
            warn_scope(&scope);
            match (plugin, all) {
                (_, true) => {
                    let mut code = 0;
                    for (name, on) in plugins::installed_names() {
                        if on {
                            code |= emit(plugins::set_enabled(&name, false), as_json);
                        }
                    }
                    code
                }
                (Some(name), false) => emit(plugins::set_enabled(&name, false), as_json),
                (None, false) => {
                    eprintln!("name a plugin, or pass --all");
                    1
                }
            }
        }
        PluginCmd::Update {
            plugin,
            scope,
            json: as_json,
            ..
        } => {
            warn_scope(&scope);
            emit(plugins::update(&plugin), as_json)
        }
        PluginCmd::Validate {
            path,
            json: as_json,
            strict,
        } => {
            println!("Validating {}\n", path.display());
            emit(validate(&path, strict), as_json)
        }
        PluginCmd::Details { name } => match plugins::details(&name, &root) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(err) => {
                eprintln!("{err}");
                1
            }
        },
        PluginCmd::Marketplace { cmd } => match cmd {
            MarketplaceCmd::Add {
                source,
                json: as_json,
                scope,
            } => {
                warn_scope(&scope);
                emit(plugins::marketplace_add(&source), as_json)
            }
            MarketplaceCmd::List { json: as_json } => {
                let text = plugins::marketplace_list();
                if as_json {
                    println!(
                        "{}",
                        json!({"marketplaces": text.lines().collect::<Vec<_>>()})
                    );
                } else {
                    println!("Configured marketplaces:\n\n{text}");
                }
                0
            }
            MarketplaceCmd::Remove {
                name,
                json: as_json,
            } => emit(plugins::marketplace_remove(&name), as_json),
            MarketplaceCmd::Update { name } => {
                if name.is_some() {
                    eprintln!("wizard: marketplaces update together; the name is ignored");
                }
                emit(plugins::marketplace_update(), false)
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_with_no_manifest_fails_validation() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate(tmp.path(), false).is_err());
    }

    #[test]
    fn a_valid_plugin_passes_and_strict_wants_a_readme() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&manifest).unwrap();
        std::fs::write(
            manifest.join("plugin.json"),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .unwrap();
        assert!(
            validate(tmp.path(), false)
                .unwrap()
                .starts_with("demo: valid")
        );
        assert!(validate(tmp.path(), true).is_err());
        std::fs::write(tmp.path().join("README.md"), "demo").unwrap();
        assert!(validate(tmp.path(), true).is_ok());
    }

    #[test]
    fn a_marketplace_directory_validates_as_one() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&manifest).unwrap();
        std::fs::write(
            manifest.join("marketplace.json"),
            r#"{"name":"mk","plugins":[{"name":"p","source":"./p"}]}"#,
        )
        .unwrap();
        assert_eq!(
            validate(tmp.path(), false).unwrap(),
            "mk: valid marketplace (1 plugins)"
        );
    }
}
