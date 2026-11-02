//! Self-update, as `docs/self-update-design.md` describes: the check schedule, the
//! background download and staging, and both ways of installing.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::linux_platform::layout as stage;
use crate::linux_platform::{self as macos, Staging, Swapped};
use crossbeam_channel::{Receiver, Sender, at, select, unbounded};
use requiem_ipc::{Client, Role, UpdateRequest, UpdateState, UpdateStatus};
use requiem_update::fetch::{self, Config, LinuxFound as Found};
use requiem_update::stage::Saved;
use requiem_update::state::{self, State};
use requiem_update::{Failure, Kind, Reason};
use semver::Version;

use crate::persist;

const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const TERM_GRACE: Duration = Duration::from_secs(10);
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const DEV_BUILD: &str = "not running from a writable Linux installation";
/// Beside the update state: where a restart keeps the files it may have to put back.
const SAVED_DIR: &str = "update-saved";

/// A staged update waiting to be installed.
pub struct Ready {
    version: Version,
    sha256: String,
    staging: Staging,
    app: PathBuf,
}

/// What the player does with a ready update once it stops serving.
pub enum Install {
    AtExit(Ready),
    Restart(Ready),
}

pub struct Updater {
    inner: Mutex<Inner>,
    statuses: Sender<UpdateStatus>,
    checks: Sender<()>,
    stop: Box<dyn Fn() + Send + Sync>,
}

struct Inner {
    /// `None` when the app can't update itself.
    installed: Option<PathBuf>,
    state: UpdateState,
    path: Option<PathBuf>,
    /// `update-state.json` as last loaded or saved.
    stored: State,
    ready: Option<Ready>,
    restart: bool,
    /// Set once the player stops serving; the worker then changes nothing.
    closing: bool,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn save(&self) {
        if let Some(path) = &self.path
            && let Err(e) = self.stored.save(path)
        {
            eprintln!("requiemd: {}: {e}", path.display());
        }
    }

    /// The state to return to after a check: a staged update stays ready.
    fn settled(&self) -> UpdateState {
        match &self.ready {
            Some(r) => UpdateState::Ready {
                version: r.version.to_string(),
            },
            None => UpdateState::Idle,
        }
    }

    /// The state after a failed check: a staged update stays ready.
    fn failed_check(&self, e: &str) -> UpdateState {
        match &self.ready {
            Some(_) => self.settled(),
            None => UpdateState::Error {
                reason: format!("the update check failed: {e}"),
            },
        }
    }

    /// Counts a failed try of `version` and forgets the staging folder.
    fn record_failure(&mut self, version: &str, sha256: &str, f: &Failure) -> UpdateState {
        self.stored.staging = None;
        let bad = self.stored.record_failure(version, sha256, f);
        self.save();
        if f.kind() == Kind::NotCounted {
            return UpdateState::Error {
                reason: format!(
                    "{version} did not install: {}; it is tried again at the next start",
                    f.reason.text()
                ),
            };
        }
        failure_state(version, f.reason.text(), bad)
    }
}

fn failure_state(version: &str, reason: &str, bad: bool) -> UpdateState {
    if bad {
        UpdateState::Failed {
            version: version.to_owned(),
            reason: reason.to_owned(),
        }
    } else {
        UpdateState::Error {
            reason: format!(
                "{version} did not install: {reason}; it is tried again at the next check"
            ),
        }
    }
}

