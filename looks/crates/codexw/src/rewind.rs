//! Backtrack (Esc Esc) and `/rewind` on wizard. Codex forks the thread before the chosen prompt;
//! wizard cannot fork over ACP, but it has its own `/rewind <turn>`: it restores the files the
//! agent edited and cuts the session back to before that turn. Wizard writes one turn marker per
//! turn into the session file, so the nth prompt on screen finds its turn number there, and the
//! flow is: confirm, send `/rewind <turn>`, wait for its answer, load the session again.

use std::path::Path;

/// One turn marker of `~/.wizard/sessions/<id>.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub turn: u64,
    pub prompt: String,
}

/// The markers of a session file in order; lines that are not markers are skipped.
pub fn markers(path: &Path) -> Vec<Marker> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            Some(Marker {
                turn: v.get("turn")?.as_u64()?,
                prompt: v.get("prompt")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// A prompt as wizard's turn list shows it: the first line, trimmed, at most 120 characters.
pub fn normalise(prompt: &str) -> String {
    prompt
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// Which turn each prompt on screen started. Slash commands that become prompts and steers do
/// not always make a turn, so the two lists are matched in order by text rather than by count.
pub fn turns_for(prompts: &[String], markers: &[Marker]) -> Vec<Option<u64>> {
    let mut at = 0;
    prompts
        .iter()
        .map(|p| {
            let want = normalise(p);
            let found = markers[at.min(markers.len())..]
                .iter()
                .position(|m| normalise(&m.prompt) == want)?;
            let m = &markers[at + found];
            at += found + 1;
            Some(m.turn)
        })
        .collect()
}

/// What wizard's `/rewind <turn>` answered: restored, or the reason it did not.
pub fn succeeded(answer: &str) -> bool {
    answer.starts_with("rewound to before turn")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(turn: u64, prompt: &str) -> Marker {
        Marker {
            turn,
            prompt: prompt.into(),
        }
    }

    #[test]
    fn markers_come_from_the_session_file_and_skip_everything_else() {
        let p = std::env::temp_dir().join(format!("cxw-rw-{}.jsonl", std::process::id()));
        std::fs::write(
            &p,
            "{\"cwd\":\"/x\",\"version\":1}\n{\"turn\":1,\"prompt\":\"first\"}\n{\"message\":{\"role\":\"user\"}}\nnot json\n{\"turn\":2,\"prompt\":\"second\"}\n",
        )
        .unwrap();
        let got = markers(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(got, vec![m(1, "first"), m(2, "second")]);
        assert!(markers(Path::new("/no/such/file")).is_empty());
    }

    #[test]
    fn prompts_match_by_text_in_order() {
        let ms = [m(1, "a"), m(2, "b"), m(3, "a")];
        let ps: Vec<String> = ["a", "b", "a"].map(String::from).to_vec();
        assert_eq!(turns_for(&ps, &ms), vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_prompt_without_a_marker_has_no_turn_and_does_not_eat_the_next_marker() {
        let ms = [m(5, "real question")];
        let ps: Vec<String> = ["/init something", "real question"]
            .map(String::from)
            .to_vec();
        assert_eq!(turns_for(&ps, &ms), vec![None, Some(5)]);
    }

    #[test]
    fn long_and_multi_line_prompts_match_on_their_first_line_cut_at_120() {
        let long = "x".repeat(300);
        let ms = [m(1, &"x".repeat(120)), m(2, "line one")];
        let ps = vec![long, "line one\nline two".to_string()];
        assert_eq!(turns_for(&ps, &ms), vec![Some(1), Some(2)]);
    }

    #[test]
    fn the_answer_decides() {
        assert!(succeeded(
            "rewound to before turn 2: no files needed restoring; conversation truncated"
        ));
        assert!(!succeeded("nothing to rewind yet"));
    }
}
