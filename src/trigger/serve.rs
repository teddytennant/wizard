//! `wizard trigger`: manage local triggers and serve their webhooks.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};

use super::{ApiError, Triggers, local, summary};
use crate::minihttp::{Request, read_request, respond};

/// Port `wizard trigger serve` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum TriggerCmd {
    /// List the triggers.
    List,
    /// Add a trigger: scheduled (`--cron` or `--at`), a webhook, or both.
    Add {
        /// A short name.
        name: String,
        /// What the headless run is asked to do.
        #[arg(long, allow_hyphen_values = true)]
        prompt: String,
        /// 5-field cron expression in local time.
        #[arg(long)]
        cron: Option<String>,
        /// Run once at this RFC 3339 time.
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
        /// Let `wizard trigger serve` start it on an authenticated POST.
        #[arg(long)]
        webhook: bool,
        /// Directory the run works in (default: the current one).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Wall-clock cap in hours for each run.
        #[arg(long)]
        max_hours: Option<f64>,
    },
    /// Remove a trigger and its schedule entry.
    Remove { id: String },
    /// Fire a disabled trigger's schedule again.
    Enable { id: String },
    /// Keep a trigger but stop its schedule and webhook.
    Disable { id: String },
    /// Start a trigger's run now, detached, and print where its log is.
    Run {
        id: String,
        /// Text handed to the run as data, like a webhook body.
        #[arg(long)]
        text: Option<String>,
    },
    /// Print a trigger's webhook token and the curl line that uses it,
    /// adding a webhook to the trigger if it has none.
    Token { id: String },
    /// Print the runs of a trigger, newest first.
    Runs { id: String },
    /// Listen for webhook calls: POST /v1/triggers/<id>/run with
    /// `Authorization: Bearer <token>`.
    Serve {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Address to bind. Loopback by default; reach it from elsewhere with
        /// an SSH tunnel, or bind a private interface on purpose.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
}

fn api(err: ApiError) -> anyhow::Error {
    anyhow!("{}", err.message)
}

pub async fn run(cmd: TriggerCmd) -> Result<i32> {
    let store = Triggers::standard().map_err(anyhow::Error::msg)?;
    let cwd = std::env::current_dir().context("reading the current directory")?;
    match cmd {
        TriggerCmd::List => {
            let all = store.list().map_err(api)?;
            if all.is_empty() {
                println!(
                    "no triggers. `wizard trigger add <name> --prompt ... --cron '0 9 * * 1-5'`"
                );
            }
            for t in all {
                let when = Triggers::next_run(&t).map_or("-".to_string(), local);
                println!(
                    "{}  {}  {}  next {}  {}{}",
                    t.id,
                    if t.enabled { "on " } else { "off" },
                    t.cron_expression
                        .as_deref()
                        .unwrap_or(if t.run_once_at.is_some() { "once" } else { "-" }),
                    when,
                    t.name,
                    if t.webhook_token.is_some() {
                        "  [webhook]"
                    } else {
                        ""
                    }
                );
            }
            Ok(0)
        }
        TriggerCmd::Add {
            name,
            prompt,
            cron,
            at,
            webhook,
            cwd: dir,
            max_hours,
        } => {
            let mut body = json!({"name": name, "prompt": prompt, "webhook": webhook});
            let map = body.as_object_mut().expect("object");
            if let Some(c) = cron {
                map.insert("cron_expression".into(), json!(c));
            }
            if let Some(a) = at {
                map.insert("run_once_at".into(), json!(a));
            }
            if let Some(d) = dir {
                map.insert("cwd".into(), json!(d));
            }
            if let Some(h) = max_hours {
                map.insert("max_hours".into(), json!(h));
            }
            let t = store.create(&body, &cwd).map_err(api)?;
            println!(
                "added {}\n{}",
                t.id,
                summary(&t, crate::schedule::daemon_running())
            );
            Ok(0)
        }
        TriggerCmd::Remove { id } => {
            store.delete(&id).map_err(api)?;
            println!("removed {id}");
            Ok(0)
        }
        TriggerCmd::Enable { id } => set_enabled(&store, &id, true),
        TriggerCmd::Disable { id } => set_enabled(&store, &id, false),
        TriggerCmd::Run { id, text } => {
            let run = store.run(&id, text.as_deref(), "manual").map_err(api)?;
            println!("{}", serde_json::to_string_pretty(&run)?);
            Ok(0)
        }
        TriggerCmd::Token { id } => {
            let t = store.add_webhook(&id).map_err(api)?;
            let token = t.webhook_token.clone().unwrap_or_default();
            println!("{token}");
            eprintln!(
                "curl -X POST -H 'Authorization: Bearer {token}' -d 'optional text' http://127.0.0.1:{DEFAULT_PORT}{}",
                Triggers::webhook_path(&t.id)
            );
            Ok(0)
        }
        TriggerCmd::Runs { id } => {
            for run in store.runs(&id) {
                println!(
                    "{}  {}  {}",
                    run["id"].as_str().unwrap_or("?"),
                    run["status"].as_str().unwrap_or("?"),
                    run["url"].as_str().unwrap_or("")
                );
            }
            Ok(0)
        }
        TriggerCmd::Serve { port, bind } => {
            let addr: SocketAddr = format!("{bind}:{port}")
                .parse()
                .with_context(|| format!("'{bind}:{port}' is not an address"))?;
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr}"))?;
            println!(
                "listening on http://{addr}  (POST /v1/triggers/<id>/run, bearer token per trigger; Ctrl-C stops it)"
            );
            if !addr.ip().is_loopback() {
                eprintln!(
                    "warning: {addr} is not loopback; anything that can reach it and holds a token can start a run"
                );
            }
            serve(listener, store).await;
            Ok(0)
        }
    }
}

