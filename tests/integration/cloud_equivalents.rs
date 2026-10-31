//! The local stand-ins for Claude Code's cloud features, run through the built
//! binary with `HOME` and `WIZARD_HOME` in a throwaway directory: artifacts,
//! triggers and their webhook, teleport, and the doctor's cloud-route row.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wizard-cloud-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).expect("create temp dir");
        Self(dir)
    }

    fn project(&self) -> PathBuf {
        self.0.join("project")
    }

    fn wizard_dir(&self) -> PathBuf {
        self.0.join(".wizard")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wizard"));
        command
            .env("HOME", &self.0)
            .env("WIZARD_HOME", self.wizard_dir())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("WIZARD_TRUST_PROJECT")
            .env_remove("CLAUDE_CODE_USE_BEDROCK")
            .env_remove("CLAUDE_CODE_USE_VERTEX")
            .env_remove("CLAUDE_CODE_USE_FOUNDRY")
            .current_dir(self.project());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("binary runs")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http(port: u16, request: &str) -> String {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream.write_all(request.as_bytes()).unwrap();
                let mut out = String::new();
                stream.read_to_string(&mut out).unwrap();
                return out;
            }
            Err(err) if Instant::now() < end => {
                let _ = err;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("nothing listening on {port}: {err}"),
        }
    }
}

