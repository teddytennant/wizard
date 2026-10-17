//! Prints every `Event` from the Claude backend as one line. Prompts come from stdin.
//!
//!     echo 'say hi' | cargo run -p backend-claude --example probe -- --model haiku
//!
//! Flags: `--cwd DIR`, `--resume ID`, `--model M`, `--effort E`, `--mode MODE`.
//! A stdin line starting with `:` is a request instead of a prompt: `:cancel`, `:new`,
//! `:sessions`, `:load ID`, `:model M`, `:effort E`, `:mode M`, `:refresh`, `:quit`.
//! Prompts wait for the running turn to end; `:` lines never wait.

use agent_core::{Event, Request};
use backend_claude::{ClaudeBackend, ClaudeOptions};
use std::io::BufRead;

fn short(s: &str) -> String {
    let flat = s.replace('\n', "\\n");
    if flat.chars().count() > 300 {
        format!("{}...", flat.chars().take(300).collect::<String>())
    } else {
        flat
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut opts = ClaudeOptions::from_env(std::env::current_dir()?);
    let mut resume = None;
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_default();
        match a.as_str() {
            "--cwd" => opts.cwd = val().into(),
            "--resume" => resume = Some(val()),
            "--model" => opts.model = Some(val()),
            "--effort" => opts.effort = Some(val()),
            "--mode" => opts.mode = val(),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    let mut h = ClaudeBackend::spawn_with(opts, resume)?;
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for l in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line_tx.send(l).is_err() {
                break;
            }
        }
    });

    let mut ready = false;
    let mut in_turn = false;
    let mut held: std::collections::VecDeque<String> = Default::default();
    let mut stdin_done = false;
    loop {
        // Send the next held prompt once idle.
        if ready && !in_turn {
            if let Some(p) = held.pop_front() {
                h.tx.send(Request::Prompt(p))?;
                in_turn = true;
            } else if stdin_done {
                break;
            }
        }
        tokio::select! {
            e = h.rx.recv() => {
                let Some(e) = e else { break };
                match &e {
                    Event::Ready { .. } => ready = true,
                    Event::TurnStart => in_turn = true,
                    Event::TurnEnd(_) => in_turn = false,
                    Event::Fatal(_) => { println!("{}", short(&format!("{e:?}"))); break }
                    _ => {}
                }
                match &e {
                    Event::TextDelta(t) => println!("TextDelta {:?}", t),
                    Event::ThoughtDelta(t) => println!("ThoughtDelta {:?}", t),
                    Event::Tool(t) => println!("{}", short(&format!("Tool {:?} {} [{:?}] {:?} parent={:?} out={:?}", t.status, t.name, t.kind, t.title, t.parent_id, t.output.as_deref().map(|o| o.chars().take(80).collect::<String>())))),
                    other => println!("{}", short(&format!("{other:?}"))),
                }
            }
            l = line_rx.recv(), if !stdin_done => match l {
                None => stdin_done = true,
                Some(l) => match l.strip_prefix(':') {
                    Some(cmd) => {
                        let (c, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim().to_string())).unwrap_or((cmd, String::new()));
                        let r = match c {
                            "cancel" => Request::Cancel,
                            "new" => Request::NewSession,
                            "sessions" => Request::ListSessions,
                            "load" => Request::LoadSession(arg),
                            "model" => Request::SetModel(arg),
                            "effort" => Request::SetEffort(arg),
                            "mode" => Request::SetMode(arg),
                            "refresh" => Request::Refresh,
                            "quit" => break,
                            other => { eprintln!("unknown request :{other}"); continue }
                        };
                        h.tx.send(r)?;
                    }
                    None if l.trim().is_empty() => {}
                    None => held.push_back(l),
                },
            },
        }
    }
    let _ = h.tx.send(Request::Shutdown);
    // Let the driver reap the process before the runtime goes away.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(())
}
