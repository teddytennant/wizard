---
name: plain-writing
description: Write prose that reads like a person wrote it, not a model. Use before writing or editing any code comment, docstring, README, doc page, tool or API description, config help text, commit message, PR title or body, review reply, issue comment, changelog entry, or release note. Also use when the user says the writing sounds like AI, is slop, is too long, or asks to make it sound human.
version: 0.1.0
---

# Plain writing

Someone decides whether to trust the work based on the first two sentences. They
also decide, in those two sentences, whether a person wrote it.

The default failure is not being wrong. It is being padded, evenly formatted, and
weightless: three paragraphs where one line was needed, a bullet list that
restates the thing above it, adjectives about how robust it is. That reads as
machine output and people discount everything after it.

## Before writing

Answer three questions. If you cannot, you are not ready to write.

1. Who reads this, and what do they already know? Never explain their own
   codebase back to them.
2. What do they do differently after reading it? If nothing, do not write it.
3. What is the shortest form that still carries the fact?

## Rules everywhere

- **No em dashes.** Comma, period, or semicolon.
- **No AI-tell vocabulary.** delve, leverage (as a verb), robust, seamless,
  comprehensive, streamlined, moreover, furthermore, "it's worth noting", "in
  conclusion", "let's dive in", "at its core", "under the hood".
- **No bolded restatement** of the title as the first line of the body. No "Key
  changes:" list that repeats what the reader can see.
- **No bullet walls.** Bullets are for things that are genuinely a list: flags,
  steps, options. Not for splitting one thought into four fragments.
- **No emoji** and no emoji headers, unless the surrounding project already uses
  them.
- **No hedge-and-claim.** Do not write "should fix" or "this likely resolves".
  Either you checked or you say plainly that you did not.
- **Never name a command you did not run**, a test you did not see pass, or a
  file you did not read. One invented detail costs more than the whole document
  earns.
- **No flattery, no apology, no closing offer to help.** "Great question", "Hope
  this helps", "Happy to elaborate" are filler.
- **No AI attribution.** Not in commits, PR text, comments, or docs.
- **A little unevenness is fine.** Perfectly sanded prose is itself the tell. A
  dry aside or a slightly informal sentence reads human. A wrong fact does not.

## Code comments

The line already says what it does. The comment says why, or it does not exist.

Delete: `// increment i`, `// Constructor`, `# returns the result`. Delete
banner blocks of `====` around a section name. Delete a comment that repeats the
function name back in a sentence.

Keep: the reason a value is 4096, the bug a workaround dodges with a link, the
invariant the next person will break, the ordering that matters and looks
arbitrary. Match the density and style of the file you are in.

```
// Retry twice. The upstream API 503s on cold start and recovers by the
// second attempt; more than that and we are papering over a real outage.
```

TODOs carry a name or an issue, or they are noise: `// TODO(#412): drop once
the v1 endpoint is gone.`

## Docstrings, tool and API descriptions

First line does the work: what it does, in one sentence, no restating the
signature. Then only what the caller cannot see from the types: what it returns
in the odd case, what it raises and when, what it mutates, units, whether it
blocks.

For a tool or agent description, the reader is choosing whether to call it. Say
what it is for and when to reach for it over the neighbor. Skip the paragraph on
how it works internally.

## READMEs and docs

As short as possible but no shorter. What it is, how to install it, how to run
it. A real example beats a description of the example.

No badge rows. No feature-bullet marketing. No "Contributing" or
"Acknowledgements" boilerplate unless the project actually has that process. No
architecture essay in the README when the reader is trying to run the thing.

If a section exists only because READMEs usually have it, cut the section.

## Commits, PRs, and comments on other people's repos

**Commit.** Imperative, present tense, specific, one logical change. `Fix panic
in Table when a column constraint underflows`, not `fix bug` or `[FIX] Panic
Issue`. Body only when the why is not obvious. Commits are permanent, so keep
them dry.

**PR body.** Three or four short paragraphs: what is broken and what triggers
it, how to see it, what you changed and why that layer, how you verified it with
the exact commands. No adjectives about how important the fix is.

**Review replies.** One or two sentences, then act. "Good catch, moved it to
layout.rs. Pushed." Never argue past one exchange. If the maintainer says no,
"Makes sense, thanks for looking" and close it.

**Issue comments.** Environment, minimal repro, observed output, expected
output. Nothing else.

Put uncertainty at the end as a question. "I put the clamp in column_widths, but
layout.rs may be the better home. Happy to move it." That reads as a person and
it makes review easier.

## Changelog and release notes

One line per change, written for someone deciding whether to upgrade. Lead with
what changed for them, not with the internal refactor that caused it. Breaking
changes first, with the migration in the same line if it fits.

## Slop and the fix

```
This PR implements a comprehensive fix to address the aforementioned overflow.
It is worth noting that the solution is robust across edge cases and should
improve reliability going forward.

**Key changes:**
- Improved overflow handling
- Enhanced test coverage
```

```
Table panics on a one-column terminal when Length is wider than the area. I hit
this resizing a scratch TUI down to nothing, which is a dumb way to find it, but
the subtraction wraps before the clamp.

Moved the clamp above the subtraction. cargo test is green; added
underflow_one_column as a regression test.
```

## Last pass, always

Run this before you hand the text over.

1. Read it out loud in your head. If you would not send it to a coworker,
   rewrite it.
2. Cut every sentence that does not change what the reader does. Usually the
   first one and the last one.
3. Search for em dashes and the banned words. Replace, do not soften.
4. Check the facts against what you actually ran. Title, body, and diff have to
   agree.
5. Ask whether half the length would lose anything. If not, ship the half.

## Delegating

Subagents default to verbose technical writing and README slop. Any subagent
prompt that will produce prose gets these constraints pasted in, not a pointer
to them.
