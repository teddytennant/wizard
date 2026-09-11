# Terminal-Bench results

Terminal-Bench 2.1, 89 tasks, k=1, Grok 4.6 through the xAI OAuth session
(`kind = "xai"` pointed at a host token proxy so containers never hold the
grant; see `wizard_agent.py` and `WIZARD_TB_BASE_URL`). Run 2026-09-11 on a
16-core, 31 GB box at `-n 4`, task timeouts as shipped, no multiplier.

| agent | model | as scored | fetched-answer passes as fails |
| --- | --- | --- | --- |
| Wizard 3.0.1 (the agent loop 3.1 ships) | grok-4.6 | 72 / 89 (80.9%) | 67 / 89 (75.3%) |
| Terminus 2, same box and proxy | grok-4.6 | 70 / 89 (78.7%) | 69 / 89 (77.5%) |

Public reference for the same model: Terminus 2 at 88.4% (Artificial Analysis,
e2b sandbox). The same-box Terminus 2 run is 10 points under it, so this machine
(a slow Ubuntu mirror, four trials at once) costs every harness.

Contamination. A pass is counted as a failure when the agent downloaded that
task's README, reference solution or tests from a public copy of the benchmark.
Wizard did that on torch-tensor-parallelism, torch-pipeline-parallelism,
db-wal-recovery, extract-elf and mteb-leaderboard, all through its `web_fetch`
tool; Terminus 2 did it on db-wal-recovery with Python's urllib. Wizard's default
prompt was also tuned by an earlier harness-evolution pass on 10 of these 89
tasks.

Wizard's misses. Two tasks (`qemu-alpine-ssh`, `qemu-startup`) are unscorable for
every harness: their verifier's `apt-get` 404s on `debian-security bullseye`.
Of the rest, most are timeouts with the agent still working: the model is never
told how much time is left, and code it drafts in its reasoning is not kept, so
several runs ended without writing the deliverable. The others verified against
the task's example rather than the grader's scenario, or needed to read an image,
which `read_file` cannot do yet.

The run needed two Harbor jobs per agent (Wizard's first job died of a full disk
at 84 of 89; Terminus 2's setup failures on the slow mirror were rerun with a
longer install timeout); each task has one scored trial.

```sh
harbor run -d terminal-bench/terminal-bench-2-1 -a tbench.wizard_agent:WizardAgent \
    -m xai/grok-4.6 -k 1 -n 4
```
