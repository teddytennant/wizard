//! Plaintext API keys for cloud providers, stored in
//! `~/.wizard/credentials.toml` (0600) keyed by provider name. Unlike
//! `config.toml`, which only ever names the env var holding a key, this file
//! holds the secret itself, so it is written atomically with tight
//! permissions and reads never hard-fail.
//!
//! "Tight permissions" is the platform's business, not this module's: the
//! write goes through [`crate::platform::secrets`], which owns the 0600/0700
//! mode bits today and the Windows ACL later. This file used to carry its own
//! copy of that sequence, one of three in the tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::platform::secrets;

/// On-disk shape of `credentials.toml`: a `[keys]` table mapping provider name
/// to its API key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

/// Key the messaging gateway's bot token is stored under.
///
/// The *name* is core's and the transport that reads it is not, which is the
/// same split [`crate::llm::registry`] makes for a provider `kind`: this file
/// owns the key namespace of `credentials.toml`, and a key namespace with a
/// hole in it on a build that left a plugin out is a namespace two features
/// can collide in. Onboarding writes the token here (core, and it asks for
/// one whether or not this build can run a gateway, because the config it is
/// writing outlives the binary that wrote it) and `plugins::gateway` reads it
/// back. Naming it in one place is what stops a paste and a read drifting
/// apart into a gateway that reports "token not set" over a token that was
/// pasted.
pub const GATEWAY_TOKEN: &str = "telegram";

/// `~/.wizard/credentials.toml`.
pub fn path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("credentials.toml"))
}

/// Read and parse the store at `path`. A missing file yields an empty store;
/// a parse error is logged and also yields an empty store — reads never
/// hard-fail, so a corrupt file degrades to "no stored keys" rather than
/// breaking provider setup.
fn load_at(path: &Path) -> Store {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Store::default(),
        Err(err) => {
            tracing::warn!("could not read {}: {err}", path.display());
            return Store::default();
        }
    };
    match toml::from_str(&raw) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!("could not parse {}: {err}", path.display());
            Store::default()
        }
    }
}

/// The stored key for `name` at `path`, or `None` when absent or empty.
fn get_at(path: &Path, name: &str) -> Option<String> {
    load_at(path)
        .keys
        .get(name)
        .filter(|key| !key.is_empty())
        .cloned()
}

/// Insert `key` for `name` and persist atomically, owner-only: the parent dir
/// is created and tightened, then a private temp file is written and renamed
/// over the target.
fn store_at(path: &Path, name: &str, key: &str) -> Result<()> {
    let mut store = load_at(path);
    store.keys.insert(name.to_string(), key.to_string());
    write_at(path, &store)
}

/// Remove `name` from the store and persist the result.
fn remove_at(path: &Path, name: &str) -> Result<()> {
    let mut store = load_at(path);
    store.keys.remove(name);
    write_at(path, &store)
}

/// Serialize `store` to `path` as a private file, atomically.
///
/// A failure to make the file (or its directory) private aborts the write.
/// That is the strict half of [`secrets`]' two policies, and the right one
/// here: the state tree degrades to a warning on a filesystem with no mode
/// bits so Wizard still starts, but a plaintext API key that cannot be kept
/// from other local users is better not written at all.
fn write_at(path: &Path, store: &Store) -> Result<()> {
    let raw = toml::to_string_pretty(store).context("serializing credentials")?;
    secrets::write_private_atomic(path, raw.as_bytes())
        .with_context(|| format!("storing credentials in {}", path.display()))
}

/// The stored API key for provider `name`, or `None` when none is set.
pub fn get(name: &str) -> Option<String> {
    let path = path().ok()?;
    get_at(&path, name)
}

/// Store `key` as the API key for provider `name`, persisting atomically
/// (0600).
pub fn store(name: &str, key: &str) -> Result<()> {
    store_at(&path()?, name, key)
}

/// Remove any stored API key for provider `name`.
pub fn remove(name: &str) -> Result<()> {
    remove_at(&path()?, name)
}

/// Strictly parse the store at `path` for diagnostics (`wizard doctor`),
/// returning the number of stored keys. Unlike normal reads — which degrade a
/// corrupt file to "no stored keys" — read and parse failures are errors here.
pub fn parse_strict(path: &Path) -> Result<usize> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let store: Store =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(store.keys.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_get_remove_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");

        // Missing file: nothing stored.
        assert_eq!(get_at(&path, "openai"), None);

        // Store then read back.
        store_at(&path, "openai", "sk-test-123").expect("store");
        assert_eq!(get_at(&path, "openai"), Some("sk-test-123".to_string()));

        // A second key coexists with the first.
        store_at(&path, "claude", "sk-ant-456").expect("store second");
        assert_eq!(get_at(&path, "openai"), Some("sk-test-123".to_string()));
        assert_eq!(get_at(&path, "claude"), Some("sk-ant-456".to_string()));

        // Empty values read back as None.
        store_at(&path, "blank", "").expect("store empty");
        assert_eq!(get_at(&path, "blank"), None);

        // Remove drops just the one key.
        remove_at(&path, "openai").expect("remove");
        assert_eq!(get_at(&path, "openai"), None);
        assert_eq!(get_at(&path, "claude"), Some("sk-ant-456".to_string()));
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");
        std::fs::write(&path, "this is not valid toml = = =").expect("write garbage");
        assert_eq!(get_at(&path, "openai"), None);
    }

    #[test]
    fn parse_strict_counts_keys_and_surfaces_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");

        // Missing file is an error under strict parsing (doctor skips it
        // before calling this).
        assert!(parse_strict(&path).is_err());

        store_at(&path, "openai", "sk-test").expect("store");
        store_at(&path, "claude", "sk-ant").expect("store");
        assert_eq!(parse_strict(&path).expect("valid store"), 2);

        std::fs::write(&path, "this is not valid toml = = =").expect("write garbage");
        let err = parse_strict(&path).expect_err("corrupt store must error");
        assert!(format!("{err:#}").contains("parsing"), "got: {err:#}");
    }

    #[test]
    fn stored_file_is_private() {
        // The exact protection, not merely "no wider than": `wizard doctor`
        // and every reader here assume the file is still writable by its
        // owner, and an unpinned create under a hostile umask produces one
        // that is not.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");
        store_at(&path, "openai", "sk-test").expect("store");
        assert!(
            secrets::is_private_file(&path).expect("stat"),
            "the credentials file is {}",
            secrets::protection_summary(&path)
        );
    }

    #[test]
    fn a_store_hardens_its_directory_and_leaves_no_readable_copy() {
        // Routing the write through the platform layer must not lose any of
        // the three properties the hand-rolled version had: a private parent
        // (created if missing), a private file, and no leftover temp file
        // holding the same key at whatever mode it was created with.
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("fresh-home");
        let path = dir.join("credentials.toml");
        store_at(&path, "openai", "sk-secret").expect("store");

        assert!(
            secrets::is_private_dir(&dir).expect("stat"),
            "the credentials directory is {}",
            secrets::protection_summary(&dir)
        );
        assert!(secrets::is_protected(&path).expect("stat"));

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "credentials.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");

        // A directory an older release left world-readable is tightened by the
        // next write rather than trusted.
        secrets::expose_to_other_users(&dir).expect("loosen");
        store_at(&path, "claude", "sk-ant").expect("store again");
        assert!(
            secrets::is_private_dir(&dir).expect("stat"),
            "a loose directory stayed {}",
            secrets::protection_summary(&dir)
        );
        assert_eq!(get_at(&path, "openai"), Some("sk-secret".to_string()));
    }
}
