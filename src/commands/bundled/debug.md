---
description: diagnose a problem from Wizard's own log
---
Diagnose a problem with this Wizard session. What the user sees: $ARGUMENTS

Read the newest files in ~/.wizard/logs (see docs/logging.md; `WIZARD_LOG=debug` makes them more verbose for the next run), then `wizard doctor` output if the problem looks like setup. Find the first error or warning that explains the symptom rather than the loudest one. Report what failed, the log lines that show it, and the smallest change that fixes it. Do not edit anything until the user agrees.
