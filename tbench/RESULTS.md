# Terminal-Bench results

Terminal-Bench 2.1, 89 tasks, k=1, Grok 4.6 through the xAI OAuth session
(`kind = "xai"` pointed at a host token proxy so containers never hold the
grant; see `wizard_agent.py` and `WIZARD_TB_BASE_URL`). Run 2026-09-11 on a
16-core, 31 GB box at `-n 4`, task timeouts as shipped, no multiplier.

| agent | model | resolved | score |
| --- | --- | --- | --- |
| Wizard 3.0.1 (the agent loop 3.1 ships) | grok-4.6 | 72 / 89 | 80.9% |

Two tasks (`qemu-alpine-ssh`, `qemu-startup`) scored 0 because their verifier's
`apt-get` 404s on `debian-security bullseye` and no test runs; excluding them
gives 72 / 87 = 82.8%. The 15 real failures: 8 timeouts with the agent still
working (in four it was writing the deliverable into its reply instead of a
file), 7 wrong answers that ended with a confident "Completed".

The run needed two Harbor jobs (`wizard-full` died of a full disk at 84 of 89;
`wizard-full-2` finished the remaining 9); each task has one scored trial.
Public reference for the same model: Terminus 2 at 88.4% (Artificial Analysis,
e2b sandbox), not reproduced here.

```sh
harbor run -d terminal-bench/terminal-bench-2-1 -a tbench.wizard_agent:WizardAgent \
    -m xai/grok-4.6 -k 1 -n 4
```
