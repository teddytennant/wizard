//! The Claude Code commands whose real versions sit on Anthropic's cloud, each
//! answered with what Wizard has instead. Every reply says what is local.
//!
//! | Command | Local counterpart |
//! | :- | :- |
//! | `/artifacts` | pages the `Artifact` tool wrote under `~/.wizard/artifacts` |
//! | `/advisor` | the `advisor` tool and the `advisorModel` setting |
//! | `/voice` | dictation from a local recorder and speech-to-text program |
//! | `/teleport` | moving a session between your own machines |
//! | `/remote-control` | attaching over ssh, ACP, or a token-guarded trigger |
//! | `/remote-env` | where triggers run |
//! | `/desktop`, `/mobile` | Wizard GUI and the Android app |
//! | `/setup-bedrock`, `/setup-vertex` | the cloud route of the `claude` provider |

use std::path::Path;

use serde_json::json;

use super::surface::CommandSurface;
use crate::artifact::Artifacts;
use crate::trigger::Triggers;

pub const NAMES: &[&str] = &[
    "artifacts",
    "advisor",
    "voice",
    "teleport",
    "remote-control",
    "remote-env",
    "desktop",
    "mobile",
    "setup-bedrock",
    "setup-vertex",
];

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

pub async fn run<S: CommandSurface + Send + ?Sized>(name: &str, args: &str, surface: &mut S) {
    let args = args.trim();
    let outcome = match name {
        "artifacts" => artifacts(args),
        "advisor" => crate::advisor::command(args),
        "voice" => return voice(args, surface).await,
        "teleport" => return teleport(args, surface).await,
        "remote-control" => Triggers::standard().and_then(|s| remote_control(&s, args)),
        "remote-env" => Triggers::standard().and_then(|s| remote_env(&s)),
        "desktop" => desktop(args, &surface.project_root()),
        "mobile" => Ok(mobile()),
        "setup-bedrock" => crate::usersettings::path().and_then(|p| setup_bedrock(&p, args)),
        "setup-vertex" => crate::usersettings::path().and_then(|p| setup_vertex(&p, args)),
        other => Err(format!("'/{other}' is not a cloud-equivalent command")),
    };
    match outcome {
        Ok(text) => surface.notice(text),
        Err(message) => surface.error(message),
    }
}

/* ---------------------------------------------------------------------- */
/* /artifacts                                                             */
/* ---------------------------------------------------------------------- */

fn artifacts(args: &str) -> Result<String, String> {
    let store = Artifacts::standard()?;
    let mut words = args.split_whitespace();
    let verb = words.next().unwrap_or("list");
    let target = words.next();
    let call = |input: serde_json::Value| store.call(&input, Path::new("."));
    match (verb, target) {
        ("list" | "ls", _) => call(json!({"action": "list"})),
        ("open", Some(url)) => call(json!({"action": "open", "url": url})),
        ("delete" | "rm", Some(url)) => call(json!({"action": "delete", "url": url})),
        ("pin", Some(url)) => call(json!({"action": "pin", "url": url})),
        ("unpin", Some(url)) => call(json!({"action": "unpin", "url": url})),
        ("serve", _) => Ok(format!(
            "Run `wizard artifact serve` in another terminal to serve {} on http://127.0.0.1:{}/ (read-only, loopback). \
             The file:// links work without it.",
            store.root().display(),
            crate::artifact::DEFAULT_PORT
        )),
        _ => Err("usage: /artifacts [list | open <slug> | delete <slug> | pin <slug> | unpin <slug> | serve]. \
                  These are local pages the Artifact tool wrote, not claude.ai artifacts."
            .to_string()),
    }
}

/* ---------------------------------------------------------------------- */
/* /voice                                                                 */
/* ---------------------------------------------------------------------- */