/// Where a player starts: an install that failed in an earlier player, which has since
/// exited, still shows. The newest failed version speaks for all.
fn stored_state(stored: &State) -> UpdateState {
    stored
        .failures
        .iter()
        .filter_map(|f| Version::parse(&f.version).ok().map(|v| (v, f)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map_or(UpdateState::Idle, |(_, f)| {
            failure_state(&f.version, &f.reason, f.bad)
        })
}

impl Updater {
    /// Loads the update state and, when the app can update itself, starts the worker that
    /// checks on schedule. `exe` is [`ui::own_exe`]'s, which finds the installed app. `stop`
    /// asks the player to quit, for **Restart to Update**. Every status change after the one
    /// [`status`](Self::status) returns now arrives on the receiver.
    pub fn start(
        exe: Option<&Path>,
        stop: impl Fn() + Send + Sync + 'static,
    ) -> (Arc<Updater>, Receiver<UpdateStatus>) {
        let path = state::state_path();
        let mut stored = match path.as_deref().map(State::load) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                eprintln!("requiemd: {e}; starting over");
                State::default()
            }
            None => State::default(),
        };
        let current = Version::parse(CURRENT).expect("the package version is semver");
        stored.forget_older(&current);
        let app = exe.and_then(macos::installed_path);
        let (installed, state) = match app {
            None => (
                None,
                UpdateState::NotUpdatable {
                    reason: DEV_BUILD.into(),
                },
            ),
            Some(app) => match macos::not_updatable(&app) {
                Some(reason) => (None, UpdateState::NotUpdatable { reason }),
                None => (Some(app), stored_state(&stored)),
            },
        };
        let (statuses, statuses_rx) = unbounded();
        let (checks, checks_rx) = unbounded();
        let updatable = installed.is_some();
        let updater = Arc::new(Updater {
            inner: Mutex::new(Inner {
                installed,
                state,
                path,
                stored,
                ready: None,
                restart: false,
                closing: false,
            }),
            statuses,
            checks,
            stop: Box::new(stop),
        });
        if updatable {
            let worker = updater.clone();
            let _ = thread::Builder::new()
                .name("requiemd-update".into())
                .spawn(move || worker.work(&checks_rx));
        }
        (updater, statuses_rx)
    }

    pub fn status(&self) -> UpdateStatus {
        self.status_of(&lock(&self.inner))
    }

    fn status_of(&self, inner: &Inner) -> UpdateStatus {
        UpdateStatus {
            current: CURRENT.into(),
            auto: inner.stored.auto,
            state: inner.state.clone(),
        }
    }

    /// Publishes while `inner` is still held, so statuses arrive in order.
    fn set(&self, inner: &mut Inner, state: UpdateState) {
        inner.state = state;
        let _ = self.statuses.send(self.status_of(inner));
    }

    /// A request ignored in the current state republishes it, so the asker gets an answer.
    pub fn request(&self, req: UpdateRequest) {
        let mut inner = lock(&self.inner);
        match req {
            UpdateRequest::SetAuto(on) => {
                inner.stored.auto = on;
                inner.save();
            }
            UpdateRequest::Check => {
                let busy = matches!(
                    inner.state,
                    UpdateState::Checking | UpdateState::Downloading { .. }
                );
                if !busy && !inner.closing && self.checks.send(()).is_ok() {
                    return;
                }
            }
            UpdateRequest::Install => {
                if matches!(inner.state, UpdateState::Ready { .. }) && !inner.closing {
                    inner.restart = true;
                    inner.closing = true;
                    drop(inner);
                    (self.stop)();
                    return;
                }
            }
        }
        let state = inner.state.clone();
        self.set(&mut inner, state);
    }

    /// Called once the player has stopped taking requests: from here the worker changes
    /// nothing, and a ready update is the caller's to install.
    pub fn finish(&self) -> Option<Install> {
        let mut inner = lock(&self.inner);
        inner.closing = true;
        let ready = inner.ready.take()?;
        Some(if inner.restart {
            Install::Restart(ready)
        } else {
            Install::AtExit(ready)
        })
    }

    fn work(&self, checks: &Receiver<()>) {
        self.remove_stale_staging();
        let saved = lock(&self.inner).path.as_deref().and_then(saved_dir);
        if let Some(dir) = saved
            && let Err(e) = remove_stale_saved(&dir)
        {
            eprintln!("requiemd: {e}");
        }
        let mut next = Instant::now() + FIRST_CHECK;
        loop {
            select! {
                recv(checks) -> msg => match msg {
                    Ok(()) => self.check(true),
                    Err(_) => return,
                },
                recv(at(next)) -> _ => {
                    next = Instant::now() + CHECK_EVERY;
                    self.check(false);
                }
            }
        }
    }

    /// A player that died before installing left its staging folder named in the state. A
    /// folder another player still holds stays named: right after Restart to Update the old
    /// player holds it until it has deleted it or rolled back, and a later start clears it.
    fn remove_stale_staging(&self) {
        let Some(path) = lock(&self.inner).stored.staging.clone() else {
            return;
        };
        match macos::remove_stale(&path) {
            Ok(()) => {
                let mut inner = lock(&self.inner);
                if inner.stored.staging.as_ref() == Some(&path) {
                    inner.stored.staging = None;
                    inner.save();
                }
            }
            Err(e) => eprintln!("requiemd: {e}"),
        }
    }

    fn check(&self, manual: bool) {
        let mut inner = lock(&self.inner);
        let Some(installed) = inner.installed.clone() else {
            return;
        };
        if inner.closing {
            return;
        }
        if manual {
            inner.stored.clear_retries();
            inner.save();
        } else if !inner.stored.auto || inner.ready.is_some() || !inner.stored.due(now()) {
            return;
        }
        let mut checked = inner.stored.clone();
        self.set(&mut inner, UpdateState::Checking);
        drop(inner);

        let found = Config::new(CURRENT).and_then(|cfg| {
            let found = fetch::check_linux(&cfg, &mut checked, now())?;
            Ok((cfg, found))
        });
        let mut inner = lock(&self.inner);
        if inner.closing {
            return;
        }
        let (cfg, found) = match found {
            Ok(f) => f,
            Err(e) => {
                eprintln!("requiemd: update check failed: {e}");
                let state = inner.failed_check(&e);
                self.set(&mut inner, state);
                return;
            }
        };
        inner.stored.highest_seen = checked.highest_seen;
        inner.stored.last_check = checked.last_check;
        inner.save();
        let (m, version) = match found {
            Found::Update(m, version) => (m, version),
            Found::UpToDate(_) => {
                let state = match inner.ready {
                    Some(_) => inner.settled(),
                    None => UpdateState::UpToDate,
                };
                self.set(&mut inner, state);
                return;
            }
        };
        if inner.ready.as_ref().is_some_and(|r| r.version >= version) {
            let state = inner.settled();
            self.set(&mut inner, state);
            return;
        }
        if let Some(f) = inner.stored.bad(&m.version, &m.sha256) {
            let state = UpdateState::Failed {
                version: f.version.clone(),
                reason: f.reason.clone(),
            };
            self.set(&mut inner, state);
            return;
        }
        if let Some(old) = inner.ready.take() {
            remove_staging(old.staging);
        }
        let staging = match Staging::create(&installed) {
            Ok(s) => s,
            Err(e) => {
                let f = Failure::new(Reason::DownloadMismatch, e);
                eprintln!("requiemd: update {version}: {f}");
                let state = inner.record_failure(&m.version, &m.sha256, &f);
                self.set(&mut inner, state);
                return;
            }
        };
        inner.stored.staging = Some(staging.path().to_owned());
        inner.save();
        self.set(
            &mut inner,
            UpdateState::Downloading {
                version: version.to_string(),
            },
        );
        drop(inner);

        let zip = staging.path().join(&m.file);
        let staged =
            fetch::download_linux(&cfg, &m, &zip).and_then(|()| macos::stage(&installed, &zip, &m));
        let mut inner = lock(&self.inner);
        if inner.closing {
            // The next start deletes the folder the state names.
            return;
        }
        match staged {
            Ok(app) => {
                eprintln!("requiemd: update {version} is ready");
                inner.ready = Some(Ready {
                    version,
                    sha256: m.sha256,
                    staging,
                    app,
                });
                let state = inner.settled();
                self.set(&mut inner, state);
            }
            Err(f) => {
                eprintln!("requiemd: update {version}: {f}");
                remove_staging(staging);
                let state = inner.record_failure(&m.version, &m.sha256, &f);
                self.set(&mut inner, state);
            }
        }
    }

    /// Section 3.4's **Restart to Update**, from step 3. The server has stopped and the
    /// state writer finished; `instance` is the single-instance lock, still held. Opens the
    /// app afterwards when `gui` was connected.
    pub fn restart(&self, ready: Ready, instance: File, gui: bool) {
        let mut inner = lock(&self.inner);
        let Some(installed) = inner.installed.clone() else {
            return;
        };
        let version = ready.version.to_string();
        let files = restart_files();
        let files: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        let saved = match save_files(inner.path.as_deref(), &files) {
            Ok(s) => s,
            Err(e) => {
                let f = Failure::new(Reason::NotInTime, format!("saving files: {e}"));
                eprintln!("requiemd: restarting to {version}: {f}");
                remove_staging(ready.staging);
                inner.record_failure(&version, &ready.sha256, &f);
                drop(instance);
                start_player(&installed, gui);
                return;
            }
        };
        match macos::swap(&ready.app, &installed, &version) {
            Ok(Swapped::Installed) => {}
            Ok(Swapped::AlreadyCurrent(v)) => {
                eprintln!("requiemd: {v} is already installed");
                remove_saved(saved);
                remove_staging(ready.staging);
                inner.stored.staging = None;
                inner.save();
                drop(instance);
                start_player(&installed, gui);
                return;
            }
            Err(f) => {
                eprintln!("requiemd: restarting to {version}: {f}");
                remove_saved(saved);
                remove_staging(ready.staging);
                inner.record_failure(&version, &ready.sha256, &f);
                drop(instance);
                start_player(&installed, gui);
                return;
            }
        }
        drop(instance);

        let handed = requiem_ipc::socket_dir()
            .map_err(|e| format!("the new player: {e}"))
            .and_then(|dir| {
                hand_off(
                    &stage::daemon_exe(&installed),
                    &dir,
                    &version,
                    HANDOFF_TIMEOUT,
                )
            });
        let Err(detail) = handed else {
            if let Err(e) = macos::confirm(&installed) {
                eprintln!("requiemd: confirming update: {e}; keeping recovery state");
                return;
            }
            eprintln!("requiemd: restarted to {version}");
            if gui {
                macos::open_app(&installed);
            }
            // The new player owns the state file now; it forgets the folder at its next
            // start.
            remove_saved(saved);
            remove_staging(ready.staging);
            return;
        };

        let f = Failure::new(Reason::NotInTime, detail);
        eprintln!("requiemd: restarting to {version}: {f}; going back");
        if let Err(e) = macos::swap_back(&ready.app, &installed) {
            eprintln!("requiemd: {e}");
        }
        if let Err(e) = saved.restore() {
            eprintln!("requiemd: restoring saved files: {e}");
        }
        remove_saved(saved);
        remove_staging(ready.staging);
        // The new version may have rewritten the update state too.
        if let Some(path) = &inner.path {
            match State::load(path) {
                Ok(s) => inner.stored = s,
                Err(e) => eprintln!("requiemd: {e}"),
            }
        }
        inner.record_failure(&version, &ready.sha256, &f);
        start_player(&installed, gui);
    }
}

