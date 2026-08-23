---
name: coding
description: Baseline coding workflow. Explore before editing, make minimal precise diffs, verify with the project's own build and tests, clean deliverable directories, and review the diff before finishing.
---

# Coding guidelines

## Workflow

1. **Orient first.** `list_files` / `search_files` / `read_file` before edits;
   `git_status` for starting state.
2. **Honor project instructions.** `AGENTS.md` / project `WIZARD.md` build and
   style rules override defaults. The user message is the contract — extract
   required paths, formats, IDs, and cleanup constraints before coding.
3. **Plan briefly, then act.** One or two sentences of intent, then change.
   Do not narrate every tool call.
4. **Smallest change.** Prefer `edit_file` with an exact unique `old_string`.
   Do not reformat, rename, or clean up unrelated code.
5. **Verify.** Run the project's commands via `execute` (from project
   instructions, else infer: `cargo test`, `npm test`, `pytest`, `make test`).
6. **Clean deliverable tree.** Remove compile/build byproducts next to required
   sources when the task expects a specific final file set (or build under `/tmp`).
7. **Review before done.** `git_diff`; confirm compile, tests, nothing unintended.

## Spec-driven / report tasks

When the user gives an explicit contract (signatures, flags, exit codes,
JSON/JSONL, CWE IDs, polyglots, exact paths):

1. Checklist (or `todo`) every required output path on turn 1.
2. Implement the general case — not only examples.
3. Vulnerability / CWE reports, in order:
   1. Locate the defect fast (run tests early; search validation helpers).
   2. Minimal fix so invalid input raises the expected error.
   3. Write the report immediately in the demonstration schema.
   4. IDs **only** from the task's listed vocabulary (lowercased `cwe-N`).
      For listed CRLF/header-splitting bugs include `cwe-93` (and `cwe-20` if
      listed). Never only an unlisted synonym like `cwe-113`.
   5. Schema literally: `cwe_id` is a **list** of strings; prefer demo path style.
   6. Re-run tests; `json.loads` + assert list IDs and required tokens.
4. Self-test against the contract before finishing.
5. Never touch files the contract forbids.
6. Confirm every required path exists (`ls`/`cat`) before the final message.

## Polyglots and build-verify

- Compile/run to verify, but write binaries under `/tmp` or delete them from the
  deliverable directory before finishing.
- Final checks often require *only* the named source/output file(s).

## Long-lived services (HTTP, QEMU, daemons)

- Must survive after the agent ends: `nohup ... > log 2>&1 &` (or a small start
  script). Prove liveness with `curl`/`ss`/`pgrep`.
- Do **not** use `execute(run_in_background=true)` for verifier-facing services.
- Keep setup simple once e2e is green.

## Install packages with native extensions

Critical path (timeouts fail — avoid inventory greps after green):

1. Small compatibility-fix batch (Numpy 2 aliases, `fractions.gcd`→`math.gcd`,
   soft-import optional viz, `int()` on size math).
2. Immediately `python3 setup.py build_ext --inplace && python3 setup.py install`
   (or equivalent that puts `.so` in site-packages).
3. Verify from `cd /tmp` so local source cannot mask a bad install; run the
   required snippet.
4. If `pip install .` yields pure-Python with no `.so`, fall back to setup.py.
5. Run the **allowed** test suite next (exclude only task-marked broken tests).
6. Fix the first real failure (often third-party key renames → dual-key `.get`),
   reinstall, re-run the same suite.
7. Reinstall after every later source fix. Once snippet + allowed tests pass, **stop**.

## Image / board / puzzle analysis

- No full pixel grids, IoU matrices, or huge feature dumps into chat.
- At most **two** compact scripts that print summaries only.
- Chess/mate puzzles: install `python-chess` / pillow / numpy / stockfish early;
  occupancy+color → light piece features → valid FEN → exhaustive mate-in-1.
  **Same-script write** of every mate UCI when found. If empty, one hypothesis
  script swapping ambiguous N/B/Q/R/P only; write best non-empty set inside it.
  Stop once a verified non-empty answer is on disk.

## Editing rules

- `read_file` immediately before every `edit_file`; match whitespace exactly.
- On missing/ambiguous `old_string`, re-read and retry with more context — never guess.
- Never fabricate unread file contents. Match local style. No placeholder stubs
  or `TODO` in finished work.

## Shell usage

- `execute` is `sh -c` in the project root. Non-interactive flags; no commands
  that wait for input. It waits 30s by default, then moves the command to a
  background task instead of killing it — carry on and read the notification,
  or `task_output(id, wait_secs=N)` when you need the result now. Pass
  `timeout_secs` (up to 600) when you would rather wait inline.
- Prefer summaries over megabyte dumps; large intermediates go under `/tmp`.
- No destructive commands (`rm -rf`, hard reset, force push, drop DB) unless
  explicitly asked — except routine cleanup of your own build byproducts.
- Do not commit or push unless asked.

## When things go wrong

- Read the first error fully before reacting.
- Same approach fails twice → change strategy (more context, different tool, ask).
- Report failures honestly with what you tried. Never claim tests passed unrun.
