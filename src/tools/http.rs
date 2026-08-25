//! The HTTP plumbing every outbound fetch in Wizard goes through: one client,
//! one SSRF guard, one hand-walked redirect chain, one body cap.
//!
//! This is core, and the `web_fetch` / `web_search` / `x_search` tools that
//! used to live in the same file are not — they are `src/plugins/web.rs` now,
//! behind `--features tool-web`. The line between them is *who the caller is*,
//! and it is the same line `src/llm/wire.rs` draws against
//! `src/plugins/openai/`: protocol machinery that several unrelated callers
//! share stays in core, and the vendor-facing thing built on top of it is the
//! plugin.
//!
//! Three callers share this file and only one of them is the web tool:
//!
//! - **`src/plugins/host.rs`** — a Lua plugin's `wizard.http` is this client,
//!   this guard and this cap reached from another caller. A build with no web
//!   tool still grants `Capability::Network`, and the guarantee that grant
//!   makes is written here.
//! - **`src/tools/image.rs`** — `generate_image` downloads a provider-named URL
//!   to the user's disk and needs the same walk with a stricter hop rule
//!   ([`HopScheme::HttpsOnly`]).
//! - **`src/plugins/web.rs`** — the tools.
//!
//! Putting any of it in the plugin would mean the other two lost their guard
//! the day somebody built without `tool-web`, which is the failure mode the
//! plugin boundary exists to make impossible rather than merely unlikely. And
//! a second copy is worse than a shared one for a specific reason this file
//! keeps re-learning: reqwest's redirect policy is a *synchronous* callback, so
//! it cannot resolve DNS, so any client that leaves the default follow-10
//! policy in place has bypassed the entire guard. There is one place that gets
//! that right and everybody starts from it.
//!
//! Settings come from `[web]` in `config.toml`
//! ([`WebConfig`](crate::config::WebConfig)) — `allow_local` and
//! `fetch_max_bytes` are read here, not in the plugin, because they are
//! promises about what this process will do on the network rather than about
//! what one tool does.

use std::net::IpAddr;
use std::time::Duration;

use futures_util::StreamExt;

/// Whole-request timeout for fetches and searches.
///
/// Applied per `send()` by [`no_redirect_client_builder`], and the chain
/// walkers additionally bound the *whole* walk — see [`REDIRECT_BUDGET`],
/// which is what stops ten hops of 30s from spending five minutes under a
/// nominal 30-second budget.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Desktop browser user agent (some sites block obvious bots outright).
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0.0.0 Safari/537.36";

// ---------------------------------------------------------------------------
// SSRF guard
// ---------------------------------------------------------------------------