async fn voice<S: CommandSurface + Send + ?Sized>(args: &str, surface: &mut S) {
    if args == "stop" {
        return if crate::voice::stop() {
            surface.notice("stopping the recording; the text arrives in the composer in a moment".into());
        } else {
            surface.error("not recording".into());
        };
    }
    let config = crate::voice::VoiceConfig::load();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = crate::voice::detect(&config, &path, dirs::home_dir().as_deref());
    if args == "status" || args == "setup" {
        let mut text = format!("voice dictation\n{}", found.report());
        text.push_str(&match found.missing() {
            Some(missing) => format!("\n{missing}"),
            None => "\nready: /voice records, /voice stop ends it, and the text lands in the composer.".into(),
        });
        return surface.notice(text);
    }
    let Some(plan) = found.plan() else {
        return surface.error(found.missing().unwrap_or_default());
    };
    let seconds = match args {
        "" => config.seconds,
        n => match n.parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => return surface.error("usage: /voice [seconds] | /voice stop | /voice status".into()),
        },
    };
    let Some(composer) = surface.composer() else {
        return surface.error(
            "dictation puts text in the terminal UI's composer, and this surface has none".into(),
        );
    };
    if crate::voice::is_recording() {
        return surface.error("already recording; /voice stop ends it".into());
    }
    surface.notice(format!(
        "recording up to {seconds}s with {} (transcribing with {}). Speak, then /voice stop.",
        plan.recorder.name(),
        plan.transcriber.name()
    ));
    tokio::spawn(async move {
        use crate::event::Event;
        match crate::voice::dictate(&plan, seconds).await {
            Ok(text) => {
                let _ = composer.send(Event::Notice(format!("heard: {text}"))).await;
                let _ = composer.send(Event::Paste(text)).await;
            }
            Err(err) => {
                let _ = composer.send(Event::Notice(format!("voice: {err}"))).await;
            }
        }
    });
}

/* ---------------------------------------------------------------------- */
/* /teleport                                                              */
/* ---------------------------------------------------------------------- */

async fn teleport<S: CommandSurface + Send + ?Sized>(args: &str, surface: &mut S) {
    let sessions = match crate::config::Config::sessions_dir() {
        Ok(dir) => dir,
        Err(err) => return surface.error(format!("{err:#}")),
    };
    let mut words = args.split_whitespace();
    match words.next() {
        None => {
            let count = std::fs::read_dir(&sessions)
                .map(|d| d.filter_map(Result::ok).count())
                .unwrap_or(0);
            surface.notice(format!(
                "teleport moves a session between machines you own; there are no cloud sessions here.\n\
                 \n\
                 Pull one onto this machine:\n\
                 \x20 /teleport user@host            the newest session on that machine (over ssh)\n\
                 \x20 /teleport user@host:<id>       a named one\n\
                 \x20 /teleport <file or https url>  an envelope made by `/teleport pack`\n\
                 Send this one away: /teleport pack [id] writes ~/.wizard/teleport/<id>.gz.\n\
                 \n\
                 To set up: Wizard 3.12 or later on both machines with `wizard` on the PATH of a non-interactive \
                 ssh login, and key-based ssh from this machine to the other (ssh-copy-id). `wizard sync` moves \
                 config only and the mesh is a read-only live view, so neither carries a session. The envelope is \
                 not signed: pull only from machines you trust. {count} sessions are stored here."
            ));
        }
        Some("pack") => {
            let id = match words.next().map(str::to_string).or_else(|| surface.snapshot().session) {
                Some(id) => id,
                None => return surface.error("no session yet; send a message first, or name one: /teleport pack <id>".into()),
            };
            let result = (|| -> anyhow::Result<std::path::PathBuf> {
                let bytes = crate::teleport::pack(&sessions, &id)?;
                let dir = crate::config::Config::wizard_dir()?.join("teleport");
                std::fs::create_dir_all(&dir)?;
                let out = dir.join(format!("{id}.gz"));
                std::fs::write(&out, bytes)?;
                Ok(out)
            })();
            match result {
                Ok(out) => surface.notice(format!(
                    "packed session {id} into {}\nOn the other machine: scp this machine:{} . && wizard teleport pull ./{id}.gz\n\
                     or, from there over ssh: /teleport {}@<this host>:{id}",
                    out.display(),
                    out.display(),
                    std::env::var("USER").unwrap_or_else(|_| "user".into())
                )),
                Err(err) => surface.error(format!("{err:#}")),
            }
        }
        Some(source) => {
            let cwd = surface.project_root();
            let pulled = async {
                let bytes = crate::teleport::fetch(&crate::teleport::parse_source(source)).await?;
                let envelope = crate::teleport::unpack(&bytes)?;
                let id = crate::teleport::import(&envelope, &sessions, &cwd)?;
                anyhow::Ok((id, envelope))
            }
            .await;
            match pulled {
                Ok((id, envelope)) => {
                    surface.notice(format!(
                        "imported session {id} from {} (it ran in {}); continuing it here in {}",
                        envelope.host,
                        if envelope.cwd.is_empty() { "an unknown directory" } else { &envelope.cwd },
                        cwd.display()
                    ));
                    surface.resume(id).await;
                }
                Err(err) => surface.error(format!("{err:#}")),
            }
        }
    }
}

