//! Journaled Linux installation. Call `recover` before loading application state.
use crate::{DirLock, linux_manifest::Manifest};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const EXECUTABLES: [&str; 3] = ["requiem", "requiemd", "requiem-gui"];
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
fn bad(s: impl ToString) -> io::Error {
    io::Error::other(s.to_string())
}
fn sync(p: &Path) -> io::Result<()> {
    File::open(p)?.sync_all()
}
fn private(p: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(p)
}

#[derive(Debug, Clone)]
pub struct Generation {
    dir: PathBuf,
}
impl Generation {
    pub fn path(&self) -> &Path {
        &self.dir
    }
    pub fn cli(&self) -> PathBuf {
        self.dir.join(EXECUTABLES[0])
    }
    pub fn daemon(&self) -> PathBuf {
        self.dir.join(EXECUTABLES[1])
    }
    pub fn gui(&self) -> PathBuf {
        self.dir.join(EXECUTABLES[2])
    }
}
/// Holds the installation directory lock. Drop before starting another updater.
pub struct Installation {
    bin: PathBuf,
    store: PathBuf,
    _lock: DirLock,
}
impl Installation {
    pub fn detect() -> io::Result<Self> {
        Self::from_executable(&std::env::current_exe()?)
    }
    /// Resolves a user symlink to the actual side-by-side installation.
    pub fn from_executable(exe: &Path) -> io::Result<Self> {
        let exe = fs::canonicalize(exe)?;
        if !EXECUTABLES
            .iter()
            .any(|n| exe.file_name().is_some_and(|f| f == *n))
        {
            return Err(bad("not a Requiem executable"));
        }
        let mut bin = exe
            .parent()
            .ok_or_else(|| bad("missing installation directory"))?
            .to_owned();
        if bin
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("generation-"))
            && bin
                .parent()
                .is_some_and(|p| p.file_name().is_some_and(|n| n == ".requiem-update"))
        {
            bin = bin
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| bad("invalid generation location"))?
                .to_owned();
        }
        if bin.starts_with("/usr")
            || bin.starts_with("/opt")
            || bin.starts_with("/bin")
            || bin.starts_with("/sbin")
        {
            return Err(bad(
                "system installation: reinstall manually; sudo is not supported",
            ));
        }
        for n in EXECUTABLES {
            let m = fs::symlink_metadata(bin.join(n))?;
            if !m.is_file() || m.uid() != unsafe { libc::geteuid() } {
                return Err(bad("requires three user-owned regular sibling executables"));
            }
        }
        let owner = fs::metadata(&bin)?;
        if owner.uid() != unsafe { libc::geteuid() } || owner.mode() & 0o022 != 0 {
            return Err(bad(
                "installation directory must be user-owned and not writable by other users",
            ));
        }
        let lock = DirLock::try_new(&bin)?.ok_or_else(|| bad("installation is locked"))?;
        let store = bin.join(".requiem-update");
        if !store.exists() {
            private(&store)?;
            sync(&bin)?;
        }
        let m = fs::symlink_metadata(&store)?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
            return Err(bad("unsafe private update directory"));
        }
        Ok(Self {
            bin,
            store,
            _lock: lock,
        })
    }
    pub fn directory(&self) -> &Path {
        &self.bin
    }
    /// Keeps preparation/rollback generations only while activation is pending.
    pub fn prune_generations(&self) -> io::Result<()> {
        if self.store.join("journal").exists() {
            return Err(bad("activation is still pending"));
        }
        let running = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_owned));
        for entry in fs::read_dir(&self.store)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("generation-")
                && entry.file_type()?.is_dir()
                && running.as_ref() != Some(&entry.path())
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
        sync(&self.store)
    }
    pub fn generation(&self, path: &Path) -> io::Result<Generation> {
        if path.parent() != Some(self.store.as_path()) || !path.is_dir() {
            return Err(bad("foreign generation"));
        }
        Ok(Generation {
            dir: path.to_owned(),
        })
    }
    pub fn save_state(&self, paths: &[PathBuf]) -> io::Result<()> {
        let mut saved: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
        for path in paths {
            let bytes = match fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            saved.push((path.clone(), bytes));
        }
        self.write_record("saved-state", &serde_json::to_vec(&saved).map_err(bad)?)
    }
    fn write_record(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let tmp = self.store.join(format!("{name}.new"));
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(tmp, self.store.join(name))?;
        sync(&self.store)
    }
    fn restore_state(&self) -> io::Result<()> {
        let bytes = match fs::read(self.store.join("saved-state")) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let saved: Vec<(PathBuf, Option<Vec<u8>>)> = serde_json::from_slice(&bytes).map_err(bad)?;
        for (path, bytes) in saved {
            match bytes {
                Some(bytes) => {
                    let tmp = path.with_extension("requiem-update-restore");
                    let mut f = OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&tmp)?;
                    f.write_all(&bytes)?;
                    f.sync_all()?;
                    fs::rename(&tmp, &path)?;
                    if let Some(parent) = path.parent() {
                        sync(parent)?;
                    }
                }
                None => match fs::remove_file(&path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    result => result?,
                },
            }
        }
        Ok(())
    }
    pub fn begin_handoff(&self, generation: &Generation) -> io::Result<()> {
        let mut bytes = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        self.write_record(
            "handoff",
            &serde_json::to_vec(&(std::process::id(), token, generation.dir.clone()))
                .map_err(bad)?,
        )
    }
    pub fn handoff_token(&self) -> io::Result<String> {
        let (_, token, _): (u32, String, PathBuf) =
            serde_json::from_slice(&fs::read(self.store.join("handoff"))?).map_err(bad)?;
        Ok(token)
    }
    #[cfg(target_os = "linux")]
    fn stop_handoff_child(&self) -> io::Result<()> {
        let Ok(text) = fs::read_to_string(self.store.join("handoff-child")) else {
            return Ok(());
        };
        let pid: u32 = text.trim().parse().map_err(bad)?;
        if pid <= 1 || pid == std::process::id() {
            return Ok(());
        }
        let Ok(token) = self.handoff_token() else {
            return Ok(());
        };
        let expected = format!("REQUIEM_LINUX_HANDOFF={token}");
        let Ok(env) = fs::read(format!("/proc/{pid}/environ")) else {
            return Ok(());
        };
        if !env
            .split(|b| *b == 0)
            .any(|entry| entry == expected.as_bytes())
        {
            return Ok(());
        }
        // The recorded child is authenticated by its per-handoff random environment token.
        if unsafe { libc::kill(pid as i32, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fs::read_link(format!("/proc/{pid}/exe")).is_ok() {
            if std::time::Instant::now() >= deadline {
                return Err(bad("handoff child did not stop"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    fn handoff_parent_alive(&self) -> bool {
        let Ok(bytes) = fs::read(self.store.join("handoff")) else {
            return false;
        };
        let Ok((pid, _, _)) = serde_json::from_slice::<(u32, String, PathBuf)>(&bytes) else {
            return false;
        };
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(target_os = "linux")]
    fn intended_handoff(&self) -> bool {
        let Ok(token) = std::env::var("REQUIEM_LINUX_HANDOFF") else {
            return false;
        };
        let Ok(bytes) = fs::read(self.store.join("handoff")) else {
            return false;
        };
        let Ok((pid, expected, _)) = serde_json::from_slice::<(u32, String, PathBuf)>(&bytes)
        else {
            return false;
        };
        token == expected && unsafe { libc::getppid() } == pid as i32
    }
    fn fresh(&self) -> io::Result<PathBuf> {
        for i in 0..10000 {
            let p = self
                .store
                .join(format!("generation-{}-{i}", std::process::id()));
            match private(&p) {
                Ok(()) => {
                    sync(&self.store)?;
                    return Ok(p);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e),
            }
        }
        Err(bad("too many retained generations"))
    }
    /// Caller authenticates manifest bytes with linux_manifest::verify first.
    /// This method verifies archive size/hash, bounded layout and executable preflight.
    pub fn prepare(&self, archive: &Path, manifest: &Manifest) -> io::Result<Generation> {
        crate::linux_manifest::parse(&serde_json::to_vec(manifest)?, std::env::consts::ARCH)
            .map_err(bad)?;
        let mut input = File::open(archive)?.take(manifest.size + 1);
        let mut compressed = Vec::new();
        input.read_to_end(&mut compressed)?;
        if compressed.len() as u64 != manifest.size {
            return Err(bad("compressed size mismatch"));
        }
        let hash = ring::digest::digest(&ring::digest::SHA256, &compressed);
        let hex: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if hex != manifest.sha256 {
            return Err(bad("archive hash mismatch"));
        }
        let dir = self.fresh()?;
        let result = (|| {
            let decoder = flate2::read::GzDecoder::new(&compressed[..]);
            let mut tar = tar::Archive::new(decoder.take(MAX_BYTES + 1));
            let root = manifest
                .file
                .strip_suffix(".tar.gz")
                .ok_or_else(|| bad("archive name"))?;
            let mut seen = HashSet::new();
            let mut total = 0u64;
            for item in tar.entries()?.raw(true) {
                let mut e = item?;
                let path = e.path()?.into_owned();
                if path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
                    || !seen.insert(path.clone())
                    || seen.len() > 256
                {
                    return Err(bad("unsafe or duplicate archive path"));
                }
                let rel = path
                    .strip_prefix(root)
                    .map_err(|_| bad("unexpected archive root"))?;
                let kind = e.header().entry_type();
                if kind.is_dir() {
                    if !rel.as_os_str().is_empty() && !rel.starts_with("icons") {
                        return Err(bad("unexpected directory"));
                    }
                    continue;
                }
                if !kind.is_file() {
                    return Err(bad("archive links and special files forbidden"));
                }
                total = total
                    .checked_add(e.size())
                    .ok_or_else(|| bad("size overflow"))?;
                if total > MAX_BYTES {
                    return Err(bad("unpacked size limit"));
                }
                let binary = EXECUTABLES.iter().any(|n| rel == Path::new(n));
                let known = matches!(
                    rel.to_str(),
                    Some(
                        "README.md"
                            | "LICENSE-MIT"
                            | "LICENSE-APACHE"
                            | "io.github.blutarche.Requiem.desktop"
                    )
                ) || (rel.starts_with("icons/hicolor")
                    && rel.extension().is_some_and(|x| x == "png"));
                if !binary && !known {
                    return Err(bad("unexpected archive file"));
                }
                if binary {
                    let mut out = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o700)
                        .open(dir.join(rel))?;
                    io::copy(&mut e, &mut out)?;
                    out.sync_all()?;
                } else {
                    io::copy(&mut e, &mut io::sink())?;
                }
            }
            for n in EXECUTABLES {
                let exe = dir.join(n);
                for arg in ["--update-self-test", "--version"] {
                    match crate::stage::run(&exe, &[arg], Duration::from_secs(10))? {
                        crate::stage::Ran::Exited(status, out)
                            if status.success()
                                && (arg != "--version"
                                    || out.trim() == format!("{n} {}", manifest.version)) =>
                        {
                            ()
                        }
                        _ => return Err(bad(format!("{n} {arg} failed or version mismatch"))),
                    }
                }
            }
            sync(&dir)?;
            Ok(Generation { dir: dir.clone() })
        })();
        if result.is_err() {
            fs::remove_dir_all(&dir)?;
        }
        result
    }
    /// Copy current files to an immutable pinned generation.
    pub fn installed(&self) -> io::Result<Generation> {
        let dir = self.fresh()?;
        for n in EXECUTABLES {
            fs::copy(self.bin.join(n), dir.join(n))?;
            sync(&dir.join(n))?;
        }
        sync(&dir)?;
        Ok(Generation { dir })
    }
    fn replace(&self, source: &Path, name: &str) -> io::Result<()> {
        let temp = self.store.join("replacement");
        // A previous crash may leave only this private scratch file.
        match fs::remove_file(&temp) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        fs::copy(source.join(name), &temp)?;
        sync(&temp)?;
        fs::rename(&temp, self.bin.join(name))?;
        sync(&self.bin)?;
        sync(&self.store)
    }
    /// Journal is durable before the first replacement. Pending activation rolls back
    /// on recovery until the caller explicitly confirms external health/state checks.
    pub fn activate(&self, generation: &Generation) -> io::Result<()> {
        self.recover()?;
        if generation.dir.parent() != Some(self.store.as_path()) {
            return Err(bad("foreign generation"));
        }
        let old = self.installed()?;
        let journal = self.store.join("journal");
        let pending = self.store.join("journal.new");
        match fs::remove_file(&pending) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)?;
        f.write_all(old.dir.file_name().unwrap().as_encoded_bytes())?;
        f.sync_all()?;
        fs::rename(&pending, &journal)?;
        sync(&self.store)?;
        for n in EXECUTABLES {
            if let Err(e) = self.replace(&generation.dir, n) {
                self.rollback()?;
                return Err(e);
            }
        }
        Ok(())
    }
    pub fn confirm(&self) -> io::Result<()> {
        // The journal commits binary activation; backups can be removed only afterwards.
        match fs::remove_file(self.store.join("journal")) {
            Ok(()) => {
                sync(&self.store)?;
                for name in ["saved-state", "handoff", "handoff-child"] {
                    let _ = fs::remove_file(self.store.join(name));
                }
                sync(&self.store)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    pub fn rollback(&self) -> io::Result<()> {
        let bytes = match fs::read_to_string(self.store.join("journal")) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !bytes.starts_with("generation-") || Path::new(&bytes).components().count() != 1 {
            return Err(bad("invalid recovery journal"));
        }
        #[cfg(target_os = "linux")]
        self.stop_handoff_child()?;
        for n in EXECUTABLES {
            self.replace(&self.store.join(&bytes), n)?;
        }
        self.restore_state()?;
        self.confirm()
    }
    pub fn recover(&self) -> io::Result<()> {
        self.rollback()
    }
}

/// Called before application state is read. Intended handoff children leave recovery to
/// their supervising old daemon; an ordinary launch rolls back an unconfirmed activation.
#[cfg(target_os = "linux")]
pub fn startup() -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let Some(parent) = exe.parent() else {
        return Ok(());
    };
    let root = if parent
        .parent()
        .is_some_and(|p| p.file_name().is_some_and(|n| n == ".requiem-update"))
    {
        parent.parent().and_then(Path::parent).unwrap_or(parent)
    } else {
        parent
    };
    if !root.join(".requiem-update").exists() {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let install = loop {
        match Installation::from_executable(&exe) {
            Ok(i) => {
                if i.intended_handoff() {
                    i.write_record("handoff-child", std::process::id().to_string().as_bytes())?;
                    return Ok(());
                }
                if i.store.join("journal").exists() && i.handoff_parent_alive() {
                    if std::time::Instant::now() >= deadline {
                        return Err(bad("update handoff is still in progress"));
                    }
                    drop(i);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                break i;
            }
            Err(e)
                if e.to_string() == "installation is locked"
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e),
        }
    };
    if install.store.join("journal").exists() {
        install.recover()?;
        let entry = install.bin.join(
            exe.file_name()
                .ok_or_else(|| bad("missing executable name"))?,
        );
        drop(install);
        return Err(std::process::Command::new(entry)
            .args(std::env::args_os().skip(1))
            .exec());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::os::unix::fs::PermissionsExt;
    fn script(n: &str, v: &str) -> Vec<u8> {
        format!("#!/bin/sh\ncase \"$1\" in --version) echo '{n} {v}';; --update-self-test) exit 0;; *) exit 4;; esac\n").into_bytes()
    }
    fn setup(p: &Path) -> Installation {
        for n in EXECUTABLES {
            fs::write(p.join(n), script(n, "1.0.0")).unwrap();
            fs::set_permissions(p.join(n), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(p.join("unrelated"), "keep").unwrap();
        Installation::from_executable(&p.join("requiem")).unwrap()
    }
    fn archive(p: &Path, extra: Option<(&str, u8)>) -> Manifest {
        archive_with(p, extra, "2.0.0", false)
    }
    fn archive_with(p: &Path, extra: Option<(&str, u8)>, version: &str, fail: bool) -> Manifest {
        let root = format!("requiem-2.0.0-linux-{}", std::env::consts::ARCH);
        let mut builder = tar::Builder::new(Vec::new());
        for n in EXECUTABLES {
            let bytes = if fail && n == "requiem-gui" {
                b"#!/bin/sh\nexit 9\n".to_vec()
            } else {
                script(n, version)
            };
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("{root}/{n}"), &bytes[..])
                .unwrap();
        }
        if let Some((name, kind)) = extra {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(0o755);
            h.set_entry_type(tar::EntryType::new(kind));
            // Raw header permits malicious paths that Builder normally rejects.
            let path = format!("{root}/{name}");
            h.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            builder.append(&h, &[][..]).unwrap();
        }
        let raw = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        let bytes = gz.finish().unwrap();
        fs::write(p, &bytes).unwrap();
        Manifest {
            format: 1,
            platform: "linux".into(),
            architecture: std::env::consts::ARCH.into(),
            bundle_identifier: crate::manifest::BUNDLE_ID.into(),
            version: "2.0.0".into(),
            file: format!("{root}.tar.gz"),
            size: bytes.len() as u64,
            sha256: ring::digest::digest(&ring::digest::SHA256, &bytes)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        }
    }
    fn versions(i: &Installation, v: &str) {
        for n in EXECUTABLES {
            let out = std::process::Command::new(i.bin.join(n))
                .arg("--version")
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(
                String::from_utf8(out.stdout).unwrap().trim(),
                format!("{n} {v}")
            );
        }
        assert_eq!(fs::read_to_string(i.bin.join("unrelated")).unwrap(), "keep");
    }
    #[test]
    fn recovery_restores_saved_application_state_before_launch() {
        let dir = TempDir::new();
        let i = setup(dir.path());
        let state = dir.path().join("player-state");
        fs::write(&state, b"old queue").unwrap();
        let path = dir.path().join("release.gz");
        let m = archive(&path, None);
        let generation = i.prepare(&path, &m).unwrap();
        i.save_state(&[state.clone()]).unwrap();
        i.activate(&generation).unwrap();
        fs::write(&state, b"incompatible new queue").unwrap();
        i.recover().unwrap();
        assert_eq!(fs::read(&state).unwrap(), b"old queue");
        versions(&i, "1.0.0");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rollback_stops_only_the_authenticated_handoff_child() {
        let dir = TempDir::new();
        let i = setup(dir.path());
        let path = dir.path().join("release.gz");
        let m = archive(&path, None);
        let generation = i.prepare(&path, &m).unwrap();
        i.activate(&generation).unwrap();
        i.begin_handoff(&generation).unwrap();
        let token = i.handoff_token().unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .env("REQUIEM_LINUX_HANDOFF", &token)
            .spawn()
            .unwrap();
        i.write_record("handoff-child", child.id().to_string().as_bytes())
            .unwrap();
        i.recover().unwrap();
        assert!(!child.wait().unwrap().success());
        versions(&i, "1.0.0");
    }

    #[test]
    fn executable_activation_rollback_and_interrupted_recovery() {
        let dir = TempDir::new();
        let i = setup(dir.path());
        let path = dir.path().join("release.gz");
        let m = archive(&path, None);
        let g = i.prepare(&path, &m).unwrap();
        assert!(Installation::from_executable(&i.bin.join("requiem")).is_err());
        assert!(Installation::from_executable(&g.daemon()).is_err());
        let old = i.installed().unwrap();
        fs::write(
            i.store.join("journal"),
            old.dir.file_name().unwrap().as_encoded_bytes(),
        )
        .unwrap();
        sync(&i.store.join("journal")).unwrap();
        sync(&i.store).unwrap();
        i.replace(&g.dir, "requiem").unwrap();
        drop(i);
        let i = Installation::from_executable(&dir.path().join("requiemd")).unwrap();
        i.recover().unwrap();
        versions(&i, "1.0.0");
        i.activate(&g).unwrap();
        versions(&i, "2.0.0");
        i.rollback().unwrap();
        versions(&i, "1.0.0");
        i.activate(&g).unwrap();
        i.confirm().unwrap();
        versions(&i, "2.0.0");
        // Simulate death after only the first original has been replaced.
        let old = i.installed().unwrap();
        fs::write(
            i.store.join("journal"),
            old.dir.file_name().unwrap().as_encoded_bytes(),
        )
        .unwrap();
        sync(&i.store.join("journal")).unwrap();
        sync(&i.store).unwrap();
        fs::write(i.bin.join("requiem"), script("requiem", "3.0.0")).unwrap();
        drop(i);
        let i = Installation::from_executable(&dir.path().join("requiemd")).unwrap();
        i.recover().unwrap();
        versions(&i, "2.0.0");
        i.recover().unwrap();
        // Forced replacement failure after CLI replacement must restore all three.
        fs::remove_file(g.daemon()).unwrap();
        assert!(i.activate(&g).is_err());
        versions(&i, "2.0.0");
    }
    #[test]
    fn malicious_archives_and_wrong_versions_are_rejected() {
        let dir = TempDir::new();
        let i = setup(dir.path());
        let path = dir.path().join("release.gz");
        for extra in [
            ("../escape", b'0'),
            ("requiem", b'0'),
            ("link", b'2'),
            ("hard", b'1'),
            ("fifo", b'6'),
            ("unknown", b'0'),
        ] {
            let m = archive(&path, Some(extra));
            assert!(i.prepare(&path, &m).is_err(), "{extra:?}");
            versions(&i, "1.0.0");
        }
        for (v, fail) in [("9.0.0", false), ("2.0.0", true)] {
            let m = archive_with(&path, None, v, fail);
            assert!(i.prepare(&path, &m).is_err());
            versions(&i, "1.0.0");
        }
        let mut m = archive(&path, None);
        m.size += 1;
        assert!(i.prepare(&path, &m).is_err());
        let m = archive(&path, None);
        let g = i.prepare(&path, &m).unwrap();
        fs::write(g.gui(), "#!/bin/sh\nexit 9\n").unwrap();
        assert!(
            matches!(crate::stage::run(&g.gui(), &["--update-self-test"],Duration::from_secs(1)).unwrap(),crate::stage::Ran::Exited(s,_) if !s.success())
        );
    }
}
