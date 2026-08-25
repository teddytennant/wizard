//! Persistent per-project memory under `~/.wizard/memory/<project-slug>/`.
//!
//! Each memory is one markdown file with a frontmatter header (name, type,
//! one-line description); `MEMORY.md` is an index regenerated from the entry
//! files on every save/delete. The index is injected into the system prompt
//! so the model can recall saved facts across sessions via the `memory`
//! tool. A memory body may point at related memories with `[[wiki-style]]`
//! links, resolved on read against the entry files — there is no link
//! database, just the directory.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::commands::MemoryAction;
use crate::config::Config;

/// Filename of the regenerated index inside a project's memory dir.
const INDEX_FILE: &str = "MEMORY.md";

/// What a memory is *about*. The type is what makes a recall selective — the
/// model reads the index and knows which entries bear on the turn — so every
/// saved memory carries one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryType {
    /// Who the user is: role, expertise, standing preferences.
    User,
    /// How the agent should work: corrections and confirmed approaches.
    Feedback,
    /// Ongoing work, goals, and constraints not derivable from the code.
    #[default]
    Project,
    /// A pointer to an external resource: URL, dashboard, ticket.
    Reference,
}

impl MemoryType {
    /// The frontmatter / index spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    /// Every type, for schemas and error messages.
    pub const ALL: [MemoryType; 4] = [
        MemoryType::User,
        MemoryType::Feedback,
        MemoryType::Project,
        MemoryType::Reference,
    ];
}

impl FromStr for MemoryType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "user" => Ok(Self::User),
            "feedback" => Ok(Self::Feedback),
            "project" => Ok(Self::Project),
            "reference" => Ok(Self::Reference),
            other => bail!("unknown memory type '{other}' (user|feedback|project|reference)"),
        }
    }
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One saved memory, as listed in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// Kebab-case slug; the file is `<name>.md`.
    pub name: String,
    /// What the memory is about. Entry files written before types existed
    /// have none in their frontmatter and read back as
    /// [`MemoryType::Project`] — an old store still loads.
    pub kind: MemoryType,
    /// One-line summary shown in the index.
    pub description: String,
}

/// A `[[wiki-style]]` link found in a memory's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLink {
    /// The linked memory's name, as written between the brackets.
    pub name: String,
    /// Whether a memory of that name is saved. A link to one that is not is
    /// not an error: it marks a memory worth writing later.
    pub saved: bool,
}