/* ---------------------------------------------------------------------- */
/* /remote-control, /remote-env                                           */
/* ---------------------------------------------------------------------- */

/// The name of the trigger `/remote-control on` creates.
const REMOTE_TRIGGER: &str = "remote-control";

fn host() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "<this-host>".into())
}

fn remote_control(store: &Triggers, args: &str) -> Result<String, String> {
    let existing = store
        .list()
        .map_err(|e| e.message)?
        .into_iter()
        .find(|t| t.name == REMOTE_TRIGGER && t.webhook_token.is_some());
    match args {
        "on" => {
            if existing.is_none() {
                let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
                store
                    .create(
                        &json!({
                            "name": REMOTE_TRIGGER,
                            "prompt": "Do what the request below asks, working in this directory.",
                            "webhook": true,
                            "max_hours": 1.0,
                        }),
                        &cwd,
                    )
                    .map_err(|e| e.message)?;
            }
            return remote_control(store, "");
        }
        "off" => {
            return match existing {
                Some(t) => {
                    store.delete(&t.id).map_err(|e| e.message)?;
                    Ok(format!("removed the {REMOTE_TRIGGER} trigger {}; its token no longer works", t.id))
                }
                None => Ok("no remote-control trigger to remove".into()),
            };
        }
        "" => {}
        _ => return Err("usage: /remote-control [on|off]".into()),
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let host = host();
    let mut out = format!(
        "remote control, without Anthropic's relay: reach this machine directly.\n\
         \n\
         Terminal or phone over ssh (the Android app does this): ssh {user}@{host}, then `wizard --resume` \
         continues the newest session in a directory.\n\
         Editors and the Android app drive the agent over ACP: ssh {user}@{host} wizard acp\n\
         Chat: `wizard gateway setup` puts the same agent behind a Telegram bot.\n"
    );
    match existing {
        Some(t) => {
            out.push_str(&format!(
                "\nHTTP trigger (on, runs a headless sovereign session in {}):\n  \
                 start it: wizard trigger serve   (listens on 127.0.0.1:{}; tunnel it with ssh -L, or --bind a private interface)\n  \
                 call it:  curl -X POST -H 'Authorization: Bearer {}' -d 'what to do' http://{host}:{}{}\n\
                 Anyone with that token can make this machine run the agent in that directory. /remote-control off revokes it.",
                t.cwd.display(),
                crate::trigger::serve::DEFAULT_PORT,
                t.webhook_token.unwrap_or_default(),
                crate::trigger::serve::DEFAULT_PORT,
                Triggers::webhook_path(&t.id),
            ));
        }
        None => out.push_str(
            "\nNo HTTP trigger yet. /remote-control on makes one for this directory and prints its token; \
             it lets a caller with the token start a headless run here.",
        ),
    }
    Ok(out)
}

fn remote_env(store: &Triggers) -> Result<String, String> {
    let triggers = store.list().map_err(|e| e.message)?;
    let mut out = String::from(
        "remote environments: Anthropic's cloud sandboxes are not available. Here an environment is a \
         directory on a machine you run: a trigger's `cwd`, `mode` and `max_hours` are its environment.\n",
    );
    if triggers.is_empty() {
        out.push_str("no triggers yet (RemoteTrigger or `wizard trigger add`).\n");
    }
    for t in &triggers {
        out.push_str(&format!(
            "  {}  {}  cwd {}  mode {}  cap {}h{}\n",
            t.id,
            t.name,
            t.cwd.display(),
            t.mode,
            t.max_hours.map_or("none".to_string(), |h| h.to_string()),
            if t.webhook_token.is_some() { "  webhook" } else { "" }
        ));
    }
    out.push_str(&format!(
        "scheduler: {}\nParallel work on this machine: `wizard fleet` and /background.",
        if crate::schedule::daemon_running() {
            "running"
        } else {
            "not running (scheduled triggers wait for `wizard scheduler`)"
        }
    ));
    Ok(out)
}

/* ---------------------------------------------------------------------- */
/* /desktop, /mobile                                                      */
/* ---------------------------------------------------------------------- */

fn desktop(args: &str, root: &Path) -> Result<String, String> {
    let home = dirs::home_dir();
    let path = std::env::var_os("PATH");
    let found = crate::desktop_app::find(std::env::consts::OS, path.as_deref(), home.as_deref());
    match (args, found) {
        ("open", Some(_)) => {
            crate::desktop_app::open(Some(root)).map_err(|e| format!("{e:#}"))?;
            Ok(format!("opened Wizard GUI in {}", root.display()))
        }
        ("", Some(_)) => Ok(format!(
            "Wizard GUI is installed. `/desktop open` (or `wizard gui`) opens it in {}. It is a separate app over the \
             same agent: it starts its own session, so use /resume there to continue this one.",
            root.display()
        )),
        (_, None) => Ok(format!(
            "Wizard GUI, the desktop app, is not installed. Get it from {}. It is not a handoff of this session: \
             it starts its own, and /resume there continues this one.",
            crate::desktop_app::RELEASES
        )),
        _ => Err("usage: /desktop [open]".into()),
    }
}

fn mobile() -> String {
    format!(
        "There is no Anthropic mobile app link. The Android app is Wizard's own: it connects to your machines over ssh and \
         drives `wizard acp` there, with no server in between. Install https://github.com/teddytennant/wizard/releases/latest/download/wizard-android.apk, \
         add this machine as ssh {}@{}, and start a chat. `/remote-control` prints the same details plus the options that \
         need no app.",
        std::env::var("USER").unwrap_or_else(|_| "user".into()),
        host()
    )
}

/* ---------------------------------------------------------------------- */
/* /setup-bedrock, /setup-vertex                                          */
/* ---------------------------------------------------------------------- */

/// `word=value` and `--word value` pairs, and the bare words around them.
fn options(args: &str) -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let mut bare = Vec::new();
    let mut named = std::collections::BTreeMap::new();
    let mut words = args.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if let Some(flag) = word.strip_prefix("--") {
            if let Some((k, v)) = flag.split_once('=') {
                named.insert(k.to_string(), v.to_string());
            } else if let Some(v) = words.next() {
                named.insert(flag.to_string(), v.to_string());
            }
        } else if let Some((k, v)) = word.split_once('=') {
            named.insert(k.to_string(), v.to_string());
        } else {
            bare.push(word.to_string());
        }
    }
    (bare, named)
}

