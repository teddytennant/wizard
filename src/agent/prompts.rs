//! System prompts: genie vs sovereign personalities, plus composition with
//! skills, the bundled WIZARD.md charter, and project `AGENTS.md`.
//!
//! ## What is resident, and what is a lookup
//!
//! The charter is 6 KB and the memory rules another 1 KB, and both used to be
//! re-sent verbatim on every model step. Depth that only matters *once a
//! subject comes up* does not have to be resident to be known: the always-on
//! prompt carries the index, the ladder's rung names, and the handful of rules
//! that govern every reply, while [`manual_page`] serves any section in full to
//! the `manual` tool. Skills already work this way (see [`crate::skills`]), so
//! the shape is not new here.
//!
//! The digest is *generated* from the charter rather than written out beside
//! it. A fork may amend `WIZARD.md` (§7 says so), and a hand-written summary
//! would keep advertising the sections the fork renamed.
//!
//! ## Section order is a cache decision
//!
//! [`system_prompt_sections`] returns the prompt stable-first, volatile-last,
//! so a provider's prompt-cache breakpoint lands as late as possible: the
//! compiled personality and the charter never change at all, the environment,
//! skills and project instructions change only on an explicit user action, and
//! the memory index changes whenever the agent saves a memory, so it goes last.
//!
//! ## The budget is enforced
//!
//! `assembled_prompt_fits_the_token_ratchet` in this file's tests is a ratchet
//! in the style of `contrib/check-file-size.sh`: only ever lower the ceiling.
//! Without it this whole diet silently undoes itself, one "just one more
//! paragraph" at a time.

use std::path::{Path, PathBuf};

use crate::config::{Config, Mode};
use crate::llm::ToolSpec;
use crate::skills::Skill;

/// The behavioral charter bundled into the binary at compile time.
/// It governs agent behavior in both modes and is inherited by every fork.
const WIZARD_CHARTER: &str = include_str!("../../WIZARD.md");

/// Genie: interactive, bypass-permissions agent — acts directly without
/// asking permission for file writes, shell, or git operations.
pub const GENIE_SYSTEM_PROMPT: &str = "\
You are Wizard, an eager and creative local agent — your user's wish \
is your command. You work inside their project using the provided tools.

Guidelines:
- Collaborate: explain what you are doing and why, briefly.
- Inspect before you act: read files and search before editing.
- Act directly: file writes, shell commands, and git operations run without \
asking permission — just do the work and narrate briefly as you go.
- Prefer small, verifiable steps. Run tests when they exist.
- When the TASK itself is genuinely ambiguous, ask instead of guessing \
(that is about intent, not permission).";

/// Sovereign: autonomous, end-to-end, tests and commits where appropriate.
pub const SOVEREIGN_SYSTEM_PROMPT: &str = include_str!("sovereign_prompt.md");

/// Appended to the system prompt while plan mode is active (the agent
/// re-composes the prompt whenever the flag flips, so this block disappears
/// once a plan is approved).
pub const PLAN_MODE_PROMPT: &str = "\
## Plan mode (active)

You are in PLAN MODE. Investigate using read-only tools only (reading, \
listing, and searching files; inspecting git state); every other tool is \
blocked until your plan is approved. Do not attempt to make changes yet. \
Once you have explored enough to understand the shape of the task but still \
have genuine open questions whose answers would change the plan (scope, \
trade-offs, ambiguous intent, where something should live), call the \
`interview` tool to ask the user a short batch of clarifying questions \
before you commit to an approach — prefer one well-aimed interview over \
guessing. Skip it when the task is already unambiguous. \
Once you understand the task, present your implementation plan by calling \
the `exit_plan` tool with the complete plan as markdown. If the plan is \
approved, plan mode ends and you carry it out; if it is rejected, refine \
the plan using the feedback you receive and call `exit_plan` again.";

/// Appended after [`PLAN_MODE_PROMPT`] when omakase mode is active: the agent
/// has full authority over the approach. It still explores read-only first,
/// but it does not interview the user and its plan is auto-approved — it
/// decides and proceeds. Like plan mode, this block disappears once omakase
/// is turned off (the prompt is recomposed on every flag flip).
pub const OMAKASE_PROMPT: &str = "\
## Omakase mode (chef's choice)

Omakase is on: this is the chef's-choice flavor of plan mode — you have full \
authority over the approach and the user has handed you the wheel. After \
exploring read-only, do NOT call `interview`; resolve every open question \
yourself by making the most reasonable assumption a senior engineer would, \
and choose the approach you judge best. Your plan is auto-approved — there \
is no human review gate — so when you call `exit_plan`, make the plan \
self-justifying: state the approach you picked, the alternatives you \
weighed, the assumptions you made, and why. Then execute it end to end, \
verify your work, and deliver a polished result. Be decisive and tasteful; \
surprise them with quality, not with questions.";

/// Appended to the system prompt when the `todo` tool is registered: keep a
/// working todo list for multi-step tasks so every surface can mirror
/// progress.
pub const TODO_PROMPT: &str = "\
## Working todo list

