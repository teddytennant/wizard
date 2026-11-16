# Terminal-Bench results

Terminal-Bench 2.1, 89 tasks, k=1, Grok 4.6 through the xAI OAuth session
(`kind = "xai"` pointed at a host token proxy so containers never hold the
grant; see `wizard_agent.py` and `WIZARD_TB_BASE_URL`). Run 2026-09-11 on a
16-core, 31 GB box at `-n 4`, task timeouts as shipped, no multiplier.

| agent | model | resolved of 89 | note |
| --- | --- | --- | --- |
| Wizard | grok-4.6 | 66 (74.2%) | benchmark sources blocked; nothing flagged by the scan |
| Wizard, earlier build | grok-4.6 | 67 (75.3%) | five fetched-answer passes counted as failures |
| Terminus 2, same box | grok-4.6 | 69 (77.5%) | one fetched-answer pass counted as a failure |
| Grok Build 1.0.24, same box | grok-4.6 | 69 (77.5%) | two fetched-answer passes counted as failures |

Public reference for the same model: Terminus 2 at 88.4% (Artificial Analysis,
e2b sandbox). The same-box Terminus 2 run is 10 points under it, so this
machine costs every harness.

Noise. One trial per task does not separate these rows. Three trials each on
the 35 hardest tasks resolved 46.7% against 34.3% for the single trial on the
same tasks; substituting the majority verdict into the table would read 72 of
89, which is why a few points either way means little.

The blocked run. Every public copy of the benchmark was blocked at the web
tools, in the container's hosts file, and in the prompt, so no pass came from
reading a task's own tests: the scan flags nothing. Web tool use fell from 164
fetches and 170 searches across 35 trials to 31 and 16 across 5. Reading
images now works and shows: the agent attached 97 images across 7 trials where
the earlier build's 16 attempts all failed as decode errors, and chess-best-move,
install-windows-3.11 and gcode-to-text flipped to passes on it. Prompt tokens
over comparable trials fell from 37.0M to 29.5M after old tool results started
shrinking.

What still fails. Fifteen timeouts with the agent working, several of which
never wrote the deliverable, and six wrong answers that verified their own
scenario rather than the grader's. Those are what a visible time budget and a
single completion review are meant to catch; neither is built yet. Two tasks
(`qemu-alpine-ssh`, `qemu-startup`) have a verifier whose `apt-get` 404s,
though `qemu-startup` now passes some of the time.

```sh
harbor run -d terminal-bench/terminal-bench-2-1 -a tbench.wizard_agent:WizardAgent \
    -m xai/grok-4.6 -k 1 -n 4
```
