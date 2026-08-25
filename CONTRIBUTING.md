# Contributing

Thanks for wanting to improve Wizard. Short guide so PRs land cleanly.

## Setup

Rust stable (edition 2024). Clone and build:

```bash
git clone https://github.com/teddytennant/wizard
cd wizard
cargo build --release
./target/release/wizard
```

Or `nix develop` for a shell with the Rust toolchain and `llama-cpp`.

## Before you open a PR

Match what CI runs (see `.github/workflows/ci.yml`):

```bash
contrib/check-file-size.sh
cargo machete
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Required if you touch `src/plugins/native/` — the native GUI is off by default, so
nothing above compiles a line of it, and it ships as its own release asset
(`wizard-native-*`). CI runs both:

```bash
cargo clippy --all-targets --locked --features native -- -D warnings
cargo test --locked --features native
```

Optional supply-chain check:

```bash
cargo deny --locked --all-features check
```

Optional audit, worth a run when you add a `pub fn` that a surface is supposed
to call:

```bash
contrib/find-unwired.py
```

It lists public functions nothing outside a test calls. Most of what it prints
is fine; it exists because the defect it catches cannot fail a test. A function
that is written, documented and unit-tested but never wired up reads as
finished and green while the behaviour its doc describes silently does not
happen — four of those have been found in this tree, and each one was a bug a
user could see. The script's header lists them.

Keep `Cargo.lock` in sync (`--locked` fails on drift). Prefer small, focused diffs that match existing style. No `todo!()` / bare `unwrap()` on fallible paths.

## What to send

- Bug fixes and tests
- Docs that match the code
- Focused features that fit the single-binary design

Open an issue first for large or architectural changes. Security-sensitive reports: see [SECURITY.md](SECURITY.md).

## Behavior and docs

- [WIZARD.md](WIZARD.md) is the agent charter forks inherit; change it only when the behavior change is intentional.
- User-facing docs live under `docs/`. Update them when you change commands, flags, or flows.
- Read [SECURITY.md](SECURITY.md) before changing tools, hooks, MCP, install, or trust boundaries.

## License

By contributing, you agree your work is licensed under the MIT license ([LICENSE-MIT](LICENSE-MIT)). That is unchanged by the crate as a whole being `MIT AND Apache-2.0`: the Apache-2.0 half is the ported terminal-UI code listed in [NOTICE](NOTICE), and new contributions are not part of it. If you are porting code from an Apache-2.0 project, say so in the PR and add it to NOTICE rather than relicensing it.
