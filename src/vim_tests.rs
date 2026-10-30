use super::*;

/// Feed `keys` (`<Esc>` for escape, `<BS>` for backspace) to a prompt holding `text`, with the
/// cursor on character `cur`, in NORMAL mode. Typing in INSERT is done here the way the
/// prompt does it.
fn run(text: &str, cur: usize, keys: &str) -> (String, usize) {
    let mut st = VimState {
        enabled: true,
        mode: VimMode::Normal,
        ..VimState::default()
    };
    drive(&mut st, text, cur, keys)
}

fn drive(st: &mut VimState, text: &str, cur: usize, keys: &str) -> (String, usize) {
    let mut text = text.to_string();
    let mut cur = cur;
    let mut chars = keys.chars().peekable();
    while let Some(c) = chars.next() {
        let key = if c == '<' {
            let mut name = String::new();
            for n in chars.by_ref() {
                if n == '>' {
                    break;
                }
                name.push(n);
            }
            match name.as_str() {
                "lt" => VimKey::Char('<'),
                "Esc" => VimKey::Esc,
                "BS" => VimKey::Backspace,
                "Left" => VimKey::Left,
                "Right" => VimKey::Right,
                _ => panic!("key {name}"),
            }
        } else {
            VimKey::Char(c)
        };
        if st.key(&mut text, &mut cur, key) == Out::Pass && st.mode == VimMode::Insert {
            let mut v: Vec<char> = text.chars().collect();
            match key {
                VimKey::Char(c) => {
                    v.insert(cur, c);
                    cur += 1;
                }
                VimKey::Backspace if cur > 0 => {
                    v.remove(cur - 1);
                    cur -= 1;
                }
                _ => {}
            }
            text = v.into_iter().collect();
        }
    }
    (text, cur)
}

/// What the fixture did: the text typed in INSERT from a fresh prompt, then Esc, `gg0` and the
/// keys. Typing is an undo step of its own there.
fn run_in_claude(text: &str, keys: &str) -> (String, usize) {
    let mut st = VimState {
        enabled: true,
        ..VimState::default()
    };
    let mut typed = String::new();
    let mut cur = 0;
    for c in text.chars() {
        st.key(&mut typed, &mut cur, VimKey::Char(c));
        let mut v: Vec<char> = typed.chars().collect();
        v.insert(cur, c);
        cur += 1;
        typed = v.into_iter().collect();
    }
    drive(&mut st, &typed, cur, &format!("<Esc>gg0{keys}"))
}

fn t(text: &str, cur: usize, keys: &str, want: &str) {
    let (got, _) = run(text, cur, keys);
    assert_eq!(got, want, "{text:?} at {cur}, keys {keys}");
}

#[test]
fn x_deletes_under_the_cursor() {
    t("hello", 0, "x", "ello");
    t("hello", 0, "3x", "lo");
    t("hello", 4, "x", "hell");
    t("hello", 2, "10x", "he");
}

#[test]
fn x_stops_at_a_line_end() {
    t("ab\ncd", 1, "5x", "a\ncd");
}

#[test]
fn word_motions() {
    assert_eq!(run("foo bar baz", 0, "w").1, 4);
    assert_eq!(run("foo bar baz", 0, "2w").1, 8);
    assert_eq!(run("foo bar baz", 0, "e").1, 2);
    assert_eq!(run("foo bar baz", 8, "b").1, 4);
    assert_eq!(run("foo.bar", 0, "w").1, 3);
    assert_eq!(run("foo.bar", 0, "W").1, 7);
    assert_eq!(run("foo bar", 0, "$").1, 6);
    assert_eq!(run("  foo", 4, "0").1, 0);
    assert_eq!(run("  foo", 4, "^").1, 2);
}

#[test]
fn w_stops_on_an_empty_line() {
    assert_eq!(run("foo\n\nbar", 0, "w").1, 4);
    assert_eq!(run("foo\n\nbar", 0, "2w").1, 5);
}

#[test]
fn dw_deletes_to_the_next_word() {
    t("foo bar baz", 4, "dw", "foo baz");
    t("foo bar baz", 0, "2dw", "baz");
    t("foo bar baz", 0, "d2w", "baz");
    t("foo bar baz", 0, "2d2w", "");
}

#[test]
fn dw_stops_at_the_line_end() {
    t("foo bar\nbaz", 4, "dw", "foo \nbaz");
    t("foo\n  bar", 0, "dw", "\n  bar");
}

