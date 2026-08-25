//! The loopback redirect a subscription sign-in comes back on.
//!
//! Both providers register one fixed loopback address with their client id —
//! OpenAI's `localhost:1455/auth/callback`, xAI's `127.0.0.1:56121/callback` —
//! and redirect the browser nowhere else. So every caller, terminal or GUI,
//! must own *that* port for the length of the flow; there is no other address
//! to serve the redirect on.
//!
//! Which makes the port a scarce resource, and the wait for it cancellable:
//! a sign-in that holds the port for its full [`CALLBACK_TIMEOUT`] would lock
//! out the most natural retry there is — close the provider tab, click sign in
//! again. So the accept loop races the browser against a [`Cancel`] signal, and
//! a cancelled flow drops the listener at once, leaving the port free for the
//! sign-in that replaced it.
//!
//! Loopback also means *the browser's* loopback, which is only this machine
//! when the browser runs here. Over SSH it is not, so a terminal sign-in can
//! take the redirect off stdin instead — see [`PasteChannel`] — and says how
//! to bridge the gap properly — see [`remote_hint`].
//!
//! The two providers differ only in how they classify a request target (their
//! paths and error shapes differ); everything else — accepting, reading,
//! answering the human's browser with a page — is here, once.

use std::io::{BufRead, Write};
use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// How long the listener waits for the browser before giving the port back.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// How long one connection has to send its request line. A browser sends it
/// immediately; anything else is not the redirect we are waiting for.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The request line fits well inside this; only the GET target is needed.
const MAX_REQUEST: usize = 8192;

/// How long a bind waits out the kernel's teardown of a listener that has just
/// been dropped.
///
/// Closing a listening socket does not hand its port back synchronously: the
/// socket stays in the kernel's bind table until it is destroyed. So the sign-in
/// that *replaces* a cancelled one — the GUI's retry, which cancels, waits for
/// the old flow's task to finish, and only then binds the port it just released
/// — can still find that port occupied, by nothing but a socket on its way out.
/// Every syscall in that window raises the same `EADDRINUSE` a genuine conflict
/// does, which is why waiting for the task is necessary but not sufficient.
///
/// Waiting it out is not the same as tolerating a conflict: a port somebody else
/// owns is still owned when the grace runs out, and still an error.
pub(crate) const REBIND_GRACE: Duration = Duration::from_millis(250);

/// Cancels the sign-in it was minted for, freeing the callback port. Dropping
/// it cancels too: a flow nobody holds a handle to is a flow nobody is waiting
/// for.
pub struct Canceller(oneshot::Sender<()>);

/// The receiving half of [`Canceller`], handed to [`serve_redirect`].
pub struct Cancel(Option<oneshot::Receiver<()>>);

/// A cancel handle and the signal it fires.
pub fn cancellation() -> (Canceller, Cancel) {
    let (tx, rx) = oneshot::channel();
    (Canceller(tx), Cancel(Some(rx)))
}

impl Canceller {
    /// Cancel the flow: it drops its listener and gives the port back.
    pub fn cancel(self) {
        let _ = self.0.send(());
    }
}

impl Cancel {
    /// A signal that never fires: the terminal flows own the port for their
    /// whole run, and nothing else in the process is competing for it.
    pub fn never() -> Self {
        Self(None)
    }

    /// Resolves when the flow is cancelled — either explicitly or because the
    /// [`Canceller`] was dropped.
    async fn triggered(&mut self) {
        match &mut self.0 {
            Some(rx) => {
                let _ = rx.await;
            }
            None => std::future::pending().await,
        }
    }
}

/// What one callback request amounted to. The provider modules classify their
/// own targets: the paths and the error shapes are theirs, the plumbing is not.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    /// The redirect, carrying an authorization code and a matching `state`.
    Code(String),
    /// The redirect carried an OAuth error, or a `state` that did not match.
    Failed(String),
    /// Some other request (favicon and friends): answer it and keep waiting.
    Ignored,
}

