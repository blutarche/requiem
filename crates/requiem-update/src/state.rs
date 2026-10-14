//! `update-state.json`: the auto-check setting, the version policy's memory, the staging
//! folder to clean up, and failed versions.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::failure::{Failure, Kind};

pub const STATE_FILE_ENV: &str = "REQUIEM_UPDATE_STATE_FILE";
const FORMAT: u32 = 1;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// A transient failure is tried this many times in all; then the version counts as bad.
pub const MAX_TRIES: u32 = 3;
/// Scheduled checks run only once the last successful one is this old.
pub const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub format: u32,
    pub auto: bool,
    pub highest_seen: Option<String>,
    /// Unix seconds of the last successful check.
    pub last_check: Option<u64>,
    pub staging: Option<PathBuf>,
    pub failures: Vec<FailureRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureRecord {
    pub version: String,
    pub sha256: String,
    pub tries: u32,
    pub bad: bool,
    /// The last failure's reason as the banner shows it.
    pub reason: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            format: FORMAT,
            auto: true,
            highest_seen: None,
            last_check: None,
            staging: None,
            failures: Vec::new(),
        }
    }
}

/// `REQUIEM_UPDATE_STATE_FILE` overrides the default, beside the player's own state file.
/// `None` when there is no usable location.
pub fn state_path() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if let Some(p) = var(STATE_FILE_ENV) {
        if p.is_absolute() {
            return Some(p);
        }
        eprintln!("requiem-update: ignoring {STATE_FILE_ENV}: not an absolute path");
    }
    default_path(
        var("HOME"),
        var("XDG_STATE_HOME"),
        cfg!(target_os = "macos"),
    )
}

fn default_path(home: Option<PathBuf>, xdg_state: Option<PathBuf>, macos: bool) -> Option<PathBuf> {
    let home = home.filter(|h| h.is_absolute());
    let base = if macos {
        home?.join("Library/Application Support")
    } else {
        match xdg_state.filter(|x| x.is_absolute()) {
            Some(x) => x,
            None => home?.join(".local/state"),
        }
    };
    Some(base.join("requiem/update-state.json"))
}

impl State {
    /// A missing file is the default state. Any other failure is an error the caller logs
    /// before going on with the default; the next save replaces the file.
    pub fn load(path: &Path) -> Result<State, String> {
        let mut bytes = Vec::new();
        match fs::File::open(path) {
            Ok(f) => f
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(format!("{}: over {MAX_FILE_BYTES} bytes", path.display()));
        }
        let state: State = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{}: unreadable: {e}", path.display()))?;
        if state.format != FORMAT {
            return Err(format!(
                "{}: format {} is not {FORMAT}",
                path.display(),
                state.format
            ));
        }
        Ok(state)
    }

