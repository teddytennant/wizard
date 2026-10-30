---
description: fetch and summarize the review comments on a pull request
---
Fetch the comments on a pull request and summarize what still needs doing.

Pull request: $ARGUMENTS (when empty, the one for the current branch: `gh pr view --json number,url`).

Use `gh pr view <n> --comments` for the conversation and `gh api repos/{owner}/{repo}/pulls/<n>/comments` for inline review comments, plus `gh api repos/{owner}/{repo}/pulls/<n>/reviews` for review verdicts. Skip resolved threads and outdated ones that no longer apply to the code.

List each open item as: author, file:line, what they ask for, and whether the code already does it. Do not change any files or reply on GitHub.