const BEDROCK_NAMES: &[&str] = &["CLAUDE_CODE_USE_BEDROCK", "AWS_REGION", "AWS_PROFILE", "ANTHROPIC_MODEL"];
const VERTEX_NAMES: &[&str] = &[
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "ANTHROPIC_MODEL",
];

fn route_verdict(file: &Path) -> String {
    let block = crate::usersettings::load_at(file.to_path_buf())
        .ok()
        .and_then(|(_, v)| crate::usersettings::env_of(&v))
        .unwrap_or_default();
    match crate::cloudroute::route_with(&block) {
        None => "no cloud route is on".into(),
        Some(r) if r.missing.is_empty() => {
            format!("{} route is complete ({})", r.cloud.name(), r.found.join(", "))
        }
        Some(r) => format!("{} route still needs: {}", r.cloud.name(), r.missing.join("; ")),
    }
}

const HOW_TO_USE: &str = "It applies to the Claude Code provider: `wizard --provider claude`, or /provider to switch. Requests \
     then go through the real `claude` with these variables, using your cloud credentials from the shell or the cloud's own \
     credential store. No keys are written to disk.";

fn setup_bedrock(file: &Path, args: &str) -> Result<String, String> {
    if args == "off" {
        let path = crate::usersettings::merge_env_at(file.to_path_buf(), &[], BEDROCK_NAMES)?;
        return Ok(format!("Bedrock route removed from {}. {}", path.display(), route_verdict(file)));
    }
    let (bare, named) = options(args);
    let region = named
        .get("region")
        .cloned()
        .or_else(|| bare.first().cloned());
    let Some(region) = region else {
        let have = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        let aws = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("aws").is_file()))
            .unwrap_or(false);
        return Ok(format!(
            "Bedrock setup (the Claude Code provider routes through AWS when CLAUDE_CODE_USE_BEDROCK is set)\n\
             \x20 aws cli: {}\n\
             \x20 AWS_REGION: {}\n\
             \x20 AWS_PROFILE: {}\n\
             \x20 ~/.aws: {}\n\
             \x20 now: {}\n\
             \n\
             Run /setup-bedrock <region> [profile=<name>] [model=<inference profile or model id>] to save the route to \
             ~/.wizard/settings.json, and /setup-bedrock off to remove it. Enable the Claude models in the Bedrock console \
             first, and sign in the usual way (`aws sso login`, a profile, or AWS_BEARER_TOKEN_BEDROCK in your shell). \
             {HOW_TO_USE}",
            if aws { "found" } else { "not found (optional)" },
            have("AWS_REGION").or_else(|| have("AWS_DEFAULT_REGION")).unwrap_or_else(|| "not set".into()),
            have("AWS_PROFILE").unwrap_or_else(|| "not set".into()),
            if dirs::home_dir().is_some_and(|h| h.join(".aws").is_dir()) { "present" } else { "absent" },
            route_verdict(file),
        ));
    };
    let mut pairs = vec![
        ("CLAUDE_CODE_USE_BEDROCK", "1".to_string()),
        ("AWS_REGION", region.clone()),
    ];
    if let Some(profile) = named.get("profile") {
        pairs.push(("AWS_PROFILE", profile.clone()));
    }
    if let Some(model) = named.get("model") {
        pairs.push(("ANTHROPIC_MODEL", model.clone()));
    }
    let path = crate::usersettings::merge_env_at(file.to_path_buf(), &pairs, &[])?;
    Ok(format!(
        "Bedrock route saved to {} ({}). Now: {}. {HOW_TO_USE}",
        path.display(),
        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
        route_verdict(file)
    ))
}

