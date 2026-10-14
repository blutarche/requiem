//! The parts of staging and installing that need no macOS API: the unzipped bundle's
//! layout, the self-tests, files saved across a restart, and stale staging folders.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::failure::{Failure, Reason};
use crate::lock::DirLock;

pub const APP_NAME: &str = "Requiem.app";
pub const SELF_TEST_ARG: &str = "--update-self-test";
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

pub fn helper_app(app: &Path) -> PathBuf {
    app.join("Contents/Helpers/requiemd.app")
}

pub fn daemon_exe(app: &Path) -> PathBuf {
    helper_app(app).join("Contents/MacOS/requiemd")
}

pub fn gui_exe(app: &Path) -> PathBuf {
    app.join("Contents/MacOS/requiem-gui")
}

pub fn cli_exe(app: &Path) -> PathBuf {
    app.join("Contents/MacOS/requiem")
}

/// Section 3.3 step 3 on the folder the zip was extracted into. Returns the app inside.
pub fn check_layout(unzipped: &Path) -> Result<PathBuf, Failure> {
    let bad = |detail: String| Failure::new(Reason::NotBuiltLikeThis, detail);
    let io_err = |e: io::Error| bad(format!("{}: {e}", unzipped.display()));
    let names = fs::read_dir(unzipped)
        .map_err(io_err)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(io_err)?;
    if names != [APP_NAME] {
        return Err(bad(format!(
            "the zip holds {names:?}, not {APP_NAME} alone"
        )));
    }
    let app = unzipped.join(APP_NAME);
    if !fs::symlink_metadata(&app).map_err(io_err)?.is_dir() {
        return Err(bad(format!("{APP_NAME} is not a real folder")));
    }
    let root = fs::canonicalize(&app).map_err(io_err)?;
    walk(&app, &root).map_err(|(path, why)| {
        let rel = path.strip_prefix(unzipped).unwrap_or(&path);
        bad(format!("{}: {why}", rel.display()))
    })?;
    Ok(app)
}

fn walk(dir: &Path, root: &Path) -> Result<(), (PathBuf, String)> {
    let entries = fs::read_dir(dir).map_err(|e| (dir.to_owned(), e.to_string()))?;
    for entry in entries {
        let path = entry.map_err(|e| (dir.to_owned(), e.to_string()))?.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| (path.clone(), e.to_string()))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            if !fs::canonicalize(&path).is_ok_and(|target| target.starts_with(root)) {
                return Err((path, "a symbolic link that resolves outside the app".into()));
            }
        } else if kind.is_dir() {
            walk(&path, root)?;
        } else if kind.is_file() {
            if meta.nlink() > 1 {
                return Err((path, format!("a file with {} links", meta.nlink())));
            }
        } else {
            return Err((path, "a device, FIFO or socket".into()));
        }
    }
    Ok(())
}

/// Section 3.3 step 6 on a staged app.
pub fn self_test(app: &Path, version: &str) -> Result<(), Failure> {
    self_test_within(app, version, SELF_TEST_TIMEOUT)
}

fn self_test_within(app: &Path, version: &str, timeout: Duration) -> Result<(), Failure> {
    for exe in [daemon_exe(app), gui_exe(app)] {
        expect_success(&exe, SELF_TEST_ARG, timeout)?;
    }
    let printed = expect_success(&cli_exe(app), "--version", timeout)?;
    let want = format!("requiem {version}");
    if printed.trim() != want {
        return Err(Failure::new(
            Reason::DidNotStart,
            format!(
                "requiem --version printed {:?}, not {want:?}",
                printed.trim()
            ),
        ));
    }
    Ok(())
}

fn expect_success(exe: &Path, arg: &str, timeout: Duration) -> Result<String, Failure> {
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    match run(exe, &[arg], timeout) {
        Ok(Ran::Exited(status, out)) if status.success() => Ok(out),
        Ok(Ran::Exited(status, _)) => Err(Failure::new(
            Reason::DidNotStart,
            format!("{name} {arg}: {status}"),
        )),
        Ok(Ran::TimedOut) => Err(Failure::new(
            Reason::NotInTime,
            format!(
                "{name} {arg}: still running after {} s",
                timeout.as_secs_f32()
            ),
        )),
        Err(e) => Err(Failure::new(
            Reason::DidNotStart,
            format!("{name} {arg}: {e}"),
        )),
    }
}

