//! `wizard gateway setup`: the guided first run for the Telegram gateway.
//!
//! Everything this does could be done by hand, and the old instructions said
//! so: create a bot, put the token somewhere, then find your own chat id by
//! **messaging the bot, letting it refuse you, and reading the id out of the
//! journal**. That last step is the reason this module exists. It asks a
//! person to provoke an error, know that an error is expected, know where the
//! error goes on their init system, and copy a number out of it — and it is
//! the *only* documented way to fill a list that, left empty, makes a
//! correctly configured bot answer nobody. So a first run reads as a broken
//! install.
//!
//! The flow here is the same four facts in the order a person can act on
//! them: token, is the token real, which chat, write it down. Nothing about
//! the security model moves: `gateway.allowed_chat_ids` stays a closed
//! allow-list ([`super::is_authorized`]), this is simply the supported way to
//! put one id on it with the operator watching. Two things it will not do:
//!
//! - **Guess.** Discovery reports the chat a message arrived from and then
//!   asks. It never writes an id nobody confirmed, and it never widens the
//!   list to "anyone".
//! - **Prompt into the void.** It is interactive, so with no terminal it
//!   refuses and says what to do instead, in the same terms as
//!   [`crate::trust`]. A piped or supervised invocation must not block on a
//!   question or take a stray byte for consent.
//!
//! The token itself is a secret with one destination: `credentials.toml` at
//! 0600 via [`crate::credentials::store`]. It is never put in `config.toml`,
//! never in an error, never logged.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::telegram::{ChatSighting, Telegram};
use crate::config::Config;
use crate::credentials::GATEWAY_TOKEN;
use crate::trust::Console;

/// How long discovery waits for the operator to send a message before giving
/// up. Long enough to switch to a phone and type something, short enough that
/// a forgotten terminal is not a process holding a long-poll open all night.
const DISCOVERY_WAIT: Duration = Duration::from_secs(180);

/// Whether a blocking question may be put on this terminal.
///
/// Same rule, and the same reasoning, as [`crate::trust::Console`] and
/// `wizard skills install`: this command owns the terminal for the length of
/// the call — no raw mode, no alternate screen, no other reader on stdin —
/// but only when there *is* one. Piped into a script, or run from a unit, a
/// blocking `read_line` either never returns or takes whatever byte happens
/// to be on stdin as consent to allow-list a chat, so the answer there is
/// [`Console::Unavailable`] and this refuses rather than guessing.
fn console() -> Console {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Console::Owned
    } else {
        Console::Unavailable
    }
}

