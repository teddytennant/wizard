---
description: review the pending changes on this branch for security problems
---
Review the changes on the current branch for security problems.

1. Run `git status` and find the base: `git symbolic-ref refs/remotes/origin/HEAD`, falling back to `main` or `master`. Read `git diff <base>...HEAD` and the uncommitted diff, then open the changed files in full where the diff alone does not show the data flow.
2. Look for flaws a reviewer would act on: injection (SQL, shell, template, path), broken authentication or authorization, secrets in code or logs, unsafe deserialization, SSRF, path traversal, missing validation at a trust boundary, weak cryptography, race conditions with security impact, and dependency changes that pull in something risky.
3. For each candidate, trace it from untrusted input to the dangerous sink. Drop it if you cannot name that path.

Report each real finding as: file:line, what an attacker does, what they gain, and the smallest fix. Order by severity. If you find nothing, say so and name what you checked. Do not report style, hardening wishes, or theoretical issues without a path.

$ARGUMENTS
