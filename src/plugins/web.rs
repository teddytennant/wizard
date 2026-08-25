//! The web tools, as a plugin: `web_fetch` (URL to markdown/text),
//! `web_search` (pluggable backends) and `x_search` (X/Twitter through xAI),
//! registered through [`Ctx::tool`] and compiled behind `--features tool-web`.
//!
//! A dependency audit picked this one because core does not reference it at
//! all: nothing outside `src/tools/registry.rs` named the three tool types,
//! and the registry names every tool. What the audit also found is why this
//! file is smaller than the `src/tools/web.rs` it came out of — the HTTP
//! plumbing underneath the tools has three callers and only one of them is a
//! tool.
//!
//! # Where the line is drawn, and why there
//!
//! [`crate::tools::http`] keeps the client, the SSRF guard, the hand-walked
//! redirect chain and the body cap. This file keeps the three tools, the
//! HTML-to-markdown reader, and the five search backends.
//!
//! The test is *who else would lose the code*. A Lua plugin's `wizard.http`
//! (`src/plugins/host.rs`) is that client and that guard reached from another
//! caller, and `generate_image` (`src/tools/image.rs`) walks the same redirect
//! chain with a stricter hop rule. Neither has anything to do with whether the
//! model can call `web_fetch`, and a build with `--no-default-features` still
//! grants `Capability::Network` and still downloads images. Moving the
//! plumbing here would mean those two silently lost their SSRF guard on the
//! day somebody left this feature out — which is exactly the failure the
//! plugin boundary is supposed to make impossible rather than merely
//! unlikely.
//!
//! This is the same split `src/llm/wire.rs` and `src/plugins/openai/` already
//! made: protocol machinery several unrelated callers share is core, and the
//! vendor-facing thing built on it is the plugin. `docs/plugins.md` states the
//! rule as "a shared transport that lived inside one plugin would be a
//! dependency edge between plugins".
//!
//! # The one entanglement that had to be cut
//!
//! `defang` reached into `crate::plugins::mesh` for its "what draws as nothing"
//! predicate. `mesh` is on its way out of core too, so that was a
//! plugin-to-plugin edge waiting to happen; the table moved down into
//! [`crate::text`] and both callers ask core. Nothing about the table changed
//! — what is invisible is a property of Unicode, not of the mesh.
//!
//! # What is still core about the web
//!
//! `[web]` in `config.toml` ([`WebConfig`](crate::config::WebConfig)) and its
//! `ToolContext::web` carrier. `allow_local` and `fetch_max_bytes` are
//! promises about what the *process* does on the network — the host bridge
//! honours them with no web tool compiled in at all — rather than settings for
//! one tool. A build without `tool-web` still reads them and still obeys them.
//!
//! All three tools are [`ToolAccess::ReadOnly`], so they stay available in
//! plan mode. Search API keys are read from the environment at call time and
//! never stored. `x_search` always uses xAI (OAuth session or API key) and
//! does not depend on `[web] search_backend`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::wire::TokenSource;
use crate::llm::xai_oauth::{self, XaiTokenSource};
use crate::tools::http::{
    HopScheme, MAX_REDIRECTS, REDIRECT_BUDGET, USER_AGENT, check_url, defang,
    get_following_redirects, read_capped, web_client,
};
use crate::tools::{
    MAX_OUTPUT_BYTES, Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args,
    truncate_output,
};

/// Default number of search results.
const DEFAULT_SEARCH_COUNT: usize = 5;

/// Hard cap on requested search results.
const MAX_SEARCH_COUNT: usize = 10;

// ---------------------------------------------------------------------------
// HTML → readable markdown
// ---------------------------------------------------------------------------

/// Tags dropped wholesale before markdown conversion: non-content machinery
/// (htmd 0.5 does not strip `<style>`/`<script>` text on its own) and page
/// chrome that only pads the output.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "form", "nav", "header", "footer",
    "aside", "button", "select", "canvas", "video", "audio", "object", "embed", "link", "meta",
];

/// A converted page smaller than this is checked for bot-challenge markers:
/// challenge interstitials sanitize down to almost nothing, real pages don't.
const CHALLENGE_MAX_CONVERTED_BYTES: usize = 2_000;

/// Convert fetched HTML to readable markdown: convert only the
/// `<main>`/`<article>` region when the page marks one, skip noise tags, and
/// tidy the result. `None` when htmd fails (the caller falls back to the raw
/// body).
fn html_to_markdown(html: &str) -> Option<String> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .build();
    let markdown = converter.convert(content_region(html)).ok()?;
    Some(tidy_markdown(&markdown))
}

/// The subtree worth converting: the first `<main>` element if the page has
/// one, else the first `<article>`, else the whole document.
fn content_region(html: &str) -> &str {
    extract_element(html, "main")
        .or_else(|| extract_element(html, "article"))
        .unwrap_or(html)
}

/// Extract the first `<{tag} ...>...</{tag}>` subtree (tags included) from
/// `html`, matching case-insensitively and accounting for nested elements of
/// the same name. Returns `None` — meaning "convert the whole document" —
/// when the tag is absent, self-closing, or never closed.
fn extract_element<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let bytes = html.as_bytes();
    let tag = tag.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let closing = bytes.get(i + 1) == Some(&b'/');
        let name_start = if closing { i + 2 } else { i + 1 };
        let name_end = name_start + tag.len();
        let named = bytes
            .get(name_start..name_end)
            .is_some_and(|name| name.eq_ignore_ascii_case(tag))
            && bytes
                .get(name_end)
                .is_some_and(|&next| next == b'>' || next == b'/' || next.is_ascii_whitespace());
        if !named {
            i += 1;
            continue;
        }
        // The tag runs to the next `>`; a tag truncated mid-document means
        // the structure is off, so fall back to the whole document.
        let gt = name_end + bytes[name_end..].iter().position(|&byte| byte == b'>')?;
        if closing {
            if depth > 0 {
                depth -= 1;
                if depth == 0 {
                    return Some(&html[start?..=gt]);
                }
            }
        } else if bytes[gt - 1] != b'/' {
            // Self-closing tags (`<main/>`) carry no subtree; skip them.
            if start.is_none() {
                start = Some(i);
            }
            depth += 1;
        }
        i = gt + 1;
    }
    None
}

/// Tidy converted markdown: drop image syntax (keeping alt text), trim
/// trailing whitespace per line, and collapse runs of 3+ newlines to 2.
fn tidy_markdown(markdown: &str) -> String {
    let stripped = strip_images(markdown);
    let mut out = String::with_capacity(stripped.len());
    let mut blank_pending = false;
    for line in stripped.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_pending = true;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_pending {
                out.push('\n');
            }
        }
        blank_pending = false;
        out.push_str(line);
    }
    out
}

/// Remove markdown image syntax `![alt](url)`, keeping non-empty alt text.
/// Anything that does not parse as a complete image is left untouched.
fn strip_images(markdown: &str) -> String {
    let bytes = markdown.as_bytes();
    let mut out = String::with_capacity(markdown.len());
    let mut copied_to = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'!'
            && bytes.get(i + 1) == Some(&b'[')
            && let Some((alt, end)) = parse_image(markdown, i)
        {
            out.push_str(&markdown[copied_to..i]);
            out.push_str(alt.trim());
            i = end;
            copied_to = end;
            continue;
        }
        i += 1;
    }
    out.push_str(&markdown[copied_to..]);
    out
}