/// Whether an address is one the web tools must not reach: anything that is
/// not a routable public internet host.
///
/// `Ipv4Addr::is_private` alone is not that set. It covers exactly RFC1918 —
/// 10/8, 172.16/12, 192.168/16 — and the interesting addresses on a real
/// deployment live outside it. `100.64.0.0/10` (carrier-grade NAT) is the
/// worst of them: `100.100.100.200` is Alibaba Cloud's instance metadata
/// endpoint, and Kubernetes and EKS routinely put pod and service CIDRs in
/// that range, so "private" in the RFC1918 sense let a fetch walk straight
/// into the cluster.
///
/// Blocked, then:
/// - IPv4: `0.0.0.0/8` (this network), `10.0.0.0/8`, `100.64.0.0/10` (CGNAT),
///   `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24` (IETF
///   protocol assignments), `192.168.0.0/16`, `198.18.0.0/15` (benchmarking),
///   `224.0.0.0/4` (multicast) and `240.0.0.0/4` (reserved, which is where
///   the `255.255.255.255` broadcast address lives).
/// - IPv6: `::`, `::1`, `fc00::/7` (unique local), `fe80::/10` (link local),
///   `ff00::/8` (multicast), the whole NAT64 allocation `64:ff9b::/32` (which
///   holds the well-known prefix `64:ff9b::/96` and the local-use
///   `64:ff9b:1::/48`), and any address carrying an IPv4 one — both the mapped
///   form `::ffff:127.0.0.1` and the deprecated compatible form `::7f00:1` —
///   which is judged by the IPv4 rules above.
fn ip_is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || a == 0 // 0.0.0.0/8, "this network" (0.0.0.0 included)
                || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64.0.0/10
                || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
                || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18.0.0/15
                || v4.is_multicast() // 224.0.0.0/4
                || a >= 240 // reserved 240.0.0.0/4, incl. 255.255.255.255
        }
        IpAddr::V6(v6) => {
            // Order matters. `to_ipv4` accepts the whole of `::/96`, so `::1`
            // converts to `0.0.0.1` and `::` to `0.0.0.0`; settling both as
            // IPv6 first keeps loopback from being judged as some other host.
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // `to_ipv4`, not `to_ipv4_mapped`: the mapped form is only half of
            // it. `::7f00:1` is IPv4-compatible rather than mapped, so
            // `to_ipv4_mapped` returned `None` for it and loopback went
            // through under the IPv6 rules, which say nothing about `::/96`.
            if let Some(v4) = v6.to_ipv4() {
                return ip_is_local(IpAddr::V4(v4));
            }
            let segments = v6.segments();
            // The NAT64 allocation, 64:ff9b::/32 — all of it, not only the two
            // prefixes defined inside it (64:ff9b::/96, the well-known one,
            // and 64:ff9b:1::/48, for local use). Both embed an IPv4 address
            // that the translator dials on our behalf, so the v6 literal says
            // nothing about where the packet lands. The rest of the /32 is
            // unassigned, so blocking it costs nothing and needs no revisit if
            // another translation prefix is carved out of it later.
            if segments[0] == 0x0064 && segments[1] == 0xff9b {
                return true;
            }
            v6.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80 // link local fe80::/10
        }
    }
}

/// Whether a hostname is a local name: `localhost` or `*.local` (mDNS).
fn host_is_local_name(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || (host.len() >= 6 && host[host.len() - 6..].eq_ignore_ascii_case(".local"))
}

/// Synchronous URL checks: scheme, literal IPs, and local hostnames. Used
/// before the request and inside the redirect policy (which cannot resolve
/// DNS asynchronously).
fn check_url_sync(url: &reqwest::Url, allow_local: bool) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported URL scheme '{other}' (only http and https are allowed)"
            ));
        }
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".to_string());
    };
    if allow_local {
        return Ok(());
    }
    let blocked = format!(
        "fetching local/private address '{host}' is blocked \
         (set [web] allow_local = true in config.toml to permit)"
    );
    // Literal IPs (IPv6 literals come bracketed in URLs).
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if ip_is_local(ip) {
            return Err(blocked);
        }
        return Ok(());
    }
    if host_is_local_name(host) {
        return Err(blocked);
    }
    Ok(())
}

/// Full SSRF check: the synchronous checks plus DNS resolution of domain
/// hosts, rejecting any URL whose host resolves to a local/private address.
///
/// Shared with [`crate::tools::image`], which downloads model-supplied URLs
/// and needs the same guard; there is one implementation of "is this address
/// reachable-but-forbidden" so the two cannot drift.
pub(crate) async fn check_url(url: &reqwest::Url, allow_local: bool) -> Result<(), String> {
    check_url_sync(url, allow_local)?;
    if allow_local {
        return Ok(());
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".to_string());
    };
    // Literal IPs were already checked synchronously.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| format!("could not resolve host '{host}': {err}"))?;
    for addr in addrs {
        if ip_is_local(addr.ip()) {
            return Err(format!(
                "host '{host}' resolves to local/private address {} — blocked \
                 (set [web] allow_local = true in config.toml to permit)",
                addr.ip()
            ));
        }
    }
    Ok(())
}