#[test]
fn artifacts_published_through_the_library_are_listed_and_served_by_the_binary() {
    let home = Home::new("artifact");
    let empty = home.run(&["artifact", "list"]);
    assert!(text(&empty).contains("No local artifacts yet"), "{}", text(&empty));

    let store = wizard::artifact::Artifacts::at(home.wizard_dir().join("artifacts"));
    let page = home.project().join("p.html");
    std::fs::write(&page, "<!doctype html><title>Served Page</title><p>hello</p>").unwrap();
    let reply = store
        .call(
            &serde_json::json!({"file_path": page.display().to_string()}),
            &home.project(),
        )
        .unwrap();
    let slug = reply
        .split_whitespace()
        .find(|w| w.starts_with("file://"))
        .unwrap()
        .trim_end_matches("/index.html")
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();

    let listed = home.run(&["artifact", "list"]);
    assert!(text(&listed).contains("Served Page"), "{}", text(&listed));
    let path = home.run(&["artifact", "path", &slug]);
    assert!(text(&path).trim().starts_with("file://"), "{}", text(&path));

    let port = free_port();
    let _server = Server(
        home.command()
            .args(["artifact", "serve", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let ok = http(port, &format!("GET /{slug}/ HTTP/1.1\r\nHost: x\r\n\r\n"));
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(ok.contains("<p>hello</p>"), "{ok}");
    let hidden = http(
        port,
        &format!("GET /{slug}/.artifact.json HTTP/1.1\r\nHost: x\r\n\r\n"),
    );
    assert!(hidden.starts_with("HTTP/1.1 403"), "{hidden}");
}

#[test]
fn a_trigger_is_scheduled_listed_fired_over_http_and_removed() {
    let home = Home::new("trigger");
    let add = home.run(&[
        "trigger", "add", "morning", "--prompt", "check CI", "--cron", "7 9 * * 1-5", "--webhook",
    ]);
    assert!(add.status.success(), "{}", text(&add));
    let added = text(&add);
    assert!(added.contains("Scheduled: next run"), "{added}");
    let id = added
        .split_whitespace()
        .nth(1)
        .unwrap()
        .trim()
        .to_string();
    assert!(id.starts_with("trg_"), "{added}");

    let schedule = std::fs::read_to_string(home.wizard_dir().join("schedule.toml")).unwrap();
    assert!(schedule.contains(&format!("trigger-{id}")), "{schedule}");
    assert!(schedule.contains("7 9 * * 1-5"), "{schedule}");

    let list = home.run(&["trigger", "list"]);
    assert!(text(&list).contains("morning") && text(&list).contains("[webhook]"), "{}", text(&list));

    let token = String::from_utf8_lossy(&home.run(&["trigger", "token", &id]).stdout)
        .trim()
        .to_string();
    assert!(token.starts_with("wzt_"), "{token}");

    // A run is started by this program, not a model session.
    let recorder = home.0.join("fake-wizard.sh");
    std::fs::write(&recorder, "#!/bin/sh\necho \"fired with: $*\"\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let port = free_port();
    let _server = Server(
        home.command()
            .env("WIZARD_TRIGGER_PROGRAM", &recorder)
            .args(["trigger", "serve", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let denied = http(
        port,
        &format!("POST /v1/triggers/{id}/run HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n"),
    );
    assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
    let body = "deploy finished";
    let accepted = http(
        port,
        &format!(
            "POST /v1/triggers/{id}/run HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(accepted.starts_with("HTTP/1.1 202"), "{accepted}");

    let runs_dir = home.wizard_dir().join("triggers/runs").join(&id);
    let end = Instant::now() + Duration::from_secs(10);
    let log = loop {
        let found = std::fs::read_dir(&runs_dir).ok().and_then(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|e| e == "exit"))
        });
        if let Some(exit) = found {
            break std::fs::read_to_string(exit.with_extension("log")).unwrap();
        }
        assert!(Instant::now() < end, "the run never finished");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(log.contains("fired with:"), "{log}");
    assert!(log.contains("check CI") && log.contains("deploy finished"), "{log}");

    let runs = home.run(&["trigger", "runs", &id]);
    assert!(text(&runs).contains("completed"), "{}", text(&runs));

    let remove = home.run(&["trigger", "remove", &id]);
    assert!(remove.status.success(), "{}", text(&remove));
    let schedule = std::fs::read_to_string(home.wizard_dir().join("schedule.toml")).unwrap_or_default();
    assert!(!schedule.contains(&id), "{schedule}");
}

#[test]
fn a_session_teleports_between_two_machines() {
    let from = Home::new("tele-from");
    let to = Home::new("tele-to");
    let sessions = from.wizard_dir().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("2026-10-31T10-00-00.jsonl"),
        format!(
            "{{\"timestamp\":\"2026-10-31T10:00:00Z\",\"cwd\":\"{}\",\"version\":1}}\n{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"remember the plan\"}}]}}\n",
            from.project().display()
        ),
    )
    .unwrap();
    let envelope = from.0.join("s.gz");
    let pack = from.run(&["teleport", "pack", "--out", &envelope.display().to_string()]);
    assert!(pack.status.success(), "{}", text(&pack));

    let pull = to.run(&["teleport", "pull", &envelope.display().to_string()]);
    assert!(pull.status.success(), "{}", text(&pull));
    assert!(text(&pull).contains("wizard --resume 2026-10-31T10-00-00"), "{}", text(&pull));
    let landed =
        std::fs::read_to_string(to.wizard_dir().join("sessions/2026-10-31T10-00-00.jsonl")).unwrap();
    assert!(landed.contains("remember the plan"), "{landed}");
    assert!(
        landed.lines().next().unwrap().contains(&to.project().display().to_string()),
        "the session now points at the directory it was pulled into: {landed}"
    );

    let bad = to.run(&["teleport", "pull", &from.0.join("missing.gz").display().to_string()]);
    assert!(!bad.status.success());
}

#[test]
fn doctor_names_a_half_configured_cloud_route() {
    let home = Home::new("doctor");
    let out = home
        .command()
        .env("CLAUDE_CODE_USE_BEDROCK", "1")
        .env_remove("AWS_REGION")
        .env_remove("AWS_DEFAULT_REGION")
        .env_remove("AWS_PROFILE")
        .args(["doctor"])
        .output()
        .unwrap();
    let report = text(&out);
    assert!(report.contains("cloud route"), "{report}");
    assert!(report.contains("Bedrock is on but needs AWS_REGION"), "{report}");
}