/// Parse one `![alt](url)` whose `!` sits at byte `start`. Returns the alt
/// text and the offset just past the closing `)`, or `None` if the syntax is
/// incomplete. Nested brackets in the alt and balanced parentheses in the
/// url are tolerated.
fn parse_image(markdown: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = markdown.as_bytes();
    let alt_start = start + 2;
    let mut depth = 1usize;
    let mut i = alt_start;
    let alt_end = loop {
        match bytes.get(i)? {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break i;
                }
            }
            _ => {}
        }
        i += 1;
    };
    if bytes.get(alt_end + 1) != Some(&b'(') {
        return None;
    }
    let mut depth = 1usize;
    let mut i = alt_end + 2;
    loop {
        match bytes.get(i)? {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&markdown[alt_start..alt_end], i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Whether raw HTML is a JavaScript bot-challenge interstitial (Cloudflare
/// and similar) rather than real content.
fn is_challenge_page(html: &str) -> bool {
    let html = html.to_lowercase();
    html.contains("_cf_chl_opt")
        || html.contains("cf-browser-verification")
        || html.contains("attention required! | cloudflare")
        || html.contains("checking if the site connection is secure")
        || (html.contains("just a moment") && html.contains("enable javascript and cookies"))
}

/// Whether a content type is textual enough to return verbatim. An absent
/// content type is treated as text.
///
/// The web tool's question, not the transport's: a plugin calling
/// `wizard.http` gets whatever the endpoint sent, and `generate_image` wants
/// bytes. Only `web_fetch` has to decide whether to put a body in front of a
/// model as prose.
fn is_texty(content_type: &str) -> bool {
    content_type.is_empty()
        || content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("yaml")
        || content_type.contains("toml")
        || content_type.contains("x-www-form-urlencoded")
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

/// Arguments for [`WebFetchTool`].
#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
    /// Response byte cap; clamped to `[web] fetch_max_bytes`.
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// `web_fetch` — fetch a URL and return its content, HTML as markdown.
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over HTTP(S) and return its content. HTML pages are converted to \
         markdown; other text content is returned as-is. Responses are size-capped."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch" },
                "max_bytes": { "type": "integer", "description": "Cap on response bytes read (default and ceiling from config)" }
            },
            "required": ["url"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: FetchArgs = parse_args(self.name(), args)?;
        let url = reqwest::Url::parse(args.url.trim()).map_err(|err| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: format!("invalid url '{}': {err}", args.url),
        })?;

        let allow_local = ctx.web.allow_local;
        if let Err(reason) = check_url(&url, allow_local).await {
            return Ok(ToolOutput::error(reason));
        }

        let client = web_client().map_err(|err| ToolError::Execution {
            tool: self.name().to_string(),
            source: anyhow::Error::new(err).context("building HTTP client"),
        })?;

        let response = match get_following_redirects(
            &client,
            url.clone(),
            allow_local,
            HopScheme::Any,
            REDIRECT_BUDGET,
        )
        .await
        {
            Ok(response) => response,
            Err(err) => return Ok(ToolOutput::error(err)),
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        let cap = args
            .max_bytes
            .unwrap_or(ctx.web.fetch_max_bytes)
            .min(ctx.web.fetch_max_bytes)
            .max(1);
        let (body, capped) = match read_capped(response, cap).await {
            Ok(read) => read,
            Err(err) => return Ok(ToolOutput::error(format!("reading response failed: {err}"))),
        };

        if !status.is_success() {
            // Defanged like the success path: an error body is page content
            // too, authored by the same stranger, and a 404 is the easiest
            // response in the world to control.
            let snippet = truncate_output(String::from_utf8_lossy(&body).into_owned(), 1_000);
            return Ok(ToolOutput::error(defang(&format!(
                "fetch of {url} returned HTTP {status}\n{snippet}"
            ))));
        }

        let text = String::from_utf8_lossy(&body).into_owned();
        let mut content = if content_type.contains("html") {
            // HTML → readable markdown; fall back to the raw HTML if
            // conversion fails.
            match html_to_markdown(&text) {
                Some(markdown) => {
                    // A page that sanitizes down to almost nothing and carries
                    // challenge markers is a bot-protection interstitial, not
                    // content.
                    if markdown.len() < CHALLENGE_MAX_CONVERTED_BYTES && is_challenge_page(&text) {
                        return Ok(ToolOutput::error(format!(
                            "fetch of {url} was blocked by bot protection (JavaScript challenge \
                             page); the page content is not accessible to a plain HTTP client"
                        )));
                    }
                    markdown
                }
                None => text,
            }
        } else if is_texty(&content_type) {
            text
        } else {
            return Ok(ToolOutput::ok(format!(
                "(binary content type '{content_type}', {} bytes — not shown)",
                body.len()
            )));
        };

        if capped {
            content.push_str(&format!("\n... [response capped at {cap} bytes]"));
        }
        Ok(ToolOutput::ok(defang(&truncate_output(
            content,
            MAX_OUTPUT_BYTES,
        ))))
    }
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

/// Byte cap on a search endpoint's response body.
///
/// The fetch path has honoured `[web] fetch_max_bytes` since it was written;
/// this path had no cap at all, because `.text()` and `.json()` read to EOF
/// and a search backend's reply "is JSON we asked for" was treated as a reason
/// to trust its length. Three of the five backends are reached over a URL the
/// operator can point anywhere (`base_url`), the DuckDuckGo one parses
/// arbitrary HTML, and none of them needs a megabyte: ten results with a
/// three-hundred-character snippet each is single-digit kilobytes. So the cap
/// is generous enough that no honest endpoint sees it and small enough that a
/// hostile one cannot spend the process's memory.
///
/// Separate from `fetch_max_bytes` because that setting is the model's budget
/// for page *content* it will read, and this is a parser's budget for a
/// protocol reply the model never sees.
const SEARCH_MAX_BYTES: usize = 2_000_000;

/// Read a search response as text, refusing one that runs past the cap.
///
/// Refusing rather than truncating: a truncated search page parses to fewer
/// results, or to none, and reports success. Every other failure on this path
/// was made loud for that exact reason (see [`send_following_redirects`]), and
/// a silent short read is the same bug wearing a different hat.
async fn capped_text(response: reqwest::Response) -> anyhow::Result<String> {
    let (bytes, capped) = read_capped(response, SEARCH_MAX_BYTES).await?;
    if capped {
        anyhow::bail!("search endpoint's response exceeded {SEARCH_MAX_BYTES} bytes");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// [`capped_text`] for a JSON body. A capped JSON body would fail to parse
/// anyway; this says *why* instead of blaming the endpoint's syntax.
async fn capped_json(response: reqwest::Response) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&capped_text(response).await?)?)
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A pluggable `web_search` backend.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>>;
}

/// Send a search-backend request, following redirects by hand.
///
/// Every client here comes from [`web_client`], which does not follow
/// redirects at all — reqwest's policy callback is synchronous and cannot run
/// the resolving SSRF check, so the chain is always walked in the open. And
/// `error_for_status` does not consider 3xx an error. Together those meant a
/// redirected search parsed the *redirect's* body and reported success with
/// nothing in it: the DuckDuckGo backend, which is the default and the one
/// that needs no API key, answered a `302` with zero results and no error.
///
/// `build` runs again for each hop, so it has to be repeatable — which is why
/// the backends hand over a closure rather than a built request. The method
/// and body are replayed as-is (browsers downgrade a POST to GET on 301/302;
/// an API endpoint that redirects its own POST means the resource moved, not
/// that it wants a GET).
///
/// Only hops that stay on the host we were configured for are followed, so
/// the request's credentials — an API key in a header or in the JSON body —
/// are never replayed to a host the operator did not name. A hop off the host
/// is an error. The point of this function is that it is never silence.
///
/// `budget` bounds the whole walk rather than each hop, for the reason
/// [`REDIRECT_BUDGET`] gives.
async fn send_following_redirects<F>(
    client: &reqwest::Client,
    start: reqwest::Url,
    budget: Duration,
    build: F,
) -> anyhow::Result<reqwest::Response>
where
    F: Fn(&reqwest::Client, reqwest::Url) -> reqwest::RequestBuilder + Send,
{
    match tokio::time::timeout(budget, walk_search_redirects(client, start, build)).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "search endpoint did not answer within {}s (following redirects)",
            budget.as_secs()
        ),
    }
}

/// [`send_following_redirects`] without the budget, so the budget can wrap it.
async fn walk_search_redirects<F>(
    client: &reqwest::Client,
    start: reqwest::Url,
    build: F,
) -> anyhow::Result<reqwest::Response>
where
    F: Fn(&reqwest::Client, reqwest::Url) -> reqwest::RequestBuilder + Send,
{
    let mut url = start;
    for _ in 0..=MAX_REDIRECTS {
        // Built rather than sent directly so the query can be deduplicated
        // first: `build` calls `.query(…)`, which *appends* through
        // `query_pairs_mut()`, and a `Location` that carries its own query
        // string is replayed as the next hop's base. Sending it as built means
        // `?q=rust&q=rust` — two values for one parameter, resolved by
        // whichever rule the endpoint happens to use.
        let mut request = build(client, url.clone()).build()?;
        drop_duplicate_query_keys(request.url_mut());
        let response = client.execute(request).await?;
        let status = response.status();
        if !status.is_redirection() {
            return Ok(response.error_for_status()?);
        }
        let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
        else {
            anyhow::bail!("search endpoint returned HTTP {status} with no Location header");
        };
        let next = url
            .join(location)
            .map_err(|err| anyhow::anyhow!("redirect to invalid url '{location}': {err}"))?;
        if !hop_stays_home(&url, &next) {
            anyhow::bail!(
                "search endpoint redirected from '{url}' to '{next}', off the configured host \
                 — refusing to replay the request (and its credentials) there"
            );
        }
        url = next;
    }
    anyhow::bail!("too many redirects from the search endpoint")
}

/// Keep the **last** value of every repeated query parameter, dropping the
/// earlier ones.
///
/// Last, not first, because the backend's own `.query(…)` is appended after
/// whatever the redirect target carried: a `Location` of
/// `/html/?q=stale&region=us` replayed with `q=rust` must search for `rust`,
/// while `region=us` — a parameter the endpoint added and the backend knows
/// nothing about — is kept. A URL with no repeats is left byte-identical
/// rather than re-encoded, since round-tripping through `query_pairs` rewrites
/// spaces and percent-escapes and nothing here is worth that risk.
fn drop_duplicate_query_keys(url: &mut reqwest::Url) {
    if url.query().is_none() {
        return;
    }
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    // Walk backwards so the first sighting of a key is its last occurrence,
    // then flip back to restore the order the endpoint sent.
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<&(String, String)> = pairs
        .iter()
        .rev()
        .filter(|(key, _)| seen.insert(key.clone()))
        .collect();
    if kept.len() == pairs.len() {
        return;
    }
    kept.reverse();
    let deduped = kept;
    url.query_pairs_mut()
        .clear()
        .extend_pairs(deduped.iter().map(|(key, value)| (key, value)));
    // `clear()` on an otherwise empty query leaves a bare `?` behind.
    if url.query() == Some("") {
        url.set_query(None);
    }
}

/// Whether a redirect keeps the request on the host it was addressed to,
/// without downgrading the transport. A different port on the same host is
/// still the same machine, so it is allowed; `https` → `http` is not.
fn hop_stays_home(from: &reqwest::Url, to: &reqwest::Url) -> bool {
    from.host_str().is_some()
        && from.host_str() == to.host_str()
        && matches!(
            (from.scheme(), to.scheme()),
            ("https", "https") | ("http", "http" | "https")
        )
}

/// Default backend: scrape the DuckDuckGo HTML endpoint. No API key.
pub struct DuckDuckGoHtml {
    base_url: String,
}

impl DuckDuckGoHtml {
    pub fn new() -> Self {
        Self::with_base_url("https://html.duckduckgo.com/html/")
    }

    /// Point the backend at a different endpoint (tests use a local server).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for DuckDuckGoHtml {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for DuckDuckGoHtml {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client()?;
        let url = reqwest::Url::parse(&self.base_url)?;
        let response = send_following_redirects(&client, url, REDIRECT_BUDGET, |client, url| {
            client.get(url).query(&[("q", query)])
        })
        .await?;
        let html = capped_text(response).await?;
        Ok(parse_duckduckgo_html(&html, count))
    }
}

