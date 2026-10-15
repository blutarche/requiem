//! The reusable sign-in librespot hands out after a device-code sign-in, kept as librespot's
//! own `Credentials` JSON. `Credentials`' `Debug` shows the secret: never format one.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use librespot_core::authentication::Credentials;

pub(crate) const ENV: &str = "REQUIEM_SPOTIFY_CREDENTIALS_FILE";
const NAME: &str = "spotify-credentials.json";
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// `REQUIEM_SPOTIFY_CREDENTIALS_FILE`, else beside the Google sign-in: in
/// `~/Library/Application Support/requiem` on macOS, `$XDG_STATE_HOME/requiem` on Linux.
pub(crate) fn from_env() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    match var(ENV) {
        Some(p) if p.is_absolute() => return Some(p),
        Some(_) => eprintln!("[spotify] ignoring {ENV}: not an absolute path"),
        None => {}
    }
    default_file(
        var("HOME"),
        var("XDG_STATE_HOME"),
        cfg!(target_os = "macos"),
    )
}

fn default_file(home: Option<PathBuf>, xdg_state: Option<PathBuf>, macos: bool) -> Option<PathBuf> {
    let home = home.filter(|h| h.is_absolute());
    let dir = if macos {
        home.map(|h| h.join("Library/Application Support"))
    } else {
        match xdg_state.filter(|x| x.is_absolute()) {
            Some(x) => Some(x),
            None => home.map(|h| h.join(".local/state")),
        }
    };
    dir.map(|d| d.join("requiem").join(NAME))
}

/// `Ok(None)` when there is no file. A file that doesn't parse is `InvalidData`, with a message
/// that never quotes it.
pub(crate) fn load(path: &Path) -> io::Result<Option<Credentials>> {
    let mut bytes = Vec::new();
    match fs::File::open(path) {
        Ok(f) => f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let not_ours = || io::Error::new(io::ErrorKind::InvalidData, "not a Spotify sign-in file");
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(not_ours());
    }
    match serde_json::from_slice::<Credentials>(&bytes) {
        Ok(c) if !c.auth_data.is_empty() => Ok(Some(c)),
        _ => Err(not_ours()),
    }
}

/// Parses the file without changing it; a missing file passes.
pub(crate) fn check(path: &Path) -> Result<(), String> {
    load(path)
        .map(drop)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Written 0600 through a temporary file and a rename, so a reader never sees half a file.
pub(crate) fn save(path: &Path, creds: &Credentials) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("credentials path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // `create` leaves an existing directory as it was.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let json = serde_json::to_vec(creds).map_err(io::Error::other)?;
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

pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use librespot_protocol::authentication::AuthenticationType;

    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn default_locations() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            default_file(home.clone(), Some("/st".into()), true).unwrap(),
            Path::new("/home/u/Library/Application Support/requiem/spotify-credentials.json")
        );
        assert_eq!(
            default_file(home.clone(), None, false).unwrap(),
            Path::new("/home/u/.local/state/requiem/spotify-credentials.json")
        );
        assert_eq!(
            default_file(home, Some("/st".into()), false).unwrap(),
            Path::new("/st/requiem/spotify-credentials.json")
        );
        assert_eq!(default_file(Some("rel".into()), None, true), None);
        assert_eq!(default_file(None, Some("rel".into()), false), None);
    }

    #[test]
    fn reads_the_file_librespot_wrote() {
        let tmp = TempDir::new();
        let path = tmp.path().join("spotify-credentials.json");
        assert!(load(&path).unwrap().is_none());
        assert_eq!(check(&path), Ok(()));
        // The shape `Cache::save_credentials` writes at librespot 4a005903e.
        fs::write(
            &path,
            r#"{"username":"someone","auth_type":1,"auth_data":"U0VDUkVULUJMT0I="}"#,
        )
        .unwrap();
        let c = load(&path).unwrap().unwrap();
        assert_eq!(c.username.as_deref(), Some("someone"));
        assert_eq!(
            c.auth_type,
            AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS
        );
        assert_eq!(c.auth_data, b"SECRET-BLOB");
        // librespot also reads the field under its old name.
        fs::write(
            &path,
            r#"{"username":"someone","auth_type":1,"encoded_auth_blob":"U0VDUkVULUJMT0I="}"#,
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().unwrap().auth_data, b"SECRET-BLOB");
    }

    #[test]
    fn a_bad_file_is_named_but_never_quoted() {
        let tmp = TempDir::new();
        let path = tmp.path().join("c.json");
        for bad in [
            "SECRET-BLOB",
            r#"{"username":"u","auth_type":1,"auth_data":""}"#,
            r#"{"username":"u","auth_type":999,"auth_data":"U0VDUkVULUJMT0I="}"#,
            r#"{"username":"u","auth_type":1,"auth_data":"SECRET-BLOB!"}"#,
        ] {
            fs::write(&path, bad).unwrap();
            let e = check(&path).unwrap_err();
            assert!(e.contains("not a Spotify sign-in file"), "{e}");
            assert!(e.contains("c.json"), "{e}");
            assert!(!e.contains("SECRET"), "{e}");
        }
    }

    #[test]
    fn saves_0600_in_a_0700_dir_and_round_trips() {
        let tmp = TempDir::new();
        let dir = tmp.path().join("state/requiem");
        let path = dir.join("spotify-credentials.json");
        let creds = Credentials {
            username: Some("someone".into()),
            auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
            auth_data: b"SECRET-BLOB".to_vec(),
        };
        save(&path, &creds).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(load(&path).unwrap().unwrap(), creds);
        assert!(!dir.join("spotify-credentials.json.tmp").exists());

        // Replacing a loose file leaves it 0600 too, and tightens a loose dir.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        save(&path, &creds).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);

        remove(&path).unwrap();
        assert!(!path.exists());
        remove(&path).unwrap();
    }
}
