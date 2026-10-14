//! The check (section 3.1) and the download (section 3.2).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use ring::digest::{Context, SHA256};
use semver::Version;
use ureq::Agent;
use ureq::http::Uri;

use crate::failure::{Failure, Reason};
use crate::manifest::{self, MAX_MANIFEST_BYTES, MAX_ZIP_BYTES, Manifest, Target};
use crate::policy::{Decision, decide};
use crate::state::State;

pub const RELEASES: &str = "https://github.com/blutarche/requiem/releases";
pub const BASE_ENV: &str = "REQUIEM_UPDATE_BASE";
pub const PUBKEY_ENV: &str = "REQUIEM_UPDATE_PUBKEY";
const MANIFEST_NAME: &str = "requiem-update.json";
const USER_AGENT: &str = "Requiem-Update/1";
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The only host a debug build's release page may have.
const LOOPBACK: &str = "127.0.0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The releases page, without a trailing slash.
    pub base: String,
    /// Base64 of the Ed25519 public key.
    pub public_key: String,
    pub running: Version,
    pub target: Target,
}

impl Config {
    /// For this build on this machine. Debug builds take `REQUIEM_UPDATE_BASE` and
    /// `REQUIEM_UPDATE_PUBKEY` from the environment; release builds ignore both, so an
    /// injected environment can't point a shipped app at old signed manifests.
    pub fn new(running: &str) -> Result<Config, String> {
        #[cfg(debug_assertions)]
        let (base, key) = (std::env::var(BASE_ENV).ok(), std::env::var(PUBKEY_ENV).ok());
        #[cfg(not(debug_assertions))]
        let (base, key) = (None, None);
        Config::with_overrides(running, base, key)
    }

    fn with_overrides(
        running: &str,
        base: Option<String>,
        key: Option<String>,
    ) -> Result<Config, String> {
        let running =
            Version::parse(running).map_err(|e| format!("running version {running:?}: {e}"))?;
        let base = match base {
            Some(base) => {
                let base = base.trim_end_matches('/');
                if !matches!(loopback_scheme(base).as_deref(), Some("http" | "https")) {
                    return Err(format!(
                        "{BASE_ENV} must be http:// or https:// on {LOOPBACK}, got {base:?}"
                    ));
                }
                base.to_owned()
            }
            None => RELEASES.to_owned(),
        };
        Ok(Config {
            base,
            public_key: key.unwrap_or_else(|| crate::key::PUBLIC_KEY.to_owned()),
            running,
            target: Target::this_machine(),
        })
    }

    fn agent(&self, timeout: Duration) -> Agent {
        // Plain HTTP only for a debug build's fake release page on this machine.
        let loopback = loopback_scheme(&self.base).as_deref() == Some("http");
        // ureq applies `https_only` to every redirect too. No cookie store is compiled in.
        let mut config = Agent::config_builder()
            .https_only(!loopback)
            .timeout_global(Some(timeout))
            .user_agent(USER_AGENT);
        if loopback {
            config = config.proxy(None);
        }
        config.build().into()
    }

    fn zip_url(&self, m: &Manifest) -> String {
        format!(
            "{}/download/v{}/{}",
            self.base,
            m.version,
            manifest::zip_name(&m.version)
        )
    }
}

/// The scheme of `url` when its host is exactly 127.0.0.1 with no user info, read as ureq
/// reads it, so a prefix like `http://127.0.0.1:80@example.com/` can't pass.
fn loopback_scheme(url: &str) -> Option<String> {
    let uri: Uri = url.parse().ok()?;
    let authority = uri.authority()?;
    if authority.as_str().contains('@') || authority.host() != LOOPBACK {
        return None;
    }
    uri.scheme_str().map(str::to_owned)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The release page offers this version, which is no newer than the running one.
    UpToDate(Version),
    Update(Manifest, Version),
}

/// Fetches and checks the latest release's manifest. On success it raises `highestSeen`
/// and sets `lastCheck` to `now`; a failed check changes nothing.
pub fn check(cfg: &Config, state: &mut State, now: u64) -> Result<Found, String> {
    let agent = cfg.agent(CHECK_TIMEOUT);
    let url = format!("{}/latest/download/{MANIFEST_NAME}", cfg.base);
    let manifest = get_capped(&agent, &url)?;
    let sig = get_capped(&agent, &format!("{url}.sig"))?;
    let sig = String::from_utf8(sig).map_err(|_| "the signature file is not text".to_owned())?;
    let found = evaluate(cfg, &manifest, &sig, state.highest_seen().as_ref())?;
    let (Found::UpToDate(v) | Found::Update(_, v)) = &found;
    state.saw(v);
    state.last_check = Some(now);
    Ok(found)
}

/// Sections 3.1 steps 3 to 5 on a fetched manifest and its signature.
pub fn evaluate(
    cfg: &Config,
    manifest: &[u8],
    sig: &str,
    highest_seen: Option<&Version>,
) -> Result<Found, String> {
    manifest::verify_signature(&cfg.public_key, manifest, sig)?;
    let (m, version) = manifest::parse(manifest, &cfg.target)?;
    Ok(match decide(&version, &cfg.running, highest_seen)? {
        Decision::UpToDate => Found::UpToDate(version),
        Decision::Update => Found::Update(m, version),
    })
}