For multi-step work, maintain a todo list with the `todo` tool: write the \
full list up front (action \"write\" replaces the entire list), keep exactly \
one item in_progress while you work on it, and mark items completed as soon \
as they are done. Skip the list for trivial single-step tasks.";

/// Always appended: how the agent should steward its own context window.
/// Single home for this guidance (not repeated in WIZARD.md).
pub const CONTEXT_PROMPT: &str = "\
## Context management (you own your window)

History is finite. Sessions persist under `~/.wizard/sessions/<id>.jsonl` and \
auto-compact at a high threshold — treat that as a safety net, not a plan. A \
live `[context pressure]` line is injected before each model step when fill is \
elevated or higher.

1. **Stay lean.** Short tool output (`head`/`tail`/`wc`, or `/tmp` + summarize). \
Delegate noisy multi-step work to `spawn_subagent` so only the final report \
enters your context.
2. **Compact when bloated.** Call the `compact` tool (mid-turn, every surface: \
TUI/GUI/headless/gateway). It summarizes older history into a progress note and \
keeps the recent tail. Prefer `compact` over `run_command` `/compact` (deferred \
until the turn ends, interactive-only). Prefer compacting over asking the user \
to clear.
3. **On task change:** save durable facts with `memory`, rewrite/clear the todo \
list, then `compact`. Full transcript stays on disk. Only if the new task must \
not see the old work at all, tell the user `/clear` (you cannot run it).
4. **Don't re-read** what compaction already summarized — open the file or \
session JSONL for a specific detail.
5. **Honor pressure.** When the signal is `elevated`, compact soon; when \
`high` or `critical`, call `compact` before more tool work. You do not need \
`/status` for this — the pressure line is the meter.";

/// Memory guidance injected when the project has saved memories; the index
/// (MEMORY.md) follows it.
///
/// Kept deliberately short. Everything a model needs *before it decides to
/// touch memory at all* is here: that the store exists, the three actions, the
/// four types (it has to pick one on every `save`), and the link syntax. The
/// rest is [`MEMORY_RULES`], one `manual` lookup away.
const MEMORY_PROMPT_WITH_INDEX: &str = "\
You have persistent project memory. The index below lists every saved memory \
(one markdown file each) with its type and a one-line description. Use the \
`memory` tool with action \"read\" to recall one in full, \"save\" to record or \
update a durable fact, \"delete\" to drop one that turned out wrong.";

/// Memory guidance injected when no memories exist yet, so memory
/// bootstraps on first use.
const MEMORY_PROMPT_EMPTY: &str = "\
You have persistent project memory via the `memory` tool, but nothing is \
saved for this project yet. When you learn a durable fact, record it with \
action \"save\": it appears in your system prompt next session, so the memory \
you write now is the one you read then.";

/// The always-on tail of the memory section: the four types and the link
/// syntax, which are needed to *write* a memory correctly, plus the pointer to
/// the rules that decide whether it should be written at all.
const MEMORY_ESSENTIALS: &str = "\
Types: `user` (who they are), `feedback` (how you should work, with the why), \
`project` (goals and constraints not derivable from the code; convert relative \
dates to absolute), `reference` (a URL, dashboard, or ticket). Link related \
memories from a body by name, `[[wiki-style]]`. Before you save or delete, \
read `manual` topic `memory`: it says what earns a place and what must never \
be written down.";

/// The rules that make memory worth having: what the types mean, how memories
/// link to each other, and what must never be written down.
///
/// Served by the `manual` tool rather than the prompt. These decide whether a
/// memory should exist, which is a question the model only asks when it is
/// already about to call the `memory` tool, so a kilobyte of it resident on
/// every step buys nothing.
const MEMORY_RULES: &str = "\
Every memory has a type:
- `user` — who the user is: their role, expertise, and standing preferences.
- `feedback` — how you should work: corrections *and* confirmed approaches. \
Include the why, not just the what.
- `project` — ongoing work, goals, and constraints that are not derivable \
from the code or the git history. Convert relative dates (\"next week\") to \
absolute ones.
- `reference` — a pointer to an external resource: a URL, a dashboard, a \
ticket.

Link related memories from a memory's body by name, `[[wiki-style]]`. A link \
to a memory that does not exist yet is fine — it marks something worth \
writing later, not an error. A `read` tells you which links resolve.

A memory has to earn its place:
- Never save what the repo already records: code structure, past fixes, \
anything in the git history. Never save what only matters to the current \
conversation.
- Before saving, look for a memory that already covers the same ground and \
update it (save over its name) instead of creating a near-duplicate.
- Delete a memory that turns out to be wrong. Names are kebab-case, \
descriptions are one line.";

// ---------------------------------------------------------------------------
// The charter: an always-on digest, and the manual behind it
// ---------------------------------------------------------------------------

/// Fixed lead of the charter digest: what the index is, and how to get depth.
const CHARTER_DIGEST_LEAD: &str = "\
Your operating charter (`WIZARD.md`) is bundled in this binary. What follows \
is its index, not its text: to read a section in full, call the `manual` tool \
with one of the topic ids below. Read the section before you act on its \
subject rather than guessing what it says.";

/// The charter rules that stay resident.
///
/// Every other section has a trigger that tells the model to go and look: a
/// capability it lacks, a fork request, a subagent-shaped task. These four
/// govern how *every* reply is written, so nothing would ever prompt the
/// lookup, and a violated line of output cannot be un-sent afterwards. The
/// last one is a summary: the prose rules themselves stay in §7, behind the
/// `writing` lookup, because the digest cannot afford the whole section.
///
/// Amending §6 or §7 of `WIZARD.md` means amending this constant too;
/// `always_on_rules_are_still_in_the_charter` fails if the charter drops one.
const CHARTER_ALWAYS_ON: &str = "\
In force on every reply, never worth a lookup:
- No em dashes in anything you write (replies, code, commits, docs). Use a \
comma, colon, period, parentheses, or a plain hyphen.
- Never fabricate success. Claim a build, test, install, fork, or publish only \
when you ran it and saw the result.
- Gates stay. Do not route around deep evolve's build and smoke gate, or plan \
mode's read-only phase.
- Write like a person: concise, plain, no AI-slop prose and no academic \
padding. The full rules are `manual` topic `writing`.";

/// Title given to the charter's preamble, which has no `##` header of its own.
const OVERVIEW_TITLE: &str = "Overview";

/// One page of the on-demand manual: a charter section, or a topic that is not
/// in the charter at all (the memory rules). None of this is resident in the
/// system prompt; the `manual` tool serves it when the model asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualPage {
    /// Lookup id, exactly as the digest advertises it (`prime-directive-build`).
    pub id: String,
    /// Section title as the charter writes it, including its number.
    pub title: String,
    /// The section body, verbatim.
    pub body: String,
}

/// Every manual page, in the order the digest lists them: the charter's
/// sections first, then the topics that live outside the charter.
///
/// Parsed on each call rather than cached. This runs a handful of times per
/// session (prompt composition, and one `manual` call), and a `OnceLock` here
/// would only make the charter harder to reason about.
pub fn manual_pages() -> Vec<ManualPage> {
    let mut pages = charter_pages(WIZARD_CHARTER);
    // Fixed id rather than one derived from the title: [`MEMORY_ESSENTIALS`]
    // tells the model to read topic `memory` by name, and a retitle must not
    // turn that into a dangling pointer.
    pages.push(ManualPage {
        id: unique_id(&pages, "memory"),
        title: "Memory: what earns a place".to_string(),
        body: MEMORY_RULES.to_string(),
    });
    pages
}

/// Look up one manual page by `topic`, the way a user or a model would type
/// it: the advertised id, a section number (`4`, `§4`), an id prefix, or any
/// substring of the title. `None` when nothing matches; the caller should then
/// list [`manual_pages`] rather than guess.
pub fn manual_page(topic: &str) -> Option<ManualPage> {
    let needle = topic.trim().trim_start_matches('§').trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let pages = manual_pages();
    pages
        .iter()
        .find(|page| page.id == needle)
        .or_else(|| {
            pages
                .iter()
                .find(|page| section_number(&page.title) == Some(needle.as_str()))
        })
        .or_else(|| {
            pages.iter().find(|page| {
                page.id.starts_with(&needle) || page.title.to_lowercase().contains(&needle)
            })
        })
        .cloned()
}

/// Split `charter` into one page per `##` heading, with everything before the
/// first heading kept as the [`OVERVIEW_TITLE`] page so no charter text is
/// unreachable.
fn charter_pages(charter: &str) -> Vec<ManualPage> {
    let mut pages: Vec<ManualPage> = Vec::new();
    let mut title = OVERVIEW_TITLE.to_string();
    let mut body = String::new();
    for line in charter.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            push_page(
                &mut pages,
                std::mem::take(&mut title),
                std::mem::take(&mut body),
            );
            title = heading.trim().to_string();
            continue;
        }
        // The preamble's `# ` title and the `---` rules between sections are
        // page furniture, not content, and cost tokens in every lookup.
        if line.starts_with("# ") || line.trim() == "---" {
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    push_page(&mut pages, title, body);
    pages
}

/// Append a page, dropping empty bodies and giving the page an id that no
/// earlier page already claimed.
fn push_page(pages: &mut Vec<ManualPage>, title: String, body: String) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    pages.push(ManualPage {
        id: unique_id(pages, &topic_id(&title)),
        title,
        body: body.to_string(),
    });
}