/// A player started from the installed app just before **Restart to Update** swaps it runs the
/// old version, and the restart takes its answer for a failed start. Called under the
/// single-instance lock, before listening: when the player installed in the app that holds
/// `exe` is newer than this one, this process becomes it, with the same pid and arguments.
/// Returns `Ok` when this player carries on as it is.
pub fn exec_newer_player(exe: &Path, reexeced: bool) -> Result<(), String> {
    let Some(app) = macos::installed_path(exe) else {
        return Ok(());
    };
    let current = Version::parse(CURRENT).expect("the package version is semver");
    let Some(installed) = newer_player(&app, &current, reexeced) else {
        return Ok(());
    };
    let player = stage::daemon_exe(&app);
    eprintln!(
        "requiemd: {} is {installed}, newer than this {CURRENT}; starting it instead",
        player.display()
    );
    let e = Command::new(&player)
        .args(std::env::args_os().skip(1))
        .env(crate::REEXECED_ENV, "1")
        .exec();
    Err(format!("starting {}: {e}", player.display()))
}

/// The version of the player installed in `app` when it is newer than `current`, unless this
/// process already replaced an older one.
fn newer_player(app: &Path, current: &Version, reexeced: bool) -> Option<Version> {
    if reexeced {
        return None;
    }
    match macos::player_version(app) {
        Ok(v) => (v > *current).then_some(v),
        Err(e) => {
            debug!("{e}");
            None
        }
    }
}

