# Driving AHE's evolve loop (`wizard evolve` / `/evolve`)

Wizard can launch and monitor [Agentic Harness Engineering](https://github.com/)'s
**real** harness-evolution loop (`evolve.py`). Wizard does not reimplement it —
it shells out to AHE's own `scripts/evolve.sh`, which runs `python evolve.py`
inside a detached `tmux` session. AHE owns all of its own configuration.

AHE executes each harness on the **local Docker daemon** — there is no cloud
sandbox. The only credentials a run needs are **LLM keys**.

## 1. Point Wizard at an AHE checkout

Add an `[evolve]` section to `~/.wizard/config.toml`:

```toml
[evolve]
ahe_repo = "/path/to/agentic-harness-engineering"
# Optional — path relative to ahe_repo (or absolute). Defaults to the
# fully-local Docker smoke-test experiment:
experiment_config = "configs/experiments/exp-local-sample.yaml"
```

Evolve is **off** until this section with a valid `ahe_repo` is present.

## 2. Prerequisites: Docker + LLM keys (no cloud sandbox)

AHE runs locally, so a loop needs only:

- **Docker** — the `docker` CLI on PATH with a running daemon (`docker ps`
  works). AHE builds and runs each task's container locally.
- **LLM keys** → `LLM_API_KEY`, `LLM_BASE_URL`, `LLM_MODEL`. These can point at
  a **local** `llama-server`/`vllm` endpoint or any OpenAI-compatible API. Put
  them in `<ahe_repo>/.env`, or export them in your environment.
- a **dataset** for the experiment config you select. The default
  `exp-local-sample.yaml` ships its own trivial local dataset
  (`dataset/local-sample`), so it runs with no external data.

No **E2B** account and no **GitHub token** are required — those were only for
the old cloud-sandbox mode (`harbor.env = "e2b"`), which AHE no longer defaults
to.

`wizard evolve start` preflights for `scripts/evolve.sh`, `evolve.py`, a working
`docker`, LLM keys (in `.env` or the environment), and the experiment config,
and reports anything missing before launching.

## 3. Run it

From the CLI:

```
wizard evolve start      # preflight, then launch in a detached tmux session
wizard evolve status     # latest experiment's scores + newest iteration
wizard evolve sessions   # list running ahe-* tmux sessions
wizard evolve stop <s>   # kill a session
wizard evolve attach     # print the `tmux attach -t …` command
```

Or from inside the TUI:

```
/evolve            # same as /evolve start
/evolve start      # launch (runs in the background, posts a notice)
/evolve status     # summarize the latest experiment
```

`evolve.sh` detaches into a session named `ahe-<name>-<timestamp>`. Attach with
`tmux attach -t <session>` to watch live; `Ctrl-b d` detaches again. Progress is
also written to `<ahe_repo>/experiments/<TIMESTAMP>__<name>/`
(`iteration_scores.md`, `evolution_history.md`), which is what `evolve status`
reads.
