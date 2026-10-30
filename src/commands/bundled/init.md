---
description: write the project's instruction file (AGENTS.md) from the code
---
Read this repository and write its agent instruction file.

If WIZARD.md, AGENTS.md or CLAUDE.md already exists, improve that file in place instead of creating a second one. Otherwise create AGENTS.md at the repository root.

Cover only what someone cannot get from a quick look at the tree:

- the exact commands to build, test, lint and run, taken from the build files and CI config rather than guessed
- the shape of the code that needs several files to see: how the main pieces connect and where the boundaries are
- conventions the code actually follows that differ from the language's defaults
- traps: generated files, slow tests, required environment variables, things that look safe to change and are not

Skip generic advice, a file-by-file listing, and anything you did not verify in this repository. Keep it short enough that it will be read. If the repository has a README or docs that already say something, point to them instead of copying.

$ARGUMENTS
