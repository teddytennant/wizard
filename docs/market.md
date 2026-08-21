# Fork and distribute

Wizard is a self-owning agent: when it modifies its own source (a deep evolve, tier 2), you can publish that variant as a GitHub fork under your account and hand anyone a one-line command that installs your Wizard.

---

## The flow

Deep evolve → publish:

1. `/evolve --deep` proposes and builds a change to Wizard's own Rust source. The source lives at `~/.wizard/src` (cloned from the repo on first use; committed by each deep evolve). Full walkthrough in [docs/evolve.md](evolve.md).

2. After a successful deep evolve, or any time you want to share the version currently at `~/.wizard/src`, run `/publish` in the TUI, call the `publish` tool in a prompt, or run `wizard --publish` from the shell.

3. Wizard forks `teddytennant/wizard` to your GitHub account (or reuses an existing fork), pushes the committed source from `~/.wizard/src` to a branch on the fork (default branch: `main`), and prints the install one-liner for your fork.

4. Anyone who runs that one-liner gets your Wizard, built from your source, installed as the `wizard` binary.

---

## Prerequisites

Publish requires the GitHub CLI (`gh`) installed and authenticated:

```bash
gh auth login
```

Wizard checks `gh auth status` before doing anything and tells you what to fix if authentication is missing. It never invents or stores credentials.

---

## The install one-liner

```
curl -fsSL https://raw.githubusercontent.com/<owner>/wizard/<ref>/install.sh | WIZARD_REPO=<owner>/wizard WIZARD_REF=<ref> WIZARD_BUILD_FROM_SOURCE=1 bash
```

`/publish` prints this line with `<owner>` and `<ref>` filled in.

| Env var | Default | Meaning |
|---------|---------|---------|
| `WIZARD_REPO` | `teddytennant/wizard` | GitHub repo to install from, as `owner/repo`. |
| `WIZARD_REF` | `main` | Branch or tag to clone and build. |
| `WIZARD_BUILD_FROM_SOURCE` | `0` | Set to `1` to build the binary from source instead of downloading a release asset. Fork installers always set this to `1`, since forks don't ship prebuilt release binaries unless you cut releases yourself. |

The installer clones your fork at `WIZARD_REF`, ensures a Rust toolchain (installs via `rustup --profile minimal` if `cargo` is absent), runs `cargo build --release`, and places the resulting binary. It works on any machine with internet access and a supported OS (Linux and macOS, x86_64 and aarch64). Build time is a few minutes the first time.

---

## What the recipient installs

Running your one-liner installs:

