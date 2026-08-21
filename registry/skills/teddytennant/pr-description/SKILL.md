---
name: pr-description
description: Write a pull-request title and body from the current branch against its base. Why, what, how to test. No marketing.
---

# Pull request description

Write the title and body. Do not open, push, or edit the PR unless the user asked.

## What to look at

1. `git_status`. Note the branch, the likely base (`main` or `master`, or whatever this repo uses), and whether anything is still uncommitted.
2. The commits and the diff against that base (`git log <base>..HEAD`, `git_diff` or `git diff <base>...HEAD`). Read both. Describe only what is in them.
3. A PR template if the repo ships one (`.github/pull_request_template.md` and the usual variants). Use its headings. Do not invent a template the tree does not have.

If there are no commits against the base, say so and stop.

## Title

One line, imperative, specific. The first conventional-commit subject on the branch is a starting point, not a requirement. Skip `[WIP]` unless the user said the PR is unfinished.

## Body

Match the repo's voice. When there is no template, use this shape:

```
## Why

<the problem this branch closes. one or two sentences. not a feature list.>

## What

<what the diff does, in the order a reviewer will read it. name files and
behaviour, not effort.>

## Test plan

- [ ] <a command a reviewer can run>
- [ ] <the case that used to fail>
```

Rules:

- Every claim is grounded in the diff or the log. "Improves performance" needs a measurement or it comes out.
- Do not list files the diff does not touch.
- Do not thank reviewers, do not add emoji, do not write a sales paragraph.
- A revert or a mechanical version bump is one sentence, not an essay.

## How to verify

Name the commands a reviewer can run. Not "tested locally". Leave template boxes unchecked unless those commands were actually run.

If the diff includes a secret, say that a secret is present and stop; do not put it in the PR body.