#[test]
fn cw_changes_to_the_word_end() {
    t("foo bar", 0, "cwxy<Esc>", "xy bar");
    t("foo bar", 1, "cwxy<Esc>", "fxy bar");
    t("foo  bar", 3, "cwx<Esc>", "fooxbar");
}

#[test]
fn line_operators() {
    t("one\ntwo\nthree", 4, "dd", "one\nthree");
    t("one\ntwo\nthree", 8, "dd", "one\ntwo");
    t("one", 1, "dd", "");
    t("one\ntwo\nthree", 0, "2dd", "three");
    t("one\ntwo\nthree", 4, "ccxx<Esc>", "one\nxx\nthree");
    t("one\ntwo", 0, "yyjp", "one\ntwo\none");
    t("one\ntwo", 0, "yyP", "one\none\ntwo");
}

#[test]
fn d_dollar_and_big_d() {
    t("hello world", 5, "D", "hello");
    t("hello world", 5, "d$", "hello");
    t("ab\ncd", 0, "D", "\ncd");
    t("hello world", 6, "Cthere<Esc>", "hello there");
}

#[test]
fn d0_and_caret() {
    t("hello world", 6, "d0", "world");
    t("  hello", 5, "d^", "  lo");
}

#[test]
fn find_motions() {
    assert_eq!(run("a,b,c", 0, "f,").1, 1);
    assert_eq!(run("a,b,c", 0, "2f,").1, 3);
    assert_eq!(run("a,b,c", 0, "t,").1, 0);
    assert_eq!(run("a,b,c", 0, "f,;").1, 3);
    assert_eq!(run("a,b,c", 4, "F,").1, 3);
    assert_eq!(run("a,b,c", 4, "F,,").1, 3);
    assert_eq!(run("a,b,c", 0, "2t,").1, 2);
}

#[test]
fn operators_with_find() {
    t("a,b,c", 0, "df,", "b,c");
    t("a,b,c", 0, "dt,", ",b,c");
    t("a,b,c", 0, "d2f,", "c");
    t("a,b,c", 4, "dF,", "a,b");
    t("a,b,c", 0, "ct,x<Esc>", "x,b,c");
}

#[test]
fn text_objects() {
    t("foo bar baz", 5, "diw", "foo  baz");
    t("foo bar baz", 5, "daw", "foo baz");
    t("foo bar", 5, "daw", "foo");
    t("say \"hi there\" now", 6, "di\"", "say \"\" now");
    t("say \"hi there\" now", 6, "da\"", "say  now");
    t("f(a, (b), c)", 4, "di(", "f()");
    t("f(a, (b), c)", 7, "di(", "f(a, (), c)");
    t("f(a, (b), c)", 7, "da(", "f(a, , c)");
    t("{ x }", 2, "di{", "{}");
    t("[1, 2]", 1, "ci[9<Esc>", "[9]");
    t("foo-bar baz", 0, "diW", " baz");
}

#[test]
fn percent_jumps_between_brackets() {
    assert_eq!(run("(a [b] c)", 0, "%").1, 8);
    assert_eq!(run("(a [b] c)", 8, "%").1, 0);
    assert_eq!(run("x (a)", 0, "%").1, 4);
    t("(a b) c", 0, "d%", " c");
}

#[test]
fn replace_and_case() {
    t("hello", 0, "rx", "xello");
    t("hello", 0, "3rx", "xxxlo");
    t("hello", 3, "9rx", "hello");
    t("hello", 0, "~", "Hello");
    t("hello", 0, "3~", "HELlo");
}

#[test]
fn join_lines() {
    t("a\nb\nc", 0, "J", "a b\nc");
    t("a\n   b", 0, "J", "a b");
    t("a\nb\nc", 0, "3J", "a b c");
    t("a", 0, "J", "a");
}

#[test]
fn open_lines() {
    t("a\nb", 0, "ox<Esc>", "a\nx\nb");
    t("a\nb", 2, "Ox<Esc>", "a\nx\nb");
    t("a", 0, "ox<Esc>", "a\nx");
}

#[test]
fn insert_commands() {
    t("abc", 1, "ix<Esc>", "axbc");
    t("abc", 1, "ax<Esc>", "abxc");
    t("abc", 1, "Ax<Esc>", "abcx");
    t("  abc", 4, "Ix<Esc>", "  xabc");
    t("abc", 1, "sx<Esc>", "axc");
    t("abc\ndef", 5, "Sx<Esc>", "abc\nx");
}

#[test]
fn escape_from_insert_steps_left() {
    assert_eq!(run("abc", 3, "i<Esc>").1, 2);
    assert_eq!(run("abc", 0, "i<Esc>").1, 0);
    assert_eq!(run("abc", 2, "a<Esc>").1, 2);
}