- **Your source code**: the Rust that came out of your deep evolve, committed at `~/.wizard/src`.
- **Your WIZARD.md charter**: the behavioral charter ([WIZARD.md](../WIZARD.md) at the repo root) that governs how Wizard behaves. It is compiled into the binary, and a generated digest of it (the ladder's rung names, an index of section topic ids, and the rules that must hold on every reply) goes into every system prompt, with the full text of each section served on demand by the `manual` tool. Edit it and your fork ships your copy: the digest and the manual pages are both generated from whatever your `WIZARD.md` says, including sections you add or renumber.
- **Your defaults**: any configuration baked into the source.

Tier-1 evolutions (skills, MCP server registrations, scripted tools, subagents) live under `~/.wizard/` on your machine and are not pushed by `/publish`. Publish is for source changes only.

---

## Gated and logged

Publish is logged like deep evolve, and both run `/publish` directly with no approval gate. Genie narrates the fork target, branch, and source commit as it proceeds; sovereign publishes as part of its unattended flow.

Every publication is appended to `~/.wizard/evolution.jsonl` alongside deep-evolve records, with the fork repo, branch, and the short commit SHA that was pushed.

---

## Amending your charter

`WIZARD.md` at the repo root is Wizard's operating charter. A fork inherits the upstream charter and may amend it:

```bash
# Edit the charter in your source checkout
$EDITOR ~/.wizard/src/WIZARD.md
```

Then rebuild and push:

```
> /evolve --deep rebuild with the updated charter
> /publish
```

See [WIZARD.md](../WIZARD.md) for the current charter.

---

## When to publish

Skills, MCP servers, scripted tools, and subagents are runtime additions that do not touch Wizard's source, so `/publish` will not do anything useful with them alone. To share a single skill or tool, use the registry below instead.

Reach for `/publish` when the change is in `~/.wizard/src`: a new built-in tool, a protocol change, a TUI feature, an amended charter.

---

# The skills and tools registry

`/publish` shares a whole Wizard. `wizard skills` shares one piece of one: a skill (markdown listed in the system-prompt index, body read when it matches) or a tool (a LuaJIT script the model can call).

The registry is a git-backed static site. This repo holds it under `registry/`: `registry/registry.json` plus a directory per entry, and nothing else. No backend, no database, no accounts. Submitting is a pull request to this repo; CI validates the manifest, checks the artifact checksum, and refuses a `registry.json` that has drifted from the tree.

Stock `wizard skills search` fetches `https://raw.githubusercontent.com/teddytennant/wizard/main/registry/registry.json`. Point `WIZARD_REGISTRY_URL` at a different index (any URL that serves `registry.json`, a fork's raw URL included) to use that one instead. The env var names the directory that holds the file, not the file itself.

A skill lives at `registry/skills/<author>/<name>/` (`SKILL.md` + `manifest.toml`); a tool at `registry/tools/<author>/<name>/`. After editing, run `contrib/check-registry.py --write` so `registry.json` matches the tree, then open the PR.

```bash
wizard skills search todo list        # every term has to match; extra terms narrow
wizard skills search todo --tools     # or --skills, to resolve one kind only
wizard skills install slugify
wizard skills list                    # what is installed, and what each one was granted
wizard skills update                  # or `wizard skills update <name>`
```

Installs land beside the skills that ship inside the binary:

| Kind | Where it lands | Receipt |
|------|----------------|---------|
| skill | `~/.wizard/skills/<name>/SKILL.md` | `~/.wizard/skills/<name>/.registry.json` (hidden, so the skills loader never sees it) |
| tool | `~/.wizard/tools/<name>.lua` + `<name>.toml` | `~/.wizard/tools/<name>.registry.json` |

The receipt records the author, version, checksum, source URL and what the install was granted. It is the "where did this come from" listing, and for a tool it is also what the runtime reads on every call. There is no central list: deleting an install deletes its record with it.

The index is cached under `~/.wizard/registry`, so `search` keeps working offline once you have fetched it once. Set `WIZARD_REGISTRY_URL` to point at a different registry.

## Installing a tool is running its author's code

This is the part worth reading twice.

`mlua`'s `StdLib::ALL_SAFE`, which every scripted tool ran under before the registry existed, excludes `debug` and `ffi` but keeps `os` and `io`. `os.execute` is a shell. "Safe" there means "cannot corrupt the VM", never "cannot run commands". So Wizard splits the difference rather than picking a side:

1. **Sandboxed by default.** A registry-installed tool gets `table`, `string`, `math`, `bit` and `jit`, and nothing else: no `os`, no `io`, no `package`, no `dofile`/`loadfile`, and the host file helpers confined to the project directory. Fewer tools are expressible. That is the price.

2. **The full stdlib only by informed opt-in.** A manifest may declare capabilities (`process`, `filesystem`). Installing such a tool **refuses by default**. It succeeds only after Wizard prints the author, the version, the source URL, the sha256 and what is being handed over, and a human answers yes:

```
Installing the tool 'deploy' version 1.2.0
  author:   alice
  source:   https://.../tools/alice/deploy/tool.lua
  sha256:   9f2c…
  asks to:
    - run commands with your privileges (os.execute, io.popen, os.getenv)

Granting this runs the author's code on your machine with your privileges, under the
full LuaJIT standard library.
Wizard cannot narrow it to the list above: `os` and `io` arrive as whole tables, so the
grant is all or nothing.
Without the grant this tool installs sandboxed and its declared capabilities will not work.
Read the source at the URL above first.

Install and grant the full standard library? [y/N]
```

Anything but an explicit `y` is a no, end of input included. Piped into a script or run under CI there is no terminal to ask on, and the install refuses rather than guessing.

`--grant-full-stdlib` gives that answer up front. It is spelled out rather than called `--yes` on purpose: a flag read back out of a shell history a month later has to say what it accepted. It still prints the grant, so what was handed over is on the screen and not only in the flag.

Refusing beats installing sandboxed anyway. A tool that declared it needs `os.execute` and got a VM without it fails somewhere in the middle of a task with a Lua error, and the user learns nothing about why.

Locally authored tools, everything `/evolve` writes and everything you drop in `~/.wizard/tools/` yourself, are untouched by all of this and keep the full stdlib. Their author is you.

## What the registry may not do

- **Take a built-in's name.** `ToolRegistry::register` replaces by name and scripted tools register last, so a registry tool called `execute` or `manual` would become the thing the model reaches when it means the built-in. Those names are refused, read from the native registry itself so the list cannot drift. The two bundled skills (`coding`, `evolve`) are reserved the same way.
- **Overwrite something you wrote.** A local skill or tool of the same name stops the install. Rename or remove yours first.
- **Change hands quietly.** An entry installed from one author and published by another is never updated automatically: `wizard skills update` reports it and leaves the old version alone. A name changing hands is how a supply chain gets taken over.
- **Get a new version on an old grant.** An install holding the full stdlib is not silently replaced either. The grant covered the code the user read, not whatever the author has pushed since, so `update` reports it and waits to be asked again.

`wizard skills update` exits non-zero only when an update genuinely failed. "Up to date", "no longer published", "author changed" and "needs consent" are decisions it made on purpose and reported; an exit code that cries wolf at those teaches people to ignore it.
