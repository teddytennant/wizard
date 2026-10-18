//! `/export`: the conversation as markdown in a file, the same in every look.

use std::path::{Path, PathBuf};

use crate::transcript::{Part, Role, Transcript};
use crate::{Event, NoticeLevel};

/// The conversation for a look whose own transcript is not a [`Transcript`]: every event and
/// prompt as it goes by, started over when the session changes.
#[derive(Debug, Default)]
pub struct Log(Transcript);

impl Log {
    pub fn apply(&mut self, ev: &Event) {
        if let Event::Ready { session_id, .. } = ev {
            if *session_id != self.0.session_id {
                self.0 = Transcript::new();
            }
        }
        self.0.apply(ev);
    }

    pub fn push_user(&mut self, text: &str) {
        self.0.push_user(text);
    }

    pub fn transcript(&self) -> &Transcript {
        &self.0
    }
}

/// What the user typed and what the model answered, with a line for each tool call.
pub fn markdown(t: &Transcript) -> String {
    let mut out = String::new();
    for m in &t.messages {
        let heading = match m.role {
            Role::User => "## You",
            Role::Assistant => "## Wizard",
            Role::Notice(_) => continue,
        };
        let mut body = Vec::new();
        for p in &m.parts {
            match p {
                Part::Text(s) if !s.trim().is_empty() => body.push(s.trim().to_string()),
                Part::Tool(c) => {
                    let what = if c.title.is_empty() {
                        &c.name
                    } else {
                        &c.title
                    };
                    body.push(format!("- `{what}`"));
                }
                _ => {}
            }
        }
        if !body.is_empty() {
            out.push_str(&format!("{heading}\n\n{}\n\n", body.join("\n\n")));
        }
    }
    out
}

/// Write the conversation to `arg`, or to `wizard-<session>.md` in `cwd` when `arg` is empty,
/// through [`write_new`]. The answer is what the look shows.
pub fn run(t: &Transcript, arg: &str, cwd: &Path) -> (NoticeLevel, String) {
    let md = markdown(t);
    if md.is_empty() {
        return (NoticeLevel::Info, "Nothing to export yet".into());
    }
    let name = target(arg, &t.session_id);
    match write_new(cwd, &name, &md) {
        Ok(path) => (
            NoticeLevel::Info,
            format!("Conversation exported to {}", path.display()),
        ),
        Err(e) => (NoticeLevel::Error, format!("Could not write {name}: {e}")),
    }
}

fn target(arg: &str, session: &str) -> String {
    let arg = arg.trim();
    if arg.is_empty() {
        return match session {
            "" => "wizard-conversation.md".to_string(),
            s => format!("wizard-{s}.md"),
        };
    }
    match (arg.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest).display().to_string(),
        _ => arg.to_string(),
    }
}

/// Write `text` to `name` (a relative name is taken against `cwd`) without replacing a file
/// that is already there: a taken name gets `-2`, `-3` and so on before the extension. Only the
/// owner can read the file. Returns the path written.
pub fn write_new(cwd: &Path, name: &str, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let first = cwd.join(name);
    let (stem, ext) = match first.extension() {
        Some(e) => (
            first.with_extension("").to_string_lossy().into_owned(),
            format!(".{}", e.to_string_lossy()),
        ),
        None => (first.to_string_lossy().into_owned(), String::new()),
    };
    let mut path = first;
    for n in 2..1000 {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        match open.open(&path) {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                path = PathBuf::from(format!("{stem}-{n}{ext}"));
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many files with that name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, ToolCall};

    fn talk() -> Transcript {
        let mut t = Transcript::new();
        t.apply(&Event::Ready {
            session_id: "s1".into(),
            config: Config::default(),
        });
        t.push_user("read the file");
        t.apply(&Event::Tool(ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            title: "Read main.rs".into(),
            ..Default::default()
        }));
        t.apply(&Event::TextDelta("It prints hello.".into()));
        t.apply(&Event::Notice {
            level: NoticeLevel::Info,
            text: "plan mode off".into(),
        });
        t
    }

    #[test]
    fn markdown_has_the_prompts_answers_and_tools_but_no_notices() {
        assert_eq!(
            markdown(&talk()),
            "## You\n\nread the file\n\n## Wizard\n\n- `Read main.rs`\n\nIt prints hello.\n\n"
        );
    }

    #[test]
    fn run_writes_the_named_file_or_one_named_for_the_session() {
        let dir = std::env::temp_dir().join(format!("agent-core-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, said) = run(&talk(), "", &dir);
        assert!(said.ends_with("wizard-s1.md"), "{said}");
        assert!(std::fs::read_to_string(dir.join("wizard-s1.md"))
            .unwrap()
            .starts_with("## You"));
        let (level, said) = run(&talk(), "out.md", &dir);
        assert_eq!(level, NoticeLevel::Info, "{said}");
        assert!(dir.join("out.md").exists());
        let (_, said) = run(&talk(), "out.md", &dir);
        assert!(
            said.ends_with("out-2.md"),
            "a second export keeps the first: {said}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("out.md"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let (level, _) = run(&talk(), "missing/out.md", &dir);
        assert_eq!(level, NoticeLevel::Error);
        assert_eq!(run(&Transcript::new(), "", &dir).1, "Nothing to export yet");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn log_starts_over_when_the_session_changes() {
        let mut log = Log::default();
        log.apply(&Event::Ready {
            session_id: "a".into(),
            config: Config::default(),
        });
        log.push_user("hi");
        log.apply(&Event::Ready {
            session_id: "a".into(),
            config: Config::default(),
        });
        assert_eq!(log.transcript().messages.len(), 1);
        log.apply(&Event::Ready {
            session_id: "b".into(),
            config: Config::default(),
        });
        assert!(log.transcript().messages.is_empty());
    }
}