/// The files a restart saves, to put back if the new player fails: the player's state and
/// every sign-in.
pub(crate) fn restart_files() -> Vec<PathBuf> {
    [
        persist::state_path(),
        state::state_path(),
        requiem_provider_ytmusic::session_file(),
        requiem_provider_spotify::credentials_file(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn saved_dir(state: &Path) -> Option<PathBuf> {
    Some(state.parent()?.join(SAVED_DIR))
}

/// Section 3.4 step 3, into the private folder that holds the update state, not the staging
/// folder on the app's volume. Copies left by a player that died go first.
fn save_files(state: Option<&Path>, files: &[&Path]) -> Result<Saved, String> {
    let dir = state
        .and_then(saved_dir)
        .ok_or("there is no folder for the update state")?;
    remove_stale_saved(&dir)?;
    Saved::new(&dir, files, requiem_provider_ytmusic::exclude_from_backups)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Deletes copies a restarting player left behind, after the same checks as a stale staging
/// folder; copies another player still holds are left alone.
fn remove_stale_saved(dir: &Path) -> Result<(), String> {
    let Some(parent) = dir.parent() else {
        return Ok(());
    };
    match fs::canonicalize(parent) {
        Ok(root) => stage::remove_stale(dir, &root),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", parent.display())),
    }
}

fn remove_saved(saved: Saved) {
    if let Err(e) = saved.remove() {
        eprintln!("requiemd: removing saved files: {e}");
    }
}

fn remove_staging(staging: Staging) {
    let path = staging.path().to_owned();
    if let Err(e) = staging.remove() {
        eprintln!("requiemd: {}: {e}", path.display());
    }
}

/// Starts the player installed at `app` and waits until it listens, then opens the window
/// if it was open.
fn start_player(app: &Path, gui: bool) {
    match Client::connect_or_spawn(&stage::daemon_exe(app), Role::Cli, HANDOFF_TIMEOUT) {
        Ok(_) => {}
        Err(e) => eprintln!("requiemd: starting the player again: {e}"),
    }
    if gui {
        macos::open_app(app);
    }
}

/// Section 3.4 steps 5 and 6: starts the new player from `exe`, listening in `dir`, and waits
/// up to `timeout` for a `Welcome` from `version`. On failure it stops that player, and the
/// one that answered if it runs from `exe`, while `exe` is still the new version's.
fn hand_off(exe: &Path, dir: &Path, version: &str, timeout: Duration) -> Result<(), String> {
    let mut child = macos::spawn_handoff(exe, dir)
        .map_err(|e| format!("the new player: {}: {e}", exe.display()))?;
    let connected = Client::connect_spawned(&mut child, dir, Role::Cli, timeout);
    let peer = connected.as_ref().ok().and_then(|(c, _)| c.peer_pid().ok());
    let failure = match &connected {
        Err(e) => format!("the new player: {e}"),
        Ok((c, _)) if c.server_version() == version => return Ok(()),
        Ok((c, _)) => format!(
            "the player that answered is {}, not {version}",
            c.server_version()
        ),
    };
    drop(connected);
    let child_pid = child.id() as i32;
    stop(&mut child);
    // Once reaped, the child's pid may belong to anyone.
    if let Some(pid) = peer
        && pid != child_pid
        && runs(pid, exe)
    {
        terminate(pid);
    }
    Err(failure)
}

/// Whether `pid` is another process running the executable at `exe`.
fn runs(pid: i32, exe: &Path) -> bool {
    // SAFETY: getpid has no preconditions.
    if pid <= 1 || pid == unsafe { libc::getpid() } {
        return false;
    }
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .zip(fs::canonicalize(exe).ok())
        .is_some_and(|(a, b)| a == b)
}

/// SIGTERM, then SIGKILL after [`TERM_GRACE`]; reaped either way.
fn stop(child: &mut Child) {
    if let Ok(None) = child.try_wait() {
        // SAFETY: kill has no memory-safety preconditions; the child is not reaped yet, so
        // the pid is still its own.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + TERM_GRACE;
        while let Ok(None) = child.try_wait() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = child.wait();
}

/// SIGTERM, then SIGKILL if `pid` is still running after [`TERM_GRACE`].
fn terminate(pid: i32) {
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + TERM_GRACE;
    // SAFETY: as above; signal 0 only checks that the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() >= deadline {
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}
