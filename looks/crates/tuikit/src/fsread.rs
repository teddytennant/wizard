//! Reading a file the model named, while drawing.
//!
//! A tool call carries any path its model chose, and a frame that wants the file (to show an
//! edit with its context) runs on the UI thread. So the read must never block and never run
//! away: only a regular file that resolves to somewhere under the working directory and is at
//! most `max` bytes. A FIFO would park the thread in `open`, `/dev/zero` would read until memory
//! ran out, a link out of the tree would show a file the user never opened.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

/// The contents of `path` as text, or `None` when it is not a regular file under `cwd`, is
/// larger than `max` bytes, or cannot be read.
pub fn read_regular_under(path: &str, cwd: &str, max: u64) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    let root = std::fs::canonicalize(cwd).ok()?;
    if !real.starts_with(&root) {
        return None;
    }
    // O_NONBLOCK so a file swapped for a FIFO after the check above still cannot park `open`.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&real)
        .ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.size() > max {
        return None;
    }
    let mut s = String::new();
    f.take(max).read_to_string(&mut s).ok()?;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_small_regular_files_under_the_cwd_are_read() {
        let d = std::env::temp_dir().join(format!("tuikit-fsread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let cwd = d.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("a.txt"), "hi").unwrap();
        std::fs::write(d.join("outside.txt"), "no").unwrap();
        std::fs::write(cwd.join("big.txt"), vec![b'x'; 101]).unwrap();
        std::os::unix::fs::symlink(d.join("outside.txt"), cwd.join("link.txt")).unwrap();
        let (c, p) = (cwd.to_string_lossy().into_owned(), |n: &str| {
            cwd.join(n).to_string_lossy().into_owned()
        });
        assert_eq!(
            read_regular_under(&p("a.txt"), &c, 100).as_deref(),
            Some("hi")
        );
        assert_eq!(
            read_regular_under(&p("big.txt"), &c, 100),
            None,
            "over the cap"
        );
        assert_eq!(
            read_regular_under(&p("link.txt"), &c, 100),
            None,
            "link out of the cwd"
        );
        assert_eq!(
            read_regular_under(&p("../outside.txt"), &c, 100),
            None,
            "dot-dot"
        );
        assert_eq!(read_regular_under("/dev/zero", &c, 100), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