fn get_capped(agent: &Agent, url: &str) -> Result<Vec<u8>, String> {
    let response = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_with_config()
        .limit(MAX_MANIFEST_BYTES + 1)
        .reader()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!("{url}: over {} KiB", MAX_MANIFEST_BYTES / 1024));
    }
    Ok(bytes)
}

/// Streams the release's zip to `dest`, a new `0600` file, and checks its size and SHA-256
/// against `m`. The address is built from the manifest's version, never taken from it.
pub fn download(cfg: &Config, m: &Manifest, dest: &Path) -> Result<(), Failure> {
    check_size(m)?;
    let url = cfg.zip_url(m);
    let response = cfg
        .agent(DOWNLOAD_TIMEOUT)
        .get(&url)
        .call()
        .map_err(|e| Failure::new(Reason::DownloadMismatch, format!("{url}: {e}")))?;
    let body = response
        .into_body()
        .into_with_config()
        .limit(m.size + 1)
        .reader();
    save_checked(body, &url, m, dest)
}

fn check_size(m: &Manifest) -> Result<(), Failure> {
    if m.size > MAX_ZIP_BYTES {
        return Err(Failure::new(
            Reason::DownloadMismatch,
            format!(
                "the release's zip is {} bytes, over {} MiB",
                m.size,
                MAX_ZIP_BYTES >> 20
            ),
        ));
    }
    Ok(())
}

/// Copies `zip` as [`download`] would fetch it, for checking a release before publishing.
pub fn copy_checked(zip: &Path, m: &Manifest, dest: &Path) -> Result<(), Failure> {
    check_size(m)?;
    let source = zip.display().to_string();
    let file = File::open(zip)
        .map_err(|e| Failure::new(Reason::DownloadMismatch, format!("{source}: {e}")))?;
    save_checked(file, &source, m, dest)
}

