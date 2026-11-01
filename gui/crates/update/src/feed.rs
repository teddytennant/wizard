//! The release feed: GitHub releases of teddytennant/wizard.
//!
//! Only two small files are fetched to learn what is newest, both through the
//! `releases/latest/download/` redirect rather than the rate-limited API:
//! `gui-checksums.txt` and its minisign signature. The signature is checked
//! against the compiled-in release key, the version is read out of the asset
//! names it lists (`wizard-gui-<ver>-<platform>.<ext>`), and the trusted
//! comment has to name that same `v<ver>`. The asset for this machine is then
//! downloaded from `releases/download/v<ver>/` and has to match its signed
//! sha256 before anything else touches it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use futures::StreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::signature::{self, ReleaseKey};

/// Where Wizard's releases live.
pub const DEFAULT_RELEASES_URL: &str = "https://github.com/teddytennant/wizard/releases";

/// Mirror or test override for [`DEFAULT_RELEASES_URL`]. Has to be an HTTPS
/// base URL laid out like GitHub's (`latest/download/…`, `download/v<ver>/…`,
/// `tag/v<ver>`). It changes where files come from, never what is trusted.
pub const RELEASES_URL_ENV: &str = "WIZARD_GUI_RELEASES_URL";

const CHECKSUMS: &str = "gui-checksums.txt";
const ASSET_PREFIX: &str = "wizard-gui-";
/// Cap on the two metadata files, read into memory before anything verifies
/// them.
const MAX_METADATA_BYTES: usize = 64 * 1024;

/// The feed base: the override when set, GitHub otherwise.
pub fn releases_base() -> Result<String> {
    match std::env::var(RELEASES_URL_ENV) {
        Ok(url) if !url.trim().is_empty() => validate_release_override(&url),
        _ => Ok(DEFAULT_RELEASES_URL.to_string()),
    }
}

pub(crate) fn validate_release_override(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value.trim()).context("invalid update feed URL")?;
    ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "update feed must use HTTPS"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "update feed must be a base URL without credentials, query, or fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// This machine's `<platform>` and archive extension in the asset names
/// release.yml publishes, or `None` where no build is published.
pub fn platform_asset(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    match (os, arch) {
        ("linux", "x86_64") => Some(("linux-x86_64", "tar.gz")),
        ("linux", "aarch64") => Some(("linux-aarch64", "tar.gz")),
        ("macos", "aarch64") => Some(("macos-arm64", "dmg")),
        ("windows", "x86_64") => Some(("windows-x86_64", "zip")),
        _ => None,
    }
}

pub fn current_platform_asset() -> Option<(&'static str, &'static str)> {
    platform_asset(std::env::consts::OS, std::env::consts::ARCH)
}

/// `wizard-gui-3.7.0-linux-x86_64.tar.gz` → (`3.7.0`, `linux-x86_64`, `tar.gz`).
/// Versions may carry a pre-release (`3.7.0-rc.1`), so the split is at the
/// platform's OS, not at the first dash.
pub fn parse_asset_name(name: &str) -> Option<(&str, &str, &str)> {
    let rest = name.strip_prefix(ASSET_PREFIX)?;
    ["linux", "macos", "windows"].iter().find_map(|os| {
        let at = rest.rfind(&format!("-{os}-"))?;
        let (version, tail) = (&rest[..at], &rest[at + 1..]);
        let (platform, ext) = tail.split_once('.')?;
        (!version.is_empty() && !platform.contains('/') && !ext.is_empty())
            .then_some((version, platform, ext))
    })
}

/// A verified release: its version and the signed sha256 of every asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    /// Asset name → lowercase sha256 hex.
    pub assets: BTreeMap<String, String>,
    base: String,
}

/// One downloadable file of a [`Release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    pub url: String,
}

impl Release {
    /// `v3.7.0`, the tag the release was cut from.
    pub fn tag(&self) -> String {
        format!("v{}", self.version)
    }

    /// The release page with its notes.
    pub fn notes_url(&self) -> String {
        format!("{}/tag/{}", self.base, self.tag())
    }

    pub fn asset(&self, platform: &str, ext: &str) -> Option<Asset> {
        let name = format!("{ASSET_PREFIX}{}-{platform}.{ext}", self.version);
        let sha256 = self.assets.get(&name)?.clone();
        Some(Asset {
            url: format!("{}/download/{}/{name}", self.base, self.tag()),
            name,
            sha256,
        })
    }

