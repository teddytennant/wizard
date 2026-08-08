# Terminal-Bench

Runs Wizard against [Terminal-Bench 2.1](https://www.tbench.ai/leaderboard/terminal-bench/2.1)
(89 tasks), the current public benchmark for terminal agents.

The harness is **Harbor**, not the older `tb` CLI — `terminal-bench`/`tb` is
frozen and only serves the retired 1.0 leaderboard. Harbor's custom-agent
contract (`BaseInstalledAgent`) is what `wizard_agent.py` implements.

## Setup

```sh
uv tool install harbor          # provides `harbor`; needs Docker + Python >=3.12
```

Build the binary the benchmark will run. Wizard is a single static binary, so
the adapter uploads this artifact into each task container rather than curling
an installer at benchmark time — the thing we score is byte-identical to the
thing we built, and no task depends on a network fetch mid-run.

The build context is the Wizard checkout you want to score, and it is **not**
this one. This adapter lives on a long-lived branch that does not track Wizard's
development line, so building `.` here compiles whatever Wizard looked like when
the branch was cut. Point the context at the checkout you develop in:

```sh
export WIZARD_TB_SOURCE=~/path/to/wizard      # the checkout to benchmark
docker build -f tbench/Dockerfile.build --target export \
    --output type=local,dest=tbench/dist "$WIZARD_TB_SOURCE"
```

`WIZARD_TB_SOURCE` is also what the adapter checks the binary against at
install time — see [Staleness](#staleness). It defaults to this checkout, which
is almost never what you want.

If the build appears to hang before its first layer, the source checkout is
missing a `.dockerignore`: `target/` runs to tens of gigabytes and Docker sends
the whole context up front. Ignore `target/`, `target-glibc/`, and `.git/`.

The build **must** be done in Docker, not with a host `cargo build`. Two reasons:
task images vary by base distro, so the binary has to be statically linked
(musl + `crt-static`) to exec on all of them; and a `cargo build` on a Nix host
links against Nix's glibc paths, which resolve nowhere inside a task container.
`Dockerfile.build` asserts the result is static and fails if it isn't — a
dynamically-linked binary would run fine in some task containers and die in
others, and Harbor scores those deaths as *agent failures*, not infrastructure
errors.

## Staleness

Pinning a prebuilt artifact is deliberate, but an unchecked pin rots silently.
This adapter shipped for months uploading a Wizard 1.1 binary while the product
moved to 2.0, and nothing in a Harbor run says so: the trials pass or fail
normally and the score describes software nobody is running.

So `install()` verifies the binary against `WIZARD_TB_SOURCE` before uploading
it and raises rather than benchmarking a mismatch. Two checks, because neither
alone is enough — `--version` catches drift across a release, and comparing
mtimes against `src/`, `Cargo.toml`, `Cargo.lock`, and `build.rs` catches edits
within one version, where the version string cannot tell a rebuild from a stale
artifact.

| variable | meaning |
| --- | --- |
| `WIZARD_TB_SOURCE` | Checkout the binary must match. Defaults to this one. |
| `WIZARD_TB_BINARY` | Artifact to upload. Defaults to `tbench/dist/wizard`. |
| `WIZARD_TB_ALLOW_STALE` | Downgrade a mismatch to a warning. |

Set `WIZARD_TB_ALLOW_STALE=1` only when the mismatch is the point — bisecting a
regression, or reproducing an old leaderboard submission. It is not a way past a
failing build.

## Running

Sanity-check the harness itself first. The `oracle` agent replays each task's
reference solution, so anything other than a pass means Docker or the dataset is
broken, not Wizard:

```sh
harbor run -d terminal-bench/terminal-bench-2-1 -a oracle -l 5
```

Then Wizard, on one task:

```sh
export XAI_API_KEY=...
PYTHONPATH="$PWD" harbor run -d terminal-bench/terminal-bench-2-1 \
    -a tbench.wizard_agent:WizardAgent -m xai/grok-4.5 \
    -i terminal-bench/build-cython-ext -k 1
```

Run from the repo root, and set `PYTHONPATH`: `harbor` is an installed console
script, so the current directory is *not* on `sys.path` and `tbench` will not
import without it. Task names are namespaced (`terminal-bench/<task>`); `-l N`
takes the first N instead of naming one.

A scoring run over the full dataset:

```sh
PYTHONPATH="$PWD" harbor run -d terminal-bench/terminal-bench-2-1 \
    -a tbench.wizard_agent:WizardAgent -m xai/grok-4.5 \
    -k 5 -n 8 --max-retries 3 --retry-include ApiRateLimitError
```

`-k` is trials per task, `-n` concurrent trials. `-n` is bounded by RAM, not
CPU — each trial is a container that spends nearly all its wall-clock blocked on
the model API. Budget ~2 GB per concurrent trial.

Retrying rate-limit errors is not optional for a number you intend to trust.
Harbor records an errored trial as **reward 0**, so a provider 429 is
indistinguishable in the final score from Wizard genuinely failing the task.

## Models

`-m <provider>/<model>` selects both. The provider must be one Wizard knows —
`xai`, `anthropic`, `openai`, `openrouter` (see `PROVIDERS` in
`wizard_agent.py`). The adapter writes a matching `~/.wizard/config.toml` into
the container and injects the corresponding API key (`XAI_API_KEY`,
`ANTHROPIC_API_KEY`, ...) from your host environment.

Use an **API key**, not the `wizard --login xai` OAuth session. Every trial is
its own container, and containers sharing one refresh token will race to refresh
it; the resulting auth failures land in the score as task failures. OAuth is
also unreproducible for anyone else trying to verify a result.

## Leaderboard

Submission is a PR flow against `harbor-framework/terminal-bench-2-1`
(`leaderboard/SUBMIT.md`), and it is strict: every task covered, **≥5 trials
each**, no timeout or resource overrides, jobs uploaded publicly to Harbor Hub.
Errored trials count as reward 0 and are not excluded. An LLM judge then reviews
every *successful* trial's trajectory for reward hacking; "harness cheating"
invalidates the whole submission.

That's ≥445 trials, so confirm a full local run looks sane before spending it.