/// Ask a yes/no question. Anything but an explicit yes is a no, end of input
/// included. Only reachable once [`console`] has answered [`Console::Owned`].
fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    if std::io::stdout().flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Read one line of free text. `None` at end of input.
fn ask_line(question: &str) -> Option<String> {
    print!("{question}");
    if std::io::stdout().flush().is_err() {
        return None;
    }
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// The gate itself, taking the verdict rather than probing for it so the
/// refusal — the branch a service or a pipe takes — is testable without a
/// terminal to take away.
///
/// It refuses *first*, before the config load and long before anything is
/// written. A run with nobody watching must not paste half a setup and then
/// discover it cannot ask the question.
fn require_console(console: Console) -> Result<()> {
    if console == Console::Owned {
        return Ok(());
    }
    bail!(
        "`wizard gateway setup` is interactive and there is no terminal here \
         (stdin or stdout is not a tty).\n\
         Run it from a shell. To configure the gateway unattended instead:\n\
         \x20 1. put the bot token under [keys] telegram = \"...\" in \
         ~/.wizard/credentials.toml (mode 0600)\n\
         \x20 2. set kind = \"telegram\" and allowed_chat_ids = [<your chat id>] \
         under [gateway] in ~/.wizard/config.toml\n\
         See docs/gateway.md."
    )
}

/// Run the guided setup. Returns the process exit code.
pub async fn run() -> Result<i32> {
    require_console(console())?;

    let config = Config::load().context("loading config")?;
    println!("Wizard gateway setup — Telegram");
    println!();

    // --- 1. the token -----------------------------------------------------
    //
    // `Telegram::connect` already resolves stored credential then env var, in
    // that order and for a documented reason; asking it is how this stays one
    // precedence rule rather than two that drift. Its only fallible step is
    // the token, so an error here means "no token anywhere" and nothing else.
    // `pasted` records whether *this* run is what put the token on disk, for
    // the failure below.
    let (telegram, pasted) = match Telegram::connect(&config.gateway) {
        Ok(telegram) => {
            println!("Using the bot token Wizard already has.");
            (telegram, false)
        }
        Err(_) => {
            println!("No bot token yet. To create one:");
            println!("  1. open Telegram and message @BotFather");
            println!("  2. send /newbot and answer its two questions");
            println!("  3. copy the token it replies with (like 123456789:AA…)");
            println!();
            let Some(token) = ask_line("Paste the bot token: ") else {
                bail!("no token entered (end of input) — nothing was changed");
            };
            if token.is_empty() {
                bail!("no token entered — nothing was changed");
            }
            crate::credentials::store(GATEWAY_TOKEN, &token)
                .context("storing the Telegram bot token")?;
            println!(
                "Stored in {} (mode 0600). It is never written to config.toml.",
                credentials_path()
            );
            (
                Telegram::connect(&config.gateway)
                    .context("reading back the bot token that was just stored")?,
                true,
            )
        }
    };

    // --- 2. is it real ----------------------------------------------------
    let bot = match telegram.get_me().await {
        Ok(bot) => bot,
        Err(err) => {
            // A token that has never authenticated must not be left behind: a
            // stored credential *outranks* the env var (see
            // `Telegram::connect`), so a typo kept here would shadow a working
            // `WIZARD_TELEGRAM_TOKEN` and make every later command fail with
            // no clue where the bad value came from. Only the value this run
            // wrote is removed — one that was already there is somebody else's
            // decision, and the failure may be a network one.
            if pasted {
                let _ = crate::credentials::remove(GATEWAY_TOKEN);
                println!("The token was not kept.");
            }
            return Err(err).context("checking the bot token with Telegram");
        }
    };
    println!("Bot: {}", bot.label());
    println!();

    // --- 3. which chat ----------------------------------------------------
    let sighting = discover(telegram, &bot.username).await?;
    println!();
    println!("Message received. {}", describe(&sighting));

    // --- 4. write it down -------------------------------------------------
    write_step(&config, &sighting)?;

    // --- 5. what to run next ----------------------------------------------
    println!();
    println!("Done. To run the gateway:");
    println!("  cd <your project> && wizard --gateway   # foreground, Ctrl-C to stop");
    println!("  wizard gateway install                  # keep it running in the background");
    println!("Then send /help in the chat to see what it understands.");
    Ok(0)
}

/// `~/.wizard/credentials.toml`, for a message. Falls back to the literal
/// path when the home directory cannot be resolved: it is a hint, and being
/// unable to print one is not a reason to fail a setup that worked.
fn credentials_path() -> String {
    crate::credentials::path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "~/.wizard/credentials.toml".to_string())
}

/// Drain whatever is already queued, tell the operator to send a message, and
/// wait for it.
async fn discover(mut telegram: Telegram, username: &str) -> Result<ChatSighting> {
    // Before the instruction, not after: see `Telegram::drain_pending`. What
    // is already in the queue is not reliably the operator's own message, and
    // the id this flow offers to allow-list has to be the one they just sent.
    let dropped = telegram
        .drain_pending()
        .await
        .context("clearing the bot's pending updates")?;
    if dropped > 0 {
        println!(
            "(ignored {dropped} message(s) already waiting for this bot — \
             they may not be yours)"
        );
    }

    let target = if username.is_empty() {
        "your bot".to_string()
    } else {
        format!("@{username}")
    };
    println!("Now open Telegram and send any message to {target}.");
    println!(
        "Waiting up to {} seconds for it (Ctrl-C to stop)…",
        DISCOVERY_WAIT.as_secs()
    );

    match telegram
        .next_chat(DISCOVERY_WAIT)
        .await
        .context("waiting for a message to the bot")?
    {
        Some(sighting) => Ok(sighting),
        None => bail!(
            "no message arrived within {} seconds, so there is no chat id to add.\n\
             The token is fine — `getMe` answered — so the usual cause is messaging a \
             different bot than {target}. Re-run `wizard gateway setup` to try again.",
            DISCOVERY_WAIT.as_secs()
        ),
    }
}