    /// The asset for the machine this runs on.
    pub fn asset_for_this_platform(&self) -> Result<Asset> {
        let (platform, ext) = current_platform_asset().with_context(|| {
            format!(
                "Wizard GUI publishes no build for {} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        self.asset(platform, ext)
            .with_context(|| format!("release {} has no {platform} build", self.tag()))
    }
}

/// Parse verified `gui-checksums.txt` text (`sha256sum` output). Every
/// `wizard-gui-*` line has to name the same version; anything else in the file
/// is ignored.
pub fn parse_checksums(text: &str, base: &str) -> Result<Release> {
    let mut version: Option<&str> = None;
    let mut assets = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hex), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        let Some((asset_version, _, _)) = parse_asset_name(name) else {
            continue;
        };
        ensure!(
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "gui-checksums.txt has a malformed sha256 for {name}"
        );
        match version {
            None => version = Some(asset_version),
            Some(seen) if seen == asset_version => {}
            Some(seen) => {
                bail!("gui-checksums.txt names two versions ({seen} and {asset_version})")
            }
        }
        assets.insert(name.to_string(), hex.to_ascii_lowercase());
    }
    let version = version.context("gui-checksums.txt lists no Wizard GUI assets")?;
    let version = semver::Version::parse(version)
        .with_context(|| format!("gui-checksums.txt names an invalid version {version:?}"))?;
    Ok(Release {
        version,
        assets,
        base: base.trim_end_matches('/').to_string(),
    })
}

/// Turn the two fetched files into a [`Release`], or refuse. Pure, so the
/// whole trust decision is testable with an injected key.
pub fn verify_release(
    key: &ReleaseKey,
    checksums: &[u8],
    signature_text: &str,
    base: &str,
) -> Result<Release> {
    let comment = signature::verify(key, checksums, signature_text)?;
    let text = std::str::from_utf8(checksums).context("gui-checksums.txt is not UTF-8")?;
    let release = parse_checksums(text, base)?;
    signature::binds_to_tag(&comment, &release.tag())?;
    Ok(release)
}

/// `current < latest` under semver; a version that does not parse is never
/// newer, so a bad feed cannot start an update loop.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| semver::Version::parse(v.trim().trim_start_matches('v')).ok();
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

pub(crate) fn http_client() -> Result<reqwest::Client> {
    http_client_with_timeouts(
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(30),
    )
}

pub(crate) fn http_client_with_timeouts(
    connect: std::time::Duration,
    read: std::time::Duration,
) -> Result<reqwest::Client> {
    #[allow(unused_mut)]
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect)
        // Inactivity timeout, not a total cap: a slow but moving download
        // still finishes.
        .read_timeout(read)
        .user_agent(concat!("wizard-gui/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many update redirects");
            }
            if attempt.url().scheme() != "https" {
                return attempt.error("update redirect left HTTPS");
            }
            attempt.follow()
        }));
    #[cfg(wizard_test_release_key)]
    if let Some(path) = std::env::var_os("WIZARD_GUI_TEST_CA") {
        let pem = std::fs::read(&path).context("reading WIZARD_GUI_TEST_CA")?;
        builder = builder.add_root_certificate(
            reqwest::Certificate::from_pem(&pem).context("parsing WIZARD_GUI_TEST_CA")?,
        );
    }
    builder.build().context("building the update HTTP client")
}

