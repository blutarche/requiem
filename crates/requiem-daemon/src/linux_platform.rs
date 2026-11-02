use requiem_update::linux::Installation;
use requiem_update::linux_manifest::Manifest;
use requiem_update::{DirLock, Failure, Reason};
use semver::Version;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub mod layout {
    pub use super::daemon_exe;
    pub use requiem_update::stage::remove_stale;
}

pub struct Staging {
    dir: PathBuf,
    _lock: DirLock,
}
impl Staging {
    pub fn create(installed: &Path) -> Result<Self, String> {
        let dir = installed.join(format!(".requiem-download-{}", std::process::id()));
        if dir.exists() {
            return Err("download directory already exists".into());
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| e.to_string())?;
        let lock = DirLock::try_new(&dir)
            .map_err(|e| e.to_string())?
            .ok_or("download directory is busy")?;
        Ok(Self { dir, _lock: lock })
    }
    pub fn path(&self) -> &Path {
        &self.dir
    }
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(self.dir)
    }
}
pub enum Swapped {
    Installed,
    AlreadyCurrent(Version),
}
pub fn daemon_exe(root: &Path) -> PathBuf {
    root.join("requiemd")
}
pub fn installed_path(exe: &Path) -> Option<PathBuf> {
    if exe.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    Installation::from_executable(exe)
        .ok()
        .map(|i| i.directory().to_owned())
}
pub fn not_updatable(root: &Path) -> Option<String> {
    Installation::from_executable(&daemon_exe(root))
        .err()
        .map(|e| e.to_string())
}
pub fn stage(root: &Path, archive: &Path, manifest: &Manifest) -> Result<PathBuf, Failure> {
    Installation::from_executable(&daemon_exe(root))
        .and_then(|i| i.prepare(archive, manifest))
        .map(|g| g.path().to_owned())
        .map_err(failure)
}
fn failure(e: io::Error) -> Failure {
    Failure::new(Reason::NotInTime, e.to_string())
}
pub fn player_version(root: &Path) -> Result<Version, String> {
    let out =
        requiem_update::stage::run(&daemon_exe(root), &["--version"], Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
    let requiem_update::stage::Ran::Exited(status, text) = out else {
        return Err("version check timed out".into());
    };
    if !status.success() {
        return Err("version check failed".into());
    }
    Version::parse(
        text.trim()
            .strip_prefix("requiemd ")
            .ok_or("invalid daemon version")?,
    )
    .map_err(|e| e.to_string())
}
pub fn swap(staged: &Path, installed: &Path, version: &str) -> Result<Swapped, Failure> {
    let want =
        Version::parse(version).map_err(|e| Failure::new(Reason::NotInTime, e.to_string()))?;
    if player_version(installed).is_ok_and(|v| v >= want) {
        return Ok(Swapped::AlreadyCurrent(want));
    }
    let i = Installation::from_executable(&daemon_exe(installed)).map_err(failure)?;
    let generation = i.generation(staged).map_err(failure)?;
    i.save_state(&super::update::restart_files())
        .map_err(failure)?;
    i.activate(&generation).map_err(failure)?;
    i.begin_handoff(&generation).map_err(failure)?;
    Ok(Swapped::Installed)
}
pub fn swap_back(_staged: &Path, installed: &Path) -> Result<(), String> {
    Installation::from_executable(&daemon_exe(installed))
        .and_then(|i| i.rollback())
        .map_err(|e| e.to_string())
}
pub fn remove_stale(path: &Path) -> Result<(), String> {
    if !path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with(".requiem-download-"))
    {
        return Err("invalid stale download path".into());
    }
    let Some(parent) = path.parent() else {
        return Err("missing download parent".into());
    };
    requiem_update::stage::remove_stale(path, parent)
}
pub fn open_app(root: &Path) {
    let _ = Command::new(root.join("requiem-gui"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
pub fn spawn_handoff(exe: &Path, dir: &Path) -> io::Result<Child> {
    let i = Installation::from_executable(exe)?;
    let token = i.handoff_token()?;
    drop(i);
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    Command::new(exe)
        .env("REQUIEM_SOCKET_DIR", dir)
        .env("REQUIEM_LINUX_HANDOFF", token)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
}
pub fn confirm(root: &Path) -> io::Result<()> {
    let i = Installation::from_executable(&daemon_exe(root))?;
    i.confirm()?;
    i.prune_generations()
}
