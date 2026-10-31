//! `wizard artifact`: list the local artifacts and serve them on localhost.
//!
//! The server is read-only, binds loopback unless told otherwise, refuses
//! dotfiles (the per-artifact record) and any path that leaves an artifact's
//! directory. It exists so a page that fetches its own files, or one opened
//! from a phone over SSH port forwarding, has a real origin; `file://` URLs
//! work without it.

use std::net::SocketAddr;
use std::path::{Component, Path};

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};

use super::{Artifacts, DEFAULT_PORT, PAGE, valid_slug};
use crate::minihttp::{Request, content_type, decode_path, read_request, respond};

#[derive(Debug, Clone, clap::Subcommand)]
pub enum ArtifactCmd {
    /// List the artifacts the `Artifact` tool published on this machine.
    List,
    /// Serve them read-only on http://127.0.0.1:8765/<slug>/.
    Serve {
        /// Port to listen on.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Address to bind. Loopback by default; anything else exposes every
        /// artifact to that network.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
    /// Print an artifact's file:// URL.
    Path {
        /// The artifact's slug or URL.
        artifact: String,
    },
}

pub async fn run(cmd: ArtifactCmd) -> Result<i32> {
    let store = Artifacts::standard().map_err(anyhow::Error::msg)?;
    match cmd {
        ArtifactCmd::List => {
            println!(
                "{}",
                store
                    .call(&serde_json::json!({"action": "list"}), Path::new("."))
                    .map_err(anyhow::Error::msg)?
            );
            Ok(0)
        }
        ArtifactCmd::Path { artifact } => {
            let slug = store.slug_of(&artifact).map_err(anyhow::Error::msg)?;
            if store.meta(&slug).is_none() {
                anyhow::bail!("no local artifact '{slug}'");
            }
            println!("{}", store.file_url(&slug));
            Ok(0)
        }
        ArtifactCmd::Serve { port, bind } => {
            let addr: SocketAddr = format!("{bind}:{port}")
                .parse()
                .with_context(|| format!("'{bind}:{port}' is not an address"))?;
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr}"))?;
            println!(
                "serving {} on http://{addr}/ (read-only; Ctrl-C stops it)",
                store.root().display()
            );
            if !addr.ip().is_loopback() {
                eprintln!("warning: {addr} is not loopback; every artifact is readable from there");
            }
            serve(listener, store).await;
            Ok(0)
        }
    }
}

/// Accept connections until the process ends.
pub async fn serve(listener: TcpListener, store: Artifacts) {
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

async fn handle(mut stream: TcpStream, store: &Artifacts) -> std::io::Result<()> {
    let request = match read_request(&mut stream).await {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(_) => return respond(&mut stream, 400, "text/plain", b"bad request").await,
    };
    let (status, kind, body) = route(store, &request);
    respond(&mut stream, status, &kind, &body).await
}

/// The response for one request: status, media type, body.
pub fn route(store: &Artifacts, request: &Request) -> (u16, String, Vec<u8>) {
    let plain = |status: u16, text: &str| (status, "text/plain".to_string(), text.as_bytes().to_vec());
    if request.method != "GET" && request.method != "HEAD" {
        return plain(405, "read-only");
    }
    let Some(path) = decode_path(&request.path) else {
        return plain(400, "bad path");
    };
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return (200, "text/html; charset=utf-8".into(), index(store).into_bytes());
    }
    let (slug, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    if !valid_slug(slug) || store.meta(slug).is_none() {
        return plain(404, "no such artifact");
    }
    if rest.is_empty() && !path.ends_with('/') {
        return (
            200,
            "text/html; charset=utf-8".into(),
            format!("<meta http-equiv=\"refresh\" content=\"0; url=/{slug}/\">").into_bytes(),
        );
    }
    let rest = if rest.is_empty() { PAGE } else { rest };
    let rel = Path::new(rest);
    let safe = rel.components().all(|c| match c {
        Component::Normal(part) => !part.to_string_lossy().starts_with('.'),
        _ => false,
    });
    if !safe {
        return plain(403, "forbidden");
    }
    match std::fs::read(store.dir(slug).join(rel)) {
        Ok(bytes) => (200, content_type(rest).to_string(), bytes),
        Err(_) => plain(404, "not found"),
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn index(store: &Artifacts) -> String {
    let mut rows = String::new();
    for meta in store.all() {
        rows.push_str(&format!(
            "<li><a href=\"/{slug}/\">{title}</a> <small>v{v}, {when}</small></li>\n",
            slug = meta.slug,
            title = html_escape(&meta.title),
            v = meta.version,
            when = meta.updated.format("%Y-%m-%d %H:%M UTC"),
        ));
    }
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Local artifacts</title><style>body{{font:16px system-ui;margin:2rem auto;max-width:40rem;padding:0 16px}}</style>\
         <h1>Local artifacts</h1><ul>\n{rows}</ul>"
    )
}
