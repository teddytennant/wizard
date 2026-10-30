---
description: review a diff, PR number, branch or path for correctness bugs
---
Review code for correctness bugs.

Target: $ARGUMENTS

With no target, review the uncommitted changes and the commits on this branch that are not on the base branch. A number is a pull request (`gh pr diff <n>`); a name is a branch to diff against the base; a path is reviewed as it stands.

Read the diff, then read the surrounding code that the changes call or are called by. Look for logic errors, off-by-one and boundary mistakes, wrong error handling, unchecked assumptions, broken invariants, concurrency problems, resource leaks and tests that do not test what they claim. Run the project's tests or build if that is how you would confirm a suspicion.

Report only findings you are confident in. For each: file:line, what goes wrong and when, and a fix. Say plainly when you find nothing. Do not comment on style or naming.