/// Follow redirects by hand, running the **full** SSRF check on every hop.
///
/// reqwest's redirect policy is a synchronous callback, so it can only reach
/// `check_url_sync` — which returns `Ok` for any host that is not a literal IP
/// or a `localhost`/`*.local` name, without resolving anything. That is the
/// whole guard bypassed in one hop: fetch `http://blog.attacker.com/post`,
/// which resolves publicly and passes; it answers `302 Location:
/// http://meta.attacker.com/…`, whose `A` record is `169.254.169.254`; the sync
/// check sees a hostname, follows, and the cloud metadata service — instance
/// credentials and all — lands in the model's context. Public wildcard-DNS
/// services make the `127.0.0.1` variant a one-liner.
///
/// So the client is told not to redirect at all, and this walks the chain
/// instead, awaiting `check_url` (which resolves) before each hop. There is
/// still a TOCTOU window between our resolution and reqwest's, which no
/// userspace check can close without owning the connector; narrowing the hole
/// from "any hostname" to "a rebinding race" is the part that is achievable
/// here.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// Wall-clock budget for a whole redirect chain, hop count notwithstanding.
///
/// [`FETCH_TIMEOUT`] is a reqwest client setting, and a reqwest client setting
/// is *per request*. A chain is [`MAX_REDIRECTS`] + 1 requests, so a server
/// that answers each hop just inside the per-request timeout spent up to five
/// minutes under what every caller and every doc comment called a 30-second
/// budget — and a hostile server controls both the hop count and the delay, so
/// it is the cheapest possible way to pin an agent turn. The budget below
/// bounds the walk itself; the final response's body is streamed afterwards
/// under [`read_capped`], which is bounded by bytes rather than by time.
pub(crate) const REDIRECT_BUDGET: Duration = FETCH_TIMEOUT;

/// Which schemes a redirect chain is allowed to use.
///
/// [`check_url_sync`] accepts `http` and `https` both, which is the right rule
/// for `web_fetch`: the model named a URL and gets whatever that URL leads to,
/// cleartext included, and the result is text in a transcript.
///
/// It is the wrong rule for the image downloader. There the *provider* names
/// the URL, the bytes are written to the user's disk, and `generate_image`
/// checked the scheme of the first URL only — so `https://cdn/…` answering
/// `302 Location: http://host/x.png` was fetched in cleartext, and the
/// promise in `docs/image.md` that these downloads are "over HTTPS only" was
/// true of exactly one hop.
/// This governs the URLs the *chain* names. The starting URL is the caller's
/// to vet — it came from somewhere this function knows nothing about — and
/// `download_image` refuses a non-https one before it gets here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HopScheme {
    /// `http` and `https` are both acceptable, the `web_fetch` rule.
    Any,
    /// `https` only; a redirect that downgrades ends the chain with an error.
    HttpsOnly,
}

impl HopScheme {
    fn allows(self, url: &reqwest::Url) -> Result<(), String> {
        if self == HopScheme::HttpsOnly && url.scheme() != "https" {
            return Err(format!(
                "refusing to follow a redirect to '{url}': https is required on every hop"
            ));
        }
        Ok(())
    }
}

///
/// `budget` bounds the *whole* walk, which the client's own timeout does not:
/// see [`REDIRECT_BUDGET`]. Callers with a longer budget than the web tools'
/// thirty seconds pass their own.
pub(crate) async fn get_following_redirects(
    client: &reqwest::Client,
    start: reqwest::Url,
    allow_local: bool,
    scheme: HopScheme,
    budget: Duration,
) -> Result<reqwest::Response, String> {
    match tokio::time::timeout(budget, walk_redirects(client, start, allow_local, scheme)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "fetch timed out after {}s following redirects",
            budget.as_secs()
        )),
    }
}