fn save_checked(
    mut body: impl Read,
    source: &str,
    m: &Manifest,
    dest: &Path,
) -> Result<(), Failure> {
    let fail = |detail: String| Failure::new(Reason::DownloadMismatch, detail);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|e| fail(format!("{}: {e}", dest.display())))?;
    let mut digest = Context::new(&SHA256);
    let mut got = 0u64;
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(fail(format!("{source}: {e}"))),
        };
        got += n as u64;
        if got > m.size {
            return Err(fail(format!(
                "{source}: more than the manifest's {} bytes",
                m.size
            )));
        }
        digest.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| fail(format!("{}: {e}", dest.display())))?;
    }
    if got != m.size {
        return Err(fail(format!("{source}: {got} bytes, not {}", m.size)));
    }
    file.sync_all()
        .map_err(|e| fail(format!("{}: {e}", dest.display())))?;
    let sha: String = digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if sha != m.sha256 {
        return Err(fail(format!("{source}: SHA-256 {sha}, not {}", m.sha256)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::{Key, Reply, Server, TempDir, manifest_json};

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn finds_an_update_and_remembers_the_check() {
        let server = Server::start();
        let key = Key::new();
        server.publish(&key, "0.2.0", b"zip");
        let mut state = State::default();
        let found = check(&server.config(&key, "0.1.0"), &mut state, NOW).unwrap();
        let Found::Update(m, v) = found else {
            panic!("{found:?}")
        };
        assert_eq!((m.version.as_str(), v), ("0.2.0", Version::new(0, 2, 0)));
        assert_eq!(state.highest_seen.as_deref(), Some("0.2.0"));
        assert_eq!(state.last_check, Some(NOW));
        let hits = server.hits.lock().unwrap().clone();
        assert_eq!(
            hits,
            [
                "/latest/download/requiem-update.json",
                "/latest/download/requiem-update.json.sig"
            ]
        );
    }

    #[test]
    fn up_to_date_and_backwards() {
        let server = Server::start();
        let key = Key::new();
        server.publish(&key, "0.2.0", b"zip");
        let mut state = State::default();
        let cfg = server.config(&key, "0.2.0");
        assert_eq!(
            check(&cfg, &mut state, NOW),
            Ok(Found::UpToDate(Version::new(0, 2, 0)))
        );
        state.highest_seen = Some("0.3.0".into());
        state.last_check = None;
        let err = check(&cfg, &mut state, NOW).unwrap_err();
        assert!(err.contains("went backwards"), "{err}");
        assert_eq!(state.last_check, None, "a failed check is not remembered");
        assert_eq!(state.highest_seen.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn a_bad_signature_fails_the_check() {
        let server = Server::start();
        let key = Key::new();
        let manifest = server.publish(&key, "0.2.0", b"zip");
        let mut flipped = manifest.clone();
        flipped[5] ^= 1;
        server.route("/latest/download/requiem-update.json", Reply::ok(flipped));
        let mut state = State::default();
        let err = check(&server.config(&key, "0.1.0"), &mut state, NOW).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        assert_eq!(state, State::default());
    }

    #[test]
    fn an_oversize_manifest_fails_the_check() {
        let server = Server::start();
        let key = Key::new();
        let mut json = manifest_json("0.2.0", b"zip");
        json["pad"] = "x".repeat(MAX_MANIFEST_BYTES as usize).into();
        let manifest = serde_json::to_vec(&json).unwrap();
        server.publish_manifest(&key, &manifest);
        let err = check(&server.config(&key, "0.1.0"), &mut State::default(), NOW).unwrap_err();
        assert!(err.contains("over 64 KiB"), "{err}");
    }

    #[test]
    fn redirects_are_followed_but_not_off_https_in_release_configs() {
        let server = Server::start();
        let key = Key::new();
        let bytes = server.publish(&key, "0.2.0", b"zip");
        let manifest = "/latest/download/requiem-update.json";
        server.route("/moved.json", Reply::ok(bytes));
        server.route(
            manifest,
            Reply::redirect(&format!("{}/moved.json", server.base)),
        );
        let found = check(&server.config(&key, "0.1.0"), &mut State::default(), NOW);
        assert!(matches!(found, Ok(Found::Update(..))), "{found:?}");

        let cfg = Config {
            base: "https://127.0.0.1:1/releases".into(),
            ..server.config(&key, "0.1.0")
        };
        let agent = cfg.agent(CHECK_TIMEOUT);
        let err = get_capped(&agent, &format!("{}{manifest}", server.base)).unwrap_err();
        assert!(err.to_lowercase().contains("https"), "{err}");
    }

    #[test]
    fn downloads_and_checks_the_zip() {
        let server = Server::start();
        let key = Key::new();
        let zip = vec![7u8; 200_000];
        let manifest = server.publish(&key, "0.2.0", &zip);
        let (m, _) = manifest::parse(&manifest, &crate::test_support::target()).unwrap();
        let dir = TempDir::new();
        let dest = dir.path().join("Requiem-0.2.0.zip");
        download(&server.config(&key, "0.1.0"), &m, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), zip);
        assert!(
            server
                .hits
                .lock()
                .unwrap()
                .contains(&"/download/v0.2.0/Requiem-0.2.0.zip".to_owned())
        );
    }

    fn download_err(serve: &[u8], edit: impl FnOnce(&mut Manifest)) -> Failure {
        let server = Server::start();
        let key = Key::new();
        let manifest = server.publish(&key, "0.2.0", b"the real zip");
        server.route("/download/v0.2.0/Requiem-0.2.0.zip", Reply::ok(serve));
        let (mut m, _) = manifest::parse(&manifest, &crate::test_support::target()).unwrap();
        edit(&mut m);
        let dir = TempDir::new();
        download(&server.config(&key, "0.1.0"), &m, &dir.path().join("z.zip")).unwrap_err()
    }

    #[test]
    fn a_sha256_mismatch_is_transient() {
        let f = download_err(b"the fake zip", |_| {});
        assert_eq!(f.reason, Reason::DownloadMismatch);
        assert_eq!(f.kind(), crate::Kind::Transient);
        assert!(f.detail.contains("SHA-256"), "{f}");
    }

    #[test]
    fn stops_past_the_manifest_size() {
        let f = download_err(b"the real zip and then some", |_| {});
        assert!(f.detail.contains("more than"), "{f}");
        let f = download_err(b"short", |_| {});
        assert!(f.detail.contains("bytes, not"), "{f}");
    }

    #[test]
    fn refuses_an_oversize_zip() {
        let f = download_err(b"the real zip", |m| m.size = MAX_ZIP_BYTES + 1);
        assert!(f.detail.contains("over 256 MiB"), "{f}");
    }

    #[test]
    fn overrides_are_127_0_0_1_only() {
        let with = |base: &str| Config::with_overrides("0.1.0", Some(base.into()), None);
        for refused in [
            "http://127.0.0.1:80@example.com/",
            "http://example.com/releases",
            "https://example.com/r",
            "http://127.0.0.1.example.com/releases",
            "http://127.0.0.1@example.com/",
            "http://user@127.0.0.1:9/r",
            "http://localhost:9/r",
            "ftp://127.0.0.1:9/r",
            "127.0.0.1:9/r",
        ] {
            assert!(with(refused).is_err(), "{refused}");
        }
        assert_eq!(
            with("https://127.0.0.1:9/r/").unwrap().base,
            "https://127.0.0.1:9/r"
        );
        let cfg = Config::with_overrides(
            "0.1.0",
            Some("http://127.0.0.1:9/r/".into()),
            Some("AAAA".into()),
        )
        .unwrap();
        assert_eq!(
            (cfg.base.as_str(), cfg.public_key.as_str()),
            ("http://127.0.0.1:9/r", "AAAA")
        );
        let cfg = Config::with_overrides("0.1.0", None, None).unwrap();
        assert_eq!((cfg.base.as_str(), cfg.public_key.as_str()), (RELEASES, ""));
    }
}