/// Serve the loopback redirect on `listener` until the browser brings the
/// authorization code back, the flow is cancelled, or [`CALLBACK_TIMEOUT`]
/// passes. The listener is dropped — and the port freed — the moment this
/// returns, whichever of the three it was.
///
/// `classify` maps a request target (`/callback?code=…&state=…`) onto a
/// [`Callback`], validating `state` as it goes.
pub async fn serve_redirect<F>(
    listener: StdTcpListener,
    mut cancel: Cancel,
    classify: F,
) -> Result<String>
where
    F: Fn(&str) -> Callback,
{
    listener
        .set_nonblocking(true)
        .context("configuring the callback listener")?;
    let listener = TcpListener::from_std(listener).context("arming the callback listener")?;
    let timeout = tokio::time::sleep(CALLBACK_TIMEOUT);
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            () = cancel.triggered() => bail!("the sign-in was replaced by a newer one"),
            () = &mut timeout => bail!("timed out waiting for the browser sign-in (5 minutes)"),
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting the OAuth callback connection")?;
                if let Some(outcome) = serve_connection(stream, &classify).await {
                    return outcome;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The paste fallback, for sign-ins with no browser on this machine
// ---------------------------------------------------------------------------

/// Whether a sign-in may take its redirect from stdin as well as from the
/// loopback listener.
///
/// The listener is not enough on a remote session. Both providers redirect to
/// a loopback address, and loopback is resolved by *the machine running the
/// browser* — so over SSH the redirect lands on the laptop's `127.0.0.1` while
/// the listener sits on the server's, and the flow can only time out. The URL
/// the browser ends up on still carries the authorization code in its address
/// bar, though, so a human who can read that bar can carry the code across the
/// gap by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteChannel {
    /// The sign-in owns the terminal (`wizard --login …`): prompt on stderr and
    /// read pasted redirects from stdin.
    Stdin,
    /// Something else owns stdin — the TUI is reading keystrokes, the GUI has
    /// no terminal at all — so there is nothing to prompt on and nothing to
    /// read. The loopback listener is the only channel.
    Disabled,
}

/// What a provider must supply to accept a pasted redirect: the callback path
/// its `classify` expects, and the `state` it minted.
///
/// The state is here for the one paste that carries no query at all — a bare
/// code, copied out of a URL by hand. Rebuilding the target around the expected
/// state is not a weakened check: `state` exists so that a *request arriving on
/// the listener* has to prove it belongs to this flow, and a human typing into
/// this process's own stdin has already proved more than that.
pub struct PasteSpec {
    pub callback_path: &'static str,
    pub state: String,
}

/// [`serve_redirect`], plus a second way in: whatever the human pastes on
/// stdin. First channel to produce a code wins, and the listener is dropped
/// either way.
///
/// `paste` of `None` is exactly [`serve_redirect`].
pub async fn serve_redirect_or_paste<F>(
    listener: StdTcpListener,
    cancel: Cancel,
    classify: F,
    paste: Option<PasteSpec>,
) -> Result<String>
where
    F: Fn(&str) -> Callback,
{
    let Some(spec) = paste else {
        return serve_redirect(listener, cancel, classify).await;
    };

    // `&F` is `Fn` too, so the listener and the paste loop can share one
    // classifier without cloning it or boxing it.
    let served = serve_redirect(listener, cancel, &classify);
    tokio::pin!(served);
    let mut pasted = stdin_lines();
    prompt_for_paste();

    loop {
        tokio::select! {
            outcome = &mut served => return outcome,
            line = pasted.recv() => {
                // stdin closed (piped input, ^D): the listener is all that is
                // left, and it still has its own timeout.
                let Some(line) = line else { return served.await };
                if line.is_empty() {
                    prompt_for_paste();
                    continue;
                }
                match spec.classify(&line, &classify) {
                    Callback::Code(code) => return Ok(code),
                    Callback::Failed(message) => bail!(message),
                    // Not a redirect at all — a stray keystroke, a half-copied
                    // line. Say so and keep both channels open.
                    Callback::Ignored => {
                        eprintln!(
                            "that does not look like the redirect URL — it should contain \
                             `?code=…&state=…`"
                        );
                        prompt_for_paste();
                    }
                }
            }
        }
    }
}

impl PasteSpec {
    /// Classify one pasted line by normalizing it into a callback target the
    /// provider's own `classify` can read.
    fn classify<F>(&self, line: &str, classify: &F) -> Callback
    where
        F: Fn(&str) -> Callback,
    {
        match self.target(line) {
            Some(target) => classify(&target),
            None => Callback::Ignored,
        }
    }

    /// Normalize a pasted line into a `/path?query` target.
    ///
    /// The host and port that come back are the browser machine's, not ours,
    /// and the path is whatever that machine was redirected to — none of it
    /// tells us anything we do not already know. Only the query matters, so
    /// the target is rebuilt around [`Self::callback_path`] and the accepted
    /// shapes are as loose as a real paste is:
    ///
    /// - a full URL, `http://127.0.0.1:56121/callback?code=…&state=…`
    /// - a scheme-less address bar copy, `127.0.0.1:56121/callback?code=…`
    /// - the query alone, with or without its `?`
    /// - a bare code, picked out of the URL by hand
    fn target(&self, line: &str) -> Option<String> {
        let line = line.trim().trim_matches(['"', '\'']);
        if line.is_empty() {
            return None;
        }
        // A fragment is never part of an OAuth redirect's query, and a paste
        // can pick one up from the address bar.
        let line = line.split('#').next().unwrap_or(line);

        let query = match line.split_once('?') {
            Some((_, query)) => query,
            // `code=…&state=…` on its own: the query, minus its punctuation.
            None if line.contains("code=") => line,
            // A bare code. Loose, but it only has to exclude prose and paths:
            // anything that gets through still faces the token exchange.
            None if is_bare_code(line) => return Some(self.target_for_code(line)),
            None => return None,
        };
        if query.is_empty() {
            return None;
        }
        Some(format!("{}?{query}", self.callback_path))
    }

    /// A target for a code the human copied on its own, carrying the state
    /// this flow minted.
    fn target_for_code(&self, code: &str) -> String {
        let mut url = reqwest::Url::parse("http://127.0.0.1/").expect("a literal loopback URL");
        url.set_path(self.callback_path);
        url.query_pairs_mut()
            .append_pair("code", code)
            .append_pair("state", &self.state);
        format!(
            "{}?{}",
            url.path(),
            url.query().expect("two pairs were just appended")
        )
    }
}

/// Whether a line could be an authorization code lifted out of a redirect URL
/// on its own: one token, long enough to be a credential, drawn from the
/// unreserved URL characters codes are issued in.
fn is_bare_code(line: &str) -> bool {
    line.len() >= 8
        && line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
}

/// Ask for the paste. Goes to stderr so that a caller redirecting stdout still
/// sees it, and so it never lands in whatever stdout is being parsed as.
fn prompt_for_paste() {
    eprint!("or paste the redirect URL here: ");
    let _ = std::io::stderr().flush();
}

/// Trimmed lines from stdin, on a detached OS thread.
///
/// Not a `spawn_blocking` task: the browser usually wins this race, leaving the
/// read blocked forever on a human who will never type. Tokio's runtime waits
/// for its blocking pool at shutdown, so that parked read would hang the
/// process *after* a successful sign-in. A plain thread has no such claim on
/// the process — `main` returning ends it, parked reader and all.
fn stdin_lines() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(1);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                // EOF, or a stdin that cannot be read at all.
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx.blocking_send(line.trim().to_string()).is_err() {
                        // The sign-in finished without us.
                        return;
                    }
                }
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// Remote-session guidance
// ---------------------------------------------------------------------------

