//! The slash menu ranks commands the way Claude Code 2.1.314 does. The fixture is what the real
//! program showed for each query (every row, by scrolling the menu) over its own command list;
//! `tools/slash_rank.py` takes it.

use claudew::fuse::{self, Item};
use serde_json::Value;

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/slash-rank.json"
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

struct Cmd {
    name: String,
    description: String,
    /// What the menu prints when it is not the description searched.
    menu: Option<String>,
    aliases: Vec<String>,
}

fn commands(v: &Value) -> Vec<Cmd> {
    v["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| Cmd {
            name: c["name"].as_str().unwrap().into(),
            description: c["description"].as_str().unwrap().into(),
            menu: c["menu"].as_str().map(String::from),
            aliases: c["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().into())
                .collect(),
        })
        .collect()
}

#[test]
fn every_query_ranks_as_claude_code_does() {
    let v = fixture();
    let cmds = commands(&v);
    let aliases: Vec<Vec<&str>> = cmds
        .iter()
        .map(|c| c.aliases.iter().map(String::as_str).collect())
        .collect();
    let items: Vec<Item> = cmds
        .iter()
        .zip(&aliases)
        .map(|(c, a)| Item {
            name: &c.name,
            description: &c.description,
            aliases: a,
        })
        .collect();
    let mut bad = Vec::new();
    let ranks = v["ranks"].as_object().unwrap();
    for (query, want) in ranks {
        let want: Vec<&str> = want
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        let got: Vec<String> = fuse::rank(&items, query)
            .into_iter()
            .map(|i| match fuse::matched_alias(&items[i], query) {
                Some(a) => format!("/{} ({a})", items[i].name),
                None => format!("/{}", items[i].name),
            })
            .collect();
        // the capture scrolls through at most 29 rows; a longer list is checked as far as it goes
        let n = if want.len() >= 29 {
            want.len()
        } else {
            got.len().max(want.len())
        };
        let got: Vec<&str> = got.iter().take(n).map(String::as_str).collect();
        if got != want {
            bad.push(format!("/{query}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} queries differ:\n{}",
        bad.len(),
        ranks.len(),
        bad.join("\n")
    );
}

#[test]
fn the_fixture_has_sixty_queries_at_least() {
    assert!(fixture()["ranks"].as_object().unwrap().len() >= 60);
}

/// Prints, for each pair of commands the real program ranked next to each other while nothing
/// but position told them apart, "a before b". Run with `--ignored --nocapture`; the pairs fix
/// the order the real program keeps its commands in, which `commands` in the fixture follows.
#[test]
#[ignore]
fn registry_order_constraints() {
    let v = fixture();
    let cmds = commands(&v);
    let aliases: Vec<Vec<&str>> = cmds
        .iter()
        .map(|c| c.aliases.iter().map(String::as_str).collect())
        .collect();
    let items: Vec<Item> = cmds
        .iter()
        .zip(&aliases)
        .map(|(c, a)| Item {
            name: &c.name,
            description: &c.description,
            aliases: a,
        })
        .collect();
    let pos = |n: &str| cmds.iter().position(|c| c.name == n).unwrap();
    for (query, want) in v["ranks"].as_object().unwrap() {
        let keys: std::collections::HashMap<usize, (Vec<f64>, bool)> =
            fuse::rank_keys(&items, query)
                .into_iter()
                .map(|(i, k, d)| (i, (k, d)))
                .collect();
        let names: Vec<usize> = want
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                pos(r
                    .as_str()
                    .unwrap()
                    .trim_start_matches('/')
                    .split(' ')
                    .next()
                    .unwrap())
            })
            .collect();
        for w in names.windows(2) {
            let (Some((ka, da)), Some((kb, db))) = (keys.get(&w[0]), keys.get(&w[1])) else {
                continue;
            };
            if ka[..3] == kb[..3] && ((*da && *db) || ka[3] == kb[3]) {
                println!("ORDER {} {}", cmds[w[0]].name, cmds[w[1]].name);
            }
        }
    }
}

#[test]
fn the_alias_table_is_the_fixtures() {
    for c in commands(&fixture()) {
        assert_eq!(
            fuse::aliases_of(&c.name),
            c.aliases.as_slice(),
            "{}",
            c.name
        );
    }
}

#[allow(dead_code)]
mod common;

/// The same queries through the look's own screen: the rows the menu shows, in order, are the
/// first the real program showed (a name cut short keeps the end of it behind a `…`).
#[test]
fn the_menu_shows_the_first_five_rows_claude_code_does() {
    let v = fixture();
    let cmds = commands(&v);
    let mut bad = Vec::new();
    for (query, want) in v["ranks"].as_object().unwrap() {
        let mut hn = common::Harness::new("static-200x50.jsonl", 300, 50);
        hn.app.commands = cmds
            .iter()
            .map(|c| agent_core::SlashCommand {
                name: c.name.clone(),
                description: c.description.clone(),
                input_hint: String::new(),
            })
            .collect();
        hn.type_str(&format!("/{query}"));
        let screen = hn.render().plain();
        let got: Vec<String> = screen
            .lines()
            .filter_map(|l| {
                let rest = l
                    .strip_prefix("  \u{276f} ")
                    .or_else(|| l.strip_prefix("    "))?;
                let name = rest.split("  ").next()?.trim();
                (name.starts_with('/') || name.starts_with('\u{2026}')).then(|| name.to_string())
            })
            .collect();
        let want: Vec<&str> = want
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .take(5)
            .collect();
        // a long description takes a second row and a whole entry must fit, so the window can
        // hold fewer than five: what it does hold is the start of the real list
        let same = got.len() >= want.len().min(2)
            && got.len() <= want.len()
            && got
                .iter()
                .zip(&want)
                .all(|(g, w)| match g.strip_prefix('\u{2026}') {
                    Some(tail) => w.ends_with(tail),
                    None => g == w,
                });
        if !same {
            bad.push(format!("/{query}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} queries differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
