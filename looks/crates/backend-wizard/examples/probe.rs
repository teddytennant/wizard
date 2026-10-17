//! Debug the backend without a UI: `cargo run -p backend-wizard --example probe [-- --resume <id>]`.
//!
//! Prints every `Event` as one line. Each line of stdin is sent as a prompt,
//! except `:cancel`, `:new`, `:list`, `:load <id>`, `:model <id>`, `:effort <x>`,
//! `:mode <x>`, `:refresh` and `:quit`. Runs in the current directory.

use agent_core::{Backend, Event, Request};
use backend_wizard::WizardBackend;

fn line(e: &Event) -> String {
    match e {
        Event::TextDelta(t) => format!("TextDelta {t:?}"),
        Event::ThoughtDelta(t) => format!("ThoughtDelta {t:?}"),
        Event::Commands(c) => format!(
            "Commands {} [{}]",
            c.len(),
            c.iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Event::Ready { session_id, config } => format!(
            "Ready {session_id} model={} effort={} mode={} models={} backend={:?}",
            config.model,
            config.effort,
            config.mode,
            config.models.len(),
            config.backend
        ),
        Event::History { session_id, items } => {
            format!("History {session_id} {} items", items.len())
        }
        other => format!("{other:?}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resume = args
        .iter()
        .position(|a| a == "--resume")
        .and_then(|i| args.get(i + 1).cloned());
    let mut h = WizardBackend::spawn(std::env::current_dir()?, resume)?;

    let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for l in std::io::stdin().lines().map_while(Result::ok) {
            if in_tx.send(l).is_err() {
                break;
            }
        }
    });

    // With piped input, stdin ends before the answers do: wait for every turn
    // we started, then give the trailing Usage a moment before shutting down.
    let mut stdin_open = true;
    let mut outstanding = 0i32;
    loop {
        if !stdin_open && outstanding <= 0 {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(600), async {
                while let Some(e) = h.rx.recv().await {
                    println!("{}", line(&e));
                }
            })
            .await;
            let _ = h.tx.send(Request::Shutdown);
            break;
        }
        tokio::select! {
            e = h.rx.recv() => match e {
                Some(e) => {
                    println!("{}", line(&e));
                    match e {
                        Event::TurnEnd(_) => outstanding -= 1,
                        Event::Fatal(_) => break,
                        _ => {}
                    }
                }
                None => break,
            },
            l = in_rx.recv(), if stdin_open => match l {
                None => stdin_open = false,
                Some(l) => {
                    let l = l.trim().to_string();
                    let (cmd, arg) = l.split_once(' ').map(|(a, b)| (a, b.trim().to_string())).unwrap_or((l.as_str(), String::new()));
                    let r = match cmd {
                        ":cancel" => Request::Cancel,
                        ":new" => Request::NewSession,
                        ":list" => Request::ListSessions,
                        ":load" => Request::LoadSession(arg),
                        ":model" => Request::SetModel(arg),
                        ":effort" => Request::SetEffort(arg),
                        ":mode" => Request::SetMode(arg),
                        ":refresh" => Request::Refresh,
                        ":quit" => Request::Shutdown,
                        _ if l.is_empty() => continue,
                        _ => Request::Prompt(l.clone()),
                    };
                    let quit = r == Request::Shutdown;
                    if matches!(r, Request::Prompt(_)) {
                        outstanding += 1;
                    }
                    let _ = h.tx.send(r);
                    if quit { break; }
                }
            },
        }
    }
    Ok(())
}