/// Parse DuckDuckGo HTML-endpoint results. Kept synchronous and self-
/// contained (scraper's DOM is not `Send`, so it must not live across an
/// await point) and separate for fixture-based unit tests.
fn parse_duckduckgo_html(html: &str, count: usize) -> Vec<SearchResult> {
    let document = scraper::Html::parse_document(html);
    let result_sel = scraper::Selector::parse("div.result").expect("valid selector");
    let title_sel = scraper::Selector::parse("a.result__a").expect("valid selector");
    let snippet_sel = scraper::Selector::parse(".result__snippet").expect("valid selector");

    let mut results = Vec::new();
    for result in document.select(&result_sel) {
        if results.len() >= count {
            break;
        }
        let Some(link) = result.select(&title_sel).next() else {
            continue;
        };
        let title = link.text().collect::<String>().trim().to_string();
        let url = decode_ddg_href(link.value().attr("href").unwrap_or(""));
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|node| node.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// Unwrap a DuckDuckGo redirect href
/// (`//duckduckgo.com/l/?uddg=<encoded target>&rut=...`) to the real target.
/// Non-redirect hrefs are returned as-is.
fn decode_ddg_href(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(url) = reqwest::Url::parse(&absolute) {
        if let Some((_, target)) = url.query_pairs().find(|(key, _)| key == "uddg") {
            return target.into_owned();
        }
        return absolute;
    }
    href.to_string()
}

/// Brave Search API backend (`X-Subscription-Token` key).
pub struct BraveSearch {
    base_url: String,
    api_key: String,
}

impl BraveSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.search.brave.com", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for BraveSearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client()?;
        let url = reqwest::Url::parse(&format!("{}/res/v1/web/search", self.base_url))?;
        let requested = count.to_string();
        let response = send_following_redirects(&client, url, REDIRECT_BUDGET, |client, url| {
            client
                .get(url)
                .header("X-Subscription-Token", &self.api_key)
                .header("Accept", "application/json")
                .query(&[("q", query), ("count", requested.as_str())])
        })
        .await?;
        let body: Value = capped_json(response).await?;
        let results = body["web"]["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .filter_map(|hit| {
                        let title = hit["title"].as_str()?.to_string();
                        let url = hit["url"].as_str()?.to_string();
                        let snippet = hit["description"].as_str().unwrap_or("").to_string();
                        Some(SearchResult {
                            title,
                            url,
                            snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }
}

/// Tavily Search API backend (key in the JSON request body).
pub struct TavilySearch {
    base_url: String,
    api_key: String,
}

impl TavilySearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.tavily.com", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for TavilySearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client()?;
        let url = reqwest::Url::parse(&format!("{}/search", self.base_url))?;
        let request = json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": count,
        });
        let response = send_following_redirects(&client, url, REDIRECT_BUDGET, |client, url| {
            client.post(url).json(&request)
        })
        .await?;
        let body: Value = capped_json(response).await?;
        let results = body["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .filter_map(|hit| {
                        let title = hit["title"].as_str()?.to_string();
                        let url = hit["url"].as_str()?.to_string();
                        let snippet = hit["content"].as_str().unwrap_or("").to_string();
                        Some(SearchResult {
                            title,
                            url,
                            snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }
}

/// Exa Search API backend (`x-api-key` header). Neural/keyword search tuned
/// for agents; we ask for short text snippets alongside each result.
pub struct ExaSearch {
    base_url: String,
    api_key: String,
}

impl ExaSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.exa.ai", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for ExaSearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client()?;
        let url = reqwest::Url::parse(&format!("{}/search", self.base_url))?;
        let request = json!({
            "query": query,
            "numResults": count,
            "contents": { "text": { "maxCharacters": 300 } },
        });
        let response = send_following_redirects(&client, url, REDIRECT_BUDGET, |client, url| {
            client
                .post(url)
                .header("x-api-key", &self.api_key)
                .header("Accept", "application/json")
                .json(&request)
        })
        .await?;
        let body: Value = capped_json(response).await?;
        let results = body["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .filter_map(|hit| {
                        let url = hit["url"].as_str()?.to_string();
                        let title = hit["title"].as_str().unwrap_or(&url).to_string();
                        let snippet = hit["text"].as_str().unwrap_or("").trim().to_string();
                        Some(SearchResult {
                            title,
                            url,
                            snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }
}

/// Serper backend: Google results via serper.dev (`X-API-KEY` header).
pub struct SerperSearch {
    base_url: String,
    api_key: String,
}

impl SerperSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://google.serper.dev", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for SerperSearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client()?;
        let url = reqwest::Url::parse(&format!("{}/search", self.base_url))?;
        let request = json!({ "q": query, "num": count });
        let response = send_following_redirects(&client, url, REDIRECT_BUDGET, |client, url| {
            client
                .post(url)
                .header("X-API-KEY", &self.api_key)
                .header("Accept", "application/json")
                .json(&request)
        })
        .await?;
        let body: Value = capped_json(response).await?;
        let results = body["organic"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .filter_map(|hit| {
                        let title = hit["title"].as_str()?.to_string();
                        let url = hit["link"].as_str()?.to_string();
                        let snippet = hit["snippet"].as_str().unwrap_or("").to_string();
                        Some(SearchResult {
                            title,
                            url,
                            snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }
}

/// How `XaiSearch` authenticates to xAI: the browser OAuth session
/// (`wizard --login xai`) or a plain `XAI_API_KEY`. OAuth is preferred.
enum XaiAuth {
    Oauth(XaiTokenSource),
    ApiKey(String),
}

/// Which Responses API server-side tool Grok should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XaiServerTool {
    /// Public web search-and-browse.
    WebSearch,
    /// X (Twitter) keyword / semantic / handle search.
    XSearch,
}

impl XaiServerTool {
    fn as_str(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::XSearch => "x_search",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::WebSearch => "web search",
            Self::XSearch => "X search",
        }
    }
}

/// xAI Grok search via the Responses API server-side `web_search` or
/// `x_search` tool.
///
/// Unlike the scraper/keyed backends this is an agentic call: Grok runs its
/// own search loop server-side and returns the synthesized hits. We ask for a
/// strict JSON envelope and fall back to the response's `url_citation`
/// annotations / top-level `citations` when the model adds prose anyway.
pub struct XaiSearch {
    base_url: String,
    model: String,
    auth: XaiAuth,
    server_tool: XaiServerTool,
    /// Extra fields merged into the `tools[0]` object (handle filters, dates).
    tool_options: Value,
}

/// Whole-request timeout for an xAI search. Still generous — the server-side
/// search loop is slower than a single scrape — but not the multi-minute wait
/// it used to be: with a non-reasoning search model a normal query lands in a
/// few seconds, so anything past this is hung, not thinking.
const XAI_SEARCH_TIMEOUT: Duration = Duration::from_secs(45);

/// Model that runs the server-side search loop.
///
/// Deliberately *not* [`xai_oauth::DEFAULT_MODEL`]: search is a fetch-and-
/// format job, not a reasoning one, and the flagship model spends most of the
/// wall clock thinking about a list of links. The non-reasoning model returns
/// the same hits in roughly a quarter of the time. Override with
/// `[web] search_model`.
const XAI_SEARCH_MODEL: &str = "grok-4.20-0309-non-reasoning";

/// Whether an xAI OAuth session exists on disk (`wizard --login xai`).
fn xai_signed_in() -> bool {
    xai_oauth::token_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Resolve xAI auth for search tools: OAuth session if signed in, else a
/// stored `xai` key, else `search_api_key_env` / `XAI_API_KEY`.
fn resolve_xai_auth(ctx: &ToolContext) -> Result<XaiSearch, String> {
    let model = ctx
        .web
        .search_model
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if xai_signed_in() {
        let source = XaiTokenSource::new()
            .map_err(|err| format!("opening the xAI OAuth token store: {err:#}"))?;
        return Ok(XaiSearch::oauth(source).with_model_override(model));
    }
    if let Some(key) = crate::credentials::get("xai")
        && !key.trim().is_empty()
    {
        return Ok(XaiSearch::api_key(key).with_model_override(model));
    }
    let env_name = ctx
        .web
        .search_api_key_env
        .as_deref()
        .unwrap_or(xai_oauth::DEFAULT_KEY_ENV);
    match std::env::var(env_name) {
        Ok(key) if !key.trim().is_empty() => Ok(XaiSearch::api_key(key).with_model_override(model)),
        _ => Err(format!(
            "xAI search needs auth: run `/login xai` to sign in (or `wizard --login xai`), \
             or set ${env_name} to an xAI API key"
        )),
    }
}

impl XaiSearch {
    /// Search using the stored xAI OAuth session (web search by default).
    fn oauth(source: XaiTokenSource) -> Self {
        Self {
            base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
            model: XAI_SEARCH_MODEL.to_string(),
            auth: XaiAuth::Oauth(source),
            server_tool: XaiServerTool::WebSearch,
            tool_options: json!({}),
        }
    }

    /// Search using a plain API key (web search by default).
    fn api_key(key: impl Into<String>) -> Self {
        Self {
            base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
            model: XAI_SEARCH_MODEL.to_string(),
            auth: XaiAuth::ApiKey(key.into()),
            server_tool: XaiServerTool::WebSearch,
            tool_options: json!({}),
        }
    }

    /// Apply a `[web] search_model` override, if one is configured.
    fn with_model_override(mut self, model: Option<&str>) -> Self {
        if let Some(model) = model {
            self.model = model.to_string();
        }
        self
    }

    /// Switch the server-side tool (`web_search` or `x_search`).
    fn with_server_tool(mut self, tool: XaiServerTool) -> Self {
        self.server_tool = tool;
        self
    }

    /// Merge extra options into the Responses `tools[0]` object (x_search
    /// handle filters and date range).
    fn with_tool_options(mut self, options: Value) -> Self {
        self.tool_options = options;
        self
    }

    /// Point the backend at a different endpoint (tests use a local server).
    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Resolve the current bearer token (refreshing the OAuth access token
    /// near expiry happens inside the token source).
    async fn bearer(&self) -> anyhow::Result<String> {
        match &self.auth {
            XaiAuth::ApiKey(key) => Ok(key.clone()),
            XaiAuth::Oauth(source) => source.bearer().await?.ok_or_else(|| {
                anyhow::anyhow!("no xAI OAuth token available; run `wizard --login xai`")
            }),
        }
    }

    /// The Responses API request body: a single user turn that hands Grok the
    /// chosen server-side tool and constrains it to a JSON-only reply.
    fn request_body(&self, model: &str, query: &str, count: usize) -> Value {
        let mut tool = json!({ "type": self.server_tool.as_str() });
        if let Some(map) = self.tool_options.as_object() {
            for (key, value) in map {
                tool[key] = value.clone();
            }
        }
        json!({
            "model": model,
            "input": [{
                "role": "user",
                "content": xai_search_prompt(self.server_tool, query, count)
            }],
            "tools": [tool],
            "include": ["no_inline_citations"],
        })
    }

    /// One search request against a given model.
    async fn search_with_model(
        &self,
        client: &reqwest::Client,
        url: &str,
        model: &str,
        query: &str,
        count: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let body = self.request_body(model, query, count);

        let mut retried = false;
        let response = loop {
            let token = self.bearer().await?;
            let response = client
                .post(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await?;
            // One forced refresh after a 401, mirroring the chat provider.
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && !retried
                && let XaiAuth::Oauth(source) = &self.auth
                && source.refresh_after_unauthorized().await.unwrap_or(false)
            {
                retried = true;
                continue;
            }
            break response.error_for_status()?;
        };

        let payload: Value = capped_json(response).await?;
        // Some errors arrive as HTTP 200 with an `error` envelope.
        if let Some(message) = payload["error"]["message"].as_str() {
            anyhow::bail!("xAI {} error: {message}", self.server_tool.label());
        }
        Ok(parse_xai_results(&payload, count))
    }
}

/// Whether a failed search looks like "that model does not exist" rather than
/// a network or auth problem. [`XAI_SEARCH_MODEL`] is a pinned snapshot, so it
/// can be retired out from under us; this is what decides to try the flagship
/// model instead of failing the search.
fn looks_like_unknown_model(err: &anyhow::Error) -> bool {
    if let Some(err) = err.downcast_ref::<reqwest::Error>() {
        return matches!(
            err.status(),
            Some(reqwest::StatusCode::NOT_FOUND) | Some(reqwest::StatusCode::BAD_REQUEST)
        );
    }
    err.to_string().to_ascii_lowercase().contains("model")
}

#[async_trait]
impl SearchBackend for XaiSearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(XAI_SEARCH_TIMEOUT)
            .build()?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let err = match self
            .search_with_model(&client, &url, &self.model, query, count)
            .await
        {
            Ok(results) => return Ok(results),
            Err(err) => err,
        };
        // The default is a pinned snapshot; if it has been retired, fall back
        // to the flagship model rather than leaving search broken. A model the
        // user configured is their choice, so that failure is reported as-is.
        if self.model != XAI_SEARCH_MODEL || !looks_like_unknown_model(&err) {
            return Err(err);
        }
        self.search_with_model(&client, &url, xai_oauth::DEFAULT_MODEL, query, count)
            .await
    }
}

/// Prompt that pins Grok to a JSON-only result envelope for a server tool.
fn xai_search_prompt(tool: XaiServerTool, query: &str, count: usize) -> String {
    let (use_line, url_hint) = match tool {
        XaiServerTool::WebSearch => (
            "Use the web_search tool to find current information for the query below",
            "with absolute https:// URLs",
        ),
        XaiServerTool::XSearch => (
            "Use the x_search tool to search X (formerly Twitter) for the query below",
            "with absolute https://x.com/… post or profile URLs when available",
        ),
    };
    format!(
        "{use_line}, then respond with ONLY a single JSON object — no prose, no markdown \
         fences, no inline citation links — matching this exact schema:\n\n\
         {{\"results\": [{{\"title\": \"string\", \"url\": \"string\", \"description\": \
         \"1-2 sentence summary\"}}]}}\n\n\
         Return at most {count} results, ordered by relevance, {url_hint}. \
         If no usable results exist, return {{\"results\": []}}.\n\n\
         Query: {query}"
    )
}

/// Parse an xAI Responses API payload into search hits, in three tiers:
/// 1. the JSON `{"results": [...]}` envelope the model was asked to emit,
/// 2. `url_citation` annotations on the output text, then
/// 3. a top-level `citations` array of bare URLs.
fn parse_xai_results(payload: &Value, count: usize) -> Vec<SearchResult> {
    let mut text = String::new();
    let mut citations: Vec<SearchResult> = Vec::new();
    if let Some(output) = payload["output"].as_array() {
        for item in output {
            let Some(content) = item["content"].as_array() else {
                continue;
            };
            for part in content {
                if let Some(chunk) = part["text"].as_str() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(chunk);
                }
                if let Some(annotations) = part["annotations"].as_array() {
                    for annotation in annotations {
                        if annotation["type"].as_str() != Some("url_citation") {
                            continue;
                        }
                        if let Some(url) = annotation["url"].as_str() {
                            citations.push(SearchResult {
                                title: annotation["title"].as_str().unwrap_or(url).to_string(),
                                url: url.to_string(),
                                snippet: String::new(),
                            });
                        }
                    }
                }
            }
        }
    }

    if let Some(results) = extract_results_json(&text)
        && !results.is_empty()
    {
        return results.into_iter().take(count).collect();
    }
    if !citations.is_empty() {
        return citations.into_iter().take(count).collect();
    }
    if let Some(urls) = payload["citations"].as_array() {
        return urls
            .iter()
            .filter_map(|url| url.as_str())
            .map(|url| SearchResult {
                title: url.to_string(),
                url: url.to_string(),
                snippet: String::new(),
            })
            .take(count)
            .collect();
    }
    Vec::new()
}

/// Pull a `{"results": [...]}` envelope out of model text. Tries the whole
/// string first, then the widest `{...}` span (which transparently strips any
/// surrounding prose or ```json fences).
fn extract_results_json(text: &str) -> Option<Vec<SearchResult>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok().or_else(|| {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str(&trimmed[start..=end]).ok()
    })?;
    let results = value["results"].as_array()?;
    Some(
        results
            .iter()
            .filter_map(|hit| {
                let title = hit["title"].as_str()?.to_string();
                let url = hit["url"].as_str()?.to_string();
                let snippet = hit["description"].as_str().unwrap_or("").to_string();
                Some(SearchResult {
                    title,
                    url,
                    snippet,
                })
            })
            .collect(),
    )
}

/// Render results as the numbered markdown list fed back to the model.
///
/// [`defang`]ed here rather than at the call sites, because every field in a
/// [`SearchResult`] is attacker-authored: titles, snippets and URLs come
/// straight out of a search backend's answer, and getting a chosen string into
/// one is a matter of publishing a page. `web_search` and `x_search` were
/// putting them in the transcript, the session JSONL and `stream-json`
/// verbatim while `web_fetch` — the same text, one hop earlier — was cleaned.
/// One filter on the one function both tools render through, so a third search
/// tool cannot forget it.
fn render_results(results: &[SearchResult]) -> String {
    let rendered = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let mut line = format!("{}. [{}]({})", index + 1, result.title, result.url);
            if !result.snippet.is_empty() {
                line.push_str("\n   ");
                line.push_str(&result.snippet);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    defang(&rendered)
}

/// Arguments for [`WebSearchTool`].
#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    /// Number of results (default 5, max 10).
    #[serde(default)]
    count: Option<usize>,
}

/// `web_search` — query the configured search backend.
pub struct WebSearchTool;

impl WebSearchTool {
    /// Build the configured backend, reading any API key from the
    /// environment at call time (keys are never stored).
    fn backend(ctx: &ToolContext) -> Result<Box<dyn SearchBackend>, String> {
        let name = ctx.web.search_backend.trim().to_ascii_lowercase();
        match name.as_str() {
            "" | "duckduckgo" => Ok(Box::new(DuckDuckGoHtml::new())),
            "brave" => Ok(Box::new(BraveSearch::new(Self::api_key(ctx, "brave")?))),
            "tavily" => Ok(Box::new(TavilySearch::new(Self::api_key(ctx, "tavily")?))),
            "exa" => Ok(Box::new(ExaSearch::new(Self::api_key(ctx, "exa")?))),
            "serper" => Ok(Box::new(SerperSearch::new(Self::api_key(ctx, "serper")?))),
            "xai" | "grok" => Ok(Box::new(
                resolve_xai_auth(ctx)?.with_server_tool(XaiServerTool::WebSearch),
            )),
            other => Err(format!(
                "unknown [web] search_backend '{other}' \
                 (expected duckduckgo, brave, tavily, exa, serper, or xai) — \
                 run /settings to configure web search"
            )),
        }
    }

    /// Resolve a keyed backend's API key. Prefers a key pasted via `/settings`
    /// or onboarding (stored in `~/.wizard/credentials.toml` under the backend
    /// name), then falls back to a configured env var.
    fn api_key(ctx: &ToolContext, backend: &str) -> Result<String, String> {
        if let Some(key) = crate::credentials::get(backend)
            && !key.trim().is_empty()
        {
            return Ok(key);
        }
        if let Some(env_name) = ctx.web.search_api_key_env.as_deref() {
            match std::env::var(env_name) {
                Ok(key) if !key.trim().is_empty() => return Ok(key),
                _ => {
                    return Err(format!(
                        "search backend '{backend}' needs an API key, but ${env_name} is unset \
                         or empty — run /settings to paste one"
                    ));
                }
            }
        }
        Err(format!(
            "search backend '{backend}' needs an API key: run /settings to paste one \
             (or set [web] search_api_key_env to the env var holding it)"
        ))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return a numbered list of results (title, url, snippet)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "integer", "description": "Number of results (default 5, max 10)" }
            },
            "required": ["query"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SearchArgs = parse_args(self.name(), args)?;
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "query must not be empty".to_string(),
            });
        }
        let count = args
            .count
            .unwrap_or(DEFAULT_SEARCH_COUNT)
            .clamp(1, MAX_SEARCH_COUNT);

        let backend = match Self::backend(ctx) {
            Ok(backend) => backend,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        let results = match backend.search(args.query.trim(), count).await {
            Ok(results) => results,
            Err(err) => return Ok(ToolOutput::error(format!("search failed: {err:#}"))),
        };
        if results.is_empty() {
            return Ok(ToolOutput::ok("(no results)"));
        }
        Ok(ToolOutput::ok(truncate_output(
            render_results(&results),
            MAX_OUTPUT_BYTES,
        )))
    }
}