/// Guidance for a session whose browser, if it has one at all, is on another
/// machine — or `None` when the browser is reachable and the flow needs no
/// explaining.
///
/// This is advice, not a gate: nothing downstream branches on it, and a wrong
/// guess costs a paragraph of text rather than a sign-in.
pub fn remote_hint(port: u16) -> Option<String> {
    remote_hint_from(port, |key| std::env::var(key).ok())
}

/// [`remote_hint`] against an injected environment, so the wording and the
/// detection can be tested without mutating this process's own env — which is
/// `unsafe` under edition 2024 and racy besides, with the suite on threads.
fn remote_hint_from<E>(port: u16, env: E) -> Option<String>
where
    E: Fn(&str) -> Option<String>,
{
    let get = |key: &str| env(key).filter(|value| !value.is_empty());

    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|key| get(key).is_some());
    // On a desktop OS the browser is always local. On Linux and the BSDs, a
    // session with neither display server is a console or a service, and
    // `xdg-open` has nothing to open.
    let headless = cfg!(not(any(target_os = "macos", target_os = "windows")))
        && get("DISPLAY").is_none()
        && get("WAYLAND_DISPLAY").is_none();
    if !ssh && !headless {
        return None;
    }

    // `SSH_CONNECTION` is `client-ip client-port server-ip server-port`, so
    // its third field is this machine as the client can already reach it —
    // better than a hostname that may not resolve from over there.
    let destination = get("SSH_CONNECTION")
        .and_then(|conn| conn.split_whitespace().nth(2).map(str::to_string))
        .map(|host| match get("USER").or_else(|| get("LOGNAME")) {
            Some(user) => format!("{user}@{host}"),
            None => host,
        })
        .unwrap_or_else(|| "<you>@<this-machine>".to_string());

    Some(format!(
        "this session has no browser of its own, and the sign-in redirects to \
         127.0.0.1:{port} — which, opened from another machine, is that machine's \
         loopback and not this one's. Either:\n\
         \x20 1. forward the port, then open the URL above in your local browser:\n\
         \x20      ssh -N -L {port}:127.0.0.1:{port} {destination}\n\
         \x20 2. or open the URL above anyway, let the final redirect fail to \
         connect, and paste that failed page's address back here."
    ))
}