    /// Atomic: a `0600` file in a `0700` folder.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("state path has no parent"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        // A leftover from a crash would make `create_new` fail.
        let _ = fs::remove_file(&tmp);
        let result = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| {
                f.write_all(&json)?;
                f.sync_all()
            })
            .and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    pub fn highest_seen(&self) -> Option<Version> {
        self.highest_seen
            .as_deref()
            .and_then(|v| Version::parse(v).ok())
    }

    /// Raises the highest version seen to `version` if it is higher.
    pub fn saw(&mut self, version: &Version) {
        if self.highest_seen().is_none_or(|h| *version > h) {
            self.highest_seen = Some(version.to_string());
        }
    }

    /// Whether a scheduled check is due at `now`: the last successful check is at least six
    /// hours old, or in the future because the clock moved back.
    pub fn due(&self, now: u64) -> bool {
        self.last_check
            .is_none_or(|t| t > now || now - t >= CHECK_INTERVAL_SECS)
    }

    /// The record of `version` with that zip, if it is marked bad.
    pub fn bad(&self, version: &str, sha256: &str) -> Option<&FailureRecord> {
        self.failures
            .iter()
            .find(|f| f.bad && f.version == version && f.sha256 == sha256)
    }

    /// Counts a failed try of `version` with that zip. True once the version is bad: a bad
    /// release at once, a transient failure on its third try. A failure that is not counted
    /// makes the next start check again instead.
    pub fn record_failure(&mut self, version: &str, sha256: &str, failure: &Failure) -> bool {
        if failure.kind() == Kind::NotCounted {
            self.last_check = None;
            return false;
        }
        let i = match self
            .failures
            .iter()
            .position(|f| f.version == version && f.sha256 == sha256)
        {
            Some(i) => i,
            None => {
                self.failures.push(FailureRecord {
                    version: version.to_owned(),
                    sha256: sha256.to_owned(),
                    tries: 0,
                    bad: false,
                    reason: String::new(),
                });
                self.failures.len() - 1
            }
        };
        let f = &mut self.failures[i];
        f.tries = f.tries.saturating_add(1);
        f.reason = failure.reason.text().to_owned();
        if failure.kind() == Kind::Bad || f.tries >= MAX_TRIES {
            f.bad = true;
        }
        f.bad
    }

    /// What a manual check does: retry counts go, versions marked bad stay.
    pub fn clear_retries(&mut self) {
        self.failures.retain(|f| f.bad);
    }

    /// Forgets failures of versions no newer than `running`, which can never be offered
    /// again.
    pub fn forget_older(&mut self, running: &Version) {
        self.failures
            .retain(|f| Version::parse(&f.version).is_ok_and(|v| v > *running));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::failure::Reason;
    use crate::test_support::TempDir;

    fn transient() -> Failure {
        Failure::new(Reason::DownloadMismatch, "connection reset")
    }

    fn bad() -> Failure {
        Failure::new(Reason::NotBuiltLikeThis, "extra entry")
    }

    #[test]
    fn transient_failures_count_to_three_then_bad() {
        let mut s = State::default();
        assert!(!s.record_failure("0.2.0", "aa", &transient()));
        assert!(!s.record_failure("0.2.0", "aa", &transient()));
        assert!(s.bad("0.2.0", "aa").is_none());
        assert!(s.record_failure("0.2.0", "aa", &transient()));
        let rec = s.bad("0.2.0", "aa").unwrap();
        assert_eq!(
            (rec.tries, rec.reason.as_str()),
            (3, "the download did not match the release")
        );
    }

    #[test]
    fn a_locked_folder_is_never_counted_and_checks_at_the_next_start() {
        let mut s = State {
            last_check: Some(1_000),
            ..State::default()
        };
        let locked = Failure::new(Reason::FolderLocked, "stayed locked for 30 s");
        for _ in 0..MAX_TRIES + 1 {
            assert!(!s.record_failure("0.2.0", "aa", &locked));
        }
        assert_eq!((s.failures.len(), s.last_check), (0, None));
        assert!(s.due(2_000));
    }

    #[test]
    fn a_bad_release_is_bad_at_once_and_only_for_that_zip() {
        let mut s = State::default();
        assert!(s.record_failure("0.2.0", "aa", &bad()));
        assert!(s.bad("0.2.0", "aa").is_some());
        assert!(
            s.bad("0.2.0", "bb").is_none(),
            "a re-published zip is tried"
        );
        assert!(s.bad("0.2.1", "aa").is_none());
    }

    #[test]
    fn a_manual_check_clears_retries_but_not_bad_versions() {
        let mut s = State::default();
        s.record_failure("0.2.0", "aa", &transient());
        s.record_failure("0.2.0", "aa", &transient());
        s.record_failure("0.3.0", "cc", &bad());
        s.clear_retries();
        assert!(s.bad("0.3.0", "cc").is_some());
        assert!(
            !s.record_failure("0.2.0", "aa", &transient()),
            "counting starts over"
        );
        assert_eq!(
            s.failures
                .iter()
                .find(|f| f.version == "0.2.0")
                .unwrap()
                .tries,
            1
        );
    }

    #[test]
    fn forgets_failures_of_versions_no_longer_newer() {
        let mut s = State::default();
        s.record_failure("0.2.0", "aa", &bad());
        s.record_failure("0.3.0", "bb", &bad());
        s.forget_older(&Version::new(0, 2, 0));
        let left: Vec<_> = s.failures.iter().map(|f| f.version.as_str()).collect();
        assert_eq!(left, ["0.3.0"]);
    }

    #[test]
    fn due_after_six_hours_or_when_never_checked() {
        let mut s = State::default();
        assert!(s.due(1_000_000));
        s.last_check = Some(1_000_000);
        assert!(!s.due(1_000_000 + CHECK_INTERVAL_SECS - 1));
        assert!(s.due(1_000_000 + CHECK_INTERVAL_SECS));
        assert!(s.due(999_999), "a clock that moved back");
    }

    #[test]
    fn highest_seen_only_rises() {
        let mut s = State::default();
        s.saw(&Version::new(0, 2, 0));
        s.saw(&Version::new(0, 1, 0));
        assert_eq!(s.highest_seen.as_deref(), Some("0.2.0"));
        s.saw(&Version::new(0, 3, 0));
        assert_eq!(s.highest_seen(), Some(Version::new(0, 3, 0)));
    }

    #[test]
    fn round_trips_privately_with_the_spec_field_names() {
        let dir = TempDir::new();
        let path = dir.path().join("sub/requiem/update-state.json");
        let mut s = State {
            auto: false,
            last_check: Some(42),
            staging: Some(PathBuf::from("/tmp/x")),
            ..State::default()
        };
        s.saw(&Version::new(0, 2, 0));
        s.record_failure("0.2.0", "aa", &bad());
        s.save(&path).unwrap();
        assert_eq!(State::load(&path), Ok(s));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for key in [
            "format",
            "auto",
            "highestSeen",
            "lastCheck",
            "staging",
            "failures",
        ] {
            assert!(v.get(key).is_some(), "{key}");
        }
        for key in ["version", "sha256", "tries", "bad"] {
            assert!(v["failures"][0].get(key).is_some(), "{key}");
        }
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["update-state.json"]);
    }

    #[test]
    fn missing_is_default_and_corrupt_is_an_error() {
        let dir = TempDir::new();
        let path = dir.path().join("update-state.json");
        assert_eq!(State::load(&path), Ok(State::default()));
        assert!(State::default().auto, "automatic checks are on by default");
        fs::write(&path, b"{nope").unwrap();
        assert!(State::load(&path).is_err());
        fs::write(&path, br#"{"format":2,"auto":true,"highestSeen":null,"lastCheck":null,"staging":null,"failures":[]}"#).unwrap();
        assert!(State::load(&path).unwrap_err().contains("format 2"));
    }

    #[test]
    fn default_paths() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            default_path(p("/Users/u"), None, true),
            p("/Users/u/Library/Application Support/requiem/update-state.json")
        );
        assert_eq!(
            default_path(p("/home/u"), None, false),
            p("/home/u/.local/state/requiem/update-state.json")
        );
        assert_eq!(default_path(p("rel"), None, true), None);
    }
}
