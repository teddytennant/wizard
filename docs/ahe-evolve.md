# Driving AHE's evolve loop (`wizard evolve` / `/evolve`)

Wizard can launch and monitor [Agentic Harness Engineering](https://github.com/)'s
**real** harness-evolution loop (`evolve.py`). Wizard does not reimplement it —
it shells out to AHE's own `scripts/evolve.sh`, which runs `python evolve.py`
inside a detached `tmux` session. AHE owns all of its own configuration.

## 1. Point Wizard at an AHE checkout

Add an `[evolve]` section to `~/.wizard/config.toml`:

```toml
[evolve]
ahe_repo = "/path/to/agentic-harness-engineering"
# Optional — path relative to ahe_repo (or absolute). Defaults to:
experiment_config = "configs/experiments/exp-simple-code-gpt54.yaml"
```

Evolve is **off** until this section with a valid `ahe_repo` is present.

## 2. Give AHE its own credentials

Wizard supplies none of AHE's keys — AHE reads them from **its own**
`<ahe_repo>/.env` and `<ahe_repo>/configs/`. To actually run a loop you need:

- an **E2B** account → `E2B_API_KEY` (sandboxed execution)
- a **GitHub token** → `GITHUB_TOKEN`
- **LLM keys** → `LLM_API_KEY`, `LLM_BASE_URL`, …
- a **dataset** for the experiment config you select

`wizard evolve start` preflights for `scripts/evolve.sh`, `evolve.py`, `.env`,
and the experiment config, and reports anything missing before launching.

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