/// Serve one connection. `Some(result)` ends the wait; `None` keeps waiting.
async fn serve_connection<F>(mut stream: TcpStream, classify: &F) -> Option<Result<String>>
where
    F: Fn(&str) -> Callback,
{
    let target = match tokio::time::timeout(READ_TIMEOUT, read_target(&mut stream)).await {
        Ok(Some(target)) => target,
        // A connection that sends nothing (or nothing in time) is not the
        // redirect; it must not end the wait.
        _ => return None,
    };

    match classify(&target) {
        Callback::Ignored => {
            respond(&mut stream, "404 Not Found", "Not found.").await;
            None
        }
        Callback::Failed(message) => {
            respond(&mut stream, "200 OK", &format!("Sign-in failed: {message}")).await;
            Some(Err(anyhow::anyhow!(message)))
        }
        Callback::Code(code) => {
            respond(
                &mut stream,
                "200 OK",
                "Signed in to Wizard. You can close this tab.",
            )
            .await;
            Some(Ok(code))
        }
    }
}

/// The request line's target, e.g. `/callback?code=…&state=…`.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = [0u8; MAX_REQUEST];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(&mut buf[len..]).await {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&buf[..len]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))?;
    Some(target.to_string())
}

/// Answer the browser with a one-line page. The text can carry a provider's
/// error string — attacker-adjacent input, rendered into HTML — so it is
/// escaped.
async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Wizard</title>\
         <body style=\"background:#0c0c0e;color:#ececee;font:14px system-ui;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <p>{}</p>",
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Escape text rendered into the callback page's HTML.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A fixed callback port is a single machine-wide resource, and unit tests run
/// in parallel threads of one process. Tests that bind one take this first, so
/// they queue rather than fight over it.
#[cfg(test)]
pub(crate) fn serial_callback_port() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// A free port no other test can take from us. The sign-in flows bind one of
/// these under `cfg(test)` in place of their registered port, and the callback
/// tests bind them directly.
///
/// Not `bind(0)`: that draws from the OS ephemeral range (32768–60999 here),
/// which is exactly the range every other test's `bind(0)` draws from. A test
/// that frees its port and then rebinds it — which is the whole point of
/// `cancelling_frees_the_port_immediately`, and of the GUI's replace-a-sign-in
/// regression — can find the kernel handed it to a concurrent test in between,
/// and fail on a race that says nothing about the code. So we walk a private
/// range below the ephemeral one, where no `bind(0)` can land, and hand each
/// port out once: a successful bind proves it is free, and dropping the
/// listener (nothing was ever accepted on it) gives it straight back to the
/// caller.
///
/// The counter starts at a per-process offset, because a probe cannot *hold*
/// the port it is vouching for — the code under test has to bind it a moment
/// later, so the probe must let go. Within one process the counter makes that
/// safe: a number is handed out once. Across two processes it is not, and two
/// suites do run at once on this machine (a second checkout, CI beside a local
/// run). From a fixed start they would deal out the same numbers, each probe
/// would find the port free before the other bound it, and one of them would
/// fail on a collision that says nothing about the code. A slice keyed by pid
/// keeps them out of each other's numbers.
#[cfg(test)]
pub(crate) fn private_test_port() -> u16 {
    /// The private range, carved into per-process slices. 16384 + 64 * 64 =
    /// 20480: clear of the ephemeral floor (32768) above, and clear of 21000
    /// below it, where a checkout that predates this slicing starts its own
    /// counter. Sixty-four ports is far more than any one process asks for.
    const BASE: u16 = 16_384;
    const SLICE: u16 = 64;
    const SLICES: u16 = 64;

    static NEXT: std::sync::OnceLock<std::sync::atomic::AtomicU16> = std::sync::OnceLock::new();
    let next = NEXT.get_or_init(|| {
        let slice = (std::process::id() % u32::from(SLICES)) as u16;
        std::sync::atomic::AtomicU16::new(BASE + slice * SLICE)
    });

    for _ in 0..SLICE {
        let port = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(
            port < BASE + SLICES * SLICE,
            "ran out of private test ports"
        );
        if StdTcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in this process's slice of the private test range");
}

/// Take the shared test callback port, waiting out a teardown still in progress.
///
/// The tests that stand in for a sign-in queue on [`serial_callback_port`], so
/// only one holds the port at a time — but the lock is released when a test
/// ends, and the kernel does not destroy that test's listener on the same
/// instant. The next test in the queue can therefore find the port still taken
/// by a socket that is already closed. Production waits that out (xAI's
/// `REBIND_GRACE`); a test that demands the port on its first syscall is holding
/// itself to a rule the code under test does not have to meet, and fails on a
/// race that means nothing.
#[cfg(test)]
pub(crate) fn take_test_port(port: u16) -> StdTcpListener {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        match StdTcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return listener,
            Err(err) if std::time::Instant::now() < deadline => {
                assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("the test port never came back: {err}"),
        }
    }
}

