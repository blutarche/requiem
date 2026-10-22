//! Where the provider keeps its files, and writing them so that only the user can read them.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::log::debug;

const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The provider's own folder: `~/Library/Application Support/requiem` on macOS,
/// `$XDG_STATE_HOME/requiem` (by default `~/.local/state/requiem`) on Linux. `None` when there
/// is no usable `HOME`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    default_dir(
        var("HOME"),
        var("XDG_STATE_HOME"),
        cfg!(target_os = "macos"),
    )
}

fn default_dir(home: Option<PathBuf>, xdg_state: Option<PathBuf>, macos: bool) -> Option<PathBuf> {
    let home = home.filter(|h| h.is_absolute());
    let base = if macos {
        home?.join("Library/Application Support")
    } else {
        match xdg_state.filter(|x| x.is_absolute()) {
            Some(x) => x,
            None => home?.join(".local/state"),
        }
    };
    Some(base.join("requiem"))
}

pub(crate) fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other(format!("over {MAX_FILE_BYTES} bytes")));
    }
    Ok(bytes)
}

/// Makes a file left readable by others (an older build, a copy, a restore) the user's only.
pub(crate) fn tighten(path: &Path) {
    if let Ok(meta) = fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
        && let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    {
        debug!("cannot tighten {}'s mode: {e}", path.display());
    }
}

/// Makes the directory of `path`, the sign-ins' own, only the user's.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("sign-in path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // `create` leaves an existing directory as it was.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// The temp file `stage_private` writes for `path`, which a crash can leave behind.
pub(crate) fn staged_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Writes `bytes` to a temp file beside `path`, which only the user can read, for `commit` to
/// put in its place.
pub(crate) fn stage_private(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let tmp = staged_path(path);
    // A leftover from a crash would make `create_new` fail.
    let _ = fs::remove_file(&tmp);
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        });
    match result {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Puts the file `stage_private` wrote in `path`'s place, so a reader never sees half of it.
pub(crate) fn commit(tmp: &Path, path: &Path) -> io::Result<()> {
    let result = fs::rename(tmp, path);
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Deletes `path` and any copy a save of it left behind; a missing one is no error. The copy
/// goes first, so on an error the file is still there.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    for p in [staged_path(path), path.to_owned()] {
        match fs::remove_file(p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn write(path: &Path, bytes: &[u8]) {
        private_dir(path).unwrap();
        let tmp = stage_private(path, bytes).unwrap();
        commit(&tmp, path).unwrap();
    }

    #[test]
    fn a_file_is_private_from_creation() {
        let tmp = TempDir::new();
        let path = tmp.path().join("a/requiem/f.json");
        write(&path, b"one");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o700);
        assert!(!staged_path(&path).exists());
        // Rewrites keep the mode.
        write(&path, b"two");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(read_capped(&path).unwrap(), b"two");
    }

    #[test]
    fn an_existing_directory_and_file_are_tightened() {
        let tmp = TempDir::new();
        let path = tmp.path().join("requiem/f.json");
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        write(&path, b"x");
        assert_eq!(mode(dir), 0o700);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        tighten(&path);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_file_over_the_cap_is_not_read() {
        let tmp = TempDir::new();
        let path = tmp.path().join("big");
        fs::write(&path, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(read_capped(&path).is_err());
        fs::write(&path, vec![b'x'; MAX_FILE_BYTES as usize]).unwrap();
        assert!(read_capped(&path).is_ok());
    }

    #[test]
    fn remove_takes_the_file_and_its_leftover_copy() {
        let tmp = TempDir::new();
        let path = tmp.path().join("f.json");
        assert!(remove(&path).is_ok());
        fs::write(&path, b"x").unwrap();
        fs::write(staged_path(&path), b"x").unwrap();
        remove(&path).unwrap();
        assert!(!path.exists() && !staged_path(&path).exists());
    }

    #[test]
    fn default_locations() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            default_dir(home.clone(), None, true).unwrap(),
            Path::new("/home/u/Library/Application Support/requiem")
        );
        assert_eq!(
            default_dir(home.clone(), None, false).unwrap(),
            Path::new("/home/u/.local/state/requiem")
        );
        assert_eq!(
            default_dir(home.clone(), Some("/st".into()), false).unwrap(),
            Path::new("/st/requiem")
        );
        assert_eq!(
            default_dir(home, Some("st".into()), false).unwrap(),
            Path::new("/home/u/.local/state/requiem")
        );
        assert_eq!(default_dir(None, None, true), None);
        assert_eq!(default_dir(Some("rel".into()), None, false), None);
    }
}