#[derive(Debug)]
pub enum Ran {
    /// With what it printed on stdout.
    Exited(ExitStatus, String),
    /// Killed at the deadline.
    TimedOut,
}

/// Runs `exe` with a deadline, keeping at most 64 KiB of its stdout. Stderr is inherited,
/// so a failing self-test explains itself in the caller's log.
pub fn run(exe: &Path, args: &[&str], timeout: Duration) -> io::Result<Ran> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut out);
        }
        out
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let out = reader.join().unwrap_or_default();
            return Ok(Ran::Exited(
                status,
                String::from_utf8_lossy(&out).into_owned(),
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Ran::TimedOut);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Files copied before a restart, to put back if it fails. Their folder stays locked while
/// they are kept, so a player starting meanwhile leaves it alone.
#[derive(Debug)]
pub struct Saved {
    dir: PathBuf,
    /// Each original path, with its copy or `None` when it did not exist.
    files: Vec<(PathBuf, Option<PathBuf>)>,
    _lock: DirLock,
}

impl Saved {
    /// Copies each existing file in `files` into `dir`, a new `0700` folder, as `0600` files.
    pub fn new(dir: &Path, files: &[&Path]) -> io::Result<Saved> {
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        let copied = DirLock::try_new(dir).and_then(|lock| {
            let lock =
                lock.ok_or_else(|| io::Error::other(format!("{} is in use", dir.display())))?;
            Ok(Saved {
                dir: dir.to_owned(),
                files: copy_private(dir, files)?,
                _lock: lock,
            })
        });
        if copied.is_err() {
            let _ = fs::remove_dir_all(dir);
        }
        copied
    }

    /// Puts every file back as it was, removing those that did not exist. Goes on past a
    /// failure and returns the first.
    pub fn restore(&self) -> io::Result<()> {
        let mut result = Ok(());
        for (path, copy) in &self.files {
            let restored = match copy {
                Some(copy) => fs::read(copy).and_then(|bytes| replace_private(path, &bytes)),
                None => match fs::remove_file(path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    other => other,
                },
            };
            if let Err(e) = restored
                && result.is_ok()
            {
                result = Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
            }
        }
        result
    }

    /// Deletes the copies and their folder.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }
}

fn copy_private(dir: &Path, files: &[&Path]) -> io::Result<Vec<(PathBuf, Option<PathBuf>)>> {
    let mut saved = Vec::new();
    for (i, &path) in files.iter().enumerate() {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                saved.push((path.to_owned(), None));
                continue;
            }
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        };
        let copy = dir.join(i.to_string());
        write_private(&copy, &bytes)?;
        saved.push((path.to_owned(), Some(copy)));
    }
    Ok(saved)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".restore");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    write_private(&tmp, bytes)
        .and_then(|()| fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

/// Deletes a staging folder that a player left behind, only if it is still what a player
/// made: after `realpath`, a real directory owned by this user with mode `0700`, directly
/// inside `temp_root`, and not held by a player using it. `temp_root` must be canonical.
/// `Ok` when it is gone, whether deleted now or before.
pub fn remove_stale(path: &Path, temp_root: &Path) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("left {} in place: {why}", path.display()));
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(meta) if meta.file_type().is_symlink() => return refuse("a symbolic link"),
        Ok(_) => {}
    }
    let real = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = fs::symlink_metadata(&real).map_err(|e| format!("{}: {e}", real.display()))?;
    if !meta.is_dir() {
        return refuse("not a real folder");
    }
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } {
        return refuse("owned by another user");
    }
    if meta.permissions().mode() & 0o7777 != 0o700 {
        return refuse("its mode is not 0700");
    }
    if real.parent() != Some(temp_root) {
        return refuse("outside the volume's temporary-items folder");
    }
    let Some(_lock) = DirLock::try_new(&real).map_err(|e| format!("{}: {e}", real.display()))?
    else {
        return refuse("another player is using it");
    };
    fs::remove_dir_all(&real).map_err(|e| format!("{}: {e}", real.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::failure::Kind;
    use crate::test_support::{TempDir, eventually};

    fn script(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// An unzipped folder holding a bundle whose binaries are shell scripts.
    fn bundle(dir: &Path, version: &str) -> PathBuf {
        let unzipped = dir.join("unzipped");
        let app = unzipped.join(APP_NAME);
        script(&daemon_exe(&app), "exit 0");
        script(&gui_exe(&app), "exit 0");
        script(&cli_exe(&app), &format!("echo 'requiem {version}'"));
        fs::create_dir_all(app.join("Contents/Frameworks")).unwrap();
        fs::write(app.join("Contents/Frameworks/libopus.0.dylib"), b"lib").unwrap();
        unzipped
    }

    fn layout_err(unzipped: &Path) -> String {
        let f = check_layout(unzipped).unwrap_err();
        assert_eq!(f.reason, Reason::NotBuiltLikeThis, "{f}");
        assert_eq!(f.kind(), Kind::Bad);
        f.detail
    }

    #[test]
    fn accepts_a_plain_bundle_with_internal_links() {
        let dir = TempDir::new();
        let unzipped = bundle(dir.path(), "0.2.0");
        let app = unzipped.join(APP_NAME);
        symlink(
            "libopus.0.dylib",
            app.join("Contents/Frameworks/libopus.dylib"),
        )
        .unwrap();
        symlink("../MacOS", app.join("Contents/Frameworks/bin")).unwrap();
        assert_eq!(check_layout(&unzipped).unwrap(), app);
    }

    #[test]
    fn refuses_a_symbolic_link_escaping_the_bundle() {
        let dir = TempDir::new();
        let unzipped = bundle(dir.path(), "0.2.0");
        let link = unzipped.join(APP_NAME).join("Contents/Resources");
        symlink("../../..", &link).unwrap();
        assert!(layout_err(&unzipped).contains("resolves outside"));
        fs::remove_file(&link).unwrap();
        symlink("/etc/hosts", &link).unwrap();
        assert!(layout_err(&unzipped).contains("Contents/Resources"));
        fs::remove_file(&link).unwrap();
        symlink("missing", &link).unwrap();
        assert!(
            layout_err(&unzipped).contains("resolves outside"),
            "dangling"
        );
    }

    #[test]
    fn refuses_a_hard_link() {
        let dir = TempDir::new();
        let unzipped = bundle(dir.path(), "0.2.0");
        let lib = unzipped
            .join(APP_NAME)
            .join("Contents/Frameworks/libopus.0.dylib");
        fs::hard_link(&lib, dir.path().join("elsewhere")).unwrap();
        assert!(layout_err(&unzipped).contains("2 links"));
    }

    #[test]
    fn refuses_an_extra_top_level_entry_or_a_linked_app() {
        let dir = TempDir::new();
        let unzipped = bundle(dir.path(), "0.2.0");
        fs::write(unzipped.join("README"), b"hi").unwrap();
        assert!(layout_err(&unzipped).contains("not Requiem.app alone"));
        fs::remove_file(unzipped.join("README")).unwrap();

        let linked = dir.path().join("linked");
        fs::create_dir_all(&linked).unwrap();
        symlink(unzipped.join(APP_NAME), linked.join(APP_NAME)).unwrap();
        assert!(layout_err(&linked).contains("not a real folder"));
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        assert!(layout_err(&dir.path().join("empty")).contains("not Requiem.app alone"));
    }

    #[test]
    fn refuses_a_fifo() {
        let dir = TempDir::new();
        let unzipped = bundle(dir.path(), "0.2.0");
        let fifo = unzipped.join(APP_NAME).join("Contents/pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(layout_err(&unzipped).contains("FIFO"));
    }

    #[test]
    fn self_tests_pass_on_a_working_bundle() {
        let dir = TempDir::new();
        let app = bundle(dir.path(), "0.2.0").join(APP_NAME);
        assert_eq!(self_test(&app, "0.2.0"), Ok(()));
    }

    #[test]
    fn a_self_test_exiting_non_zero_marks_the_release_bad() {
        let dir = TempDir::new();
        let app = bundle(dir.path(), "0.2.0").join(APP_NAME);
        script(&daemon_exe(&app), "exit 3");
        let f = self_test(&app, "0.2.0").unwrap_err();
        assert_eq!((f.reason, f.kind()), (Reason::DidNotStart, Kind::Bad));
        assert!(f.detail.contains("requiemd --update-self-test"), "{f}");
    }

    #[test]
    fn a_self_test_timing_out_is_transient() {
        let dir = TempDir::new();
        let app = bundle(dir.path(), "0.2.0").join(APP_NAME);
        script(&gui_exe(&app), "exec sleep 30");
        let started = Instant::now();
        let f = self_test_within(&app, "0.2.0", Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!((f.reason, f.kind()), (Reason::NotInTime, Kind::Transient));
        assert!(f.detail.contains("requiem-gui"), "{f}");
    }

    #[test]
    fn the_cli_must_print_the_manifest_version() {
        let dir = TempDir::new();
        let app = bundle(dir.path(), "0.1.0").join(APP_NAME);
        let f = self_test(&app, "0.2.0").unwrap_err();
        assert_eq!(f.reason, Reason::DidNotStart);
        assert!(f.detail.contains("requiem 0.1.0"), "{f}");
    }

    #[test]
    fn saved_files_come_back_and_new_ones_go() {
        let dir = TempDir::new();
        let folder = dir.path().join("saved");
        let state = dir.path().join("state.json");
        let token = dir.path().join("google-token.json");
        fs::write(&state, b"old state").unwrap();
        let saved = Saved::new(&folder, &[&state, &token]).unwrap();
        let copy = folder.join("0");
        assert_eq!(fs::read(&copy).unwrap(), b"old state");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&copy), mode(&folder)), (0o600, 0o700));
        assert!(
            DirLock::try_new(&folder).unwrap().is_none(),
            "locked while kept"
        );

        fs::write(&state, b"new state").unwrap();
        fs::write(&token, b"new token").unwrap();
        saved.restore().unwrap();
        assert_eq!(fs::read(&state).unwrap(), b"old state");
        assert_eq!(mode(&state), 0o600);
        assert!(!token.exists());
        saved.remove().unwrap();
        assert!(!folder.exists());
    }

    fn stale(root: &Path) -> PathBuf {
        let path = root.join("NSIRD_requiemd_x");
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("Requiem-0.2.0.zip"), b"zip").unwrap();
        path
    }

    #[test]
    fn removes_a_stale_staging_folder() {
        let root = TempDir::new();
        let path = stale(root.path());
        assert_eq!(remove_stale(&path, root.path()), Ok(()));
        assert!(!path.exists());
        assert_eq!(remove_stale(&path, root.path()), Ok(()), "already gone");
    }

    #[test]
    fn refuses_a_symlinked_staging_folder() {
        let root = TempDir::new();
        let real = stale(root.path());
        let link = root.path().join("NSIRD_link");
        symlink(&real, &link).unwrap();
        assert!(
            remove_stale(&link, root.path())
                .unwrap_err()
                .contains("symbolic link")
        );
        assert!(real.exists());
    }

    #[test]
    fn refuses_a_foreign_staging_folder() {
        let root = TempDir::new();
        let elsewhere = TempDir::new();
        let path = stale(elsewhere.path());
        let err = remove_stale(&path, root.path()).unwrap_err();
        assert!(err.contains("outside"), "{err}");
        assert!(path.exists());

        let nested = stale(root.path()).join("inner");
        fs::DirBuilder::new().mode(0o700).create(&nested).unwrap();
        assert!(
            remove_stale(&nested, root.path())
                .unwrap_err()
                .contains("outside")
        );
    }

    #[test]
    fn refuses_a_staging_folder_with_another_mode_or_in_use() {
        let root = TempDir::new();
        let path = stale(root.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            remove_stale(&path, root.path())
                .unwrap_err()
                .contains("0700")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let held = DirLock::try_new(&path).unwrap().unwrap();
        assert!(
            remove_stale(&path, root.path())
                .unwrap_err()
                .contains("using it")
        );
        assert!(path.exists());
        drop(held);
        assert!(eventually(|| remove_stale(&path, root.path()).is_ok()));
    }
}
