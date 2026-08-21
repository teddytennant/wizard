---
name: conventional-commit
description: Write a conventional commit subject and body from the current diff. One change per commit. Do not invent a scope.
---

# Conventional commits

Write the commit message. Do not run `git commit` unless the user asked.

## What to look at

1. `git_status`, then `git_diff`. Prefer the staged diff. If nothing is staged, use the unstaged work and say so.
2. The repo's own commit style: `git log -15 --oneline` on this branch. Match its type vocabulary and whether it uses scopes.
3. Do not invent files you have not seen. If the diff is empty, say so and stop.

## Subject

```
<type>(<optional scope>): <imperative summary>
```

Default types, unless the repo's history or `CONTRIBUTING` names others: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.

- Imperative, present tense: "add", not "added" or "adds".
- Aim for 50 characters; 72 is the hard stop.
- No trailing period, no issue numbers in the subject.
- Scope is a real area of this tree (`registry`, `gateway`, `tui`), not a mood (`misc`, `stuff`, `update`). Omit the scope rather than guessing.
- A breaking change gets `!` after the type or scope (`feat(api)!: drop the old flag`) and a `BREAKING CHANGE:` footer. Size is not a break.

## Body

Only when the subject cannot carry the why. Wrap at 72. Explain the reason; the diff already shows the edit. No "this commit", no changelog padding, no marketing.

## Footers

- `BREAKING CHANGE: <what a caller must do now>` when a public contract changes.
- `Refs: #123` / `Closes: #123` only when the user named the issue. Do not scrape numbers out of branch names.

## Split

One logical change per commit. If the diff mixes a fix and a refactor, say so and propose the split. Do not write one message that pretends they are one change.

If the user also wants the commit created, use the project's existing commit path (their hook, `git commit`, the repo's helper). Do not skip hooks unless they said to.