fn set_enabled(store: &Triggers, id: &str, enabled: bool) -> Result<i32> {
    let t = store
        .update(id, &json!({"enabled": enabled}), Path::new("."))
        .map_err(api)?;
    println!("{} {}", if enabled { "enabled" } else { "disabled" }, t.id);
    Ok(0)
}

pub async fn serve(listener: TcpListener, store: Triggers) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let store = store.clone();
        tokio::spawn(async move {
            let _ = handle(stream, &store).await;
        });
    }
}

async fn handle(mut stream: TcpStream, store: &Triggers) -> std::io::Result<()> {
    let request = match read_request(&mut stream).await {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(_) => return respond(&mut stream, 400, "text/plain", b"bad request").await,
    };
    let store = store.clone();
    let (status, body) = tokio::task::spawn_blocking(move || route(&store, &request))
        .await
        .unwrap_or((500, json!({"error": "internal error"})));
    respond(
        &mut stream,
        status,
        "application/json",
        body.to_string().as_bytes(),
    )
    .await
}

/// The response for one request: status and JSON body.
pub fn route(store: &Triggers, request: &Request) -> (u16, Value) {
    let err = |e: ApiError| (e.status, json!({"error": e.message}));
    if request.path == "/healthz" {
        return (200, json!({"ok": true}));
    }
    let Some(id) = request
        .path
        .strip_prefix("/v1/triggers/")
        .and_then(|rest| rest.strip_suffix("/run"))
    else {
        return (404, json!({"error": "not found"}));
    };
    if request.method != "POST" {
        return (405, json!({"error": "POST only"}));
    }
    let presented = request
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if let Err(e) = store.authorize(id, presented) {
        // An unknown trigger and a bad token read the same from outside.
        return err(if e.status == 404 {
            ApiError {
                status: 401,
                message: "bad token".into(),
            }
        } else {
            e
        });
    }
    let payload = String::from_utf8_lossy(&request.body).into_owned();
    match store.run(id, Some(&payload), "webhook") {
        Ok(run) => (202, run),
        Err(e) => err(e),
    }
}