// ---------------------------------------------------------------------------
// x_search
// ---------------------------------------------------------------------------

/// Arguments for [`XSearchTool`].
#[derive(Debug, Deserialize)]
struct XSearchArgs {
    query: String,
    /// Number of results (default 5, max 10).
    #[serde(default)]
    count: Option<usize>,
    /// Only include posts from these X handles (max 20).
    #[serde(default)]
    allowed_x_handles: Option<Vec<String>>,
    /// Exclude posts from these X handles (max 20).
    #[serde(default)]
    excluded_x_handles: Option<Vec<String>>,
    /// Inclusive start date (`YYYY-MM-DD`).
    #[serde(default)]
    from_date: Option<String>,
    /// Inclusive end date (`YYYY-MM-DD`).
    #[serde(default)]
    to_date: Option<String>,
}

/// Max handles accepted for allow/exclude lists (matches xAI docs).
const MAX_X_HANDLES: usize = 20;

/// `x_search` — search X (Twitter) via xAI Grok's server-side `x_search` tool.
///
/// Always uses xAI credentials (OAuth from `/login xai` preferred, else a
/// stored/env API key). Independent of `[web] search_backend`.
pub struct XSearchTool;

impl XSearchTool {
    /// Build tool options for the Responses `x_search` object, rejecting
    /// mutually exclusive handle filters and empty query already handled
    /// upstream.
    fn tool_options(args: &XSearchArgs) -> Result<Value, String> {
        let allowed = normalize_handles(args.allowed_x_handles.as_deref());
        let excluded = normalize_handles(args.excluded_x_handles.as_deref());
        if !allowed.is_empty() && !excluded.is_empty() {
            return Err(
                "allowed_x_handles and excluded_x_handles cannot be set together".to_string(),
            );
        }
        if allowed.len() > MAX_X_HANDLES {
            return Err(format!(
                "allowed_x_handles accepts at most {MAX_X_HANDLES} handles (got {})",
                allowed.len()
            ));
        }
        if excluded.len() > MAX_X_HANDLES {
            return Err(format!(
                "excluded_x_handles accepts at most {MAX_X_HANDLES} handles (got {})",
                excluded.len()
            ));
        }
        let mut options = serde_json::Map::new();
        if !allowed.is_empty() {
            options.insert("allowed_x_handles".to_string(), json!(allowed));
        }
        if !excluded.is_empty() {
            options.insert("excluded_x_handles".to_string(), json!(excluded));
        }
        if let Some(from) = args
            .from_date
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            options.insert("from_date".to_string(), json!(from));
        }
        if let Some(to) = args
            .to_date
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            options.insert("to_date".to_string(), json!(to));
        }
        Ok(Value::Object(options))
    }
}