// ---------------------------------------------------------------------------
// PKCE and JWT: the parts of an OAuth flow that belong to neither provider
// ---------------------------------------------------------------------------
//
// Both sign-ins are authorization-code-with-PKCE against an OIDC discovery
// document, so the verifier/challenge pair and the `exp` claim of the access
// token are the same arithmetic in both. They lived in `xai_oauth.rs` and
// `chatgpt_oauth.rs` imported them from there, which is a dependency edge
// between two backends that have nothing to do with each other — and once each
// is a plugin, an edge that makes deleting one break the other. RFC 7636 is
// not xAI's, so it sits here with the rest of the flow's shared machinery.

/// A PKCE verifier/challenge pair (RFC 7636, S256).
#[derive(Debug)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE pair: the verifier is base64url(64 random bytes) without
/// padding, capped at the RFC maximum of 128 chars; the challenge is
/// base64url(sha256(verifier)) without padding.
pub fn generate_pkce() -> Result<Pkce> {
    use base64::Engine;

    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow::anyhow!("gathering PKCE randomness: {err}"))?;
    let mut verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    verifier.truncate(128);
    let challenge = pkce_challenge(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// S256 challenge for a verifier: base64url(sha256(verifier)), no padding.
pub fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The `exp` claim of a JWT, or `None` when the token is not a parseable JWT.
///
/// Deliberately unverified: this is a client reading its own token to decide
/// whether to refresh it early, not a server deciding whether to trust one. A
/// forged `exp` costs the forger a refresh round trip and nothing else.
pub fn jwt_exp(token: &str) -> Option<i64> {
    use base64::Engine;

    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_i64()
}

#[cfg(test)]
mod tests {

    /// Unsigned JWT carrying the given JSON payload.
    fn jwt_with_payload(payload: &str) -> String {
        use base64::Engine;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = enc.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = enc.encode(payload.as_bytes());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_well_formed() {
        let pkce = generate_pkce().expect("pkce");
        // 64 random bytes encode to 86 base64url chars, inside RFC bounds.
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "len {}",
            pkce.verifier.len()
        );
        assert!(
            pkce.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier {} has invalid chars",
            pkce.verifier
        );
        assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
        // A second pair must differ (randomness).
        assert_ne!(generate_pkce().expect("pkce").verifier, pkce.verifier);
    }

    #[test]
    fn jwt_exp_reads_the_exp_claim() {
        let token = jwt_with_payload(r#"{"sub":"u1","exp":1234567890}"#);
        assert_eq!(jwt_exp(&token), Some(1_234_567_890));
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp(&jwt_with_payload(r#"{"sub":"u1"}"#)), None);
    }
    use super::*;

    /// Bind a port the rest of the suite cannot take from us.
    fn bind_private() -> (StdTcpListener, u16) {
        let port = private_test_port();
        let listener = StdTcpListener::bind(("127.0.0.1", port)).expect("the private port is ours");
        (listener, port)
    }

    /// Play the browser: send the redirect and read the page back. Async on
    /// purpose — a blocking client on the test's runtime thread would starve the
    /// server it is talking to, and the two would sit there until the timeout.
    async fn request(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut page = String::new();
        stream.read_to_string(&mut page).await.expect("read");
        page
    }

    /// The redirect: everything before the code arrives is plumbing.
    #[tokio::test]
    async fn the_browsers_redirect_yields_the_code() {
        let (listener, port) = bind_private();
        let served = tokio::spawn(serve_redirect(
            listener,
            Cancel::never(),
            |target| match target.split_once("code=") {
                Some((_, code)) => Callback::Code(code.to_string()),
                None => Callback::Ignored,
            },
        ));

        let page = request(port, "/callback?code=abc").await;

        assert_eq!(served.await.expect("join").expect("code"), "abc");
        assert!(page.contains("Signed in to Wizard"), "{page}");
    }

    /// The regression the GUI needs: a cancelled flow gives the port back at
    /// once, rather than sitting on it for the full timeout.
    #[tokio::test]
    async fn cancelling_frees_the_port_immediately() {
        let (listener, port) = bind_private();
        let (canceller, cancel) = cancellation();
        let served = tokio::spawn(serve_redirect(listener, cancel, |_| Callback::Ignored));
        // The listener is live: the port cannot be taken from under it.
        assert!(StdTcpListener::bind(("127.0.0.1", port)).is_err());

        canceller.cancel();
        let err = served.await.expect("join").expect_err("cancelled");
        assert!(err.to_string().contains("replaced"), "{err}");

        // And now the port comes back. The task's future — and the listener it
        // owns — is dropped before its `JoinHandle` resolves, but the kernel
        // does not always have the socket torn down by the time the very next
        // syscall asks for the port back, so the bind is retried briefly rather
        // than demanded on the first try. The claim under test is unharmed: the
        // point is that the port returns in milliseconds instead of being held
        // for CALLBACK_TIMEOUT, and a whole second is still three hundred times
        // short of that.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match StdTcpListener::bind(("127.0.0.1", port)) {
                Ok(_) => break,
                Err(err) if std::time::Instant::now() < deadline => {
                    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("the port was never given back: {err}"),
            }
        }
    }

    /// Dropping the handle is as good as cancelling: a flow nobody holds is a
    /// flow nobody is waiting for.
    #[tokio::test]
    async fn dropping_the_canceller_cancels() {
        let (listener, _) = bind_private();
        let (canceller, cancel) = cancellation();
        let served = tokio::spawn(serve_redirect(listener, cancel, |_| Callback::Ignored));
        drop(canceller);
        assert!(served.await.expect("join").is_err());
    }

    /// A provider's error text lands in a page a human is looking at.
    #[test]
    fn provider_text_is_escaped_into_the_page() {
        assert_eq!(html_escape("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
        assert!(html_escape("<script>alert(1)</script>").contains("&lt;script&gt;"));
    }

    fn spec() -> PasteSpec {
        PasteSpec {
            callback_path: "/callback",
            state: "s7".to_string(),
        }
    }

    /// What a human actually pastes varies by browser and by how much of the
    /// address bar they caught. Every shape has to land on the same target,
    /// because a paste that is silently ignored looks exactly like a hang.
    #[test]
    fn every_shape_of_pasted_redirect_reaches_the_same_target() {
        let want = "/callback?code=abc&state=s7";
        for pasted in [
            "http://127.0.0.1:56121/callback?code=abc&state=s7",
            // Chrome hides the scheme when you copy from the address bar.
            "127.0.0.1:56121/callback?code=abc&state=s7",
            // A tunnel makes it `localhost`, and https if the human retyped it.
            "https://localhost:56121/callback?code=abc&state=s7",
            // Trailing fragment, surrounding quotes, stray whitespace.
            "  \"http://127.0.0.1:56121/callback?code=abc&state=s7#\"  ",
            // The query on its own, with and without its punctuation.
            "?code=abc&state=s7",
            "code=abc&state=s7",
        ] {
            assert_eq!(spec().target(pasted).as_deref(), Some(want), "{pasted}");
        }
    }

    /// The path a paste carries is the *browser machine's* — a tunnel rewrites
    /// nothing, but a human retyping might. Only the query is load-bearing, so
    /// the target is rebuilt around the path the provider expects.
    #[test]
    fn a_pasted_path_is_replaced_by_the_providers_own() {
        let spec = PasteSpec {
            callback_path: "/auth/callback",
            state: "s7".to_string(),
        };
        assert_eq!(
            spec.target("http://localhost:1455/callback?code=abc&state=s7")
                .as_deref(),
            Some("/auth/callback?code=abc&state=s7"),
        );
    }

    /// A code copied out of the URL by hand carries no state, so the flow's own
    /// is supplied. The reconstruction has to survive the provider's parser.
    #[test]
    fn a_bare_code_is_rebuilt_around_the_flows_state() {
        let target = spec().target("ac_01HQZ-x.y_z~").expect("a bare code");
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).expect("parses");
        assert_eq!(url.path(), "/callback");
        let pairs: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("code".to_string(), "ac_01HQZ-x.y_z~".to_string()),
                ("state".to_string(), "s7".to_string()),
            ],
        );
    }

    /// Anything that is not a redirect must leave both channels open rather
    /// than fail the sign-in: the browser may still be on its way.
    #[test]
    fn prose_and_fragments_are_not_mistaken_for_a_redirect() {
        for pasted in [
            "",
            "   ",
            "y",
            "no idea what goes here",
            "http://127.0.0.1:56121/callback",
            "/callback?",
        ] {
            assert_eq!(spec().target(pasted), None, "{pasted:?}");
        }
    }

    /// The paste goes through the provider's own classifier, so a forged or
    /// stale `state` is refused exactly as it would be on the listener.
    #[test]
    fn a_pasted_redirect_is_classified_by_the_provider() {
        let classify = |target: &str| {
            let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).expect("parses");
            let mut code = None;
            let mut state = None;
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "code" => code = Some(value.into_owned()),
                    "state" => state = Some(value.into_owned()),
                    _ => {}
                }
            }
            match (code, state.as_deref() == Some("s7")) {
                (Some(code), true) => Callback::Code(code),
                _ => Callback::Failed("state mismatch".to_string()),
            }
        };

        let spec = spec();
        assert_eq!(
            spec.classify(
                "http://127.0.0.1:56121/callback?code=abc&state=s7",
                &classify
            ),
            Callback::Code("abc".to_string()),
        );
        assert!(matches!(
            spec.classify(
                "http://127.0.0.1:56121/callback?code=abc&state=nope",
                &classify
            ),
            Callback::Failed(_),
        ));
        // Unparseable input never reaches the classifier at all.
        assert_eq!(spec.classify("what?", &classify), Callback::Ignored);
    }

    /// With no paste channel the wait is [`serve_redirect`], unchanged.
    #[tokio::test]
    async fn without_a_paste_channel_the_listener_is_the_only_way_in() {
        let (listener, port) = bind_private();
        let served = tokio::spawn(serve_redirect_or_paste(
            listener,
            Cancel::never(),
            |target| match target.split_once("code=") {
                Some((_, code)) => Callback::Code(code.to_string()),
                None => Callback::Ignored,
            },
            None,
        ));

        request(port, "/callback?code=abc").await;

        assert_eq!(served.await.expect("join").expect("code"), "abc");
    }

    /// The whole point of the hint: a command the user can paste back, naming
    /// this machine as their own client already reaches it.
    #[test]
    fn an_ssh_session_is_told_how_to_forward_the_port() {
        let hint = remote_hint_from(56121, |key| {
            match key {
                "SSH_CONNECTION" => Some("198.51.100.23 38008 203.0.113.10 22"),
                "USER" => Some("ada"),
                _ => None,
            }
            .map(str::to_string)
        })
        .expect("an SSH session has no browser of its own");
        assert!(
            hint.contains("ssh -N -L 56121:127.0.0.1:56121 ada@203.0.113.10"),
            "{hint}"
        );
    }

    /// X11 forwarding gives the session a `DISPLAY`, and a browser — on the
    /// *other* machine, whose loopback is not ours. Still a remote session.
    #[test]
    fn a_forwarded_display_does_not_make_the_session_local() {
        let hint = remote_hint_from(1455, |key| {
            match key {
                "SSH_TTY" => Some("/dev/pts/3"),
                "DISPLAY" => Some("localhost:10.0"),
                _ => None,
            }
            .map(str::to_string)
        });
        assert!(hint.is_some());
    }

    /// A session with a browser of its own needs none of this said to it.
    #[test]
    fn a_local_desktop_session_gets_no_hint() {
        let hint = remote_hint_from(56121, |key| match key {
            "DISPLAY" => Some(":0".to_string()),
            // An empty SSH var is as good as unset — a login shell can export
            // one without a connection behind it.
            "SSH_CLIENT" => Some(String::new()),
            _ => None,
        });
        assert_eq!(hint, None);
    }

    /// Without a `SSH_CONNECTION` to read this machine's address out of, the
    /// command still has to be recognisable — a placeholder, not a broken line.
    #[test]
    fn a_headless_session_still_gets_a_usable_command() {
        let hint = remote_hint_from(56121, |_| None);
        // A desktop OS always has a local browser; there is nothing to explain.
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert_eq!(hint, None);
            return;
        }
        let hint = hint.expect("no display server and no SSH: a console or a service");
        assert!(
            hint.contains("ssh -N -L 56121:127.0.0.1:56121 <you>@<this-machine>"),
            "{hint}"
        );
    }
}