/// `base`, or `base-2`, `base-3`, ... if an earlier page already took it. Two
/// pages sharing an id would make one of them unreachable through the very
/// lookup the digest advertises.
fn unique_id(pages: &[ManualPage], base: &str) -> String {
    let mut id = base.to_string();
    let mut suffix = 2;
    while pages.iter().any(|page| page.id == id) {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    id
}

/// A lookup id for a section title: its leading `N. ` dropped, lowercased,
/// non-alphanumeric runs collapsed to `-`, and capped at three words so ids
/// stay short enough to type into a tool call.
fn topic_id(title: &str) -> String {
    let words = section_body_title(title)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(3)
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if words.is_empty() {
        "untitled".to_string()
    } else {
        words.join("-")
    }
}

/// The `N` of a `N. Title` heading, if it has one.
fn section_number(title: &str) -> Option<&str> {
    let (number, _) = title.split_once(". ")?;
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit())).then_some(number)
}

/// A heading with its `N. ` prefix removed.
fn section_body_title(title: &str) -> &str {
    match section_number(title) {
        Some(number) => title[number.len() + 2..].trim_start(),
        None => title,
    }
}

/// The always-on charter block: the lead, the ladder's rung names, the topic
/// index, and the rules that stay resident. Roughly a fifth of the size of the
/// charter it stands in for.
fn charter_digest() -> String {
    let pages = manual_pages();
    let mut out = String::from("## Wizard charter (WIZARD.md)\n\n");
    out.push_str(CHARTER_DIGEST_LEAD);
    if let Some(ladder) = ladder_summary(&pages) {
        out.push_str("\n\n");
        out.push_str(&ladder);
    }
    out.push_str("\n\nTopics: ");
    let topics: Vec<String> = pages
        .iter()
        .map(|page| format!("`{}` ({})", page.id, page.title))
        .collect();
    out.push_str(&topics.join("; "));
    out.push_str(".\n\n");
    out.push_str(CHARTER_ALWAYS_ON);
    out
}

/// The capability ladder reduced to its rung *names* plus the one instruction
/// that makes the ladder work (climb it, do not refuse).
///
/// The names are read out of whichever page actually carries a numbered
/// `**bold**` list, so a fork that renames or renumbers a rung gets the new
/// names in its digest for free. The first such page wins: §1 is the ladder
/// today, and §4's numbered list sits behind it.
fn ladder_summary(pages: &[ManualPage]) -> Option<String> {
    let (page, rungs) = pages.iter().find_map(|page| {
        let rungs = numbered_bold_items(&page.body);
        (rungs.len() >= 3).then_some((page, rungs))
    })?;
    let mut summary = format!(
        "Capability ladder. A task that needs a capability you lack is work, \
         not a refusal: acquire it, and refuse only after trying and hitting a \
         hard wall. Climb cheapest rung first and pick the lowest that solves \
         it: {rungs}. What each rung costs: `manual` topic `{id}`.",
        rungs = rungs.join(", "),
        id = page.id,
    );
    // Browsing is the most common capability gap by a wide margin, and the
    // charter answers it with a literal `evolve` call rather than advice, so
    // the digest spends one more id on it. It has to be *that* page's id: the
    // ladder page says only "browser use belongs here (see §2)", so pointing
    // there costs the second lookup this sentence exists to avoid.
    if let Some(recipe) = browser_recipe_page(pages) {
        summary.push_str(&format!(
            " The recipe for browser use: `manual` topic `{}`.",
            recipe.id
        ));
    }
    Some(summary)
}

/// The page carrying the browser-use recipe, matched on its title so a fork
/// that renumbers or retitles the section still gets a live pointer. `None`
/// when a fork drops the section, in which case the digest says nothing about
/// browsing rather than advertising an id that resolves elsewhere.
fn browser_recipe_page(pages: &[ManualPage]) -> Option<&ManualPage> {
    pages
        .iter()
        .find(|page| page.title.to_lowercase().contains("browser"))
}