fn setup_vertex(file: &Path, args: &str) -> Result<String, String> {
    if args == "off" {
        let path = crate::usersettings::merge_env_at(file.to_path_buf(), &[], VERTEX_NAMES)?;
        return Ok(format!("Vertex route removed from {}. {}", path.display(), route_verdict(file)));
    }
    let (bare, named) = options(args);
    let project = named.get("project").cloned().or_else(|| bare.first().cloned());
    let Some(project) = project else {
        let have = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        let adc = dirs::home_dir()
            .is_some_and(|h| h.join(".config/gcloud/application_default_credentials.json").is_file());
        let gcloud = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("gcloud").is_file()))
            .unwrap_or(false);
        return Ok(format!(
            "Vertex setup (the Claude Code provider routes through Google Cloud when CLAUDE_CODE_USE_VERTEX is set)\n\
             \x20 gcloud: {}\n\
             \x20 application-default credentials: {}\n\
             \x20 GOOGLE_APPLICATION_CREDENTIALS: {}\n\
             \x20 now: {}\n\
             \n\
             Run /setup-vertex <project-id> [region=<region>] [model=<model id>] to save the route to \
             ~/.wizard/settings.json (region defaults to global), and /setup-vertex off to remove it. Enable the Claude \
             models in Vertex Model Garden first, and sign in with `gcloud auth application-default login`. {HOW_TO_USE}",
            if gcloud { "found" } else { "not found (optional)" },
            if adc { "present" } else { "absent" },
            have("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_else(|| "not set".into()),
            route_verdict(file),
        ));
    };
    let region = named.get("region").cloned().unwrap_or_else(|| "global".into());
    let mut pairs = vec![
        ("CLAUDE_CODE_USE_VERTEX", "1".to_string()),
        ("ANTHROPIC_VERTEX_PROJECT_ID", project),
        ("CLOUD_ML_REGION", region),
    ];
    if let Some(model) = named.get("model") {
        pairs.push(("ANTHROPIC_MODEL", model.clone()));
    }
    let path = crate::usersettings::merge_env_at(file.to_path_buf(), &pairs, &[])?;
    Ok(format!(
        "Vertex route saved to {} ({}). Now: {}. {HOW_TO_USE}",
        path.display(),
        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
        route_verdict(file)
    ))
}

#[cfg(test)]
mod tests;
