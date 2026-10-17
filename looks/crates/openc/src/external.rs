// OWNER: input
//! Running `$VISUAL`, `$EDITOR` and `$PAGER` on a file, and saying why when it does not work.

use std::path::Path;
use std::process::Command;

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// A fresh file for the text handed to an editor. Created exclusively (so it cannot be a
/// symlink somebody planted at a guessable name), mode 0600, with a random name, in
/// `$XDG_RUNTIME_DIR` when there is one (a directory only you can enter) and the temp
/// directory otherwise.
pub fn temp_file() -> io::Result<(PathBuf, File)> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..16 {
        let path = dir.join(format!("openc-{}.md", random_suffix()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0x20000) // O_NOFOLLOW on Linux; create_new already implies O_EXCL
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn random_suffix() -> String {
    let mut b = [0u8; 8];
    let ok = File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok();
    if !ok {
        // No urandom: time and pid are enough when create_new refuses a collision.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        b = ((n as u64) ^ (u64::from(std::process::id()) << 32)).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Run `cmd` on `path` with the terminal already handed back. `Ok` when it exits cleanly; the
/// error is one line for the status row.
///
/// `cmd` may carry arguments (`code -w`, `less -R`) and quoting, so a command line goes through
/// `sh` the way git runs its editor; a `cmd` that is itself a file, like `/my dir/ed.sh`, runs
/// as that file.
pub fn run(cmd: &str, path: &Path, what: &str) -> Result<(), String> {
    let status = if Path::new(cmd).is_file() {
        Command::new(cmd).arg(path).status()
    } else {
        // `$0` is the placeholder, `"$@"` the file, so a path with spaces stays one argument.
        Command::new("sh")
            .arg("-c")
            .arg(format!("{cmd} \"$@\""))
            .arg("sh")
            .arg(path)
            .status()
    };
    match status {
        Err(e) => Err(format!("could not run {what} {cmd}: {e}")),
        // The shell's own "not found" and "cannot execute" codes.
        Ok(s) if s.code() == Some(127) => Err(format!("{what} not found: {cmd}")),
        Ok(s) if s.code() == Some(126) => Err(format!("{what} is not executable: {cmd}")),
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "{what} {cmd} exited with {}",
            s.code()
                .map_or_else(|| "a signal".to_string(), |c| format!("status {c}"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("openc-ext-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_failing_editor_a_missing_one_and_a_clean_one_each_say_what_happened() {
        let d = dir("a");
        let f = d.join("msg.md");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(run("true", &f, "editor"), Ok(()));
        let e = run("false", &f, "editor").unwrap_err();
        assert!(e.contains("exited with status 1"), "{e}");
        let e = run("no-such-editor-xyz", &f, "editor").unwrap_err();
        assert!(e.contains("editor not found: no-such-editor-xyz"), "{e}");
    }

    #[test]
    fn a_path_with_a_space_and_a_command_with_arguments_both_run() {
        let d = dir("b");
        let sub = d.join("my dir");
        std::fs::create_dir_all(&sub).unwrap();
        let ed = sub.join("ed.sh");
        std::fs::write(&ed, "#!/bin/sh\nprintf edited > \"$1\"\n").unwrap();
        std::fs::set_permissions(&ed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = d.join("with space.md");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(run(ed.to_str().unwrap(), &f, "editor"), Ok(()));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "edited");
        // Arguments before the file.
        std::fs::write(&f, "x").unwrap();
        assert_eq!(
            run("sh -c 'printf viaargs > \"$1\"' sh", &f, "editor"),
            Ok(())
        );
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "viaargs");
    }

    #[test]
    fn the_temp_file_is_private_fresh_and_never_a_planted_link() {
        use std::os::unix::fs::PermissionsExt;
        let (p1, _f1) = temp_file().unwrap();
        let (p2, _f2) = temp_file().unwrap();
        assert_ne!(p1, p2, "two files share a name");
        assert_eq!(
            std::fs::metadata(&p1).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(p1.file_name().unwrap().to_string_lossy().len() > "openc-.md".len() + 8);
        // an existing path, link included, is refused rather than followed
        let d = dir("link");
        let victim = d.join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        let link = d.join("planted.md");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let r = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0x20000)
            .open(&link);
        assert!(r.is_err(), "a planted symlink was opened");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        let _ = std::fs::remove_file(p1);
        let _ = std::fs::remove_file(p2);
    }
}