/// The `N. **Name** ...` items of a markdown list, rendered `N. Name`.
fn numbered_bold_items(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            let (number, rest) = line.trim_start().split_once(". ")?;
            if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let (name, _) = rest.strip_prefix("**")?.split_once("**")?;
            Some(format!("{number}. {name}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Environment: the facts the model cannot derive from the conversation
// ---------------------------------------------------------------------------

/// What this machine is, in three lines.
///
/// The shell is the load-bearing one: a model that assumes `sh` writes
/// `ls | grep` where PowerShell needs `Get-ChildItem`, and it has no way to
/// find out except by running something and reading the failure. It comes from
/// [`crate::platform::shell::name`], the same function that builds the command
/// the `execute` tool spawns (`crate::tools::shell`) and the one background
/// tasks spawn (`crate::tools::tasks`), so the prompt cannot describe a shell
/// other than the one those actually run.
///
/// Hooks are *not* claimed here, though they were: `crate::hooks` spawns
/// `sh -c` directly rather than through the platform layer, so on any target
/// where [`crate::platform::shell::name`] is not `sh` this line would have the
/// model writing hook commands in a syntax hooks never feed to that shell. The
/// two are identical on unix today; the sentence stays narrow so it does not
/// become false the moment they are not.
///
/// The theme is a snapshot taken when the prompt was composed. The prompt is
/// recomposed on `/reload`, on a model switch and on plan-mode flips, so a
/// palette change shows up at the next recomposition rather than immediately;
/// that is fine for a fact the model only uses to describe what the user sees.
fn environment_section() -> String {
    format!(
        "## Environment\n\n\
         - Shell: `{shell}`. Command lines from `execute` and background tasks \
         are parsed by this shell, so write syntax it accepts.\n\
         - OS: {os} ({arch}).\n\
         - UI theme: `{theme}`.",
        shell = crate::platform::shell::name(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        theme = crate::theme::active().name,
    )
}

/// Resolve the base personality prompt for `mode`. An external override —
/// `$WIZARD_SYSTEM_PROMPT` if set, otherwise `~/.wizard/system_prompt.md` —
/// replaces the compiled default when it exists and is non-empty. This is the
/// single file external harness-evolution tools (e.g. AHE) mutate; with no
/// override present, the result is byte-identical to the baked prompt. Only the
/// *personality* is overridable: [`build_system_prompt`] appends the charter
/// digest, the environment, skills, instructions, and memory on top of whatever
/// this returns, and the digest's depth is served by the `manual` tool from a
/// compiled-in constant, so neither the charter nor its lookup can be evolved
/// away here.
fn base_system_prompt(mode: Mode) -> String {
    let default = match mode {
        Mode::Genie => GENIE_SYSTEM_PROMPT,
        Mode::Sovereign => SOVEREIGN_SYSTEM_PROMPT,
    };
    override_path()
        .as_deref()
        .and_then(read_prompt_override)
        .unwrap_or_else(|| default.to_string())
}

/// The path an override would live at, if any: the harness bundle's
/// `system_prompt.md` wins when it exists (so a bundle missing the file
/// degrades to the next candidate), then `$WIZARD_SYSTEM_PROMPT`, then the
/// well-known `~/.wizard/system_prompt.md`.
fn override_path() -> Option<PathBuf> {
    if let Some(dir) = Config::harness_dir() {
        let bundled = dir.join("system_prompt.md");
        if bundled.exists() {
            return Some(bundled);
        }
    }
    if let Some(p) = std::env::var_os("WIZARD_SYSTEM_PROMPT") {
        return Some(PathBuf::from(p));
    }
    Config::system_prompt_path().ok()
}

/// Read an override file, returning its trimmed contents only when the file
/// exists and is non-empty. A missing or empty file yields `None` so the
/// caller falls back to the baked default.
fn read_prompt_override(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// One labelled slice of the assembled system prompt.
///
/// The prompt is built as a list of these rather than one growing `String` for
/// two reasons: the order is a cache decision that should be visible in one
/// place, and `wizard doctor` can print a per-section byte and token breakdown
/// that cannot drift from the prompt it describes, because it *is* the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    /// Stable identifier, for the doctor breakdown and for tests.
    pub name: &'static str,
    /// The section's text, exactly as it appears in the assembled prompt.
    pub text: String,
}

impl PromptSection {
    /// Size on the wire.
    pub fn bytes(&self) -> usize {
        self.text.len()
    }

    /// Rough token cost, by the same estimate the status bar uses. Never
    /// exact; good enough for a budget and for a breakdown.
    pub fn est_tokens(&self) -> u64 {
        crate::llm::estimate_tokens_from_chars(self.text.chars().count())
    }
}

/// The separator between assembled sections. Each section carries its own
/// heading, so a blank line between them is all the structure needed.
const SECTION_SEPARATOR: &str = "\n\n";

/// The sections of the system prompt for `mode`, stable-first and
/// volatile-last so a provider's prompt-cache breakpoint lands as late as
/// possible. Sections that would be empty are omitted entirely.
///
/// Order and rationale:
///
/// 1. `personality`: a compiled constant (or a harness override read once).
/// 2. `charter`: generated from a compiled constant; identical every run.
/// 3. `environment`: fixed for the process, except the theme (see
///    [`environment_section`]).
/// 4. `skills`: changes only on `/reload`.
/// 5. `instructions`: changes only when the working directory does.
/// 6. `memory`: a constant, but it introduces the index, so it sits with it.
/// 7. `memory-index`: rewritten whenever the agent saves or deletes a memory,
///    which can happen mid-turn. Nothing may follow it.
pub fn system_prompt_sections(
    mode: Mode,
    skills: &[Skill],
    agents_md: Option<&str>,
    memory_index: Option<&str>,
) -> Vec<PromptSection> {
    sections_from_base(base_system_prompt(mode), skills, agents_md, memory_index)
}

/// [`system_prompt_sections`] with the personality prompt supplied directly,
/// so the token ratchet can measure the *baked* prompt rather than whatever
/// `~/.wizard/system_prompt.md` happens to hold on the machine running the
/// tests.
fn sections_from_base(
    base: String,
    skills: &[Skill],
    agents_md: Option<&str>,
    memory_index: Option<&str>,
) -> Vec<PromptSection> {
    let mut sections = vec![
        PromptSection {
            name: "personality",
            text: base,
        },
        // The charter governs every session, genie and sovereign alike, and
        // forks inherit it. Only its digest is resident; `manual` serves the
        // rest.
        PromptSection {
            name: "charter",
            text: charter_digest(),
        },
        PromptSection {
            name: "environment",
            text: environment_section(),
        },
    ];

    let skills_section = crate::skills::render_for_prompt(skills);
    if !skills_section.is_empty() {
        sections.push(PromptSection {
            name: "skills",
            text: skills_section,
        });
    }

    // Project instructions may include the same WIZARD.md that the charter
    // digest already stands for (wizard's own checkout, or a copied
    // ~/.wizard/WIZARD.md). Drop those duplicate sections: the manual serves
    // that text on demand, so re-injecting it verbatim would pay the full 6 KB
    // the digest exists to avoid. Real project-specific rules are kept.
    if let Some(filtered) = agents_md.and_then(|raw| filter_charter_dupes(raw, WIZARD_CHARTER)) {
        sections.push(PromptSection {
            name: "instructions",
            text: format!("## Project instructions\n\n{filtered}"),
        });
    }

    let memory_intro = match memory_index {
        Some(_) => MEMORY_PROMPT_WITH_INDEX,
        None => MEMORY_PROMPT_EMPTY,
    };
    sections.push(PromptSection {
        name: "memory",
        text: format!("## Memory\n\n{memory_intro}\n\n{MEMORY_ESSENTIALS}"),
    });
    if let Some(index) = memory_index {
        sections.push(PromptSection {
            name: "memory-index",
            text: format!("### Memory index (MEMORY.md)\n\n{index}"),
        });
    }

    sections
}

/// Compose the full system prompt for `mode`: the personality prompt, the
/// bundled charter's digest, this machine's environment, a rendered skills
/// section, the project's instruction hierarchy (`agents_md`, assembled by
/// [`crate::instructions`] from WIZARD.md/AGENTS.md/CLAUDE.md files), and the
/// persistent memory section (`memory_index` is the project's MEMORY.md, when
/// any memories are saved).
///
/// This is [`system_prompt_sections`] joined; see it for what each section is
/// and why it sits where it does.
pub fn build_system_prompt(
    mode: Mode,
    skills: &[Skill],
    agents_md: Option<&str>,
    memory_index: Option<&str>,
) -> String {
    join_sections(&system_prompt_sections(
        mode,
        skills,
        agents_md,
        memory_index,
    ))
}

/// The assembled prompt for a list of sections.
pub fn join_sections(sections: &[PromptSection]) -> String {
    sections
        .iter()
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join(SECTION_SEPARATOR)
}

/// The name of the one section that is rewritten during a session. Ordering
/// the prompt around it is the whole reason [`system_prompt_sections`] exists.
const VOLATILE_SECTION: &str = "memory-index";

/// Byte length of the leading run of sections that stays identical for the
/// life of a session, including the separator that follows it.
///
/// This is where a provider-side prompt cache should be cut: everything before
/// the offset is re-sent byte-for-byte on every step of the session, and a
/// breakpoint any earlier leaves cacheable tokens uncached. Returns the whole
/// prompt length when no volatile section is present.
pub fn cache_breakpoint(sections: &[PromptSection]) -> usize {
    let mut offset = 0;
    for (index, section) in sections.iter().enumerate() {
        if section.name == VOLATILE_SECTION {
            return offset;
        }
        if index > 0 {
            offset += SECTION_SEPARATOR.len();
        }
        offset += section.bytes();
    }
    offset
}

/// Drop instruction sections whose body matches the bundled charter
/// (whitespace-normalized). Returns `None` when nothing project-specific
/// remains, so a checkout whose only instruction file is the same WIZARD.md the
/// charter digest stands for does not pay 6 KB to say it again.
fn filter_charter_dupes(agents_md: &str, charter: &str) -> Option<String> {
    let charter_norm = normalize_prompt_section(charter);
    if charter_norm.is_empty() {
        let trimmed = agents_md.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }

    // instructions::load prefixes each file with
    // `<!-- instructions from PATH -->\n`. Scan line-by-line and flush on each
    // header so every file is judged independently.
    let mut kept: Vec<String> = Vec::new();
    let mut current_header: Option<String> = None;
    let mut current_body = String::new();

    let flush = |header: &mut Option<String>, body: &mut String, out: &mut Vec<String>| {
        let text = std::mem::take(body);
        let text = text.trim();
        let header = header.take();
        if text.is_empty() {
            return;
        }
        if normalize_prompt_section(text) == charter_norm {
            return;
        }
        match header {
            Some(h) => out.push(format!("{h}\n{text}")),
            None => out.push(text.to_string()),
        }
    };

    for line in agents_md.lines() {
        if let Some(rest) = line.strip_prefix("<!-- instructions from ")
            && let Some(path) = rest.strip_suffix(" -->")
        {
            flush(&mut current_header, &mut current_body, &mut kept);
            current_header = Some(format!("<!-- instructions from {path} -->"));
            continue;
        }
        if !current_body.is_empty() {
            current_body.push('\n');
        }
        current_body.push_str(line);
    }
    flush(&mut current_header, &mut current_body, &mut kept);

    // No section headers (raw agents_md string in tests): treat the whole
    // block as one section — already handled by the flush above when there
    // were no headers and current_body held everything.
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("\n\n"))
    }
}

/// Collapse runs of whitespace so trivial formatting drift does not defeat
/// charter-duplicate detection (e.g. trailing newlines from file reads).
fn normalize_prompt_section(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Instructions appended to the system prompt when the model lacks native
/// tool calling: defines the prompt-based JSON tool protocol the parser in
/// the agent loop understands (see `docs/byom.md`).
pub const JSON_TOOL_PROTOCOL_PROMPT: &str = "\
You do not have native function calling. To call a tool, reply with ONLY a \
single JSON object on its own line, no other text:

{\"tool\": \"<tool_name>\", \"arguments\": { ... }}

You will receive the tool result in the next message. When you are finished \
with tools, reply with your final answer as plain text.";

/// Render the JSON tool protocol plus the available tool roster, appended to
/// the system prompt for models without native tool calling.
///
/// This is the largest block the prompt can emit, and it is deliberately still
/// full-fidelity: a model on the JSON protocol has no `tools` field on the
/// request, so this roster is the *only* place it learns a tool exists, what it
/// does, and what its arguments are. Trimming descriptions here would not move
/// a cost that native-tool-calling models pay (they get the same schemas over
/// the wire either way); the lever for both is which tools the registry hands
/// over in the first place, which belongs to the registry, not to this file.
pub fn render_tool_protocol(specs: &[ToolSpec]) -> String {
    let mut section = String::from(JSON_TOOL_PROTOCOL_PROMPT);
    if specs.is_empty() {
        return section;
    }
    section.push_str("\n\n## Available tools\n");
    for spec in specs {
        let schema =
            serde_json::to_string(&spec.function.parameters).unwrap_or_else(|_| "{}".to_string());
        section.push_str(&format!(
            "\n- `{}`: {}\n  arguments schema: {}",
            spec.function.name, spec.function.description, schema
        ));
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The charter's digest must appear in the composed prompt for both modes:
    /// the header, the ladder's rung names, and a live pointer to every page of
    /// the depth. This verifies the `include_str!` path is correct and the
    /// digest is derived from a charter that actually parsed.
    ///
    /// The index is the whole contract with the `manual` tool, so it is checked
    /// page by page rather than by looking for the word "manual": a digest that
    /// lists eight of nine topics leaves one section unreachable, and the model
    /// has no way to discover the id it was never told.
    #[test]
    fn system_prompt_contains_the_charter_digest() {
        let pages = manual_pages();
        assert!(
            pages.len() > 5,
            "the charter parsed into {} pages",
            pages.len()
        );
        for mode in [Mode::Genie, Mode::Sovereign] {
            let prompt = build_system_prompt(mode, &[], None, None);
            assert!(
                prompt.contains("## Wizard charter (WIZARD.md)"),
                "charter header missing in {mode} prompt"
            );
            assert!(
                prompt.contains("Capability ladder"),
                "ladder summary missing in {mode} prompt"
            );
            assert!(
                prompt.contains("call the `manual` tool"),
                "the {mode} prompt must name the tool that serves the depth"
            );
            for page in &pages {
                assert!(
                    prompt.contains(&format!("`{}` ({})", page.id, page.title)),
                    "the {mode} digest does not advertise topic {:?}, so nothing can \
                     reach {:?}",
                    page.id,
                    page.title
                );
            }
        }
    }

    /// The one extra id the digest spends beyond the index has to be the
    /// browser recipe's own page. Pointing at the ladder page instead costs a
    /// second lookup on the most common capability gap there is, which is the
    /// exact round trip this sentence exists to save.
    #[test]
    fn the_digest_points_browser_use_at_the_recipe_not_the_ladder() {
        let pages = manual_pages();
        let recipe = browser_recipe_page(&pages).expect("the charter has a browser recipe");
        assert!(
            recipe.body.contains("@playwright/mcp"),
            "the page named for browser use must be the one carrying the recipe: {}",
            recipe.title
        );

        let prompt = build_system_prompt(Mode::Genie, &[], None, None);
        assert!(
            prompt.contains(&format!(
                "The recipe for browser use: `manual` topic `{}`.",
                recipe.id
            )),
            "the digest must send browser use to {:?}:\n{prompt}",
            recipe.id
        );

        // And the ladder page is still advertised for what it does carry, under
        // its own id, so the two pointers do not collapse into one.
        let ladder = pages
            .iter()
            .find(|page| numbered_bold_items(&page.body).len() >= 3)
            .expect("the ladder is a page");
        assert_ne!(
            ladder.id, recipe.id,
            "the ladder and the recipe are two pages"
        );
        assert!(prompt.contains(&format!(
            "What each rung costs: `manual` topic `{}`.",
            ladder.id
        )));
    }

    /// The point of the split: the always-on prompt names every rung of the
    /// ladder, and carries none of the prose behind them.
    ///
    /// Rung names are cheap and the model needs them to know a rung exists at
    /// all. The paragraph on each one is what `manual` is for. If this test
    /// ever fails because the full ladder came back, the diet has been undone.
    #[test]
    fn always_on_prompt_has_the_rung_names_but_not_the_ladder() {
        let prompt = build_system_prompt(Mode::Genie, &[], None, None);

        // Every rung the charter defines, by name.
        let ladder = manual_page("prime").expect("the ladder section is a manual page");
        let rungs = numbered_bold_items(&ladder.body);
        assert_eq!(rungs.len(), 5, "charter defines five rungs: {rungs:?}");
        for rung in &rungs {
            assert!(
                prompt.contains(rung),
                "rung {rung:?} missing from the always-on prompt"
            );
        }

        // And none of the body that explains them. These are sentences from
        // §1, §2 and §4 that used to ride along on every single model step.
        for buried in [
            "knowledge or procedure, not new code",
            "Don't deep-evolve what a skill covers",
            "npx -y @playwright/mcp@latest",
            "No hallucinated project knowledge",
        ] {
            assert!(
                !prompt.contains(buried),
                "charter depth {buried:?} is resident again; it belongs in `manual`"
            );
        }
        assert!(
            !prompt.contains(WIZARD_CHARTER.trim()),
            "the whole charter is resident again"
        );
    }

    /// The depth has to still be *reachable* and *complete*: every line of the
    /// charter must live on some manual page, or the split turned a diet into
    /// a deletion.
    #[test]
    fn the_manual_serves_the_whole_charter() {
        let pages = manual_pages();
        let served = pages
            .iter()
            .map(|page| normalize_prompt_section(&page.body))
            .collect::<Vec<_>>()
            .join(" ");

        for line in WIZARD_CHARTER.lines() {
            let line = line.trim();
            // Headings, the `---` rules and the `# ` title are furniture: the
            // headings survive as page titles, the rest carries no content.
            if line.is_empty() || line.starts_with('#') || line == "---" {
                continue;
            }
            assert!(
                served.contains(&normalize_prompt_section(line)),
                "charter line {line:?} is not served by any manual page"
            );
        }

        for heading in WIZARD_CHARTER
            .lines()
            .filter_map(|line| line.strip_prefix("## "))
        {
            assert!(
                pages.iter().any(|page| page.title == heading.trim()),
                "charter section {heading:?} has no manual page"
            );
        }

        // The rules that left the prompt in this change are reachable too,
        // under exactly the id MEMORY_ESSENTIALS tells the model to ask for.
        let memory = manual_page("memory").expect("memory rules are a manual page");
        assert_eq!(memory.id, "memory");
        assert!(memory.body.contains("A memory has to earn its place"));
        assert!(memory.body.contains("update it (save over its name)"));
        assert!(
            memory
                .body
                .contains("Never save what the repo already records")
        );
    }

    /// A `manual` call has to work with whatever the model types: the id the
    /// digest advertised, a section number, a prefix, or a word from the title.
    #[test]
    fn manual_lookup_accepts_ids_numbers_and_titles() {
        let guardrails = manual_page("guardrails").expect("id lookup");
        assert_eq!(guardrails.title, "6. Guardrails");

        // The digest advertises ids, so an exact id must always resolve.
        for page in manual_pages() {
            assert_eq!(
                manual_page(&page.id).map(|found| found.title),
                Some(page.title.clone()),
                "advertised id {:?} did not resolve",
                page.id
            );
        }

        for spelling in ["6", "§6", "GUARD", "Guardrails"] {
            assert_eq!(
                manual_page(spelling).map(|page| page.title),
                Some(guardrails.title.clone()),
                "{spelling:?} should reach the guardrails page"
            );
        }
        assert_eq!(
            manual_page("browser").map(|page| page.title),
            Some("2. Recipe: browser use".to_string())
        );
        assert_eq!(manual_page("no such topic"), None);
        assert_eq!(manual_page("   "), None, "an empty topic is not a match");
    }

    /// Ids must be unique, or the digest advertises a lookup that silently
    /// resolves to the wrong page.
    #[test]
    fn manual_page_ids_are_unique() {
        let pages = manual_pages();
        let mut ids: Vec<&str> = pages.iter().map(|page| page.id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate manual ids in {ids:?}");

        // A duplicate title still gets its own id rather than colliding.
        let mut pages = Vec::new();
        push_page(&mut pages, "Guardrails".into(), "one".into());
        push_page(&mut pages, "Guardrails".into(), "two".into());
        assert_eq!(pages[0].id, "guardrails");
        assert_eq!(pages[1].id, "guardrails-2");
    }

    /// The three rules that stayed resident are quoted from the charter. If a
    /// fork amends §6, this fails and points at [`CHARTER_ALWAYS_ON`] rather
    /// than letting the prompt keep enforcing a rule the charter dropped.
    ///
    /// Both directions matter and neither is a restatement: the charter must
    /// still say the rule (or the prompt is enforcing a dropped one), and the
    /// *composed prompt* must still carry it (or a rule that cannot be un-sent
    /// once violated has quietly become a lookup nothing triggers). Asserting
    /// the constant contains what was copied out of the constant proves
    /// neither, so it does not appear here.
    #[test]
    fn always_on_rules_are_still_in_the_charter() {
        let prompt = build_system_prompt(Mode::Genie, &[], None, None);
        for rule in ["No em dashes", "Never fabricate success", "Gates stay"] {
            assert!(
                WIZARD_CHARTER.contains(rule),
                "charter no longer says {rule:?}; update CHARTER_ALWAYS_ON"
            );
            assert!(
                prompt.contains(rule),
                "{rule:?} must stay resident in the composed prompt, not become a lookup"
            );
        }
    }

    /// Skills and AGENTS.md appear after the charter.
    #[test]
    fn charter_comes_before_agents_md() {
        let prompt = build_system_prompt(Mode::Genie, &[], Some("# Project rules"), None);
        let charter_pos = prompt
            .find("## Wizard charter (WIZARD.md)")
            .expect("charter present");
        let agents_pos = prompt
            .find("## Project instructions")
            .expect("project instructions section present");
        assert!(
            charter_pos < agents_pos,
            "charter must appear before project instructions"
        );
    }

    /// A project-instructions block that is only a re-copy of the bundled
    /// charter (the common case when working inside the wizard checkout) must
    /// not appear under "## Project instructions". Since the digest landed this
    /// is the only path by which the full charter could still reach the prompt,
    /// so it is also the path that would silently undo the whole diet.
    #[test]
    fn project_instructions_drop_bundled_charter_duplicate() {
        let fake_load = format!(
            "<!-- instructions from /tmp/proj/WIZARD.md -->\n{}",
            WIZARD_CHARTER.trim_end()
        );
        let prompt = build_system_prompt(Mode::Genie, &[], Some(&fake_load), None);
        assert!(
            prompt.contains("## Wizard charter (WIZARD.md)"),
            "the digest is still there"
        );
        // A sentence from §1's body, which the digest paraphrases rather than
        // quotes, so finding it means the whole charter came back in.
        assert_eq!(
            prompt
                .matches("Pick the lowest rung that solves it")
                .count(),
            0,
            "the charter body must not ride in through project instructions"
        );
        assert!(
            !prompt.contains("## Project instructions"),
            "empty after filtering, so no project-instructions section"
        );
    }

    /// Real project rules sitting next to a charter-identical section are kept;
    /// only the charter copy is dropped.
    #[test]
    fn project_instructions_keep_non_charter_sections() {
        let mixed = format!(
            "<!-- instructions from /tmp/WIZARD.md -->\n{}\n\n\
             <!-- instructions from /tmp/proj/AGENTS.md -->\n# Local rules\nuse cargo nextest\n",
            WIZARD_CHARTER.trim_end()
        );
        let prompt = build_system_prompt(Mode::Genie, &[], Some(&mixed), None);
        assert!(prompt.contains("## Project instructions"));
        assert!(prompt.contains("use cargo nextest"));
        assert!(prompt.contains("<!-- instructions from /tmp/proj/AGENTS.md -->"));
        assert!(
            !prompt.contains("<!-- instructions from /tmp/WIZARD.md -->"),
            "charter-identical section dropped"
        );
        assert_eq!(
            prompt
                .matches("Pick the lowest rung that solves it")
                .count(),
            0,
            "only the charter-identical section was dropped, and it stayed dropped"
        );
    }

    /// `read_prompt_override` returns trimmed contents for a non-empty file,
    /// and `None` for an empty or missing one (so the baked default is used).
    #[test]
    fn prompt_override_reads_nonempty_file_only() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();

        let present = dir.join(format!("wizard_prompt_override_{pid}.md"));
        std::fs::write(&present, "  CUSTOM EVOLVED PROMPT\n").expect("write temp prompt");
        assert_eq!(
            read_prompt_override(&present).as_deref(),
            Some("CUSTOM EVOLVED PROMPT"),
            "non-empty override should be read and trimmed"
        );
        std::fs::remove_file(&present).ok();

        let empty = dir.join(format!("wizard_prompt_override_empty_{pid}.md"));
        std::fs::write(&empty, "   \n\t").expect("write empty temp prompt");
        assert_eq!(
            read_prompt_override(&empty),
            None,
            "whitespace-only override should fall back to default"
        );
        std::fs::remove_file(&empty).ok();

        let missing = dir.join(format!("wizard_prompt_override_missing_{pid}.md"));
        assert_eq!(
            read_prompt_override(&missing),
            None,
            "missing override → None"
        );
    }

    /// The memory index appears verbatim under its own section when saved
    /// memories exist; without one, the bootstrap guidance still mentions
    /// the `memory` tool.
    #[test]
    fn memory_index_is_injected_when_present() {
        let index = "- [build-system](build-system.md) [project] — uses cargo with lto\n";
        let prompt = build_system_prompt(Mode::Genie, &[], None, Some(index));
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("### Memory index (MEMORY.md)"));
        assert!(prompt.contains(index));

        let prompt = build_system_prompt(Mode::Genie, &[], None, None);
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("`memory` tool"));
        assert!(
            !prompt.contains("### Memory index (MEMORY.md)"),
            "no index section without saved memories"
        );
    }

    /// What a memory *is* stays resident whether or not anything is saved yet:
    /// the four types (one has to be chosen on every save) and the link
    /// syntax. What decides whether a memory should exist at all moved to the
    /// manual, and the prompt has to say so, or the model never looks.
    #[test]
    fn memory_types_stay_resident_and_the_rules_become_a_lookup() {
        for index in [None, Some("- [x](x.md) [user] — y\n")] {
            let prompt = build_system_prompt(Mode::Genie, &[], None, index);
            for kind in crate::memory::MemoryType::ALL {
                assert!(
                    prompt.contains(&format!("`{kind}`")),
                    "the {kind} type is named (index: {index:?})"
                );
            }
            assert!(prompt.contains("[[wiki-style]]"));
            assert!(
                prompt.contains("`manual` topic `memory`"),
                "the prompt must point at the rules it no longer carries"
            );
            assert!(
                !prompt.contains("Never save what the repo already records"),
                "the rules are a lookup now, not a resident kilobyte"
            );
        }
    }

    /// The context-management block is a free-standing constant the agent loop
    /// appends after the composed base prompt. Sanity-check the guidance that
    /// models actually need is present, so a rewrite cannot silently drop it.
    #[test]
    fn context_prompt_teaches_compact_and_task_change_hygiene() {
        let text = CONTEXT_PROMPT;
        assert!(text.contains("## Context management"));
        assert!(text.contains("`compact`"));
        assert!(text.contains("[context pressure]"));
        assert!(text.contains("~/.wizard/sessions/"));
        assert!(text.contains("On task change"));
        assert!(text.contains("spawn_subagent"));
        assert!(text.contains("memory"));
        assert!(text.contains("high") || text.contains("critical"));
    }

    /// Models on the JSON protocol only know the tools this section names —
    /// it must carry the roster with each tool's argument schema, and stay
    /// bare when no tools are registered.
    #[test]
    fn tool_protocol_renders_the_roster_with_schemas() {
        let specs = vec![ToolSpec::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )];
        let section = render_tool_protocol(&specs);
        assert!(section.contains("You do not have native function calling"));
        assert!(section.contains("## Available tools"));
        assert!(section.contains("`read_file`: Read a file."));
        assert!(
            section.contains("\"path\""),
            "the argument schema is inlined"
        );

        let bare = render_tool_protocol(&[]);
        assert!(bare.contains("You do not have native function calling"));
        assert!(
            !bare.contains("## Available tools"),
            "no roster section without tools"
        );
    }

    /// The environment block answers the questions the conversation cannot:
    /// which shell will parse the command lines the model writes, which OS it
    /// is on, and what the user is looking at.
    ///
    /// The theme is pinned to this thread first. `active()` otherwise reads a
    /// process-wide slot that every `App::new` in the suite writes (see the
    /// note in `src/theme.rs`), so composing the prompt and then formatting the
    /// expectation would be two reads of a value another thread can change
    /// between them: a test that fails on timing rather than on behavior.
    #[test]
    fn environment_names_the_shell_os_and_theme() {
        let theme = crate::theme::active();
        let _pinned = crate::theme::pin(theme.clone());

        let prompt = build_system_prompt(Mode::Genie, &[], None, None);
        assert!(prompt.contains("## Environment"));
        assert!(
            prompt.contains(&format!("Shell: `{}`", crate::platform::shell::name())),
            "the prompt must name the shell that actually runs `execute`"
        );
        assert!(
            !prompt.contains("hooks"),
            "hooks spawn `sh` directly, so the shell line must not claim them"
        );
        assert!(prompt.contains(&format!("OS: {}", std::env::consts::OS)));
        assert!(prompt.contains(&format!("UI theme: `{}`", theme.name)));
    }

    /// Section order is a cache decision: the memory index is the only part
    /// that changes mid-session, so nothing may be appended after it, and the
    /// compiled-constant sections must come first.
    #[test]
    fn sections_run_stable_to_volatile() {
        let skills = vec![Skill {
            name: "demo".into(),
            meta: Default::default(),
            body: "do the thing".into(),
            path: PathBuf::from("/tmp/demo/SKILL.md"),
        }];
        let sections = system_prompt_sections(
            Mode::Genie,
            &skills,
            Some("<!-- instructions from /tmp/p/AGENTS.md -->\nuse cargo nextest"),
            Some("- [x](x.md) [user] saved fact\n"),
        );
        let names: Vec<&str> = sections.iter().map(|section| section.name).collect();
        assert_eq!(
            names,
            [
                "personality",
                "charter",
                "environment",
                "skills",
                "instructions",
                "memory",
                "memory-index",
            ],
            "prompt sections must stay ordered stable-first, volatile-last"
        );

        // Optional sections drop out entirely rather than emitting an empty
        // heading, which would cost tokens and cache nothing.
        let bare = system_prompt_sections(Mode::Genie, &[], None, None);
        let names: Vec<&str> = bare.iter().map(|section| section.name).collect();
        assert_eq!(names, ["personality", "charter", "environment", "memory"]);
    }

    /// The doctor breakdown must account for every byte of the prompt that is
    /// actually sent, and locate each section where it says it is. A breakdown
    /// that can drift is worse than none, because it is the number people
    /// would trust when they go looking for what to cut.
    ///
    /// So this walks the offsets the breakdown implies instead of restating its
    /// definitions: each section's bytes must sit at the running offset, the
    /// running offset plus the separators must land exactly on the end of the
    /// prompt, and no section may be a duplicate name (two rows labelled
    /// `memory` cannot be attributed to anything). Text appended outside the
    /// section list, a separator that stops matching, or a dropped section all
    /// fail here.
    #[test]
    fn the_breakdown_accounts_for_every_byte_of_the_prompt() {
        let index = "- [x](x.md) [user] a saved fact\n";
        let sections = system_prompt_sections(Mode::Genie, &[], None, Some(index));
        let prompt = build_system_prompt(Mode::Genie, &[], None, Some(index));

        let mut names: Vec<&str> = sections.iter().map(|section| section.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "section names must be unique");

        let mut offset = 0;
        for (position, section) in sections.iter().enumerate() {
            if position > 0 {
                assert_eq!(
                    &prompt[offset..offset + SECTION_SEPARATOR.len()],
                    SECTION_SEPARATOR,
                    "no separator before section {:?}",
                    section.name
                );
                offset += SECTION_SEPARATOR.len();
            }
            let end = offset + section.bytes();
            assert!(end <= prompt.len(), "{} runs past the prompt", section.name);
            assert_eq!(
                &prompt[offset..end],
                section.text,
                "section {:?} is not at the offset its byte counts imply",
                section.name
            );
            offset = end;
        }
        assert_eq!(
            offset,
            prompt.len(),
            "the breakdown covers {offset} of {} bytes; something reaches the model \
             without appearing in any section",
            prompt.len()
        );

        // Token estimates are per section but read as a budget for the whole,
        // so the parts must add up to the total the status bar shows. Each
        // section rounds up independently and the separators belong to no
        // section, which bounds the gap.
        let whole = crate::llm::estimate_tokens_from_chars(prompt.chars().count());
        let parts: u64 = sections.iter().map(|section| section.est_tokens()).sum();
        assert!(
            parts >= whole.saturating_sub(sections.len() as u64)
                && parts <= whole + sections.len() as u64,
            "per-section estimates sum to {parts}, the whole prompt measures {whole}"
        );
    }

    /// The breakpoint has to be the exact byte offset where the volatile tail
    /// starts. A caching layer that cuts one section early throws away the
    /// tokens the ordering was designed to save; one section late caches a
    /// prefix that changes and never hits.
    #[test]
    fn the_cache_breakpoint_is_the_stable_prefix() {
        let index = "- [build-system](build-system.md) [project] cargo with lto\n";
        let sections = system_prompt_sections(Mode::Genie, &[], None, Some(index));
        let prompt = join_sections(&sections);
        let cut = cache_breakpoint(&sections);

        assert!(
            prompt[..cut].ends_with(MEMORY_ESSENTIALS),
            "the stable prefix must run to the end of the last stable section"
        );
        assert!(
            !prompt[..cut].contains(index),
            "the memory index must fall on the volatile side of the cut"
        );
        assert!(prompt[cut..].starts_with("\n\n### Memory index (MEMORY.md)"));

        // With nothing saved there is no volatile section, so the whole prompt
        // is cacheable.
        let stable = system_prompt_sections(Mode::Genie, &[], None, None);
        assert_eq!(cache_breakpoint(&stable), join_sections(&stable).len());
    }

    /// Token ratchet for the assembled system prompt, one ceiling per mode.
    ///
    /// Only ever lower these numbers, exactly as `contrib/check-file-size.sh`
    /// says of its line limit. Raising one to make a new paragraph fit is how
    /// the prompt got to 8 KB in the first place: every addition was small,
    /// and nothing measured the total.
    ///
    /// Measured before the charter and the memory rules moved behind `manual`:
    /// ~2483 tokens (genie), ~3502 (sovereign). Measured after, with the
    /// environment block added: ~1170 and ~2189. The ceilings sit 5% above
    /// those so a longer OS, shell or theme name on another platform cannot
    /// trip the build.
    const PROMPT_TOKEN_CEILING: [(Mode, u64); 2] = [(Mode::Genie, 1228), (Mode::Sovereign, 2298)];

    /// The assembled prompt for a default configuration, as
    /// `Agent::compose_system_prompt` builds it: the composed base, plus the
    /// two blocks that are appended on every ordinary run (the todo tool is
    /// registered by default, and context guidance is unconditional). Plan
    /// mode, omakase and the JSON tool protocol are excluded because they are
    /// not the default, not because they are free.
    ///
    /// The personality prompt is passed in rather than resolved so the ratchet
    /// measures the baked default even on a machine with a
    /// `~/.wizard/system_prompt.md` override.
    fn assembled_default_prompt(mode: Mode) -> String {
        let base = match mode {
            Mode::Genie => GENIE_SYSTEM_PROMPT,
            Mode::Sovereign => SOVEREIGN_SYSTEM_PROMPT,
        };
        let sections = sections_from_base(base.to_string(), &[], None, None);
        format!(
            "{}{SECTION_SEPARATOR}{TODO_PROMPT}{SECTION_SEPARATOR}{CONTEXT_PROMPT}",
            join_sections(&sections)
        )
    }

    #[test]
    fn assembled_prompt_fits_the_token_ratchet() {
        for (mode, ceiling) in PROMPT_TOKEN_CEILING {
            let prompt = assembled_default_prompt(mode);
            let tokens = crate::llm::estimate_tokens_from_chars(prompt.chars().count());
            assert!(
                tokens <= ceiling,
                "the {mode} system prompt is ~{tokens} tokens, over its {ceiling}-token \
                 ratchet. Move depth behind `manual` (see manual_pages) instead of \
                 raising the ceiling.\n{}",
                breakdown(mode),
            );
            // A ratchet that is never tightened is a ceiling nobody notices
            // sliding. If a diet bought real headroom, bank it here.
            let slack = ceiling.saturating_sub(tokens) * 100 / ceiling;
            assert!(
                slack < 15,
                "the {mode} system prompt is ~{tokens} tokens, {slack}% under its \
                 {ceiling}-token ratchet. Lower PROMPT_TOKEN_CEILING to about {} and \
                 keep the win.\n{}",
                tokens + tokens / 20,
                breakdown(mode),
            );
        }
    }

    /// Per-section byte and token breakdown, in the shape `wizard doctor`
    /// prints. Used as the failure message of the ratchet, so a developer who
    /// trips it can see which section grew without instrumenting anything.
    fn breakdown(mode: Mode) -> String {
        let base = match mode {
            Mode::Genie => GENIE_SYSTEM_PROMPT,
            Mode::Sovereign => SOVEREIGN_SYSTEM_PROMPT,
        };
        sections_from_base(base.to_string(), &[], None, None)
            .iter()
            .map(|section| {
                format!(
                    "  {:<14} {:>6} bytes  ~{:>5} tokens\n",
                    section.name,
                    section.bytes(),
                    section.est_tokens()
                )
            })
            .collect()
    }
}
