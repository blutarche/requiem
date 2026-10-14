//! Staging and installing with macOS APIs and tools (sections 3.3 and 3.4).

use std::ffi::{CString, c_void};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask, NSURL};
use semver::Version;

use crate::failure::{Failure, Reason};
use crate::lock::DirLock;
use crate::manifest::{BUNDLE_ID, PLAYER_BUNDLE_ID};
use crate::stage::{self, APP_NAME};

const SWAP_LOCK_WAIT: Duration = Duration::from_secs(30);
/// Debug builds only: live tests install into a scratch folder that LaunchServices should
/// not learn about.
pub const SKIP_LSREGISTER_ENV: &str = "REQUIEM_UPDATE_SKIP_LSREGISTER";

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSRegisterURL(url: *const c_void, update: u8) -> i32;
}

/// A staging folder on the installed app's volume, locked while this player uses it so
/// another player's stale-folder cleanup leaves it alone.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
    _lock: DirLock,
}

impl Staging {
    pub fn create(installed: &Path) -> Result<Staging, String> {
        let dir = item_replacement_dir(installed)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let lock = DirLock::try_new(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .ok_or_else(|| format!("{} is locked", dir.display()))?;
        Ok(Staging { dir, _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Where the unzipped app sits, and after a swap the old app.
    pub fn app(&self) -> PathBuf {
        self.dir.join("unzipped").join(APP_NAME)
    }

    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }
}

/// A new folder from `NSItemReplacementDirectory` on `near`'s volume, as a canonical path.
fn item_replacement_dir(near: &Path) -> Result<PathBuf, String> {
    autoreleasepool(|_| {
        let url = NSURL::from_file_path(near)
            .ok_or_else(|| format!("{} is not a file path", near.display()))?;
        let dir = NSFileManager::defaultManager()
            .URLForDirectory_inDomain_appropriateForURL_create_error(
                NSSearchPathDirectory::ItemReplacementDirectory,
                NSSearchPathDomainMask::UserDomainMask,
                Some(&url),
                true,
            )
            .map_err(|e| {
                format!(
                    "no staging folder near {}: {}",
                    near.display(),
                    e.localizedDescription()
                )
            })?;
        let path = dir
            .to_file_path()
            .ok_or("the staging folder has no file path")?;
        fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))
    })
}

/// The canonical temporary-items folder that `NSItemReplacementDirectory` makes its folders
/// in on `near`'s volume.
pub fn temporary_items_root(near: &Path) -> Result<PathBuf, String> {
    let probe = item_replacement_dir(near)?;
    let _ = fs::remove_dir(&probe);
    probe
        .parent()
        .map(Path::to_owned)
        .ok_or_else(|| format!("{} has no parent", probe.display()))
}

/// Deletes a staging folder a player left behind, after the checks of section 3.3.
pub fn remove_stale(path: &Path) -> Result<(), String> {
    let near = path.parent().unwrap_or(path);
    if !near.exists() {
        return Ok(());
    }
    stage::remove_stale(path, &temporary_items_root(near)?)
}

/// Section 3.3 steps 2 to 6 on a zip whose size and SHA-256 matched the signed manifest.
/// Returns the staged app.
pub fn stage(staging: &Staging, zip: &Path, version: &str) -> Result<PathBuf, Failure> {
    let unzipped = staging.dir.join("unzipped");
    let out = Command::new("/usr/bin/ditto")
        .args(["-x", "-k", "--noqtn", "--noacl"])
        .arg(zip)
        .arg(&unzipped)
        .output()
        .map_err(|e| Failure::new(Reason::DownloadMismatch, format!("ditto: {e}")))?;
    if !out.status.success() {
        return Err(Failure::new(
            Reason::DownloadMismatch,
            format!("ditto: {}", String::from_utf8_lossy(&out.stderr).trim()),
        ));
    }
    let _ = fs::remove_file(zip);
    let app = stage::check_layout(&unzipped)?;
    check_bundle(&app, version)?;
    stage::self_test(&app, version)?;
    Ok(app)
}

/// Section 3.3 steps 4 and 5: the Info.plists and the code signatures.
pub fn check_bundle(app: &Path, version: &str) -> Result<(), Failure> {
    let bad = |detail: String| Failure::new(Reason::NotBuiltLikeThis, detail);
    let info = app.join("Contents/Info.plist");
    let helper_info = stage::helper_app(app).join("Contents/Info.plist");
    for (plist, key, want) in [
        (&info, "CFBundleIdentifier", BUNDLE_ID),
        (&info, "CFBundleShortVersionString", version),
        (&helper_info, "CFBundleIdentifier", PLAYER_BUNDLE_ID),
    ] {
        let got = plist_value(plist, key).map_err(bad)?;
        if got != want {
            let rel = plist.strip_prefix(app).unwrap_or(plist);
            return Err(bad(format!(
                "{} {key} is {got:?}, not {want:?}",
                rel.display()
            )));
        }
    }
    let items = [
        app.to_owned(),
        stage::helper_app(app),
        stage::cli_exe(app),
        app.join("Contents/Frameworks/libopus.0.dylib"),
    ];
    for item in &items {
        codesign_verify(item, false).map_err(bad)?;
    }
    codesign_verify(app, true).map_err(bad)
}

fn plist_value(plist: &Path, key: &str) -> Result<String, String> {
    let out = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Print :{key}")])
        .arg(plist)
        .output()
        .map_err(|e| format!("PlistBuddy: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{}: no {key}: {}",
            plist.display(),
            String::from_utf8_lossy(&out.stdout).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_owned())
}

fn codesign_verify(item: &Path, deep: bool) -> Result<(), String> {
    let mut cmd = Command::new("/usr/bin/codesign");
    cmd.args(["--verify", "--strict"]);
    if deep {
        cmd.arg("--deep");
    }
    let out = cmd
        .arg(item)
        .output()
        .map_err(|e| format!("codesign: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "codesign --verify{} {}: {}",
            if deep { " --deep" } else { "" },
            item.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The installed app's `CFBundleShortVersionString`.
pub fn installed_version(app: &Path) -> Result<Version, String> {
    let text = plist_value(
        &app.join("Contents/Info.plist"),
        "CFBundleShortVersionString",
    )?;
    Version::parse(&text).map_err(|e| format!("installed version {text:?}: {e}"))
}

/// Why the installed app can't replace itself, or `None` when it can.
pub fn not_updatable(installed: &Path) -> Option<String> {
    if installed
        .as_os_str()
        .as_bytes()
        .windows(18)
        .any(|w| w == b"/AppTranslocation/")
    {
        return Some("Requiem runs from a translocated copy".into());
    }
    let Ok(path) = cstring(installed) else {
        return Some("Requiem's path is not valid".into());
    };
    // SAFETY: statfs fills the zeroed struct; the path is NUL-terminated.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut fs) } != 0 {
        return Some(format!(
            "{}: {}",
            installed.display(),
            io::Error::last_os_error()
        ));
    }
    if fs.f_flags & libc::MNT_RDONLY as u32 != 0 {
        return Some("Requiem's volume is read-only".into());
    }
    let parent = installed.parent().and_then(|p| cstring(p).ok());
    // SAFETY: a NUL-terminated path.
    if parent.is_none_or(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK) } != 0) {
        return Some("Requiem's folder is not writable".into());
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Swapped {
    /// The staged app is installed; the old app now sits where the staged one was.
    Installed,
    /// The installed app is already this version or newer; nothing moved.
    AlreadyCurrent(Version),
}

/// Section 3.4's swap of the staged app for the installed one.
pub fn swap(staged: &Path, installed: &Path, version: &str) -> Result<Swapped, Failure> {
    let fail = |detail: String| Failure::new(Reason::NotInTime, detail);
    let staged_version = Version::parse(version).map_err(|e| fail(e.to_string()))?;
    let lock = lock_folder(installed).map_err(fail)?;
    let current = installed_version(installed).map_err(fail)?;
    if staged_version <= current {
        return Ok(Swapped::AlreadyCurrent(current));
    }
    rename_swap(staged, installed).map_err(fail)?;
    if let Err(f) = check_bundle(installed, version) {
        let back = match rename_swap(staged, installed) {
            Ok(()) => String::new(),
            Err(e) => format!("; swapping back failed: {e}"),
        };
        return Err(fail(format!("after the swap: {}{back}", f.detail)));
    }
    drop(lock);
    register(installed);
    Ok(Swapped::Installed)
}

/// Puts the old app, which a swap left at `staged`, back in place.
pub fn swap_back(staged: &Path, installed: &Path) -> Result<(), String> {
    swap_back_unregistered(staged, installed)?;
    register(installed);
    Ok(())
}

fn swap_back_unregistered(staged: &Path, installed: &Path) -> Result<(), String> {
    let _lock = lock_folder(installed)?;
    rename_swap(staged, installed)
}

/// Every player on the machine, for any user, locks the folder holding the app around a
/// swap, so two swaps never interleave.
fn lock_folder(installed: &Path) -> Result<DirLock, String> {
    let folder = installed
        .parent()
        .ok_or_else(|| format!("{} has no folder", installed.display()))?;
    DirLock::wait(folder, SWAP_LOCK_WAIT).map_err(|e| format!("{}: {e}", folder.display()))
}

fn rename_swap(a: &Path, b: &Path) -> Result<(), String> {
    let (ca, cb) = (cstring(a)?, cstring(b)?);
    // SAFETY: both paths are NUL-terminated.
    if unsafe { libc::renamex_np(ca.as_ptr(), cb.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(format!(
            "swapping {} and {}: {}",
            a.display(),
            b.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} holds a NUL byte", path.display()))
}

/// Forced registration with LaunchServices, so it sees the new version.
fn register(app: &Path) {
    #[cfg(debug_assertions)]
    if std::env::var_os(SKIP_LSREGISTER_ENV).is_some() {
        return;
    }
    let status = autoreleasepool(|_| {
        let Some(url) = NSURL::from_directory_path(app) else {
            return -1;
        };
        // SAFETY: NSURL is toll-free bridged with CFURLRef, and `url` outlives the call.
        unsafe { LSRegisterURL(Retained::as_ptr(&url).cast(), 1) }
    });
    if status != 0 {
        eprintln!(
            "requiem-update: LSRegisterURL {} failed: {status}",
            app.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn fake_app(dir: &Path, version: &str, id: &str, player_id: &str) -> PathBuf {
        let app = dir.join(APP_NAME);
        let plist = |path: &Path, id: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>
"#
                ),
            )
            .unwrap();
        };
        plist(&app.join("Contents/Info.plist"), id);
        plist(
            &stage::helper_app(&app).join("Contents/Info.plist"),
            player_id,
        );
        app
    }

    fn requiem(dir: &Path, version: &str) -> PathBuf {
        fake_app(dir, version, BUNDLE_ID, PLAYER_BUNDLE_ID)
    }

    #[test]
    fn staging_is_private_and_inside_the_temporary_items_folder() {
        let dir = TempDir::new();
        let installed = requiem(dir.path(), "0.1.0");
        let staging = Staging::create(&installed).unwrap();
        let path = staging.path().to_owned();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
        let root = temporary_items_root(&installed).unwrap();
        assert_eq!(path.parent(), Some(root.as_path()));
        assert!(
            remove_stale(&path).unwrap_err().contains("using it"),
            "a folder in use is left alone"
        );
        drop(staging);
        assert_eq!(remove_stale(&path), Ok(()));
        assert!(!path.exists());
    }

    #[test]
    fn rejects_a_wrong_bundle_id_or_version() {
        let dir = TempDir::new();
        let app = fake_app(dir.path(), "0.2.0", "dev.other.app", PLAYER_BUNDLE_ID);
        let f = check_bundle(&app, "0.2.0").unwrap_err();
        assert_eq!(f.reason, Reason::NotBuiltLikeThis);
        assert!(f.detail.contains("dev.other.app"), "{f}");

        let dir = TempDir::new();
        let app = fake_app(dir.path(), "0.2.0", BUNDLE_ID, "dev.other.player");
        assert!(
            check_bundle(&app, "0.2.0")
                .unwrap_err()
                .detail
                .contains("dev.other.player")
        );

        let dir = TempDir::new();
        let app = requiem(dir.path(), "0.1.9");
        let f = check_bundle(&app, "0.2.0").unwrap_err();
        assert!(
            f.detail.contains("CFBundleShortVersionString is \"0.1.9\""),
            "{f}"
        );
    }

    #[test]
    fn rejects_an_unsigned_bundle() {
        let dir = TempDir::new();
        let app = requiem(dir.path(), "0.2.0");
        let f = check_bundle(&app, "0.2.0").unwrap_err();
        assert_eq!(f.reason, Reason::NotBuiltLikeThis);
        assert!(f.detail.contains("codesign"), "{f}");
    }

    #[test]
    fn never_swaps_in_a_version_no_newer_than_the_installed_one() {
        let apps = TempDir::new();
        let installed = requiem(apps.path(), "0.2.0");
        for staged_version in ["0.2.0", "0.1.0"] {
            let staging = TempDir::new();
            let staged = requiem(staging.path(), staged_version);
            assert_eq!(
                swap(&staged, &installed, staged_version),
                Ok(Swapped::AlreadyCurrent(Version::new(0, 2, 0)))
            );
            assert_eq!(installed_version(&installed), Ok(Version::new(0, 2, 0)));
            assert_eq!(
                installed_version(&staged),
                Ok(Version::parse(staged_version).unwrap())
            );
        }
    }

    #[test]
    fn swaps_back_when_the_installed_app_fails_its_checks() {
        let apps = TempDir::new();
        let staging = TempDir::new();
        let installed = requiem(apps.path(), "0.1.0");
        let staged = requiem(staging.path(), "0.2.0");
        let f = swap(&staged, &installed, "0.2.0").unwrap_err();
        assert_eq!(
            (f.reason, f.kind()),
            (Reason::NotInTime, crate::Kind::Transient)
        );
        assert!(f.detail.contains("after the swap"), "{f}");
        assert!(!f.detail.contains("swapping back failed"), "{f}");
        assert_eq!(installed_version(&installed), Ok(Version::new(0, 1, 0)));
        assert_eq!(installed_version(&staged), Ok(Version::new(0, 2, 0)));
    }

    #[test]
    fn swap_back_exchanges_the_two_apps() {
        let apps = TempDir::new();
        let staging = TempDir::new();
        let installed = requiem(apps.path(), "0.2.0");
        let old = requiem(staging.path(), "0.1.0");
        swap_back_unregistered(&old, &installed).unwrap();
        assert_eq!(installed_version(&installed), Ok(Version::new(0, 1, 0)));
        assert_eq!(installed_version(&old), Ok(Version::new(0, 2, 0)));
    }

    #[test]
    fn an_app_in_a_writable_folder_is_updatable() {
        let dir = TempDir::new();
        let installed = requiem(dir.path(), "0.1.0");
        assert_eq!(not_updatable(&installed), None);
        let translocated = Path::new("/private/var/folders/x/AppTranslocation/ABC/d/Requiem.app");
        assert!(
            not_updatable(translocated)
                .unwrap()
                .contains("translocated")
        );
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let readonly = not_updatable(&installed);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(readonly.unwrap().contains("not writable"));
    }
}
