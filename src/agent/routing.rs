//! Decision-routed prompt loading. Before the first model call of a session,
//! one [`crate::decisions`] call scores the user's prompt against the
//! conditional sections of the sovereign prompt and against every installed
//! skill. Sections that clear `section_min` stay in the system prompt and the
//! rest are left out; skills that clear `skill_min` have their bodies loaded
//! next to the skills index.
//!
//! ## The system prompt is decided once
//!
//! It is the prefix of every request, so rewriting it on a later prompt would
//! throw the provider's prompt cache away. The first prompt's answer is
//! therefore final for the system prompt. A later prompt is still scored, and
//! a section or skill it needs that is not loaded yet is appended after it as
//! a system note, which leaves everything before it byte-identical.
//!
//! A call that fails or times out pins the unrouted prompt for the same
//! reason: the full prompt has gone out once, so it stays.

use std::time::Instant;

use tokio::sync::mpsc;

use super::prompts::{self, PromptSection, SOVEREIGN_SYSTEM_PROMPT};
use super::{Agent, AgentEvent, emit};
use crate::config::{DecisionsConfig, Mode};
use crate::decisions::{Question, Scores};
use crate::llm::ChatMessage;
use crate::skills::Skill;

/// A part of the sovereign prompt that only some tasks need.
pub struct Section {
    pub id: &'static str,
    /// The yes/no question that keeps it. One short positive statement: on
    /// gpt-6-luna a trailing "answer no when" clause pushed a plain question
    /// and "hi" from under 0.1 to 0.5 and above.
    pub question: &'static str,
    /// Where it sits in `sovereign_prompt.md`: a `## ` heading owns everything
    /// down to the next heading, a `- ` prefix owns that one bullet.
    parts: &'static [&'static str],
}

pub const SECTIONS: &[Section] = &[
    Section {
        id: "codebase",
        question: "The request is to change code in an existing project: fix a bug, add or change a feature, refactor, or write tests.",
        parts: &["## Changing an existing codebase"],
    },
    Section {
        id: "deliverables",
        question: "The request asks for a result to be written to a named output file, or in a stated format, schema or list of allowed IDs or labels.",
        parts: &[
            "## Required deliverables first",
            "- When the instruction lists allowed IDs",
            "- Match demonstration schemas",
            "- If multiple winning answers",
            "- For JSON/JSONL/fixed-format files",
        ],
    },
    Section {
        id: "services",
        question: "The request needs a server, daemon, virtual machine or other long-running process to be started and left running after the work is done.",
        parts: &["## Durable services"],
    },
    Section {
        id: "native-install",
        question: "The request involves building or installing a package from its source, or compiling a native extension (C, C++, Cython, Fortran or Rust) so that it can be imported.",
        parts: &["## Install-from-source / native extensions"],
    },
    Section {
        id: "images",
        question: "The request involves looking at an image, screenshot, photo, diagram, chart or game board to answer or act on it.",
        parts: &["- Images/boards/screenshots"],
    },
];

pub const MAX_SKILLS: usize = 3;
pub const MAX_SKILL_CHARS: usize = 8000;

/// What routing put in the system prompt. Fixed once set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routing {
    /// Conditional sections kept in the sovereign prompt. `None` leaves the
    /// personality whole: it was not the baked sovereign prompt when the
    /// session was routed, or the call failed.
    pub sections: Option<Vec<&'static str>>,
    /// Skills whose bodies sit in the system prompt.
    pub skills: Vec<String>,
    /// Section ids and skill names sent since as trailing notes.
    pub added: Vec<String>,
}