#[test]
fn normal_cursor_stays_on_a_character() {
    assert_eq!(run("abc", 2, "l").1, 2);
    assert_eq!(run("abc", 2, "$").1, 2);
    assert_eq!(run("abc", 2, "x").1, 1);
    assert_eq!(run("ab\ncd", 1, "l").1, 1);
    assert_eq!(run("ab\ncd", 0, "j").1, 3);
    assert_eq!(run("ab\ncd", 0, "jk").1, 0);
}

#[test]
fn vertical_motions_keep_the_column() {
    assert_eq!(run("abcd\nx\nabcd", 3, "j").1, 5);
    assert_eq!(run("abcd\nx\nabcd", 3, "jj").1, 10);
    assert_eq!(run("abcd\nx\nabcd", 3, "jjk").1, 5);
}

#[test]
fn j_and_k_at_the_edges_reach_history() {
    let mut st = VimState {
        enabled: true,
        mode: VimMode::Normal,
        ..VimState::default()
    };
    let (mut text, mut cur) = ("a\nb".to_string(), 0usize);
    assert_eq!(
        st.key(&mut text, &mut cur, VimKey::Char('k')),
        Out::HistoryUp
    );
    assert_eq!(st.key(&mut text, &mut cur, VimKey::Char('j')), Out::Handled);
    assert_eq!(
        st.key(&mut text, &mut cur, VimKey::Char('j')),
        Out::HistoryDown
    );
}

#[test]
fn gg_and_g() {
    assert_eq!(run("a\nb\nc", 4, "gg").1, 0);
    assert_eq!(run("a\nb\nc", 0, "G").1, 4);
    assert_eq!(run("a\nb\nc", 0, "2G").1, 2);
    assert_eq!(run("a\nb\nc", 4, "2gg").1, 2);
    t("a\nb\nc", 2, "dG", "a");
    t("a\nb\nc", 2, "dgg", "c");
    t("a\nb\nc", 0, "dj", "c");
    t("a\nb\nc", 2, "dk", "c");
    t("a\nb\nc", 0, "yjGp", "a\nb\nc\na\nb");
}

#[test]
fn paste_variants() {
    t("abc", 0, "xp", "bac");
    t("abc", 0, "xP", "abc");
    t("abc", 0, "yl3p", "aaaabc");
    t("one two", 0, "dwP", "one two");
    t("one two", 0, "dw$p", "twoone ");
}

#[test]
fn indent() {
    t("a", 0, ">>", "  a");
    t("a\nb", 0, "2>>", "  a\n  b");
    t("    a", 4, "<lt><lt>", "  a");
    t("  a", 2, "<lt><lt>", "a");
    t(" a", 1, "<lt><lt>", "a");
}

#[test]
fn undo_steps_one_command_at_a_time() {
    t("abc", 0, "xxu", "bc");
    t("abc", 0, "xxuu", "abc");
    t("abc", 1, "ixy<Esc>u", "abc");
    t("one two", 0, "dwu", "one two");
}

#[test]
fn dot_repeats_the_last_change() {
    t("a b c d", 0, "dw.", "c d");
    t("a b c d", 0, "dw2.", "d");
    t("abcd", 0, "x.", "cd");
    t("one two", 0, "cwx<Esc>w.", "x x");
    t("a\nb\nc", 0, "ddj.", "b");
    t("abc", 0, "ix<Esc>.", "xxabc");
    t("a b", 0, "Ax<Esc>.", "a bxx");
    t("abcdef", 0, "rx.", "xbcdef");
    t("abcdef", 0, "rxl.", "xxcdef");
}

#[test]
fn counts_multiply() {
    t("a b c d e f", 0, "2d2w", "e f");
    t("abcdef", 0, "2d3l", "");
}

#[test]
fn visual_mode_selects_and_acts() {
    t("hello world", 0, "vex", " world");
    t("hello world", 0, "vey", "hello world");
    t("hello world", 0, "veyP", "hellohello world");
    t("hello world", 0, "vlld", "lo world");
    t("a\nb\nc", 2, "Vd", "a\nc");
    t("a\nb\nc", 0, "Vjd", "c");
    t("hello", 0, "vlU", "HEllo");
    t("hello", 0, "vl~", "HEllo");
    t("hello", 0, "vlrx", "xxllo");
    t("ab c", 0, "vec9<Esc>", "9 c");
    t("foo bar", 4, "viwd", "foo ");
    t("foo bar", 0, "vp", "foo bar");
}