/// Strip empty entries and leading `@` from handle lists.
fn normalize_handles(handles: Option<&[String]>) -> Vec<String> {
    handles
        .unwrap_or(&[])
        .iter()
        .map(|h| h.trim().trim_start_matches('@').to_string())
        .filter(|h| !h.is_empty())
        .collect()
}

#[async_trait]
impl Tool for XSearchTool {
    fn name(&self) -> &str {
        "x_search"
    }

    fn description(&self) -> &str {
        "Search X (formerly Twitter) via xAI Grok and return a numbered list of \
         posts/profiles (title, url, snippet). Requires `/login xai` or an xAI API key. \
         Prefer this over web_search for live discussion, posts, and handles on X."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (keywords, topic, or @handle context)"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results (default 5, max 10)"
                },
                "allowed_x_handles": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only include posts from these X handles (max 20; no leading @)"
                },
                "excluded_x_handles": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exclude posts from these X handles (max 20; cannot combine with allowed_x_handles)"
                },
                "from_date": {
                    "type": "string",
                    "description": "Inclusive start date (YYYY-MM-DD)"
                },
                "to_date": {
                    "type": "string",
                    "description": "Inclusive end date (YYYY-MM-DD)"
                }
            },
            "required": ["query"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: XSearchArgs = parse_args(self.name(), args)?;
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "query must not be empty".to_string(),
            });
        }
        let options = match Self::tool_options(&args) {
            Ok(options) => options,
            Err(message) => {
                return Err(ToolError::InvalidArgs {
                    tool: self.name().to_string(),
                    message,
                });
            }
        };
        let count = args
            .count
            .unwrap_or(DEFAULT_SEARCH_COUNT)
            .clamp(1, MAX_SEARCH_COUNT);

        let backend = match resolve_xai_auth(ctx) {
            Ok(backend) => backend
                .with_server_tool(XaiServerTool::XSearch)
                .with_tool_options(options),
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        let results = match backend.search(args.query.trim(), count).await {
            Ok(results) => results,
            Err(err) => return Ok(ToolOutput::error(format!("X search failed: {err:#}"))),
        };
        if results.is_empty() {
            return Ok(ToolOutput::ok("(no results)"));
        }
        Ok(ToolOutput::ok(truncate_output(
            render_results(&results),
            MAX_OUTPUT_BYTES,
        )))
    }
}

// ---------------------------------------------------------------------------
// the plugin
// ---------------------------------------------------------------------------

/// `web`: the three tools, and nothing else.
pub struct WebPlugin {
    manifest: PluginManifest,
}

impl WebPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "web".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Fetch and search the web".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                // Not in `pi`: `docs/plugins.md` defines that profile as
                // "minimal plus a local provider, JIT tuned, no mesh, no GUI,
                // no web".
                profiles: vec!["full".to_string(), "server".to_string()],
            },
        }
    }
}