async fn fetch_small(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "fetching {url} failed with HTTP {status}"
    );
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        ensure!(
            body.len() + chunk.len() <= MAX_METADATA_BYTES,
            "{url} is larger than any checksums file; refusing it"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Fetch and verify the newest release under the compiled-in key.
pub async fn fetch_latest() -> Result<Release> {
    if signature::test_key_build() {
        tracing::warn!("this build trusts a test release key, not Wizard's");
    }
    let key = signature::release_key()?;
    fetch_latest_with(&http_client()?, &releases_base()?, &key).await
}

pub(crate) async fn fetch_latest_with(
    client: &reqwest::Client,
    base: &str,
    key: &ReleaseKey,
) -> Result<Release> {
    let latest = format!("{base}/latest/download");
    let checksums = fetch_small(client, &format!("{latest}/{CHECKSUMS}")).await?;
    let signature = fetch_small(client, &format!("{latest}/{CHECKSUMS}.minisig")).await?;
    verify_release(key, &checksums, &String::from_utf8_lossy(&signature), base)
        .context("verifying the latest Wizard GUI release")
}

/// Stream `asset` to `dest` through a `.partial` sidecar, reporting
/// `(bytes so far, total if known)`, and refuse it unless its sha256 matches
/// the signed checksum. A mismatch deletes the partial file.
pub async fn download(
    asset: &Asset,
    dest: &Path,
    mut progress: impl FnMut(u64, Option<u64>) + Send,
) -> Result<()> {
    download_with(&http_client()?, asset, dest, &mut progress).await
}

pub(crate) async fn download_with(
    client: &reqwest::Client,
    asset: &Asset,
    dest: &Path,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<()> {
    let partial = dest.with_extension("partial");
    let response = client
        .get(&asset.url)
        .send()
        .await
        .with_context(|| format!("downloading {}", asset.url))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "downloading {} failed with HTTP {status}",
        asset.url
    );
    let total = response.content_length();
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut seen = 0u64;
    let mut stream = response.bytes_stream();
    let result: Result<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading the download")?;
            hasher.update(&chunk);
            out.write_all(&chunk)
                .await
                .context("writing the download")?;
            seen += chunk.len() as u64;
            progress(seen, total);
        }
        out.flush().await.context("writing the download")?;
        Ok(())
    }
    .await;
    drop(out);
    if let Err(err) = result {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(err);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != asset.sha256 {
        let _ = tokio::fs::remove_file(&partial).await;
        bail!(
            "{} does not match its signed checksum (expected {}, got {actual}); nothing was installed",
            asset.name,
            asset.sha256
        );
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::testing::TestKey;

    const BASE: &str = "https://example.com/releases";

    fn checksums(version: &str) -> String {
        [
            "linux-x86_64.tar.gz",
            "linux-aarch64.tar.gz",
            "macos-arm64.dmg",
            "windows-x86_64.zip",
        ]
        .iter()
        .enumerate()
        .map(|(i, platform)| {
            format!(
                "{}  wizard-gui-{version}-{platform}\n",
                format!("{i}").repeat(64)
            )
        })
        .collect()
    }

    #[test]
    fn the_published_v361_checksums_parse() {
        let text = include_str!("fixtures/gui-checksums-v3.6.1.txt");
        let release = parse_checksums(text, DEFAULT_RELEASES_URL).unwrap();
        assert_eq!(release.version, semver::Version::new(3, 6, 1));
        assert_eq!(release.assets.len(), 4);
        let asset = release.asset("linux-x86_64", "tar.gz").unwrap();
        assert_eq!(
            asset.sha256,
            "a26420b4f9d23813102706e91b138d0b79af11b2a936e2202b86743de7733f70"
        );
        assert_eq!(
            asset.url,
            "https://github.com/teddytennant/wizard/releases/download/v3.6.1/wizard-gui-3.6.1-linux-x86_64.tar.gz"
        );
        assert_eq!(
            release.notes_url(),
            "https://github.com/teddytennant/wizard/releases/tag/v3.6.1"
        );
    }

    #[test]
    fn asset_names_split_at_the_platform() {
        assert_eq!(
            parse_asset_name("wizard-gui-3.7.0-linux-x86_64.tar.gz"),
            Some(("3.7.0", "linux-x86_64", "tar.gz"))
        );
        assert_eq!(
            parse_asset_name("wizard-gui-3.7.0-rc.1-macos-arm64.dmg"),
            Some(("3.7.0-rc.1", "macos-arm64", "dmg"))
        );
        assert_eq!(
            parse_asset_name("wizard-gui-3.7.0-windows-x86_64.zip"),
            Some(("3.7.0", "windows-x86_64", "zip"))
        );
        assert_eq!(
            parse_asset_name("wizard-x86_64-unknown-linux-gnu.tar.gz"),
            None
        );
        assert_eq!(parse_asset_name("wizard-gui--linux-x86_64.tar.gz"), None);
        assert_eq!(parse_asset_name("wizard-gui-3.7.0-linux-x86_64"), None);
    }

    #[test]
    fn checksums_must_agree_on_one_version() {
        let mut text = checksums("3.7.0");
        text.push_str(&format!(
            "{}  wizard-gui-3.6.0-linux-x86_64.tar.gz\n",
            "a".repeat(64)
        ));
        let err = parse_checksums(&text, BASE).unwrap_err();
        assert!(err.to_string().contains("two versions"), "{err}");
        assert!(parse_checksums("", BASE).is_err());
        assert!(parse_checksums("zz  wizard-gui-3.7.0-linux-x86_64.tar.gz\n", BASE).is_err());
        assert!(
            parse_checksums(
                &format!("{}  wizard-gui-3.7-linux-x86_64.tar.gz\n", "a".repeat(64)),
                BASE
            )
            .is_err(),
            "not semver"
        );
        // Lines that are not GUI assets are ignored.
        let mut text = checksums("3.7.0");
        text.push_str("junk\nabc  wizard-x86_64-unknown-linux-gnu.tar.gz\n");
        assert_eq!(parse_checksums(&text, BASE).unwrap().assets.len(), 4);
    }

    #[test]
    fn each_platform_gets_its_own_asset() {
        let release = parse_checksums(&checksums("3.7.0"), BASE).unwrap();
        let pick = |os, arch| {
            platform_asset(os, arch)
                .and_then(|(platform, ext)| release.asset(platform, ext))
                .map(|asset| asset.name)
        };
        assert_eq!(
            pick("linux", "x86_64").as_deref(),
            Some("wizard-gui-3.7.0-linux-x86_64.tar.gz")
        );
        assert_eq!(
            pick("linux", "aarch64").as_deref(),
            Some("wizard-gui-3.7.0-linux-aarch64.tar.gz")
        );
        assert_eq!(
            pick("macos", "aarch64").as_deref(),
            Some("wizard-gui-3.7.0-macos-arm64.dmg")
        );
        assert_eq!(
            pick("windows", "x86_64").as_deref(),
            Some("wizard-gui-3.7.0-windows-x86_64.zip")
        );
        // Intel Macs and ARM Windows have no published build.
        assert_eq!(pick("macos", "x86_64"), None);
        assert_eq!(pick("windows", "aarch64"), None);
        let asset = release.asset("macos-arm64", "dmg").unwrap();
        assert_eq!(
            asset.url,
            "https://example.com/releases/download/v3.7.0/wizard-gui-3.7.0-macos-arm64.dmg"
        );
    }

    #[test]
    fn a_verified_release_needs_the_key_and_the_matching_tag() {
        let key = TestKey::new(5);
        let text = checksums("3.7.0");
        let signature = key.sign(text.as_bytes(), "wizard v3.7.0 gui checksums");
        let release = verify_release(&key.public, text.as_bytes(), &signature, BASE).unwrap();
        assert_eq!(release.tag(), "v3.7.0");

        // v3.6.0's genuinely signed files served as "latest" for v3.7.0.
        let stale = key.sign(text.as_bytes(), "wizard v3.6.0 gui checksums");
        let err = verify_release(&key.public, text.as_bytes(), &stale, BASE).unwrap_err();
        assert!(format!("{err:#}").contains("not for v3.7.0"), "{err:#}");

        // Signed by some other key.
        let other = TestKey::new(6).sign(text.as_bytes(), "wizard v3.7.0 gui checksums");
        assert!(verify_release(&key.public, text.as_bytes(), &other, BASE).is_err());

        // One flipped digest.
        let tampered = text.replacen('0', "1", 1);
        assert!(verify_release(&key.public, tampered.as_bytes(), &signature, BASE).is_err());
    }

    #[test]
    fn semver_compare() {
        assert!(is_newer("3.6.2", "3.6.1"));
        assert!(is_newer("3.10.0", "3.9.9"));
        assert!(is_newer("v4.0.0", "3.99.0"));
        assert!(is_newer("v2026.11.1", "3.12.0"));
        assert!(is_newer("3.7.0", "3.7.0-rc.1"));
        assert!(!is_newer("3.7.0-rc.1", "3.7.0"));
        assert!(!is_newer("3.6.1", "3.6.1"));
        assert!(!is_newer("3.6.0", "3.6.1"));
        assert!(!is_newer("", "3.6.1"));
        assert!(!is_newer("nightly", "3.6.1"));
        assert!(!is_newer("3.7", "3.6.1"));
    }

    #[test]
    fn the_override_has_to_be_an_https_base_url() {
        assert_eq!(
            validate_release_override(" https://example.com/releases/ ").unwrap(),
            "https://example.com/releases"
        );
        for url in [
            "http://example.com/releases",
            "file:///tmp/update",
            "https://user:password@example.com",
            "https://example.com?feed=x",
            "https://example.com/#fragment",
            "not a url",
        ] {
            assert!(validate_release_override(url).is_err(), "accepted {url}");
        }
    }

    /// A one-shot HTTP server answering every request on one connection with
    /// `body`, for the download path.
    async fn serve(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        format!("http://{address}/asset")
    }

    #[tokio::test]
    async fn a_download_is_kept_only_when_its_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"the real archive".to_vec();
        let good = format!("{:x}", Sha256::digest(&body));
        let client = reqwest::Client::new();

        let asset = Asset {
            name: "wizard-gui-3.7.0-linux-x86_64.tar.gz".into(),
            sha256: good,
            url: serve(body.clone()).await,
        };
        let dest = dir.path().join("ok.tar.gz");
        let mut last = (0, None);
        download_with(&client, &asset, &dest, &mut |seen, total| {
            last = (seen, total)
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(last, (body.len() as u64, Some(body.len() as u64)));

        let asset = Asset {
            sha256: "0".repeat(64),
            url: serve(body).await,
            ..asset
        };
        let dest = dir.path().join("bad.tar.gz");
        let err = download_with(&client, &asset, &dest, &mut |_, _| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("signed checksum"), "{err}");
        assert!(!dest.exists());
        assert!(!dest.with_extension("partial").exists());
    }

    /// Serve `files` (path → body) over plain HTTP until the test ends.
    async fn serve_files(files: Vec<(String, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]);
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let response = match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(body);
                        out
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = socket.write_all(&response).await;
            }
        });
        format!("http://{address}/releases")
    }

    #[tokio::test]
    async fn the_latest_release_is_fetched_verified_and_downloaded() {
        let key = TestKey::new(4);
        let archive = b"linux tarball bytes".to_vec();
        let digest = format!("{:x}", Sha256::digest(&archive));
        let text = format!("{digest}  wizard-gui-9.2.0-linux-x86_64.tar.gz\n");
        let signature = key.sign(text.as_bytes(), "wizard v9.2.0 gui checksums");
        let base = serve_files(vec![
            (
                "/releases/latest/download/gui-checksums.txt".into(),
                text.clone().into_bytes(),
            ),
            (
                "/releases/latest/download/gui-checksums.txt.minisig".into(),
                signature.into_bytes(),
            ),
            (
                "/releases/download/v9.2.0/wizard-gui-9.2.0-linux-x86_64.tar.gz".into(),
                archive.clone(),
            ),
        ])
        .await;
        let client = reqwest::Client::new();
        let release = fetch_latest_with(&client, &base, &key.public)
            .await
            .unwrap();
        assert_eq!(release.tag(), "v9.2.0");
        assert_eq!(release.notes_url(), format!("{base}/tag/v9.2.0"));
        let asset = release.asset("linux-x86_64", "tar.gz").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(&asset.name);
        download_with(&client, &asset, &dest, &mut |_, _| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), archive);

        // The same feed under another key is refused before any download.
        let err = fetch_latest_with(&client, &base, &TestKey::new(5).public)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("signed by key"), "{err:#}");
        // And a feed with no signature is refused too.
        let unsigned = serve_files(vec![(
            "/releases/latest/download/gui-checksums.txt".into(),
            text.into_bytes(),
        )])
        .await;
        let err = fetch_latest_with(&client, &unsigned, &key.public)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }

    #[tokio::test]
    async fn stalled_connections_time_out() {
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let client =
            http_client_with_timeouts(Duration::from_millis(100), Duration::from_millis(100))
                .unwrap();
        let err = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("https://{address}")).send(),
        )
        .await
        .expect("bounded handshake")
        .unwrap_err();
        assert!(err.is_timeout());
        server.abort();
    }
}