/// Each line of the sovereign prompt with the conditional section it belongs
/// to, if any.
fn owned_lines() -> impl Iterator<Item = (Option<&'static str>, &'static str)> {
    let owner = |line: &str| {
        SECTIONS
            .iter()
            .find(|s| s.parts.iter().any(|part| line.starts_with(part)))
            .map(|s| s.id)
    };
    let (mut heading, mut bullet) = (None, None);
    SOVEREIGN_SYSTEM_PROMPT
        .split_inclusive('\n')
        .map(move |line| {
            if line.starts_with("## ") {
                heading = owner(line);
                bullet = None;
            } else if line.starts_with("- ") {
                bullet = owner(line);
            } else if !line.starts_with("  ") {
                bullet = None;
            }
            (bullet.or(heading), line)
        })
}

/// The sovereign prompt with only the conditional sections in `keep`.
pub fn sovereign_with(keep: &[&str]) -> String {
    owned_lines()
        .filter(|(owner, _)| owner.is_none_or(|id| keep.contains(&id)))
        .map(|(_, line)| line)
        .collect()
}

/// The text of the conditional sections in `ids`, on their own.
fn section_text(ids: &[&str]) -> String {
    owned_lines()
        .filter(|(owner, _)| owner.is_some_and(|id| ids.contains(&id)))
        .map(|(_, line)| line)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// A skill routing may load: listed in the prompt, with a body that is not
/// already resident.
fn loadable(skill: &Skill) -> bool {
    crate::skills::skill_visible(skill) && !skill.meta.always && !skill.body.trim().is_empty()
}

fn section_question(section: &Section) -> Question {
    Question::new(format!("section_{}", section.id), section.question)
}

fn skill_question(index: usize, skill: &Skill) -> Question {
    Question::new(
        format!("skill_{index}"),
        format!(
            "A set of instructions named \"{}\" is available. It is for: {} Answer yes only when the user's request is that kind of task, so these instructions would help do it. Answer no for any other kind of task, and for greetings and small talk.",
            skill.name,
            skill
                .meta
                .description
                .as_deref()
                .unwrap_or(&skill.name)
                .trim()
        ),
    )
}

/// The sections in `asked` that clear `min`. One with no answer is kept: a
/// spare rule costs tokens, a missing one costs the task.
fn pick_sections(asked: &[&'static Section], scores: &Scores, min: f64) -> Vec<&'static str> {
    asked
        .iter()
        .filter(|s| {
            scores
                .get(&section_question(s).name)
                .is_none_or(|p| *p >= min)
        })
        .map(|s| s.id)
        .collect()
}

/// The skills in `asked` that clear `min`, best first, within the count and
/// size bounds.
fn pick_skills(asked: &[&Skill], scores: &Scores, min: f64) -> Vec<String> {
    let mut ranked: Vec<(&Skill, f64)> = asked
        .iter()
        .enumerate()
        .filter_map(|(i, s)| Some((*s, *scores.get(&format!("skill_{i}"))?)))
        .filter(|(_, p)| *p >= min)
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.name.cmp(&b.0.name)));
    let mut used = 0;
    let mut loaded = Vec::new();
    for (skill, _) in ranked {
        let size = skill.body.chars().count();
        if loaded.len() < MAX_SKILLS && used + size <= MAX_SKILL_CHARS {
            used += size;
            loaded.push(skill.name.clone());
        }
    }
    loaded
}

fn skill_bodies(skills: &[Skill], names: &[String]) -> String {
    names
        .iter()
        .filter_map(|name| skills.iter().find(|s| &s.name == name))
        .map(|s| format!("### {}\n\n{}", s.name, s.body.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Rewrite `sections` to what `routing` decided: the sovereign personality
/// cut to its kept sections, and the loaded skills' bodies after the skills
/// index. A personality that is not the baked sovereign prompt is left alone.
pub fn apply(sections: &mut Vec<PromptSection>, skills: &[Skill], routing: &Routing) {
    if let Some(keep) = &routing.sections
        && let Some(personality) = sections.iter_mut().find(|s| s.name == "personality")
        && personality.text == SOVEREIGN_SYSTEM_PROMPT
    {
        personality.text = sovereign_with(keep);
    }
    let bodies = skill_bodies(skills, &routing.skills);
    if let Some(index) = sections.iter().position(|s| s.name == "skills")
        && !bodies.is_empty()
    {
        sections.insert(
            index + 1,
            PromptSection {
                name: "skills-loaded",
                text: format!(
                    "## Skills for this task\n\nThese skills were chosen for this task and are already loaded. Follow them.\n\n{bodies}"
                ),
            },
        );
    }
}

fn list(names: &[impl AsRef<str>]) -> String {
    if names.is_empty() {
        return "none".to_string();
    }
    names
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(", ")
}

fn system_prompt_tokens(agent: &Agent) -> u64 {
    let chars = agent
        .history
        .first()
        .map_or(0, |m| m.text().chars().count());
    crate::llm::estimate_tokens_from_chars(chars)
}

impl Agent {
    /// Route `input` (see the module docs). Returns the trailing note a later
    /// prompt's additions travel in, for the caller to push after the user
    /// message. Does nothing unless a decision model is configured.
    pub(super) async fn route(
        &mut self,
        input: &str,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Option<ChatMessage> {
        let provider = self.decisions.clone()?;
        if self.mode == Mode::Chat {
            return None;
        }
        let config: DecisionsConfig = self.config.decisions.clone().unwrap_or_default();
        let first = self.routing.is_none();
        let have = self.routing.clone().unwrap_or_default();
        let held = |name: &str| {
            have.sections.as_ref().is_some_and(|s| s.contains(&name))
                || have.skills.iter().chain(&have.added).any(|n| n == name)
        };

        let sovereign = self.mode == Mode::Sovereign
            && prompts::base_system_prompt(self.mode) == SOVEREIGN_SYSTEM_PROMPT
            && (first || have.sections.is_some());
        let sections: Vec<&'static Section> = SECTIONS
            .iter()
            .filter(|s| sovereign && !held(s.id))
            .collect();
        let skills: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|s| loadable(s) && !held(&s.name))
            .collect();
        if sections.is_empty() && skills.is_empty() {
            self.routing.get_or_insert_default();
            return None;
        }
        let questions: Vec<Question> = sections
            .iter()
            .map(|s| section_question(s))
            .chain(skills.iter().enumerate().map(|(i, s)| skill_question(i, s)))
            .collect();

        let started = Instant::now();
        let answer = tokio::time::timeout(config.timeout(), provider.decide(input, &questions));
        let scores = match answer.await {
            Ok(Ok(scores)) => scores,
            failed => {
                match failed {
                    Ok(Err(err)) => tracing::warn!("decisions: {err:#}"),
                    _ => tracing::warn!("decisions: no answer in {} ms", config.timeout_ms),
                }
                self.routing.get_or_insert_default();
                return None;
            }
        };
        let ms = started.elapsed().as_millis();
        let picked_sections = pick_sections(&sections, &scores, config.section_min);
        let picked_skills = pick_skills(&skills, &scores, config.skill_min);

        let (summary, note) = if first {
            let unrouted = system_prompt_tokens(self);
            self.routing = Some(Routing {
                sections: sovereign.then(|| picked_sections.clone()),
                skills: picked_skills.clone(),
                added: Vec::new(),
            });
            self.refresh_system_prompt();
            let sections = if sovereign {
                format!("sections {}; ", list(&picked_sections))
            } else {
                String::new()
            };
            let summary = format!(
                "routed in {ms} ms: {sections}skills {}; system prompt ~{} tokens, ~{unrouted} unrouted",
                list(&picked_skills),
                system_prompt_tokens(self),
            );
            (summary, None)
        } else {
            let mut parts = Vec::new();
            let text = section_text(&picked_sections);
            if !text.is_empty() {
                parts.push(text);
            }
            let bodies = skill_bodies(&self.skills, &picked_skills);
            if !bodies.is_empty() {
                parts.push(format!("## Skills for this task\n\n{bodies}"));
            }
            let added: Vec<String> = picked_sections
                .iter()
                .map(|id| id.to_string())
                .chain(picked_skills)
                .collect();
            let summary = format!("routed in {ms} ms: added {}", list(&added));
            self.routing.get_or_insert_default().added.extend(added);
            let note = (!parts.is_empty()).then(|| {
                ChatMessage::system(format!(
                    "[routing] This request also calls for the guidance below. Follow it.\n\n{}",
                    parts.join("\n\n")
                ))
            });
            (summary, note)
        };
        emit(events, AgentEvent::Notice(summary.clone())).await;
        self.turn_routing = Some(summary);
        note
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillMeta;

    fn skill(name: &str, body: &str) -> Skill {
        Skill {
            name: name.to_string(),
            path: format!("/skills/{name}/SKILL.md").into(),
            meta: SkillMeta {
                description: Some(format!("{name} work.")),
                ..SkillMeta::default()
            },
            body: body.to_string(),
        }
    }

    fn scores(pairs: &[(&str, f64)]) -> Scores {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn all() -> Vec<&'static Section> {
        SECTIONS.iter().collect()
    }

    #[test]
    fn every_part_names_text_the_sovereign_prompt_still_has() {
        for section in SECTIONS {
            for part in section.parts {
                assert!(
                    part.starts_with("## ") || part.starts_with("- "),
                    "{part:?} is neither a heading nor a bullet"
                );
                assert_eq!(
                    SOVEREIGN_SYSTEM_PROMPT
                        .lines()
                        .filter(|line| line.starts_with(part))
                        .count(),
                    1,
                    "{part:?} must match exactly one line of sovereign_prompt.md"
                );
            }
        }
    }

    #[test]
    fn keeping_every_section_is_the_prompt_byte_for_byte() {
        let ids: Vec<&str> = SECTIONS.iter().map(|s| s.id).collect();
        assert_eq!(sovereign_with(&ids), SOVEREIGN_SYSTEM_PROMPT);
    }

    #[test]
    fn a_dropped_section_goes_whole_and_takes_nothing_else_with_it() {
        let core = sovereign_with(&[]);
        for gone in [
            "## Durable services",
            "nohup",
            "## Install-from-source",
            "site-packages",
            "## Changing an existing codebase",
            "## Required deliverables first",
            "cwe-93",
            "demonstration schemas",
            "multiple winning answers",
            "json.loads",
            "Images/boards",
            "FEN",
        ] {
            assert!(!core.contains(gone), "{gone:?} survived");
        }
        for kept in [
            "You are Wizard in sovereign mode",
            "## Contract fidelity",
            "- If a file is forbidden to modify",
            "## Verify before finishing",
            "- Do not claim success without evidence",
            "## Efficiency",
            "- Install needed tools early",
            "## Editing discipline",
            "- `old_string` must match exactly and uniquely.",
        ] {
            assert!(core.contains(kept), "{kept:?} was lost");
        }
        assert!(!core.contains("\n\n\n"));
        assert!(core.len() * 10 < SOVEREIGN_SYSTEM_PROMPT.len() * 7);

        let one = sovereign_with(&["services"]);
        assert!(one.contains("## Durable services") && one.contains("nohup"));
        assert!(!one.contains("## Install-from-source"));
        assert_eq!(
            section_text(&["images"]).lines().next(),
            Some("- Images/boards/screenshots: `read_file` returns the image. Look at it; don't")
        );
        assert!(section_text(&["services"]).ends_with("cleaned up when the agent ends."));
    }

    #[test]
    fn a_section_at_the_threshold_is_kept_and_so_is_one_with_no_answer() {
        let s = scores(&[
            ("section_codebase", 0.4),
            ("section_deliverables", 0.39),
            ("section_services", 0.0),
            ("section_images", 1.0),
        ]);
        assert_eq!(
            pick_sections(&all(), &s, 0.4),
            ["codebase", "native-install", "images"]
        );
    }

    #[test]
    fn skills_load_best_first_within_the_count_and_size_caps() {
        let installed = [
            skill("a", "x"),
            skill("b", "x"),
            skill("c", "x"),
            skill("d", "x"),
            skill("e", "x"),
        ];
        let asked: Vec<&Skill> = installed.iter().collect();
        let s = scores(&[
            ("skill_0", 0.6),
            ("skill_1", 0.59),
            ("skill_2", 0.9),
            ("skill_3", 0.7),
            ("skill_4", 0.8),
        ]);
        assert_eq!(pick_skills(&asked, &s, 0.6), ["c", "e", "d"]);
        assert!(pick_skills(&asked, &Scores::new(), 0.6).is_empty());

        let big = [
            skill("a", &"x".repeat(5000)),
            skill("b", &"y".repeat(5000)),
            skill("c", &"z".repeat(2000)),
        ];
        let asked: Vec<&Skill> = big.iter().collect();
        let s = scores(&[("skill_0", 0.9), ("skill_1", 0.8), ("skill_2", 0.7)]);
        assert_eq!(pick_skills(&asked, &s, 0.6), ["a", "c"]);
    }

    #[test]
    fn a_skill_question_carries_the_name_and_description() {
        let q = skill_question(2, &skill("frontend", "- body"));
        assert_eq!(q.name, "skill_2");
        assert!(q.instructions.contains("named \"frontend\""));
        assert!(
            q.instructions
                .contains("It is for: frontend work. Answer yes only")
        );
    }

    fn sections(skills: &[Skill]) -> Vec<PromptSection> {
        let mut sections = vec![PromptSection {
            name: "personality",
            text: SOVEREIGN_SYSTEM_PROMPT.to_string(),
        }];
        let index = crate::skills::render_for_prompt(skills);
        if !index.is_empty() {
            sections.push(PromptSection {
                name: "skills",
                text: index,
            });
        }
        sections.push(PromptSection {
            name: "memory",
            text: "## Memory".to_string(),
        });
        sections
    }

    #[test]
    fn applying_a_routing_cuts_the_personality_and_loads_bodies_after_the_index() {
        let skills = [
            skill("frontend", "- Use the design tokens."),
            skill("git", "- Rebase."),
        ];
        let before = sections(&skills);
        let mut routed = before.clone();
        apply(
            &mut routed,
            &skills,
            &Routing {
                sections: Some(vec!["codebase"]),
                skills: vec!["frontend".to_string()],
                added: Vec::new(),
            },
        );
        let names: Vec<&str> = routed.iter().map(|s| s.name).collect();
        assert_eq!(names, ["personality", "skills", "skills-loaded", "memory"]);
        assert_eq!(routed[0].text, sovereign_with(&["codebase"]));
        assert_eq!(routed[1], before[1], "the index is untouched");
        assert!(
            routed[2]
                .text
                .contains("### frontend\n\n- Use the design tokens.")
        );
        assert!(!routed[2].text.contains("Rebase"));

        let mut unrouted = before.clone();
        apply(&mut unrouted, &skills, &Routing::default());
        assert_eq!(unrouted, before);

        let mut overridden = before.clone();
        overridden[0].text = "A harness wrote this.".to_string();
        apply(
            &mut overridden,
            &skills,
            &Routing {
                sections: Some(Vec::new()),
                ..Routing::default()
            },
        );
        assert_eq!(overridden[0].text, "A harness wrote this.");
    }

    #[test]
    fn a_skill_that_is_hidden_resident_or_empty_is_not_asked_about() {
        assert!(loadable(&skill("a", "- body")));
        assert!(!loadable(&skill("a", "  ")));
        let mut resident = skill("a", "- body");
        resident.meta.always = true;
        assert!(!loadable(&resident));
        let mut gated = skill("a", "- body");
        gated.meta.when_env = vec!["WIZARD_TEST_ROUTING_ENV_THAT_IS_NOT_SET".to_string()];
        assert!(!loadable(&gated));
    }
}
