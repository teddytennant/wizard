# Web tools

Three tools give the agent network research access: `web_fetch` (read a page), `web_search` (query a search engine), and `x_search` (search X / Twitter via xAI). All are read-only, so they stay available in plan mode. Settings for fetch and web search live in the `[web]` section of `~/.wizard/config.toml`; `x_search` always uses xAI credentials and does not use `search_backend`.

They are a plugin (`--features tool-web`, on by default). A build without it
has no `web_fetch`, no `web_search` and no `x_search`, and the model is not
told about them — an absent tool is absent from the roster rather than
advertised and broken. What stays in every build is the plumbing underneath:
the HTTP client, the SSRF guard below, the hand-walked redirect chain and the
body cap are `src/tools/http.rs`, because a Lua plugin's `wizard.http` and
`generate_image`'s downloader use them too. `[web]` in `config.toml` stays as
well, for the same reason: `allow_local` and `fetch_max_bytes` are promises
about what the process does on the network, not settings for one tool. See
[plugins.md](plugins.md).

## web_fetch

Fetch a URL over HTTP(S) and return its content.

- **Arguments:** `url` (required), `max_bytes` (optional cap on response bytes read; clamped to the config cap)
- HTML pages are converted to markdown; other text content types (plain text, JSON, XML, ...) are returned as-is; binary content is summarized, not dumped
- Conversion keeps the readable content: the `<main>`/`<article>` region when the page marks one, with script/style/nav/footer chrome and image syntax stripped
- A JavaScript bot-challenge interstitial returns a one-line error instead of challenge markup. It is detected by marker (`_cf_chl_opt`, `cf-browser-verification`, Cloudflare's "Attention Required!", "Checking if the site connection is secure", or "Just a moment" *together with* "enable JavaScript and cookies") and only when the converted page is under 2 000 bytes, so a real article that happens to quote one of those strings is not thrown away
- Sends a desktop browser user agent, follows redirects (max 10), 10-second connect timeout and a 30-second overall timeout. The 30 seconds is the budget for the *whole* redirect chain, not per hop: a reqwest timeout is per request, so ten hops used to be able to run for five minutes under a nominal thirty seconds, which a hostile server controls both halves of
- The response body is read up to `fetch_max_bytes` (default 100 000) and marked when capped

### SSRF guard

By default, `web_fetch` refuses to touch the local network. A request is rejected when its host:

- is a literal address outside the routable public internet. For IPv4: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10` (carrier-grade NAT — where Alibaba Cloud's metadata endpoint and most Kubernetes pod/service CIDRs live), `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `224.0.0.0/4` and `240.0.0.0/4` (which is where `255.255.255.255` sits). For IPv6: `::`, `::1`, `fc00::/7`, `fe80::/10`, `ff00::/8`, and the whole NAT64 allocation `64:ff9b::/32`, which holds the well-known prefix `64:ff9b::/96` and the local-use `64:ff9b:1::/48`
- carries an IPv4 address inside an IPv6 one — the mapped form `::ffff:127.0.0.1` and the deprecated compatible form `::7f00:1` alike — and that IPv4 address is blocked by the list above
- is `localhost` or a `*.local` mDNS name
- resolves via DNS to any of the above ranges

Redirects get the **same** check, hop by hop, DNS resolution included. reqwest's redirect policy is a synchronous callback and cannot resolve anything, so the client is told not to redirect at all and the chain is walked by hand instead — one hop used to be enough to reach a link-local metadata endpoint through a hostname that resolved there. What is left is the rebinding race between this resolution and the connector's, which no userspace check can close. Non-`http(s)` schemes are always rejected, `allow_local` or not. To fetch from your own LAN or a local dev server, set `allow_local = true`.

## web_search

Query a search backend and return a numbered markdown list of results (title, url, snippet).

- **Arguments:** `query` (required), `count` (optional, default 5, clamped to 1–10)

The default backend is `duckduckgo`, which needs no key, so `web_search` works on a fresh install with no `[web]` section at all. The **keyed** backends are what needs configuring: pick one, and paste an API key, during onboarding or any time via **`/settings` → Web search backend**. The picker writes `search_backend` to config and stores pasted keys in `~/.wizard/credentials.toml` (0600). Backends, selected by `search_backend` (case-insensitive):

| Backend | Key needed | How |
|---------|-----------|-----|
| `duckduckgo` (default) | none | scrapes the DuckDuckGo HTML endpoint |
| `brave` | yes | Brave Search API (`X-Subscription-Token`) |
| `tavily` | yes | Tavily Search API |
| `exa` | yes | Exa Search API (`x-api-key`) |
| `serper` | yes | Serper (Google) Search API (`X-API-KEY`) |
| `xai` / `grok` | sign-in or key | xAI Grok web search via the Responses API server-side `web_search` tool |

A key pasted via `/settings`/onboarding is stored under the backend name in `~/.wizard/credentials.toml` and read at call time. As a fallback (e.g. CI), `search_api_key_env` may name an environment variable holding the key instead; a stored key takes precedence.

Search endpoints get their redirects walked by hand too, since these clients follow nothing on their own. A redirect that stays on the configured host (and does not downgrade `https` to `http`) is followed with the request replayed as-is; one that leaves the host is an error, so an API key in a header or a request body is never handed to a host you did not configure. Either way a `3xx` cannot be mistaken for a page with no results in it. The whole chain shares one 30-second budget, and the reply is read up to 2 MB and *refused* past that rather than truncated — a truncated search page parses to fewer results, or none, and would otherwise report success.

### xAI Grok web search

The `xai` backend runs Grok's own server-side search-and-browse loop (the same mechanism as in the Grok app) and returns the synthesized results. It authenticates with the xAI OAuth session created by `wizard --login xai` / `/login xai` (the same credentials as the `xai-oauth` provider). **If you are already signed in, selecting xAI for web search reuses that session; it does not ask you to authenticate again.** If you have not signed in, it falls back to a stored key or `XAI_API_KEY`.

The search runs on a fast non-reasoning Grok, not the flagship chat model. Searching is a fetch-and-format job, and the flagship model spends most of the wall clock reasoning about a list of links for the same hits, so a typical query lands in a few seconds instead of twenty. Set `[web] search_model` to pin a different model. The default is a pinned snapshot; if xAI retires it, the search retries once on the flagship model rather than failing. The request timeout is 45 s.

## x_search

Search X (formerly Twitter) via xAI Grok's server-side `x_search` tool on the Responses API. Prefer this over `web_search` when you want live posts, handles, threads, or discussion on X.

- **Arguments:**
  - `query` (required) — keywords, topic, or handle context
  - `count` (optional, default 5, clamped to 1–10)
  - `allowed_x_handles` (optional) — only posts from these handles (max 20 after blanks are dropped; leading `@` is stripped)
  - `excluded_x_handles` (optional) — exclude these handles (same limit; cannot combine with `allowed_x_handles`)
  - `from_date` / `to_date` (optional) — passed through to xAI as-is; give them as `YYYY-MM-DD`. Wizard does not validate the format, so a malformed date fails at the API, not here
- **Auth:** same as xAI web search — OAuth from `/login xai` first, then a stored `xai` key, then `XAI_API_KEY` / `[web] search_api_key_env`. Independent of `search_backend`.
- **Timeout:** 45 s (server-side search loop). Runs on the same fast model as xAI web search, and honors `[web] search_model` too.

No extra config is required beyond xAI sign-in or an API key. Results render as the same numbered title/url/snippet list as `web_search`.

## Configuration

```toml
[web]
fetch_max_bytes = 100000          # cap on web_fetch response bytes (default 100000)
allow_local = false               # permit localhost/private-range fetches (default false)
search_backend = "duckduckgo"     # duckduckgo | brave | tavily | exa | serper | xai
search_api_key_env = "BRAVE_API_KEY"  # optional env-var fallback when no key was pasted
search_model = "grok-4.6"         # xai backend only: model that runs the search (default: a fast non-reasoning Grok)
```

Every key is optional; a missing `[web]` section means the defaults above. Prefer `/settings` over editing this by hand: it also handles the API key.
