//! Process handling against a fake `wizard`, no network and no real binary.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use agent_core::*;
use backend_wizard::WizardBackend;
use tokio::time::timeout;

fn fake(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let p = dir.join("wizard");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'wizard 1.2.3'; exit 0; fi\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const SERVE: &str = r#"
marker="$0.closed"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"version":"1.2.3"}}}' ;;
    *'"session/new"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake","configOptions":[]}}' ;;
  esac
done
echo closed > "$marker"
"#;

// One test: it sets OPENW_WIZARD_BIN, which is process-wide.
#[tokio::test]
async fn ready_shutdown_and_death() {
    let dir = std::env::temp_dir().join(format!("openw-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Handshake, version from `--version`, and stdin closed on Shutdown.
    let bin = fake(&dir, SERVE);
    std::env::set_var("OPENW_WIZARD_BIN", &bin);
    let mut h = WizardBackend::spawn(dir.clone(), None).unwrap();
    let e = timeout(Duration::from_secs(5), h.rx.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Ready { session_id, config } = e else {
        panic!("{e:?}")
    };
    assert_eq!(session_id, "fake");
    assert_eq!(config.backend, "wizard 1.2.3");
    h.tx.send(Request::Shutdown).unwrap();
    assert!(timeout(Duration::from_secs(10), async {
        while h.rx.recv().await.is_some() {}
    })
    .await
    .is_ok());
    assert!(
        dir.join("wizard.closed").exists(),
        "child never saw EOF on stdin"
    );

    // Dropping the handle is a shutdown too.
    std::fs::remove_file(dir.join("wizard.closed")).unwrap();
    let h = WizardBackend::spawn(dir.clone(), None).unwrap();
    drop(h);
    for _ in 0..50 {
        if dir.join("wizard.closed").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(dir.join("wizard.closed").exists());

    // The process dying is a Fatal that carries its stderr.
    let bin = fake(&dir, "echo 'auth: token expired' >&2\nexit 3");
    std::env::set_var("OPENW_WIZARD_BIN", &bin);
    let mut h = WizardBackend::spawn(dir.clone(), None).unwrap();
    let e = timeout(Duration::from_secs(5), h.rx.recv())
        .await
        .unwrap()
        .unwrap();
    let Event::Fatal(msg) = e else {
        panic!("{e:?}")
    };
    assert!(msg.contains("exit") && msg.contains("3"), "{msg}");
    assert!(msg.contains("token expired"), "{msg}");

    // A missing binary is a spawn error, not a hang.
    std::env::set_var("OPENW_WIZARD_BIN", dir.join("nope"));
    let err = WizardBackend::spawn(dir.clone(), None)
        .err()
        .expect("spawn should fail")
        .to_string();
    assert!(err.contains("OPENW_WIZARD_BIN"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
