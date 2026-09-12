# Images

## Showing the model an image

Three ways in, all of them landing on the same `Image` block on a user message:

- **Paste** into the TUI. A `data:image/...` URL, one or more image paths, or an
  image sitting on the OS clipboard (terminals cannot deliver image bytes through
  bracketed paste, so Wizard reads the clipboard itself). The composer shows
  `[Image #1]` and the file goes with your next message.
- **`@path/to/shot.png`** in a message, on any surface, headless included. The text
  keeps a short `[image: shot.png]` placeholder and the file rides alongside.
- **`read_file`**, which is how the agent looks at something by itself. A path whose
  first bytes are png, jpeg, gif, webp, bmp or pnm comes back as the image plus a
  line naming the file, its pixel size and its format:

  ```text
  screen.ppm: 1024x768 PNM, 2359349 bytes
  ```

  BMP and PNM are re-encoded as PNG on the way out, because no vision API takes
  either; `qemu screendump` writes a `.ppm`, which is the reason both are here.
  Anything over the 10 MB transport cap is halved until it fits, and the line then
  says what size the model is actually looking at. Detection is on the bytes, not
  the extension. Non-image files read as text exactly as before.

If the model behind your provider is text-only, say so and Wizard stops spending
context on pictures it cannot use:

```toml
[[providers]]
name = "local"
kind = "llamacpp"
vision = false
```

`read_file` on an image then returns the description line and an explanation
instead of the bytes. Unset means yes: every endpoint Wizard speaks to carries
images, and there is no API call that answers "can this model see" without
sending it one.

## Image generation

The native `generate_image` tool creates images from a text prompt via an OpenAI-compatible `POST {base}/images/generations` endpoint. On xAI that is the Imagine API (`https://api.x.ai/v1/images/generations`). The result is always written to a local file so the agent (and you) can open it.

It also comes back *inline*: the image rides on the tool result, so it is rendered in the TUI and the GUI and kept in the session's image store under `~/.wizard/images/<session-id>/`. It is then handed back to the model on a following user message rather than on the tool result itself, because the tool role carries no image blocks — but the model does see what it made. An image too large to carry, or one the decoder rejects, is still written to disk and named, with `(not shown inline: …)` on the result. See [architecture.md](architecture.md#images).

## generate_image

- **Arguments**
  - `prompt` (required) — image description
  - `path` (optional) — save location, relative to the project root or absolute. Default: `generated/<YYYYMMDD-HHMMSS>-<slug>.png` under the project root, local time, the slug taken from the prompt and capped at 48 characters (the directory is created as needed)
  - `model` (optional) — defaults to `grok-imagine-image-quality` when the request goes to xAI (an `xai`/`xaioauth` provider, or an OpenAI/OpenRouter provider whose `base_url` contains `api.x.ai`), and `dall-e-3` otherwise
  - `n` (optional, 1–4, default 1) — number of images; anything outside the range is rejected, not clamped. A single image keeps the plain path; multi-image saves get `-1`, `-2`, … suffixes
  - `aspect_ratio` (optional) — passed through unvalidated; xAI Imagine ratios such as `1:1`, `16:9`, `9:16`, `4:3`, `3:4`, …
  - `resolution` (optional) — `1k` or `2k`; anything else is rejected
  - `response_format` (optional) — `url` (default; download to file) or `b64_json` (decode to file)
- **Access:** execute (network + disk). Not available in plan mode.
- Temporary CDN URLs from `response_format=url` are downloaded over HTTPS only — on every hop, not just the first: a redirect that downgrades to `http://` ends the download with an error rather than fetching the image in cleartext. They are capped at 20 MB and go through `web_fetch`'s own SSRF check: the host is **resolved** and refused if any address it answers with is outside the routable public internet ([the ranges are listed in the web tools guide](web.md#ssrf-guard)), and redirects are followed by hand with the same resolving check run on every hop. What remains is the DNS-rebinding race every userspace check has — the window between this resolution and the connector's.

### Auth resolution

1. If the **active provider** is `xai` / `xaioauth` / `openai` / `openrouter`, use its `base_url` and token (OAuth for `xaioauth`, stored credentials / env key otherwise).
2. Else fall back to an xAI OAuth session (`/login xai`), then a stored `xai` key, then `XAI_API_KEY`, against `https://api.x.ai/v1`.
3. If nothing is configured, the tool returns a clear error asking you to `/login xai` or set a key.

Those two defaults are the only model names Wizard knows; anything else you pass in `model` is sent through untouched, so the provider decides whether it exists.

## Example

Ask the agent to generate an image, or call the tool directly in a turn:

```text
generate a 16:9 concept art of a lighthouse at dusk
```

Files land under `generated/` in the project root unless you pass `path`.
