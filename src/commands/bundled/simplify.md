---
description: clean up the changed code for reuse, simplicity and efficiency
---
Look at the code changed on this branch (uncommitted changes plus commits not on the base branch) and clean it up. Target, if given: $ARGUMENTS

Check each change for:

- reuse: does the repository already have a helper for this, or a copy of this logic nearby?
- simplicity: dead code, needless indirection, a flag or option nothing uses, a comment that restates the code
- efficiency: repeated work in a loop, an unnecessary copy or allocation, a query or read done many times
- altitude: logic in the wrong layer, or a change bigger than the problem needs

Apply the fixes you are sure of, keep behaviour identical, and run the tests afterwards. Do not hunt for bugs here; mention one only if you trip over it. End with what you changed in a few lines.