impl Default for WebPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for WebPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Three registrations and no fallback. A tool the model is told about but
    /// cannot run is worse than an absent one: it burns a turn discovering
    /// that, and it does so in the middle of somebody's work. So a build
    /// without this feature advertises nothing here, and the model's tool list
    /// is exactly what the build can do.
    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.tool(std::sync::Arc::new(WebFetchTool))?;
        ctx.tool(std::sync::Arc::new(WebSearchTool))?;
        ctx.tool(std::sync::Arc::new(XSearchTool))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::config::WebConfig;

    // -- local fixture server -------------------------------------------------

    /// Serve a fixed raw HTTP response on a loopback listener; returns the
    /// bound address. Every connection gets the same response.
    async fn serve(response: String) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        addr
    }

    fn http_response(content_type: &str, body: &str) -> String {
        http_response_status(200, content_type, body)
    }

    fn http_response_status(status: u16, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Like [`serve`], but records each request body and picks the response
    /// from the request count (1-based), so a test can drive a retry.
    async fn serve_recording<F>(bodies: Arc<Mutex<Vec<String>>>, respond: F) -> SocketAddr
    where
        F: Fn(usize) -> String + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let respond = Arc::new(respond);
            while let Ok((mut socket, _)) = listener.accept().await {
                let bodies = Arc::clone(&bodies);
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, body)| body.to_string())
                        .unwrap_or_default();
                    let count = {
                        let mut seen = bodies.lock().expect("lock");
                        seen.push(body);
                        seen.len()
                    };
                    let _ = socket.write_all(respond(count).as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        addr
    }

    /// Context whose `[web]` settings allow loopback fetches.
    fn local_ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            allow_local: true,
            ..WebConfig::default()
        })
    }

    // -- web_fetch ------------------------------------------------------------

    #[tokio::test]
    async fn fetch_converts_html_to_markdown() {
        let addr = serve(http_response(
            "text/html; charset=utf-8",
            "<html><body><h1>Spellbook</h1><p>Read the <a href=\"https://example.com/docs\">docs</a>.</p></body></html>",
        ))
        .await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("# Spellbook"), "{}", out.content);
        assert!(
            out.content.contains("[docs](https://example.com/docs)"),
            "{}",
            out.content
        );
        assert!(
            !out.content.contains("<h1>"),
            "no raw html: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_returns_plain_text_as_is() {
        let addr = serve(http_response("text/plain", "plain payload, not markdown")).await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "plain payload, not markdown");
    }

    #[tokio::test]
    async fn fetch_notes_binary_content_instead_of_dumping_it() {
        let addr = serve(http_response("application/octet-stream", "\u{1}\u{2}\u{3}")).await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("binary content type 'application/octet-stream'"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_caps_the_response_at_the_configured_bytes() {
        let body = "a".repeat(5_000);
        let addr = serve(http_response("text/plain", &body)).await;
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            allow_local: true,
            fetch_max_bytes: 100,
            ..WebConfig::default()
        });
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("[response capped at 100 bytes]"),
            "{}",
            out.content
        );
        assert!(out.content.len() < 200, "content stayed small");
    }

    #[tokio::test]
    async fn fetch_max_bytes_arg_is_clamped_to_the_config_cap() {
        let body = "b".repeat(5_000);
        let addr = serve(http_response("text/plain", &body)).await;
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            allow_local: true,
            fetch_max_bytes: 100,
            ..WebConfig::default()
        });
        // Asking for more than the config cap still stops at the cap.
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/"), "max_bytes": 50_000 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("capped at 100 bytes"),
            "{}",
            out.content
        );

        // Asking for less reads less.
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/"), "max_bytes": 10 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("capped at 10 bytes"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_blocks_loopback_by_default() {
        // No request is made: the guard rejects before connecting, so an
        // unbound port is fine.
        let ctx = ToolContext::new(std::env::temp_dir());
        assert!(!ctx.web.allow_local, "guard on by default");
        let out = WebFetchTool
            .execute(json!({ "url": "http://127.0.0.1:1/" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("blocked"), "{}", out.content);
    }

    #[tokio::test]
    async fn fetch_reports_http_errors_as_tool_errors() {
        let addr = serve(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found".to_string(),
        )
        .await;
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/missing") }),
                &local_ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("HTTP 404"), "{}", out.content);
        assert!(out.content.contains("not found"), "{}", out.content);
    }

    #[tokio::test]
    async fn fetch_rejects_invalid_urls_as_invalid_args() {
        let err = WebFetchTool
            .execute(json!({ "url": "not a url" }), &local_ctx())
            .await
            .expect_err("invalid url");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "web_fetch"));
    }

    // -- HTML → readable markdown ----------------------------------------------

    /// Trimmed-down Cloudflare "Just a moment..." interstitial, cut from a
    /// real capture of https://www.britannica.com/place/Chad: title, inline
    /// CSS, the noscript hint, and the `_cf_chl_opt` challenge script.
    const CF_CHALLENGE_FIXTURE: &str = r#"<!DOCTYPE html><html lang="en-US"><head><title>Just a moment...</title><meta http-equiv="Content-Type" content="text/html; charset=UTF-8"><meta name="robots" content="noindex,nofollow"><style>*{box-sizing:border-box;margin:0;padding:0}html{line-height:1.15;-webkit-text-size-adjust:100%;color:#313131;font-family:system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif}body{display:flex;flex-direction:column;height:100vh;min-height:100vh}.main-content{margin:8rem auto;padding-left:1.5rem;max-width:60rem}@media (width <= 720px){.main-content{margin-top:4rem}}#challenge-error-text{background-image:url("data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIzMiIgaGVpZ2h0PSIzMiIgZmlsbD0ibm9uZSI+PC9zdmc+");background-repeat:no-repeat;background-size:contain;padding-left:34px}</style><meta http-equiv="refresh" content="360"></head><body><div class="main-wrapper" role="main"><div class="main-content"><noscript><div class="h2"><span id="challenge-error-text">Enable JavaScript and cookies to continue</span></div></noscript></div></div><script nonce="7VqN6B2znfxOt1sma6oiIH">(function(){window._cf_chl_opt = {cFPWv: 'b',cITimeS: '1783599881',cType: 'managed',cZone: 'www.britannica.com',cUPMDTk:"/place/Chad?__cf_chl_tk=O0KNo0DbPl4",cvId: '3'};var trkjs = document.createElement('img');trkjs.setAttribute('src', '/cdn-cgi/images/trace/managed/nojs/transparent.gif?ray=a1874e9c5b7abaf2');}());</script></body></html>"#;

    #[test]
    fn style_and_script_content_never_reach_the_markdown() {
        // Regression for the user-reported bug: htmd 0.5 emits the text of
        // <style>/<script>/<noscript>/<svg> unless the tags are skipped.
        let html = "<html><head><style>body{color:red;font-family:Arial}</style></head><body>\
                    <script>var tracker = 1;</script><p>Real content.</p>\
                    <noscript>Enable JavaScript</noscript>\
                    <svg><path d=\"M0 0L1 1\"/></svg></body></html>";
        let md = html_to_markdown(html).unwrap();
        assert!(md.contains("Real content."), "{md}");
        assert!(!md.contains("color:red"), "style stripped: {md}");
        assert!(!md.contains("var tracker"), "script stripped: {md}");
        assert!(!md.contains("Enable JavaScript"), "noscript stripped: {md}");
        assert!(!md.contains("M0 0"), "svg stripped: {md}");
    }

    #[test]
    fn page_chrome_is_dropped_from_the_markdown() {
        let html = "<body><nav><a href=\"/a\">Home</a></nav><header>Site header</header>\
                    <p>Body text.</p><form><button>Subscribe</button></form>\
                    <footer>Copyright</footer><aside>Related links</aside></body>";
        let md = html_to_markdown(html).unwrap();
        assert!(md.contains("Body text."), "{md}");
        for junk in [
            "Home",
            "Site header",
            "Subscribe",
            "Copyright",
            "Related links",
        ] {
            assert!(!md.contains(junk), "'{junk}' leaked into: {md}");
        }
    }

    #[test]
    fn content_region_prefers_main_then_article_then_whole_document() {
        let html = "<body><article>from article</article><main id=\"m\">from main</main></body>";
        assert_eq!(content_region(html), "<main id=\"m\">from main</main>");
        let html = "<body><p>pre</p><article>from article</article><p>post</p></body>";
        assert_eq!(content_region(html), "<article>from article</article>");
        let html = "<body><p>no landmark here</p></body>";
        assert_eq!(content_region(html), html);
    }

    #[test]
    fn extract_element_is_case_insensitive_and_tracks_nesting() {
        let html = "<MAIN class=\"a\"><p>x</p><main>inner</main>tail</MAIN>rest";
        assert_eq!(
            extract_element(html, "main"),
            Some("<MAIN class=\"a\"><p>x</p><main>inner</main>tail</MAIN>")
        );
    }

    #[test]
    fn extract_element_ignores_lookalike_names() {
        // Neither a longer tag name nor attribute values may match.
        let html = "<mainframe>no</mainframe><div class=\"main\" data-main=\"x\">no</div>";
        assert_eq!(extract_element(html, "main"), None);
    }

    #[test]
    fn extract_element_survives_malformed_html_without_matching() {
        for html in [
            "",
            "<",
            "</",
            "<main",
            "<main ",
            "<main class=", // truncated inside the opening tag
            "<main>never closed",
            "<main><main>closed once</main>",
            "</main>close before open",
            "<main/>", // self-closing: no subtree
            "<main attr=\"</main>\"",
            "<main\u{2192}>unicode after name</main\u{2192}>",
        ] {
            assert_eq!(extract_element(html, "main"), None, "{html:?}");
        }
        // A stray close does not stop a later real element from matching.
        assert_eq!(
            extract_element("</main><main>real</main>", "main"),
            Some("<main>real</main>")
        );
        // A self-closing lookalike does not stop a later real element.
        assert_eq!(
            extract_element("<main/><main>real</main>", "main"),
            Some("<main>real</main>")
        );
    }

    #[test]
    fn image_syntax_is_removed_keeping_alt_text() {
        assert_eq!(
            strip_images("before ![a chart](https://x/y.png) after"),
            "before a chart after"
        );
        assert_eq!(strip_images("![](https://x/decorative.png)text"), "text");
        // Balanced parentheses inside the url are consumed.
        assert_eq!(
            strip_images("![x](https://en.wikipedia.org/wiki/Chad_(country))!"),
            "x!"
        );
        // A linked image keeps the link, dropping only the image part.
        assert_eq!(
            strip_images("[![logo](https://x/l.svg)](https://x/)"),
            "[logo](https://x/)"
        );
        // Incomplete syntax is left untouched.
        assert_eq!(strip_images("![dangling](no-close"), "![dangling](no-close");
        assert_eq!(strip_images("![no-url]"), "![no-url]");
        assert_eq!(
            strip_images("plain ! bang [link](url)"),
            "plain ! bang [link](url)"
        );
        assert_eq!(strip_images("!["), "![");
    }

    #[test]
    fn markdown_blank_runs_and_trailing_whitespace_are_collapsed() {
        assert_eq!(tidy_markdown("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(tidy_markdown("a\n\nb"), "a\n\nb", "double newline kept");
        assert_eq!(tidy_markdown("a  \nb\t\nc"), "a\nb\nc");
        assert_eq!(tidy_markdown("\n\na\n\n"), "a");
        assert_eq!(tidy_markdown(""), "");
    }

    #[test]
    fn challenge_markers_are_detected() {
        assert!(is_challenge_page(CF_CHALLENGE_FIXTURE));
        assert!(is_challenge_page(
            "<title>Attention Required! | Cloudflare</title>"
        ));
        assert!(is_challenge_page(
            "checking if the site connection is secure"
        ));
        assert!(is_challenge_page("<div class=\"cf-browser-verification\">"));
        // "just a moment" alone is not enough.
        assert!(!is_challenge_page("<p>Just a moment while I check</p>"));
        assert!(!is_challenge_page(
            "<main>Chad is a landlocked country in Africa.</main>"
        ));
    }

    #[tokio::test]
    async fn fetch_reports_challenge_pages_as_a_one_line_error() {
        let addr = serve(http_response(
            "text/html; charset=utf-8",
            CF_CHALLENGE_FIXTURE,
        ))
        .await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.content,
            format!(
                "fetch of http://{addr}/ was blocked by bot protection (JavaScript challenge \
                 page); the page content is not accessible to a plain HTTP client"
            )
        );
    }

    #[tokio::test]
    async fn fetch_extracts_the_main_region_and_drops_junk() {
        let html = "<html><head><title>T</title><style>.junk{display:none}</style></head>\
            <body><nav><a href=\"/one\">Elsewhere</a></nav>\
            <main><h1>Chad</h1><p>Chad is a landlocked country.</p>\
            <img src=\"/map.png\" alt=\"Map of Chad\"></main>\
            <footer>Cookie banner. Sign up!</footer>\
            <script>analytics();</script></body></html>";
        let addr = serve(http_response("text/html", html)).await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("# Chad"), "{}", out.content);
        assert!(
            out.content.contains("Chad is a landlocked country."),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("Map of Chad"),
            "image alt text kept: {}",
            out.content
        );
        for junk in [
            "display:none",
            "Elsewhere",
            "Cookie banner",
            "analytics()",
            "map.png",
        ] {
            assert!(!out.content.contains(junk), "'{junk}' in: {}", out.content);
        }
    }

    // -- web_search -----------------------------------------------------------

    /// DuckDuckGo-shaped HTML fixture: two results, one with a wrapped
    /// redirect href and one with a plain absolute href.
    const DDG_FIXTURE: &str = r#"<!DOCTYPE html><html><body>
      <div class="serp__results">
        <div class="result results_links results_links_deep web-result">
          <div class="links_main links_deep result__body">
            <h2 class="result__title">
              <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&amp;rut=abc123">Rust Programming Language</a>
            </h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&amp;rut=abc123">A language empowering everyone to build reliable software.</a>
          </div>
        </div>
        <div class="result results_links results_links_deep web-result">
          <div class="links_main links_deep result__body">
            <h2 class="result__title">
              <a rel="nofollow" class="result__a" href="https://doc.rust-lang.org/book/">The Rust Book</a>
            </h2>
            <a class="result__snippet" href="https://doc.rust-lang.org/book/">An introductory book about Rust.</a>
          </div>
        </div>
      </div>
    </body></html>"#;

    #[test]
    fn duckduckgo_parser_extracts_results_from_fixture() {
        let results = parse_duckduckgo_html(DDG_FIXTURE, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(
            results[0].url, "https://www.rust-lang.org/",
            "uddg redirect href is unwrapped"
        );
        assert!(results[0].snippet.contains("reliable software"));
        assert_eq!(results[1].title, "The Rust Book");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn duckduckgo_parser_honors_count_and_empty_input() {
        assert_eq!(parse_duckduckgo_html(DDG_FIXTURE, 1).len(), 1);
        assert!(parse_duckduckgo_html("<html><body>no results</body></html>", 5).is_empty());
    }

    #[test]
    fn ddg_href_decoding_handles_plain_and_wrapped_links() {
        assert_eq!(
            decode_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%20b&rut=x"),
            "https://example.com/a b"
        );
        assert_eq!(
            decode_ddg_href("https://example.com/direct"),
            "https://example.com/direct"
        );
        assert_eq!(decode_ddg_href(""), "");
    }

    #[tokio::test]
    async fn duckduckgo_backend_searches_a_local_fixture_server() {
        let addr = serve(http_response("text/html", DDG_FIXTURE)).await;
        let backend = DuckDuckGoHtml::with_base_url(format!("http://{addr}/html/"));
        let results = backend.search("rust", 5).await.expect("search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
    }

    /// A redirected search follows the hop instead of reporting nothing found.
    ///
    /// The search clients stopped following redirects when the SSRF hardening
    /// gave every web client `Policy::none()`, and nothing was put in their
    /// place. `error_for_status` does not treat 3xx as an error, so the
    /// backend went on to parse the redirect's empty body: `web_search`
    /// answered "no results" — the honest-looking failure, on the backend that
    /// is the default and the only one that needs no API key.
    #[tokio::test]
    async fn a_redirected_search_follows_the_hop_rather_than_finding_nothing() {
        let addr = serve_redirect_then(DDG_FIXTURE).await;
        let backend = DuckDuckGoHtml::with_base_url(format!("http://{addr}/html"));
        let results = backend.search("rust", 5).await.expect("search ok");
        assert_eq!(
            results.len(),
            2,
            "the redirect was followed, not parsed as an empty page"
        );
    }

    /// A search endpoint that redirects off its host is an error, not a
    /// silently unauthenticated request somewhere else. The keyed backends put
    /// an API key in a header or a JSON body; replaying that at whatever host
    /// a `302` names hands the key to that host.
    #[tokio::test]
    async fn a_search_redirect_off_the_configured_host_is_refused() {
        let addr = serve(
            "HTTP/1.1 302 Found\r\nLocation: http://93.184.216.34/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        )
        .await;
        let backend = DuckDuckGoHtml::with_base_url(format!("http://{addr}/html"));
        let err = format!(
            "{:#}",
            backend.search("rust", 5).await.expect_err("refused")
        );
        assert!(
            err.contains("off the configured host"),
            "the hop off-host is what must be refused: {err}"
        );
    }

    /// A redirect whose `Location` carries its own query string must not make
    /// the backend send its parameters twice.
    ///
    /// `.query(…)` appends through `query_pairs_mut()`, so replaying the
    /// request against a `Location` of `?q=stale` produced `?q=stale&q=rust`:
    /// two values for one parameter, and which one wins is the endpoint's
    /// business, not ours. What the endpoint added for itself has to survive,
    /// which is why this is a de-duplication and not a `set_query(None)`.
    #[tokio::test]
    async fn a_redirect_carrying_a_query_does_not_duplicate_the_search_parameter() {
        let (addr, seen) =
            serve_redirect_carrying_a_query("?q=stale&region=us-en", DDG_FIXTURE).await;
        let backend = DuckDuckGoHtml::with_base_url(format!("http://{addr}/html"));
        let results = backend.search("rust", 5).await.expect("search ok");
        assert_eq!(results.len(), 2, "the redirect was followed");

        let targets = seen.lock().expect("recorder").clone();
        assert_eq!(targets.len(), 2, "one hop, then the replay: {targets:?}");
        let replayed = &targets[1];
        assert_eq!(
            replayed.matches("q=").count(),
            1,
            "exactly one q= on the replayed hop: {replayed}"
        );
        assert!(replayed.contains("q=rust"), "{replayed}");
        assert!(
            !replayed.contains("q=stale"),
            "the redirect's stale value must lose to the backend's: {replayed}"
        );
        assert!(
            replayed.contains("region=us-en"),
            "a parameter the endpoint added for itself is kept: {replayed}"
        );
    }

    #[test]
    fn de_duplicating_a_query_leaves_a_url_without_repeats_alone() {
        for untouched in [
            "http://example.com/search",
            "http://example.com/search?q=a%20b&region=us-en",
            "http://example.com/search#frag",
        ] {
            let mut url = reqwest::Url::parse(untouched).expect("parse");
            drop_duplicate_query_keys(&mut url);
            assert_eq!(url.as_str(), untouched, "rewritten needlessly");
        }

        // And a query that is *only* repeats collapses without leaving the
        // bare `?` that `query_pairs_mut().clear()` writes.
        let mut url = reqwest::Url::parse("http://example.com/s?q=one&q=two").expect("parse");
        drop_duplicate_query_keys(&mut url);
        assert_eq!(url.as_str(), "http://example.com/s?q=two");
    }

    /// Answer the first request with a same-host `302` whose `Location` also
    /// carries `location_query`, then serve `body`. Every request target the
    /// server saw is recorded, so a test can read what the replayed hop asked
    /// for rather than infer it from the answer.
    async fn serve_redirect_carrying_a_query(
        location_query: &'static str,
        body: &'static str,
    ) -> (SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            let mut first = true;
            while let Ok((mut socket, _)) = listener.accept().await {
                let redirect = std::mem::take(&mut first);
                let recorder = std::sync::Arc::clone(&recorder);
                let response = if redirect {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/html/{location_query}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    http_response("text/html", body)
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).into_owned();
                    if let Some(target) = request.split_whitespace().nth(1) {
                        recorder.lock().expect("recorder").push(target.to_string());
                    }
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (addr, seen)
    }

    /// Answer the first request with a same-host `302` and every later one
    /// with `body` as HTML.
    async fn serve_redirect_then(body: &'static str) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut first = true;
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = if std::mem::take(&mut first) {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/html/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    http_response("text/html", body)
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn brave_backend_parses_the_api_shape() {
        let body = json!({
            "web": { "results": [
                { "title": "Result One", "url": "https://one.example/", "description": "first" },
                { "title": "Result Two", "url": "https://two.example/", "description": "second" }
            ]}
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = BraveSearch::with_base_url(format!("http://{addr}"), "test-key");
        let results = backend.search("anything", 5).await.expect("search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result One");
        assert_eq!(results[1].snippet, "second");
    }

    #[tokio::test]
    async fn tavily_backend_parses_the_api_shape() {
        let body = json!({
            "results": [
                { "title": "Tavily Hit", "url": "https://hit.example/", "content": "summary text" }
            ]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = TavilySearch::with_base_url(format!("http://{addr}"), "test-key");
        let results = backend.search("anything", 5).await.expect("search ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "summary text");
    }

    #[tokio::test]
    async fn exa_backend_parses_the_api_shape() {
        let body = json!({
            "results": [
                { "title": "Exa Hit", "url": "https://exa.example/", "text": "  neural summary  " },
                { "url": "https://exa.example/notitle", "text": "no title falls back to url" }
            ]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = ExaSearch::with_base_url(format!("http://{addr}"), "test-key");
        let results = backend.search("anything", 5).await.expect("search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Exa Hit");
        assert_eq!(results[0].snippet, "neural summary", "text is trimmed");
        assert_eq!(results[1].title, "https://exa.example/notitle");
    }

    #[tokio::test]
    async fn serper_backend_parses_the_api_shape() {
        let body = json!({
            "organic": [
                { "title": "Serper One", "link": "https://serper.example/1", "snippet": "first" },
                { "title": "Serper Two", "link": "https://serper.example/2", "snippet": "second" }
            ]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = SerperSearch::with_base_url(format!("http://{addr}"), "test-key");
        let results = backend.search("anything", 5).await.expect("search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://serper.example/1");
        assert_eq!(results[1].snippet, "second");
    }

    #[tokio::test]
    async fn xai_backend_extracts_the_json_envelope() {
        // Grok replies with the JSON envelope we asked for inside an
        // output_text part.
        let envelope =
            r#"{"results":[{"title":"Grok 4.3","url":"https://x.ai/","description":"flagship"}]}"#;
        let body = json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": envelope, "annotations": [] }]
            }]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = XaiSearch::api_key("test-key").with_base_url(format!("http://{addr}"));
        let results = backend.search("grok", 5).await.expect("search ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Grok 4.3");
        assert_eq!(results[0].url, "https://x.ai/");
        assert_eq!(results[0].snippet, "flagship");
    }

    #[test]
    fn xai_parser_strips_prose_and_fences_around_the_envelope() {
        let payload = json!({
            "output": [{
                "content": [{
                    "type": "output_text",
                    "text": "Here you go:\n```json\n{\"results\":[{\"title\":\"A\",\"url\":\"https://a.example/\",\"description\":\"d\"}]}\n```",
                    "annotations": []
                }]
            }]
        });
        let results = parse_xai_results(&payload, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://a.example/");
    }

    #[test]
    fn xai_parser_falls_back_to_url_citation_annotations() {
        // No JSON envelope — recover from annotations on the text part.
        let payload = json!({
            "output": [{
                "content": [{
                    "type": "output_text",
                    "text": "Grok rambled without emitting JSON.",
                    "annotations": [
                        { "type": "url_citation", "title": "Cited One", "url": "https://one.example/" },
                        { "type": "url_citation", "url": "https://two.example/" }
                    ]
                }]
            }]
        });
        let results = parse_xai_results(&payload, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Cited One");
        assert_eq!(
            results[1].title, "https://two.example/",
            "url stands in for a missing title"
        );
    }

    #[test]
    fn xai_parser_falls_back_to_top_level_citations() {
        let payload = json!({
            "output": [{ "content": [{ "type": "output_text", "text": "no json, no annotations" }] }],
            "citations": ["https://cite.example/a", "https://cite.example/b"]
        });
        let results = parse_xai_results(&payload, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].url, "https://cite.example/b");
    }

    #[test]
    fn xai_parser_honors_count_and_empty_results() {
        let payload = json!({
            "output": [{ "content": [{ "type": "output_text", "text": "{\"results\":[]}" }] }]
        });
        assert!(parse_xai_results(&payload, 5).is_empty());
        assert!(parse_xai_results(&json!({}), 5).is_empty());
    }

    #[test]
    fn results_render_as_a_numbered_markdown_list() {
        let rendered = render_results(&[
            SearchResult {
                title: "One".to_string(),
                url: "https://one.example/".to_string(),
                snippet: "first snippet".to_string(),
            },
            SearchResult {
                title: "Two".to_string(),
                url: "https://two.example/".to_string(),
                snippet: String::new(),
            },
        ]);
        assert_eq!(
            rendered,
            "1. [One](https://one.example/)\n   first snippet\n2. [Two](https://two.example/)"
        );
    }

    /// Search results are defanged like fetched pages are.
    ///
    /// Titles, snippets and URLs are written by whoever published the page the
    /// backend indexed, which is anyone. `web_fetch` cleaned its body and these
    /// two tools — the same text one hop earlier — did not, so a CSI in a
    /// result title went into the transcript, the session JSONL and
    /// `stream-json` untouched.
    #[test]
    fn rendered_results_cannot_carry_escapes_or_invisible_text() {
        let rendered = render_results(&[SearchResult {
            title: "Clean\u{1b}[2J title".to_string(),
            url: "https://evil.example/\u{202e}gnp.exe".to_string(),
            snippet: "snippet\u{1b}]0;pwned\u{7} tail\u{200b}".to_string(),
        }]);
        assert!(
            !rendered
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t'),
            "a control character survived: {rendered:?}"
        );
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(!rendered.contains('\u{200b}'), "{rendered:?}");
        assert!(rendered.contains("title"), "{rendered:?}");
    }

    /// An error body is page content too, and the easiest response to control.
    #[tokio::test]
    async fn an_http_error_body_cannot_carry_escapes_either() {
        let body = "gone\u{1b}[2Jaway\u{202e}";
        let addr = serve(format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/missing") }),
                &local_ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            !out.content
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t'),
            "a control character survived: {:?}",
            out.content
        );
        assert!(!out.content.contains('\u{202e}'), "{}", out.content);
        assert!(out.content.contains("HTTP 404"), "{}", out.content);
    }

    #[tokio::test]
    async fn search_rejects_empty_queries() {
        let err = WebSearchTool
            .execute(json!({ "query": "  " }), &local_ctx())
            .await
            .expect_err("empty query");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "web_search"));
    }

    #[tokio::test]
    async fn search_unknown_backend_is_a_tool_error_without_network() {
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            search_backend: "askjeeves".to_string(),
            ..WebConfig::default()
        });
        let out = WebSearchTool
            .execute(json!({ "query": "rust" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .contains("unknown [web] search_backend 'askjeeves'"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn search_keyed_backends_require_a_key_env() {
        // No env var configured at all.
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            search_backend: "brave".to_string(),
            search_api_key_env: None,
            ..WebConfig::default()
        });
        let out = WebSearchTool
            .execute(json!({ "query": "rust" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("search_api_key_env"),
            "{}",
            out.content
        );

        // Env var configured but unset in the environment.
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            search_backend: "tavily".to_string(),
            search_api_key_env: Some("WIZARD_TEST_KEY_THAT_DOES_NOT_EXIST".to_string()),
            ..WebConfig::default()
        });
        let out = WebSearchTool
            .execute(json!({ "query": "rust" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("WIZARD_TEST_KEY_THAT_DOES_NOT_EXIST"),
            "{}",
            out.content
        );
    }

    // -- x_search -------------------------------------------------------------

    #[test]
    fn x_search_request_body_uses_x_search_tool_and_options() {
        let backend = XaiSearch::api_key("test-key")
            .with_server_tool(XaiServerTool::XSearch)
            .with_tool_options(json!({
                "allowed_x_handles": ["xai"],
                "from_date": "2026-01-01"
            }));
        let body = backend.request_body(&backend.model, "status of grok", 3);
        assert_eq!(body["tools"][0]["type"], "x_search");
        assert_eq!(body["tools"][0]["allowed_x_handles"], json!(["xai"]));
        assert_eq!(body["tools"][0]["from_date"], "2026-01-01");
        let prompt = body["input"][0]["content"].as_str().expect("prompt");
        assert!(prompt.contains("x_search"), "{prompt}");
        assert!(prompt.contains("status of grok"), "{prompt}");
        assert!(prompt.contains("at most 3 results"), "{prompt}");
    }

    #[test]
    fn web_search_request_body_still_uses_web_search_tool() {
        let backend = XaiSearch::api_key("test-key");
        let body = backend.request_body(&backend.model, "rust", 5);
        assert_eq!(body["tools"][0]["type"], "web_search");
        let prompt = body["input"][0]["content"].as_str().expect("prompt");
        assert!(prompt.contains("web_search"), "{prompt}");
    }

    #[test]
    fn xai_search_defaults_to_the_fast_model() {
        // Searching on the flagship reasoning model is several times slower
        // for the same links, so the default must stay the fast one.
        assert_eq!(XaiSearch::api_key("test-key").model, XAI_SEARCH_MODEL);
        assert_ne!(XAI_SEARCH_MODEL, xai_oauth::DEFAULT_MODEL);
    }

    #[test]
    fn search_model_config_overrides_the_default() {
        let backend = XaiSearch::api_key("test-key").with_model_override(Some("grok-4.6"));
        assert_eq!(backend.model, "grok-4.6");
        let body = backend.request_body(&backend.model, "rust", 5);
        assert_eq!(body["model"], "grok-4.6");
    }

    #[test]
    fn blank_search_model_keeps_the_default() {
        let backend = XaiSearch::api_key("test-key").with_model_override(None);
        assert_eq!(backend.model, XAI_SEARCH_MODEL);
    }

    #[tokio::test]
    async fn a_retired_default_model_falls_back_to_the_flagship() {
        let envelope =
            r#"{"results":[{"title":"Rust","url":"https://rust-lang.org","description":"lang"}]}"#;
        let ok = json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": envelope, "annotations": [] }]
            }]
        })
        .to_string();
        // First request 404s (model gone), second succeeds.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let addr = serve_recording(recorded, move |calls| {
            if calls == 1 {
                http_response_status(404, "application/json", r#"{"error":"model not found"}"#)
            } else {
                http_response_status(200, "application/json", &ok)
            }
        })
        .await;
        let backend = XaiSearch::api_key("test-key").with_base_url(format!("http://{addr}"));
        let results = backend.search("rust", 5).await.expect("search ok");
        assert_eq!(results.len(), 1);
        let bodies = seen.lock().expect("lock");
        assert_eq!(bodies.len(), 2, "expected one retry");
        assert!(bodies[0].contains(XAI_SEARCH_MODEL), "{}", bodies[0]);
        assert!(
            bodies[1].contains(xai_oauth::DEFAULT_MODEL),
            "{}",
            bodies[1]
        );
    }

    #[tokio::test]
    async fn x_search_backend_extracts_the_json_envelope() {
        let envelope = r#"{"results":[{"title":"@xai","url":"https://x.com/xai/status/1","description":"post"}]}"#;
        let body = json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": envelope, "annotations": [] }]
            }]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = XaiSearch::api_key("test-key")
            .with_server_tool(XaiServerTool::XSearch)
            .with_base_url(format!("http://{addr}"));
        let results = backend.search("status", 5).await.expect("search ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "@xai");
        assert_eq!(results[0].url, "https://x.com/xai/status/1");
        assert_eq!(results[0].snippet, "post");
    }

    #[tokio::test]
    async fn x_search_rejects_empty_queries() {
        let err = XSearchTool
            .execute(json!({ "query": "  " }), &local_ctx())
            .await
            .expect_err("empty query");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "x_search"));
    }

    #[test]
    fn x_search_rejects_both_handle_filters() {
        let err = XSearchTool::tool_options(&XSearchArgs {
            query: "rust".to_string(),
            count: None,
            allowed_x_handles: Some(vec!["a".to_string()]),
            excluded_x_handles: Some(vec!["b".to_string()]),
            from_date: None,
            to_date: None,
        })
        .expect_err("mutually exclusive");
        assert!(err.contains("cannot be set together"), "{err}");
    }

    #[test]
    fn x_search_normalizes_handles_and_strips_at() {
        let options = XSearchTool::tool_options(&XSearchArgs {
            query: "rust".to_string(),
            count: None,
            allowed_x_handles: Some(vec![
                "@xai".to_string(),
                "  ".to_string(),
                "grok".to_string(),
            ]),
            excluded_x_handles: None,
            from_date: Some("2026-06-01".to_string()),
            to_date: Some("".to_string()),
        })
        .expect("ok");
        assert_eq!(options["allowed_x_handles"], json!(["xai", "grok"]));
        assert_eq!(options["from_date"], json!("2026-06-01"));
        assert!(options.get("to_date").is_none());
        assert!(options.get("excluded_x_handles").is_none());
    }

    #[test]
    fn x_search_tool_metadata_is_read_only() {
        assert_eq!(XSearchTool.name(), "x_search");
        assert_eq!(XSearchTool.access(), ToolAccess::ReadOnly);
        assert!(XSearchTool.description().contains("X"));
        assert_eq!(XSearchTool.parameters()["required"], json!(["query"]));
    }

    /// A search response past the cap is an error, not a short read.
    ///
    /// The fetch path has honoured `[web] fetch_max_bytes` since it was
    /// written; this path read to EOF, because `.text()` and `.json()` do and
    /// nobody asked how long a search reply could be. Three of the five
    /// backends point at an operator-supplied `base_url` and the DuckDuckGo one
    /// parses whatever HTML comes back, so "it is a reply to a request we made"
    /// was never a bound.
    ///
    /// Erroring rather than truncating for the reason
    /// [`send_following_redirects`] gives about silence: a truncated search
    /// page parses to fewer results, or none, and reports success.
    #[tokio::test]
    async fn a_search_response_past_the_cap_is_refused_rather_than_truncated() {
        let body = "x".repeat(SEARCH_MAX_BYTES + 1);
        let addr = serve(http_response("text/html", &body)).await;
        let backend = DuckDuckGoHtml::with_base_url(format!("http://{addr}/html/"));
        let err = backend.search("rust", 5).await.expect_err("past the cap");
        assert!(
            err.to_string().contains("exceeded"),
            "the cap must say so: {err}"
        );
    }

    /// The search chain gets the same wall-clock budget the fetch chain does.
    ///
    /// Separate from the fetch-side test because this is a second walker —
    /// [`send_following_redirects`] rebuilds the request per hop so the query
    /// can be de-duplicated — and a budget added to one and not the other is
    /// exactly the drift two walkers invite.
    #[tokio::test]
    async fn a_search_redirect_chain_is_bounded_in_wall_clock() {
        let hop = Duration::from_millis(120);
        let addr = serve_slow_search_redirect_loop(hop).await;
        let client = web_client().expect("client");
        let url = reqwest::Url::parse(&format!("http://{addr}/html/")).expect("url");

        let began = std::time::Instant::now();
        let err =
            send_following_redirects(&client, url, Duration::from_millis(250), |client, url| {
                client.get(url).query(&[("q", "rust")])
            })
            .await
            .expect_err("the walk must end on its budget");
        let elapsed = began.elapsed();

        assert!(err.to_string().contains("did not answer within"), "{err}");
        assert!(
            elapsed < hop * (MAX_REDIRECTS as u32),
            "the walk ran for {elapsed:?}, which is the whole chain rather than the budget"
        );
    }

    /// A search endpoint that redirects to itself forever, `delay` per hop.
    /// Same host every time, so [`hop_stays_home`] keeps following it.
    async fn serve_slow_search_redirect_loop(delay: Duration) -> SocketAddr {
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
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/html/\r\n\
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