/// Handle to one project's memory directory. The directory is created
/// lazily on first write, so opening a store never touches the disk.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    /// Open the memory store for `project_root`:
    /// `~/.wizard/memory/<slug>/`, where the slug is the canonicalized root
    /// path with every non-alphanumeric character replaced by `-` (e.g.
    /// `-home-user-projects-app`).
    pub fn open(project_root: &Path) -> Result<Self> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let dir = Config::memory_dir()?.join(project_slug(&canonical));
        Ok(Self { dir })
    }

    /// Directory this store reads and writes (may not exist yet).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write (or overwrite) memory `name` and regenerate the index.
    /// `description` is flattened to one line.
    pub fn save(
        &self,
        name: &str,
        kind: MemoryType,
        description: &str,
        content: &str,
    ) -> Result<()> {
        validate_name(name)?;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let description = flatten(description);
        let body = format!(
            "---\nname: {name}\ndescription: {description}\nmetadata:\n  type: {kind}\n---\n\n{}\n",
            content.trim()
        );
        let path = self.entry_path(name);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        self.regenerate_index()
    }

    /// Full file contents of memory `name` (frontmatter included).
    pub fn read(&self, name: &str) -> Result<String> {
        validate_name(name)?;
        let path = self.entry_path(name);
        std::fs::read_to_string(&path)
            .with_context(|| format!("no memory named '{name}' ({})", path.display()))
    }

    /// The `[[wiki-style]]` links in `contents`, in first-appearance order and
    /// deduplicated, each marked with whether that memory is saved. Resolution
    /// is a file existence check — the directory *is* the link database.
    pub fn links(&self, contents: &str) -> Vec<MemoryLink> {
        link_names(contents)
            .into_iter()
            .map(|name| {
                let saved = validate_name(&name).is_ok() && self.entry_path(&name).is_file();
                MemoryLink { name, saved }
            })
            .collect()
    }

    /// Remove memory `name` and regenerate the index.
    pub fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let path = self.entry_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => self.regenerate_index(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("no memory named '{name}' ({})", path.display())
            }
            Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// All saved memories (name, type, description), sorted by name. An absent
    /// memory dir simply means no memories yet.
    pub fn list(&self) -> Result<Vec<MemoryEntry>> {
        let dir = match std::fs::read_dir(&self.dir) {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.dir.display()));
            }
        };
        let mut entries = Vec::new();
        for entry in dir {
            let path = entry?.path();
            let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy()) else {
                continue;
            };
            if path.extension().is_none_or(|ext| ext != "md")
                || path.file_name().is_some_and(|file| file == INDEX_FILE)
            {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            entries.push(MemoryEntry {
                name: stem.into_owned(),
                // A file with no type is one written before types existed:
                // load it as a project memory rather than failing the store.
                kind: parse_field(&contents, "type")
                    .and_then(|kind| kind.parse().ok())
                    .unwrap_or_default(),
                description: parse_field(&contents, "description").unwrap_or_default(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Contents of `MEMORY.md`, or `None` when it is absent or empty.
    pub fn index(&self) -> Result<Option<String>> {
        let path = self.dir.join(INDEX_FILE);
        match std::fs::read_to_string(&path) {
            Ok(contents) if contents.trim().is_empty() => Ok(None),
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn entry_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.md"))
    }

    /// Rebuild `MEMORY.md` from the entry files: one
    /// `- [name](name.md) [type] — description` line per memory. The index is
    /// derived, never appended to, so it cannot drift from the files.
    fn regenerate_index(&self) -> Result<()> {
        let mut index = String::new();
        for entry in self.list()? {
            index.push_str(&format!(
                "- [{0}]({0}.md) [{1}] — {2}\n",
                entry.name, entry.kind, entry.description
            ));
        }
        let path = self.dir.join(INDEX_FILE);
        std::fs::write(&path, index).with_context(|| format!("writing {}", path.display()))
    }
}

/// Answer a `/memory` command for `project_root`. The TUI and the GUI both
/// print this text, so the two surfaces cannot drift into describing the same
/// store differently.
pub fn report(project_root: &Path, action: &MemoryAction) -> String {
    let store = match MemoryStore::open(project_root) {
        Ok(store) => store,
        Err(err) => return format!("could not open memory store: {err:#}"),
    };
    match action {
        MemoryAction::List => match store.list() {
            Err(err) => format!("could not list memories: {err:#}"),
            Ok(entries) if entries.is_empty() => {
                format!("no memories saved yet ({})", store.dir().display())
            }
            Ok(entries) => {
                let mut text = format!("saved memories ({}):\n", store.dir().display());
                for entry in &entries {
                    text.push_str(&format!(
                        "  {} [{}] — {}\n",
                        entry.name, entry.kind, entry.description
                    ));
                }
                text.push_str("\n/memory read <name> · /memory forget <name>");
                text
            }
        },
        MemoryAction::Read(name) => match store.read(name) {
            Ok(contents) => contents.trim_end().to_string(),
            Err(err) => format!("{err:#}"),
        },
        MemoryAction::Forget(name) => match store.delete(name) {
            Ok(()) => format!("forgot memory '{name}'"),
            Err(err) => format!("{err:#}"),
        },
    }
}

/// Project root path → directory slug: every character that is not ASCII
/// alphanumeric becomes `-` (so `/home/user/app` → `-home-user-app`).
fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Reject anything that is not a kebab-case slug. This doubles as path
/// traversal protection: `/`, `\`, and `.` are not in the allowed set.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("memory name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("memory name '{name}' must be kebab-case (lowercase letters, digits, and hyphens)");
    }
    Ok(())
}

/// Collapse a description to a single trimmed line for the frontmatter and
/// the index.
fn flatten(description: &str) -> String {
    // Whitespace-collapsing is what stops a description forging a second index
    // row, and it is not enough on its own: `ESC`, the C1 introducers and the
    // bidi/zero-width set are not whitespace, so they survived — into an index
    // line that is pinned into this project's system prompt for every later
    // session. A page that talks the model into saving one memory gets
    // persistence, which is the reason to be strict here and not in a renderer.
    //
    // `crate::text`'s predicate, which the mesh's sanitiser and the web
    // plugin's `defang` also use, so there is one audited answer to "what is
    // invisible" rather than three that drift.
    description
        .chars()
        .filter(|ch| !crate::text::is_invisible(*ch))
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pull a `key: value` line out of the frontmatter block of an entry file.
/// Lines are trimmed first, so `type` is found whether it sits at the top
/// level or (as written) nested under `metadata:`.
fn parse_field(contents: &str, key: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// The `[[name]]` links in a memory body, in first-appearance order, without
/// duplicates. Whitespace-only and unclosed brackets are simply not links.
fn link_names(contents: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = contents;
    while let Some(open) = rest.find("[[") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find("]]") else { break };
        let name = rest[..close].trim().to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
        rest = &rest[close + 2..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp memory dir removed on drop.
    struct TempStore {
        store: MemoryStore,
    }

    impl TempStore {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            Self {
                store: MemoryStore { dir },
            }
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.store.dir());
        }
    }

    #[test]
    fn a_description_cannot_smuggle_escapes_into_the_system_prompt() {
        // The index line built from this is pinned into the project's system
        // prompt for every later session, so a page that persuades the model to
        // save one memory gets persistence. Collapsing whitespace stops a
        // forged second row; it does nothing about `ESC`, the C1 introducers,
        // or bidi and zero-width characters, none of which are whitespace.
        let hostile = "notes\u{1b}[2Jcleared\u{9b}31m and \u{202e}reversed\u{202c}\u{200b}";
        let flat = flatten(hostile);

        assert!(
            !flat.chars().any(char::is_control),
            "a control character reached the index: {flat:?}"
        );
        assert!(!flat.contains('\u{202e}'), "{flat:?}");
        assert!(!flat.contains('\u{200b}'), "{flat:?}");
        assert!(flat.contains("notes"), "{flat:?}");
        assert!(flat.contains("reversed"), "{flat:?}");

        // Still one line, still single-spaced — the property it always had.
        assert!(!flat.contains('\n'));
        assert_eq!(flatten("  a   b \n c "), "a b c");
    }

    #[test]
    fn save_read_delete_round_trip() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store
            .save(
                "build-system",
                MemoryType::Project,
                "uses cargo with lto",
                "Release builds use lto = true.",
            )
            .unwrap();

        let contents = store.read("build-system").unwrap();
        assert!(contents.starts_with("---\nname: build-system\n"));
        assert!(contents.contains("description: uses cargo with lto"));
        assert!(contents.contains("metadata:\n  type: project"));
        assert!(contents.contains("Release builds use lto = true."));

        let entries = store.list().unwrap();
        assert_eq!(
            entries,
            [MemoryEntry {
                name: "build-system".to_string(),
                kind: MemoryType::Project,
                description: "uses cargo with lto".to_string(),
            }]
        );

        store.delete("build-system").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(
            store.read("build-system").is_err(),
            "deleted memory is gone"
        );
    }

    /// Every type round-trips through the frontmatter, and the index carries
    /// it — the index is what the model reads, so a type that survives the
    /// file but not the index is not saved at all.
    #[test]
    fn every_type_round_trips_into_the_index() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        for kind in MemoryType::ALL {
            let name = format!("mem-{kind}");
            store.save(&name, kind, "one line", "body").unwrap();
        }

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), MemoryType::ALL.len());
        for kind in MemoryType::ALL {
            let entry = entries
                .iter()
                .find(|entry| entry.name == format!("mem-{kind}"))
                .expect("saved memory is listed");
            assert_eq!(entry.kind, kind);
        }

        let index = store.index().unwrap().expect("index exists");
        assert!(
            index.contains("- [mem-user](mem-user.md) [user] — one line\n"),
            "index carries the type: {index}"
        );
        assert!(index.contains("[feedback] — one line"));
        assert!(index.contains("[project] — one line"));
        assert!(index.contains("[reference] — one line"));
    }

    /// A memory file written before types existed has no `type` in its
    /// frontmatter. It must still load — as a project memory — and the
    /// regenerated index must not lose its description.
    #[test]
    fn an_untyped_file_from_the_old_format_still_loads() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(
            store.dir().join("legacy.md"),
            "---\nname: legacy\ndescription: written by an older wizard\n---\n\nStill here.\n",
        )
        .unwrap();

        let entries = store.list().unwrap();
        assert_eq!(
            entries,
            [MemoryEntry {
                name: "legacy".to_string(),
                kind: MemoryType::Project,
                description: "written by an older wizard".to_string(),
            }],
            "a missing type reads as project, not an error"
        );
        assert!(store.read("legacy").unwrap().contains("Still here."));

        // Saving anything regenerates the index from every file, including
        // this one: its description and type must survive that.
        store
            .save("new-one", MemoryType::User, "the user", "body")
            .unwrap();
        let index = store.index().unwrap().expect("index exists");
        assert!(
            index.contains("- [legacy](legacy.md) [project] — written by an older wizard\n"),
            "the old file keeps its description in the index: {index}"
        );
    }

    #[test]
    fn index_is_regenerated_on_save_and_delete() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        assert_eq!(store.index().unwrap(), None, "no index before first save");

        store
            .save("alpha", MemoryType::Project, "first fact", "A.")
            .unwrap();
        store
            .save("beta", MemoryType::Feedback, "second fact", "B.")
            .unwrap();
        let index = store.index().unwrap().expect("index exists");
        assert_eq!(
            index,
            "- [alpha](alpha.md) [project] — first fact\n\
             - [beta](beta.md) [feedback] — second fact\n"
        );

        store.delete("alpha").unwrap();
        let index = store.index().unwrap().expect("index still exists");
        assert_eq!(index, "- [beta](beta.md) [feedback] — second fact\n");

        store.delete("beta").unwrap();
        assert_eq!(store.index().unwrap(), None, "empty index reads as None");
    }

    #[test]
    fn save_overwrites_without_duplicating_index_lines() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store
            .save("pref", MemoryType::User, "old description", "old")
            .unwrap();
        store
            .save("pref", MemoryType::Feedback, "new description", "new")
            .unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "new description");
        assert_eq!(entries[0].kind, MemoryType::Feedback, "the type is updated");
        let index = store.index().unwrap().expect("index exists");
        assert_eq!(index.lines().count(), 1);
    }

    /// `[[links]]` are reported in body order, deduplicated, each marked with
    /// whether it resolves. A link to a memory nobody has written yet is not
    /// an error — it marks one worth writing.
    #[test]
    fn links_resolve_against_the_saved_files() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store
            .save("release-flow", MemoryType::Project, "how we ship", "…")
            .unwrap();
        store
            .save(
                "ci-setup",
                MemoryType::Project,
                "CI",
                "Gates [[release-flow]], see also [[test-policy]] and [[release-flow]].",
            )
            .unwrap();

        let contents = store.read("ci-setup").unwrap();
        assert_eq!(
            store.links(&contents),
            [
                MemoryLink {
                    name: "release-flow".to_string(),
                    saved: true,
                },
                MemoryLink {
                    name: "test-policy".to_string(),
                    saved: false,
                },
            ]
        );
    }

    #[test]
    fn bracket_text_that_is_not_a_link_is_not_reported() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        assert!(
            store
                .links("an [[ ]] blank, an [[unclosed and a [single] bracket")
                .is_empty()
        );
    }

    #[test]
    fn multiline_descriptions_are_flattened() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store
            .save("style", MemoryType::User, "line one\nline two", "body")
            .unwrap();
        assert_eq!(store.list().unwrap()[0].description, "line one line two");
    }

    #[test]
    fn names_must_be_kebab_case() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        for bad in ["", "../evil", "UPPER case", "with space", "dot.md", "a/b"] {
            assert!(
                store.save(bad, MemoryType::Project, "d", "c").is_err(),
                "must reject '{bad}'"
            );
            assert!(store.read(bad).is_err(), "read must reject '{bad}'");
            assert!(store.delete(bad).is_err(), "delete must reject '{bad}'");
        }
        store
            .save("kebab-case-2", MemoryType::Project, "fine", "ok")
            .unwrap();
    }

    #[test]
    fn types_parse_from_their_frontmatter_spelling() {
        for kind in MemoryType::ALL {
            assert_eq!(kind.as_str().parse::<MemoryType>().unwrap(), kind);
        }
        assert!("archived".parse::<MemoryType>().is_err());
    }

    /// A temp project directory, together with the memory store its path slugs
    /// to — what `/memory` reports on. Both are removed on drop.
    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp project");
            Self(dir)
        }

        fn store(&self) -> MemoryStore {
            MemoryStore::open(&self.0).expect("open store")
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.store().dir());
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `/memory`, `/memory read <name>`, `/memory forget <name>` — the one
    /// renderer both chat surfaces answer the command with.
    #[test]
    fn report_lists_reads_and_forgets() {
        let tmp = TempProject::new();

        assert!(
            report(&tmp.0, &MemoryAction::List).starts_with("no memories saved yet"),
            "an empty store says so plainly"
        );

        tmp.store()
            .save(
                "prefers-rust",
                MemoryType::User,
                "reaches for Rust over Python",
                "Rust for anything that ships.",
            )
            .unwrap();

        let listed = report(&tmp.0, &MemoryAction::List);
        assert!(
            listed.contains("  prefers-rust [user] — reaches for Rust over Python"),
            "the list carries the type: {listed}"
        );

        let shown = report(&tmp.0, &MemoryAction::Read("prefers-rust".to_string()));
        assert!(shown.contains("Rust for anything that ships."));
        assert!(
            report(&tmp.0, &MemoryAction::Read("absent".to_string()))
                .contains("no memory named 'absent'")
        );

        assert_eq!(
            report(&tmp.0, &MemoryAction::Forget("prefers-rust".to_string())),
            "forgot memory 'prefers-rust'"
        );
        assert!(report(&tmp.0, &MemoryAction::List).starts_with("no memories saved yet"));
        assert!(
            report(&tmp.0, &MemoryAction::Forget("prefers-rust".to_string()))
                .contains("no memory named 'prefers-rust'"),
            "forgetting twice is an honest error, not a silent success"
        );
    }

    #[test]
    fn delete_missing_memory_is_a_clear_error() {
        let tmp = TempStore::new();
        let err = tmp.store.delete("nope").expect_err("missing must fail");
        assert!(err.to_string().contains("no memory named 'nope'"));
    }

    #[test]
    fn slug_replaces_non_alphanumerics() {
        assert_eq!(
            project_slug(Path::new("/home/user/projects/my_app")),
            "-home-user-projects-my-app"
        );
    }
}
