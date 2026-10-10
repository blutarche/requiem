use std::env;
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::sys;

pub(crate) const DIR_ENV: &str = "REQUIEM_SOCKET_DIR";
pub(crate) const SOCKET_NAME: &str = "daemon.sock";
pub(crate) const LOCK_NAME: &str = "daemon.lock";
pub(crate) const LOG_NAME: &str = "daemon.log";
pub(crate) const OLD_LOG_NAME: &str = "daemon.log.old";

/// `sockaddr_un.sun_path`, terminating NUL included.
#[cfg(target_os = "macos")]
pub(crate) const SUN_PATH_LEN: usize = 104;
#[cfg(target_os = "linux")]
pub(crate) const SUN_PATH_LEN: usize = 108;

/// The per-user socket directory, created with mode 0700 if missing. It must be a real
/// directory owned by this user; an existing one is reset to mode 0700.
/// `REQUIEM_SOCKET_DIR` overrides the location and is checked the same way.
pub fn socket_dir() -> io::Result<PathBuf> {
    let dir = match env::var_os(DIR_ENV).filter(|v| !v.is_empty()) {
        Some(v) => {
            let dir = PathBuf::from(v);
            if !dir.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{DIR_ENV} must be absolute, got {}", dir.display()),
                ));
            }
            dir
        }
        None => default_dir()?,
    };
    prepare_dir(&dir)?;
    Ok(dir)
}

pub fn socket_path() -> io::Result<PathBuf> {
    socket_path_in(&socket_dir()?)
}

pub(crate) fn socket_path_in(dir: &Path) -> io::Result<PathBuf> {
    let path = dir.join(SOCKET_NAME);
    check_sun_path(&path)?;
    Ok(path)
}

/// Fails rather than letting the path be truncated.
pub(crate) fn check_sun_path(path: &Path) -> io::Result<()> {
    let len = path.as_os_str().len();
    if len >= SUN_PATH_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is {len} bytes; the limit is {}",
                path.display(),
                SUN_PATH_LEN - 1
            ),
        ));
    }
    Ok(())
}

/// Not `$TMPDIR`: it differs between Finder- and shell-launched processes, and between
/// sandboxed and unsandboxed ones, so the GUI, CLI and daemon could disagree on the path.
#[cfg(target_os = "macos")]
pub(crate) fn default_dir() -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    loop {
        // SAFETY: `buf` is valid for `buf.len()` bytes.
        let n = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_TEMP_DIR,
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n <= buf.len() {
            buf.truncate(n - 1);
            break;
        }
        buf.resize(n, 0);
    }
    Ok(PathBuf::from(OsString::from_vec(buf)).join("requiem"))
}

#[cfg(target_os = "linux")]
pub(crate) fn default_dir() -> io::Result<PathBuf> {
    match env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(d) if d.is_absolute() => Ok(d.join("requiem")),
        _ => Ok(PathBuf::from(format!("/tmp/requiem-{}", sys::euid()))),
    }
}

/// Creates `dir` with mode 0700 if missing. An existing `dir` must be a real directory (not a
/// symlink) owned by this user; any group or other permission bits are removed.
pub(crate) fn prepare_dir(dir: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("create {}: {e}", dir.display()),
                    ));
                }
            }
            fs::symlink_metadata(dir)?
        }
        meta => meta?,
    };
    let refuse = |why: String| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing socket directory {}: {why}", dir.display()),
        ))
    };
    if !meta.is_dir() {
        return refuse("not a directory".into());
    }
    let euid = sys::euid();
    if meta.uid() != euid {
        return refuse(format!("owned by uid {}, not {euid}", meta.uid()));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    }
    Ok(())
}