#[test]
fn visual_swap_and_toggle() {
    let mut st = VimState {
        enabled: true,
        mode: VimMode::Normal,
        ..VimState::default()
    };
    drive(&mut st, "abc", 0, "v");
    assert_eq!(st.mode, VimMode::Visual);
    drive(&mut st, "abc", 0, "V");
    assert_eq!(st.mode, VimMode::VisualLine);
    drive(&mut st, "abc", 0, "V");
    assert_eq!(st.mode, VimMode::Normal);
    assert_eq!(st.label(), Some("NORMAL"));
}

#[test]
fn visual_paste_replaces_the_selection() {
    t("one two", 0, "yiwwviwp", "one one");
    t("one two", 0, "yiwwviwpP", "one ontwoe");
}

#[test]
fn visual_line_indent_and_join() {
    t("a\nb\nc", 0, "Vj>", "  a\n  b\nc");
    t("a\nb\nc", 0, "VjJ", "a b\nc");
}

#[test]
fn esc_drops_a_half_typed_command() {
    t("hello", 0, "d<Esc>x", "ello");
    t("hello", 0, "2<Esc>x", "ello");
}

#[test]
fn unknown_keys_pass_through() {
    let mut st = VimState {
        enabled: true,
        mode: VimMode::Normal,
        ..VimState::default()
    };
    let (mut text, mut cur) = ("abc".to_string(), 0usize);
    assert_eq!(st.key(&mut text, &mut cur, VimKey::Other), Out::Pass);
    assert_eq!(st.key(&mut text, &mut cur, VimKey::Char('Z')), Out::Pass);
    assert_eq!(text, "abc");
}

#[test]
fn selection_range() {
    let mut st = VimState {
        enabled: true,
        mode: VimMode::Normal,
        ..VimState::default()
    };
    let (mut text, mut cur) = ("one two".to_string(), 0usize);
    st.key(&mut text, &mut cur, VimKey::Char('v'));
    st.key(&mut text, &mut cur, VimKey::Char('e'));
    assert_eq!(st.selection(&text, cur), Some((0, 3)));
    st.key(&mut text, &mut cur, VimKey::Esc);
    assert_eq!(st.selection(&text, cur), None);
}

#[test]
fn unicode_is_one_character() {
    t("héllo wörld", 0, "dw", "wörld");
    t("héllo", 1, "x", "hllo");
    t("日本語", 0, "lx", "日語");
}

#[test]
fn empty_prompt_is_safe() {
    for keys in [
        "x", "dd", "dw", "p", "J", "~", "rx", "D", "$", "w", "b", "e", "u", ".", "yy", "cc<Esc>",
    ] {
        run("", 0, keys);
    }
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\t", "\t")
}

/// Where Claude Code 2.1.314 does something no vim does and the engine keeps vim's way: the
/// case, and what Claude did.
const NOT_FOLLOWED: &[(&str, &str, &str)] = &[(
    "one\\ntwo\\nthree",
    "3dd",
    "leaves the text alone when the count covers every line; 2dd and 5dd delete as usual",
)];

/// Every case in `tests/fixtures/vim-claude.tsv` was typed into Claude Code 2.1.314 by
/// `looks/crates/claudew/tools/vimdiff.py`; the engine has to end up with the same text and
/// the same cursor.
#[test]
fn matches_claude_code_2_1_314() {
    let mut bad = Vec::new();
    let mut redo = Vec::new();
    let mut total = 0;
    for line in include_str!("../tests/fixtures/vim-claude.tsv").lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 4 {
            continue;
        }
        if NOT_FOLLOWED
            .iter()
            .any(|(t, k, _)| *t == f[0] && *k == f[1])
        {
            continue;
        }
        total += 1;
        let text = unescape(f[0]);
        let want = unescape(f[2]);
        let want_cursor: i64 = f[3].parse().unwrap_or(-1);
        let (got, cur) = run_in_claude(&text, f[1]);
        // the screen shows no trailing blanks
        let got: String = got
            .split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        if got != want || cur as i64 != want_cursor {
            redo.push(format!("{}\t{}", f[0], f[1]));
            bad.push(format!(
                "{:?} keys {} : claude {:?} @{}, engine {:?} @{}",
                text, f[1], want, want_cursor, got, cur
            ));
        }
    }
    assert!(total > 100, "the fixture is missing");
    // VIM_REDO=file lists the cases that differ, to record them again with vimdiff.py
    if let Some(path) = std::env::var_os("VIM_REDO") {
        std::fs::write(path, redo.join("\n") + "\n").ok();
    }
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        total,
        bad.join("\n")
    );
}
