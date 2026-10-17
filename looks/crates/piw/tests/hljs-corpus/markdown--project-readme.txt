# fastgrep

Fast, *parallel* **recursive** search. Written in Rust, inspired by `ripgrep` and [ag](https://geoff.greer.fm/ag/ "The Silver Searcher").

![Build status](https://img.shields.io/badge/build-passing-green.svg)
[![crates.io][crate-badge]][crate]

## Installation

```sh
cargo install fastgrep
# or, from source
git clone https://github.com/example/fastgrep && cd fastgrep
cargo build --release
```

## Usage  

```console
$ fg --type rust 'fn\s+main' src/
src/main.rs:12:fn main() {
```

Flags worth knowing:

* `-i`, `--ignore-case`: case-insensitive
* `-w`: match whole words
  * combine with `-i` for *both*
  * `-F` treats the pattern as a literal
    - nested deeper
* `--json`: machine-readable output

1. First step
2. Second step
   1. Sub-step a
   2. Sub-step b
10. Tenth (numbering is cosmetic)

> **Note**
> Windows paths need quoting: `fg "C:\Users\me" .`
>
> > Nested quote with a trailing space.  

---

| Flag | Default | Description |
|------|:-------:|------------:|
| `-j` | `auto`  | threads     |
| `-C` | `0`     | context lines |

Supports UTF-8 text: café, 日本語, emoji 🔎. Press <kbd>Ctrl</kbd>+<kbd>C</kbd> to stop.<br>
Hard break above. Escaped \*stars\* and a footnote[^1].

[crate-badge]: https://img.shields.io/crates/v/fastgrep.svg
[crate]: https://crates.io/crates/fastgrep
[^1]: See the benchmarks in `docs/bench.md`.