/// One line naming the chat a message came from.
fn describe(sighting: &ChatSighting) -> String {
    let kind = if sighting.kind.is_empty() {
        String::new()
    } else {
        format!(" ({})", sighting.kind)
    };
    match &sighting.from {
        Some(from) => format!("Chat id {}{kind}, from {from}.", sighting.chat_id),
        None => format!("Chat id {}{kind}.", sighting.chat_id),
    }
}

/// Ask about the discovered id and, with a yes, fold it into config.toml.
fn write_step(config: &Config, sighting: &ChatSighting) -> Result<()> {
    let chat_id = sighting.chat_id;
    if config.gateway.allowed_chat_ids.contains(&chat_id) {
        println!("That chat is already in gateway.allowed_chat_ids — nothing to change.");
        return Ok(());
    }

    // Before the question, never after: this is the one thing about the
    // decision the operator cannot see for themselves.
    if let Some(warning) = crate::config::group_chat_warning(&[chat_id]) {
        println!();
        println!("WARNING: {warning}");
    }

    let path = Config::path().context("resolving ~/.wizard/config.toml")?;
    println!();
    if !confirm(&format!(
        "Add chat {chat_id} to gateway.allowed_chat_ids in {}?",
        path.display()
    )) {
        println!(
            "Not changed. To do it yourself, put this in {}:",
            path.display()
        );
        println!();
        println!("  [gateway]");
        println!("  kind = \"telegram\"");
        println!("  allowed_chat_ids = [{chat_id}]");
        return Ok(());
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    match allow_chat_id(&raw, chat_id)? {
        Edit::Unchanged => println!("{} already says that.", path.display()),
        Edit::Rewritten(contents) => {
            if let Some(parent) = path.parent() {
                crate::platform::secrets::create_private_dir(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, contents)
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "Added chat {chat_id} to gateway.allowed_chat_ids in {}.",
                path.display()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rewriting config.toml
// ---------------------------------------------------------------------------

/// Outcome of folding a chat id into the raw text of `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    /// The file already allows that chat over Telegram. Nothing to write.
    Unchanged,
    /// The new contents of the file.
    Rewritten(String),
}

/// Add `chat_id` to `gateway.allowed_chat_ids` (and set `gateway.kind` to
/// `"telegram"`, without which the id would sit in a file `--gateway` refuses
/// to act on) in the *text* of a config file, preserving everything else.
///
/// Text, not a `Config` round-trip, and this is the whole reason the function
/// exists. `Config::save` serializes the in-memory struct, which drops every
/// comment in the file and every key Wizard's structs do not model — so
/// "let me add your chat id" would silently rewrite a config the person hand
/// wrote. Here the only lines that move are the ones being set.
///
/// Adding is idempotent: an id already on the list is left alone rather than
/// appended twice.
///
/// The result is re-parsed and checked before it is returned, because the
/// rewrite is textual and TOML has spellings this does not model (a dotted
/// `gateway.kind = …` at top level, an inline `gateway = { … }`). Rather than
/// corrupt a file on one of them, an output that does not parse, does not say
/// what it was meant to say, or disturbs any other top-level key is an error
/// telling the operator what to add by hand.
fn allow_chat_id(raw: &str, chat_id: i64) -> Result<Edit> {
    let before: toml::Value = toml::from_str(raw).with_context(|| {
        "~/.wizard/config.toml does not parse as TOML, so it cannot be edited safely; \
         fix it first, then re-run"
    })?;

    let ids = allowed_ids(&before);
    let kind_ok = gateway_kind(&before) == Some("telegram");
    if kind_ok && ids.contains(&chat_id) {
        return Ok(Edit::Unchanged);
    }

    let mut out = raw.to_string();
    if !ids.contains(&chat_id) {
        let mut next = ids.clone();
        next.push(chat_id);
        let rendered = format!(
            "[{}]",
            next.iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        out = set_in_section(&out, "gateway", "allowed_chat_ids", &rendered);
    }
    if !kind_ok {
        out = set_in_section(&out, "gateway", "kind", "\"telegram\"");
    }

    let after: toml::Value = toml::from_str(&out).map_err(|err| hand_edit(chat_id, &err))?;
    let ok = gateway_kind(&after) == Some("telegram")
        && allowed_ids(&after).contains(&chat_id)
        && untouched_elsewhere(&before, &after);
    if !ok {
        bail!(hand_edit(
            chat_id,
            &"the rewrite did not come out as intended"
        ));
    }
    Ok(Edit::Rewritten(out))
}

/// The message for every way the rewrite can decline: say what happened, then
/// say the three lines that would have been written.
fn hand_edit(chat_id: i64, cause: &dyn std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "could not update ~/.wizard/config.toml automatically ({cause}), so nothing was \
         written. Add this by hand:\n\
         \x20 [gateway]\n\
         \x20 kind = \"telegram\"\n\
         \x20 allowed_chat_ids = [{chat_id}]"
    )
}

/// `gateway.allowed_chat_ids`, or an empty list.
fn allowed_ids(doc: &toml::Value) -> Vec<i64> {
    doc.get("gateway")
        .and_then(|gateway| gateway.get("allowed_chat_ids"))
        .and_then(toml::Value::as_array)
        .map(|ids| ids.iter().filter_map(toml::Value::as_integer).collect())
        .unwrap_or_default()
}

/// `gateway.kind`, when it is a string.
fn gateway_kind(doc: &toml::Value) -> Option<&str> {
    doc.get("gateway")
        .and_then(|gateway| gateway.get("kind"))
        .and_then(toml::Value::as_str)
}

/// Every top-level key except `gateway` still has the value it had.
///
/// The post-condition that matters: whatever else is in that file — providers,
/// keys, a `[ui]` section, something a newer Wizard writes and this one does
/// not model — has to come out the other side identical.
fn untouched_elsewhere(before: &toml::Value, after: &toml::Value) -> bool {
    let (Some(before), Some(after)) = (before.as_table(), after.as_table()) else {
        return false;
    };
    let others = |table: &toml::Table| -> Vec<(String, toml::Value)> {
        table
            .iter()
            .filter(|(key, _)| key.as_str() != "gateway")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    others(before) == others(after)
}

/// Set `key = value` inside `[section]` of raw TOML text, leaving every other
/// byte alone. `value` is already-rendered TOML (`[1, 2]`, `"telegram"`).
///
/// Three cases, in the order they are looked for: the key exists in that
/// section (replace it, span and all, so a multi-line array is not left with
/// orphaned lines); the section exists but the key does not (insert directly
/// under the header — not at the end of the section, where a `[gateway.sub]`
/// sub-table could have started and the key would land in the wrong table);
/// neither exists (append both at the end).
fn set_in_section(raw: &str, section: &str, key: &str, value: &str) -> String {
    let newline = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let ends_with_newline = raw.is_empty() || raw.ends_with('\n');
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    let assignment = format!("{key} = {value}");

    match locate(&lines, section, key) {
        Site::Key { start, end } => {
            lines.splice(start..=end, [assignment]);
        }
        Site::Section { header } => lines.insert(header + 1, assignment),
        Site::Missing => {
            if lines.last().is_some_and(|last| !last.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.push(assignment);
        }
    }

    let mut out = lines.join(newline);
    if ends_with_newline {
        out.push_str(newline);
    }
    out
}

/// Where a key does or does not live in a TOML file's lines.
#[derive(Debug, PartialEq, Eq)]
enum Site {
    /// `lines[start..=end]` is the assignment, values spanning lines included.
    Key { start: usize, end: usize },
    /// The section header is at this line; the key is not in the section.
    Section { header: usize },
    /// No such section.
    Missing,
}

fn locate(lines: &[String], section: &str, key: &str) -> Site {
    let mut header = None;
    let mut current: Option<String> = None;
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if let Some(name) = section_header(trimmed) {
            current = Some(name.to_string());
            if name == section && header.is_none() {
                header = Some(index);
            }
            index += 1;
            continue;
        }
        // Any assignment, not only the one being looked for: skipping past a
        // value's full span is what stops a line *inside* somebody else's
        // multi-line array from being read as a section header or a key.
        if let Some(name) = assignment_key(trimmed) {
            let end = value_end(lines, index);
            if current.as_deref() == Some(section) && name == key {
                return Site::Key { start: index, end };
            }
            index = end + 1;
            continue;
        }
        index += 1;
    }
    match header {
        Some(header) => Site::Section { header },
        None => Site::Missing,
    }
}

/// `[name]` → `Some("name")`. Array-of-tables headers (`[[name]]`) are not
/// sections in this sense and are deliberately not matched.
fn section_header(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix('[')?;
    if rest.starts_with('[') {
        return None;
    }
    let end = rest.find(']')?;
    Some(rest[..end].trim())
}

/// The key of `key = value` (bare or quoted), or `None` on any other line.
fn assignment_key(trimmed: &str) -> Option<&str> {
    let (left, right) = trimmed.split_once('=')?;
    if right.starts_with('=') {
        return None;
    }
    let name = left.trim().trim_matches('"');
    let bare = |ch: char| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.';
    (!name.is_empty() && name.chars().all(bare)).then_some(name)
}

/// Last line of the value that starts on line `start`: the same line for a
/// scalar, the closing bracket for an array or inline table that spans lines.
///
/// Counts brackets outside `#` comments. A `#` inside a *string* would end the
/// scan early, which is why the caller re-parses what comes out rather than
/// trusting this.
fn value_end(lines: &[String], start: usize) -> usize {
    let mut depth = 0i32;
    let mut index = start;
    loop {
        let line = &lines[index];
        let code = line.split('#').next().unwrap_or(line);
        for ch in code.chars() {
            match ch {
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 || index + 1 == lines.len() {
            return index;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(raw: &str, chat_id: i64) -> String {
        match allow_chat_id(raw, chat_id).expect("the rewrite succeeds") {
            Edit::Rewritten(out) => out,
            Edit::Unchanged => panic!("expected a rewrite, got Unchanged"),
        }
    }

    /// The case this command exists for: no config at all, or one with no
    /// `[gateway]` section. The section is appended whole, and everything the
    /// operator already had is still there, byte for byte.
    #[test]
    fn a_missing_gateway_section_is_appended_and_the_rest_is_left_alone() {
        let out = rewritten("", 123);
        assert_eq!(
            out,
            "[gateway]\nkind = \"telegram\"\nallowed_chat_ids = [123]\n"
        );

        let existing = "# my config\nmax_steps = 40\n\n[ui]\nvim = true\n";
        let out = rewritten(existing, 555);
        assert!(
            out.starts_with(existing),
            "the original file is a prefix of the result:\n{out}"
        );
        assert!(out.contains("[gateway]"), "{out}");
        assert!(out.contains("allowed_chat_ids = [555]"), "{out}");
        // Comments survive, which a `Config::save` round-trip would not do.
        assert!(out.contains("# my config"), "{out}");
    }

    /// An existing `[gateway]` section keeps its other keys, and the id is
    /// appended to the list rather than replacing it.
    #[test]
    fn an_existing_section_keeps_its_keys_and_the_id_is_appended() {
        let raw = "[gateway]\nkind = \"telegram\"\ntoken_env = \"MY_TOKEN\"\n\
                   allowed_chat_ids = [111]\n\n[ui]\nvim = true\n";
        let out = rewritten(raw, 222);
        assert!(out.contains("allowed_chat_ids = [111, 222]"), "{out}");
        assert!(out.contains("token_env = \"MY_TOKEN\""), "{out}");
        assert!(out.contains("[ui]\nvim = true"), "{out}");
        // One list, not two.
        assert_eq!(out.matches("allowed_chat_ids").count(), 1, "{out}");
        assert_eq!(out.matches("[gateway]").count(), 1, "{out}");
    }

    /// A `[gateway]` section that never had the key gets it, and a
    /// `kind = "none"` (or absent) is turned into `"telegram"` — an id on a
    /// list `--gateway` refuses to read is not a working setup.
    #[test]
    fn a_section_without_the_key_gains_it_and_the_kind_is_set() {
        let out = rewritten("[gateway]\ntoken_env = \"MY_TOKEN\"\n", 7);
        assert!(out.contains("allowed_chat_ids = [7]"), "{out}");
        assert!(out.contains("kind = \"telegram\""), "{out}");
        assert!(out.contains("token_env = \"MY_TOKEN\""), "{out}");

        let out = rewritten("[gateway]\nkind = \"none\"\nallowed_chat_ids = [1]\n", 2);
        assert!(out.contains("kind = \"telegram\""), "{out}");
        assert!(!out.contains("\"none\""), "{out}");
        assert!(out.contains("allowed_chat_ids = [1, 2]"), "{out}");
    }

    /// Idempotent: running setup twice for the same chat writes nothing the
    /// second time, and does not double the id.
    #[test]
    fn an_id_that_is_already_allowed_is_not_added_twice() {
        let raw = "[gateway]\nkind = \"telegram\"\nallowed_chat_ids = [111, 222]\n";
        assert_eq!(
            allow_chat_id(raw, 222).expect("no error"),
            Edit::Unchanged,
            "already allowed over telegram"
        );
        // Present but the kind is wrong: still a rewrite, and still one copy
        // of the id.
        let out = rewritten("[gateway]\nallowed_chat_ids = [222]\n", 222);
        assert!(out.contains("allowed_chat_ids = [222]"), "{out}");
        assert!(out.contains("kind = \"telegram\""), "{out}");
    }

    /// A list written across several lines is replaced as a unit; leaving the
    /// old closing bracket behind would produce a file that does not parse.
    #[test]
    fn a_multi_line_list_is_replaced_whole() {
        let raw = "[gateway]\nkind = \"telegram\"\nallowed_chat_ids = [\n  111,\n  222,\n]\n\
                   token_env = \"MY_TOKEN\"\n";
        let out = rewritten(raw, 333);
        assert!(out.contains("allowed_chat_ids = [111, 222, 333]"), "{out}");
        assert!(!out.contains("  111,"), "the old span is gone:\n{out}");
        assert!(out.contains("token_env = \"MY_TOKEN\""), "{out}");
        let parsed: toml::Value = toml::from_str(&out).expect("still parses");
        assert_eq!(allowed_ids(&parsed), vec![111, 222, 333]);
    }

    /// The `[gateway]` keys are set in the gateway table even when a
    /// sub-table follows it, which is what inserting under the header (rather
    /// than at the end of the section) buys.
    #[test]
    fn keys_land_in_the_gateway_table_not_a_following_sub_table() {
        let raw = "[gateway]\nkind = \"telegram\"\n\n[gateway.extra]\nsomething = 1\n";
        let out = rewritten(raw, 9);
        let parsed: toml::Value = toml::from_str(&out).expect("parses");
        assert_eq!(allowed_ids(&parsed), vec![9]);
        assert_eq!(
            parsed
                .get("gateway")
                .and_then(|g| g.get("extra"))
                .and_then(|e| e.get("something"))
                .and_then(toml::Value::as_integer),
            Some(1),
            "the sub-table is untouched:\n{out}"
        );
    }

    /// Unparseable input is refused rather than overwritten, and the refusal
    /// says what to write by hand.
    #[test]
    fn a_broken_config_is_refused_with_the_lines_to_add() {
        let err = allow_chat_id("this is not = = toml [[[", 5)
            .expect_err("a broken file cannot be edited safely");
        let text = format!("{err:#}");
        assert!(text.contains("does not parse"), "{text}");

        // And a spelling the textual rewrite does not model: a dotted
        // top-level key. Appending a `[gateway]` section under it is a
        // duplicate-key error, which the post-parse catches — so this refuses
        // with instructions instead of destroying the file.
        let err = allow_chat_id("gateway.kind = \"telegram\"\n", 5)
            .expect_err("an unmodelled spelling is refused, not corrupted");
        let text = format!("{err:#}");
        assert!(text.contains("allowed_chat_ids = [5]"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
    }

    /// Everything outside `[gateway]` is a post-condition, checked on the
    /// parsed values rather than by eye.
    #[test]
    fn no_other_key_changes_value() {
        let raw = "max_steps = 40\nactive_provider = \"anthropic\"\n\n\
                   [[providers]]\nname = \"anthropic\"\nkind = \"anthropic\"\n\n\
                   [gateway]\nallowed_chat_ids = [1]\n";
        let out = rewritten(raw, 2);
        let before: toml::Value = toml::from_str(raw).expect("parses");
        let after: toml::Value = toml::from_str(&out).expect("parses");
        assert!(untouched_elsewhere(&before, &after));
        assert_eq!(
            after.get("max_steps").and_then(toml::Value::as_integer),
            Some(40)
        );
        assert_eq!(allowed_ids(&after), vec![1, 2]);
    }

    /// Discovery reports the chat, its type and who sent it — the three facts
    /// needed to recognise your own chat before allowing it.
    #[test]
    fn a_sighting_names_the_chat_its_type_and_the_sender() {
        let line = describe(&ChatSighting {
            chat_id: 4242,
            kind: "private".to_string(),
            from: Some("Teddy Tennant (@teddy)".to_string()),
        });
        assert_eq!(line, "Chat id 4242 (private), from Teddy Tennant (@teddy).");

        // A channel post carries no sender; the line still names the chat.
        let line = describe(&ChatSighting {
            chat_id: -100123,
            kind: "supergroup".to_string(),
            from: None,
        });
        assert_eq!(line, "Chat id -100123 (supergroup).");
    }

    /// The line-level helpers, pinned directly: the parts of the rewrite that
    /// have no other way to go wrong loudly.
    #[test]
    fn the_line_scanner_knows_headers_from_assignments() {
        assert_eq!(section_header("[gateway]"), Some("gateway"));
        assert_eq!(section_header("  [gateway.extra]  "), Some("gateway.extra"));
        assert_eq!(section_header("[[providers]]"), None, "not a table");
        assert_eq!(section_header("x = [1]"), None);

        assert_eq!(assignment_key("kind = \"telegram\""), Some("kind"));
        assert_eq!(assignment_key("\"kind\" = 1"), Some("kind"));
        assert_eq!(assignment_key("# kind = 1"), None);
        assert_eq!(assignment_key("[gateway]"), None);
        assert_eq!(assignment_key("  111,"), None);

        let lines: Vec<String> = "a = [\n 1,\n]\nb = 2\n"
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(value_end(&lines, 0), 2, "the array spans three lines");
        assert_eq!(value_end(&lines, 3), 3, "a scalar is one line");
    }

    /// With no terminal this refuses, and says the two things to do by hand
    /// instead. It must not hang on a `read_line` that will never return, and
    /// it must not treat whatever byte is on a pipe as consent to allow-list
    /// a chat id — which is why the gate takes a declared [`Console`] rather
    /// than probing, exactly as [`crate::trust`] does.
    #[test]
    fn with_no_terminal_it_refuses_and_names_the_manual_route() {
        require_console(Console::Owned).expect("an owned terminal may be asked");

        let err = require_console(Console::Unavailable)
            .expect_err("a pipe or a unit must not be prompted");
        let text = format!("{err:#}");
        assert!(text.contains("interactive"), "{text}");
        assert!(text.contains("credentials.toml"), "{text}");
        assert!(text.contains("allowed_chat_ids"), "{text}");
        // The default is the refusing one: a caller that has not thought about
        // it gets the safe branch.
        assert!(require_console(Console::default()).is_err());
    }

    /// CRLF in, CRLF out: a config edited on Windows is not silently
    /// rewritten into LF by adding one key to it.
    #[test]
    fn line_endings_are_preserved() {
        let out = rewritten("[gateway]\r\nkind = \"telegram\"\r\n", 1);
        assert!(out.contains("\r\n"), "{out:?}");
        assert!(!out.contains("\n\n"), "no bare LF was introduced: {out:?}");
    }
}