/// [`get_following_redirects`] without the budget, so the budget can wrap it.
async fn walk_redirects(
    client: &reqwest::Client,
    start: reqwest::Url,
    allow_local: bool,
    scheme: HopScheme,
) -> Result<reqwest::Response, String> {
    let mut url = start;
    for _ in 0..=MAX_REDIRECTS {
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|err| format!("fetch failed: {err}"))?;

        if !response.status().is_redirection() {
            return Ok(response);
        }
        let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
        else {
            // A 3xx with nowhere to go is the response.
            return Ok(response);
        };
        // Relative locations are legal and common.
        let next = url
            .join(location)
            .map_err(|err| format!("redirect to invalid url '{location}': {err}"))?;
        scheme.allows(&next)?;
        check_url(&next, allow_local).await?;
        url = next;
    }
    Err("too many redirects".to_string())
}

/// Builder for any client whose redirects must be walked by hand rather than
/// followed by reqwest — see [`get_following_redirects`]. Leaving reqwest's
/// default follow-10 policy in place is the whole SSRF guard bypassed, because
/// the policy callback is synchronous and cannot resolve DNS, so every caller
/// that fetches an untrusted URL starts from here.
pub(crate) fn no_redirect_client_builder(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
}

/// HTTP client for the web tools: desktop UA and a 30s timeout. Redirects are
/// **not** followed here — see [`get_following_redirects`]. It used to take
/// `allow_local`, for a redirect policy that no longer lives here.
///
/// `pub(crate)` because a Lua plugin's `wizard.http` is the web tool's policy
/// reached from another caller, not a second web tool: same UA, same budget,
/// same refusal to follow a redirect without re-resolving it. A private client
/// here would have meant a second one over there, and the second one is always
/// the one that forgets the redirect rule.
pub(crate) fn web_client() -> Result<reqwest::Client, reqwest::Error> {
    no_redirect_client_builder(FETCH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
}

/// Strip what an arbitrary page should not be able to draw with.
///
/// A fetched body is the most hostile text this program handles — anyone can
/// author one — and it went straight into the transcript, the session JSONL and
/// `--output-format stream-json` as a bare `String`. Peer text, which comes from
/// somebody the user explicitly trusted, goes through a newtype that cannot be
/// bypassed; web text had nothing.
///
/// Nothing repaints a terminal today, and that is worth stating precisely
/// rather than leaving as luck: ratatui filters control-containing graphemes
/// when it fills its buffer, so the TUI is covered by a dependency's
/// implementation detail with no test in this repo asserting it. Headless
/// `print!` has no such cover. Either way the escape belongs nowhere
/// downstream, so it is removed at the boundary it enters through.
///
/// Deliberately gentler than the mesh's sanitiser: this keeps newlines and
/// tabs, because a fetched page is *content* the model is meant to read and
/// collapsing its layout would damage the thing the tool exists to deliver.
/// What goes is the set that draws nothing and moves things: C0 and C1 controls
/// other than `\n`/`\t`, and the invisible/bidi set that
/// [`crate::text::is_invisible`] already defines — reused rather than
/// re-listed, so there is one audited answer to "what is invisible" instead of
/// three that can drift.
///
/// That table used to live in `crate::mesh` and this function reached across
/// for it. Once the web tools became a plugin that was an edge from a plugin
/// into a subsystem on its own way out of core, so the table moved down rather
/// than sideways: core holds the one answer and the mesh, `memory` and this all
/// ask core.
pub(crate) fn defang(text: &str) -> String {
    text.chars()
        .filter(|ch| !crate::text::is_invisible(*ch))
        .map(|ch| match ch {
            '\n' | '\t' => ch,
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect()
}

/// Stream a response body, stopping after `cap` bytes. Returns the (possibly
/// capped) body and whether the cap cut anything off.
///
/// `pub(crate)` for the same reason as [`web_client`]: `fetch_max_bytes` is a
/// `[web]` setting, and a caller that reads the body some other way is a
/// caller that does not honour it.
pub(crate) async fn read_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body: Vec<u8> = Vec::new();
    let mut capped = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > cap {
            let room = cap - body.len();
            body.extend_from_slice(&chunk[..room]);
            capped = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, capped))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- SSRF guard -----------------------------------------------------------

    #[test]
    fn local_ips_are_detected() {
        for ip in [
            "127.0.0.1",
            "127.8.8.8",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "169.254.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(ip_is_local(ip.parse().unwrap()), "{ip} is local");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "172.15.0.1",
            "2606:4700::1111",
        ] {
            assert!(!ip_is_local(ip.parse().unwrap()), "{ip} is public");
        }
    }

    #[test]
    fn local_hostnames_are_detected() {
        assert!(host_is_local_name("localhost"));
        assert!(host_is_local_name("LOCALHOST"));
        assert!(host_is_local_name("localhost."));
        assert!(host_is_local_name("printer.local"));
        assert!(host_is_local_name("nas.Local"));
        assert!(!host_is_local_name("example.com"));
        assert!(!host_is_local_name("local"));
        assert!(!host_is_local_name("notlocal.com"));
    }

    /// A redirect to a private address is refused, not followed.
    ///
    /// This is the SSRF hole the manual redirect walk exists to close.
    /// reqwest's redirect policy is a *synchronous* callback, so it could only
    /// reach `check_url_sync`, which returns `Ok` for any host that is not a
    /// literal IP or a `localhost`/`*.local` name — without resolving it. One
    /// hop through a public hostname that resolves privately was enough to
    /// reach the cloud metadata service and put its credentials in the model's
    /// context.
    ///
    /// A real listener on loopback stands in for "resolves privately": the
    /// server is reached only if the redirect is followed, and the assertion is
    /// that it is not.
    /// A fetched page cannot carry terminal escapes or invisible text into the
    /// transcript.
    ///
    /// This is the most hostile text the program handles — anyone can author a
    /// page — and it used to arrive as a bare `String` and go straight into the
    /// transcript, the session JSONL and `stream-json`. Peer text, which comes
    /// from somebody the user explicitly trusted, has always gone through a
    /// newtype that cannot be bypassed.
    ///
    /// Newlines and tabs survive on purpose: a page is content the model is
    /// meant to read, and collapsing its layout would damage the thing the tool
    /// exists to deliver.
    #[test]
    fn a_fetched_page_cannot_carry_escapes_or_invisible_text() {
        // A cursor-moving CSI, a title-setting OSC, and a C1 introducer.
        let hostile = "before\u{1b}[2J\u{1b}]0;pwned\u{7}\u{9b}31mafter";
        let clean = defang(hostile);
        assert!(
            !clean
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t'),
            "a control character survived: {clean:?}"
        );
        assert!(
            clean.contains("before") && clean.contains("after"),
            "{clean:?}"
        );

        // Trojan-source bidi and zero-width joiners, which draw nothing and
        // reorder what is read.
        let sneaky = "let admin = \u{202e}false\u{202c};\u{200b}\u{2060}";
        let clean = defang(sneaky);
        assert!(!clean.contains('\u{202e}'), "{clean:?}");
        assert!(!clean.contains('\u{200b}'), "{clean:?}");
        assert!(clean.contains("let admin = false;"), "{clean:?}");

        // Layout the model needs is untouched.
        let page = "# Title\n\n- one\n- two\n\n\tindented\n";
        assert_eq!(defang(page), page, "newlines and tabs must survive");
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_refused() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let secret = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let secret_addr = secret.local_addr().expect("addr");
        let reached = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&reached);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = secret.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\n\r\nSECRET")
                    .await;
            }
        });

        // The hop that redirects. It is itself on loopback, so the test runs
        // with `allow_local = true` for the first request and relies on the
        // *redirect* being checked with the real policy.
        let hop = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let hop_addr = hop.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = hop.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{secret_addr}/creds\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = sock.write_all(body.as_bytes()).await;
            }
        });

        let client = web_client().expect("client");
        let start = reqwest::Url::parse(&format!("http://{hop_addr}/start")).expect("url");
        let result =
            get_following_redirects(&client, start, false, HopScheme::Any, REDIRECT_BUDGET).await;

        assert!(
            result.is_err(),
            "the redirect to a private address must be refused"
        );
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "the private listener was contacted — the redirect was followed"
        );

        // What the run above does *not* prove, said plainly: its hop target is
        // a literal IP, which the old synchronous check caught too. The actual
        // hole was a *hostname* that resolves privately, and reproducing that
        // needs DNS this suite must not depend on. So the resolving check being
        // the one on the redirect path is asserted from the source instead —
        // swapping it back for `check_url_sync` is the regression, and it would
        // not fail either of the assertions above.
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/http.rs"))
                .expect("this file");
        let walk = source
            .split_once("async fn walk_redirects")
            .expect("the redirect walk exists")
            .1;
        let body = walk
            .split_once("\npub(crate) fn ")
            .map_or(walk, |(body, _)| body);
        assert!(
            body.contains("check_url(&next, allow_local).await"),
            "the redirect walk must await the resolving check, not the sync one"
        );
    }

    #[tokio::test]
    async fn check_url_rejects_private_ranges_and_local_names() {
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.1:8080/path",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://localhost/",
            "http://localhost:3000/",
            "http://printer.local/",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            let err = check_url(&parsed, false).await.expect_err(url);
            assert!(err.contains("blocked"), "{url}: {err}");
        }
    }

    /// The ranges that are not routable public internet, and that
    /// `Ipv4Addr::is_private` says nothing about.
    ///
    /// RFC1918 is three prefixes; "somewhere a fetch must not go" is a great
    /// many more. The one that mattered was `100.64.0.0/10`: `is_private`
    /// returned false for `100.100.100.200`, which is Alibaba Cloud's instance
    /// metadata endpoint, and Kubernetes and EKS habitually allocate pod and
    /// service CIDRs out of the same block — so a `web_fetch`, or a
    /// `generate_image` download, could read cluster-internal services. The
    /// rest are cheap to block and were equally open.
    #[tokio::test]
    async fn check_url_rejects_the_ranges_that_are_not_the_public_internet() {
        for (url, what) in [
            ("http://100.64.0.1/", "CGNAT 100.64.0.0/10, low end"),
            ("http://100.100.100.200/", "Alibaba Cloud instance metadata"),
            ("http://100.127.255.255/", "CGNAT 100.64.0.0/10, high end"),
            (
                "http://192.0.0.1/",
                "IETF protocol assignments 192.0.0.0/24",
            ),
            ("http://198.18.0.1/", "benchmarking 198.18.0.0/15"),
            ("http://198.19.255.255/", "benchmarking 198.18.0.0/15"),
            ("http://224.0.0.1/", "multicast 224.0.0.0/4"),
            ("http://239.255.255.250/", "SSDP multicast"),
            ("http://240.0.0.1/", "reserved 240.0.0.0/4"),
            ("http://255.255.255.255/", "broadcast"),
            ("http://0.0.0.0/", "unspecified"),
            ("http://0.0.0.1/", "this network 0.0.0.0/8"),
            ("http://[::7f00:1]/", "IPv4-compatible IPv6 loopback"),
            ("http://[::ffff:7f00:1]/", "IPv4-mapped IPv6 loopback"),
            ("http://[::ffff:6440:1]/", "IPv4-mapped CGNAT"),
            ("http://[64:ff9b::7f00:1]/", "NAT64 well-known prefix"),
            ("http://[64:ff9b:1::1]/", "NAT64 local-use prefix"),
            (
                "http://[64:ff9b:beef::1]/",
                "the rest of the NAT64 allocation 64:ff9b::/32",
            ),
            ("http://[ff02::1]/", "IPv6 all-nodes multicast"),
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            let err = check_url(&parsed, false).await.expect_err(what);
            assert!(err.contains("blocked"), "{what} ({url}): {err}");
        }
    }

    /// And the neighbours of those ranges are still reachable — a guard that
    /// blocks the public internet is not a guard, it is an outage.
    #[tokio::test]
    async fn check_url_still_allows_the_addresses_next_door() {
        for (url, what) in [
            ("http://100.63.255.255/", "just below CGNAT"),
            ("http://100.128.0.1/", "just above CGNAT"),
            ("http://192.0.1.1/", "just above 192.0.0.0/24"),
            ("http://198.17.255.255/", "just below the benchmark range"),
            ("http://198.20.0.1/", "just above the benchmark range"),
            ("http://223.255.255.255/", "just below multicast"),
            ("http://1.1.1.1/", "a public resolver"),
            ("http://[2606:4700:4700::1111]/", "a public IPv6 resolver"),
            ("http://[64:ff9a::1]/", "just below the NAT64 prefix"),
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            check_url(&parsed, false).await.expect(what);
        }
    }

    #[tokio::test]
    async fn check_url_allows_local_when_configured() {
        for url in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://10.0.0.1/",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            check_url(&parsed, true).await.expect(url);
        }
    }

    #[tokio::test]
    async fn check_url_rejects_non_http_schemes_even_when_local_is_allowed() {
        for url in ["ftp://example.com/", "file:///etc/passwd", "gopher://x/"] {
            let parsed = reqwest::Url::parse(url).unwrap();
            for allow_local in [false, true] {
                let err = check_url(&parsed, allow_local)
                    .await
                    .expect_err("non-http scheme rejected");
                assert!(err.contains("unsupported URL scheme"), "{url}: {err}");
            }
        }
    }

    #[tokio::test]
    async fn check_url_allows_public_ip_literals_without_dns() {
        // A public IP literal needs no DNS resolution to pass.
        let parsed = reqwest::Url::parse("http://8.8.8.8/").unwrap();
        check_url(&parsed, false).await.expect("public IP allowed");
    }

    /// A ten-hop redirect chain is bounded by the wall clock, not by ten
    /// separate timeouts.
    ///
    /// [`FETCH_TIMEOUT`] is a reqwest client setting and reqwest applies it per
    /// request, so a chain of [`MAX_REDIRECTS`] + 1 requests had a real budget
    /// of eleven times what every caller thought it had — up to five minutes
    /// under a nominal thirty seconds. The server below controls both halves of
    /// that (how many hops, and how slow each one is), which is what makes it a
    /// way to pin an agent turn rather than a curiosity.
    ///
    /// The assertion is on the clock and not only on the message, because a
    /// walk that ran out of *redirects* also returns an error and would satisfy
    /// a message-only test while taking the full unbudgeted time.
    #[tokio::test]
    async fn a_redirect_chain_is_bounded_in_wall_clock_not_per_hop() {
        let hop = Duration::from_millis(120);
        let addr = serve_slow_redirect_loop(hop).await;
        let client = no_redirect_client_builder(FETCH_TIMEOUT)
            .build()
            .expect("client");
        let start = reqwest::Url::parse(&format!("http://{addr}/")).expect("url");

        let began = std::time::Instant::now();
        let err = get_following_redirects(
            &client,
            start,
            true,
            HopScheme::Any,
            Duration::from_millis(250),
        )
        .await
        .expect_err("the walk must end on its budget");
        let elapsed = began.elapsed();

        assert!(err.contains("timed out"), "{err}");
        assert!(
            elapsed < hop * (MAX_REDIRECTS as u32),
            "the walk ran for {elapsed:?}, which is the whole chain rather than the budget"
        );
    }

    /// A server that redirects to itself forever, `delay` per hop. Loopback, so
    /// the walk has to be given `allow_local`.
    async fn serve_slow_redirect_loop(delay: Duration) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/next\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        addr
    }
}
