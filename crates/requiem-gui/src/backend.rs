//! Where commands go and events come from: the fixed demo state, or `requiemd` over IPC.
//! Each window hands over its own way to reach its main thread.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError, bounded};
use requiem_core::{Command, Event};
use requiem_ipc::{Client, Incoming, Role, UpdateRequest, UpdateStatus};

use crate::demo;
use crate::vm::{Link, LinkError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Commands waiting for the writer thread. Beyond this the daemon is not keeping up, and
/// newer commands are dropped rather than queued without bound.
const COMMAND_QUEUE: usize = 64;
const BACKOFF: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

#[derive(Debug)]
pub enum Msg {
    Event(Event),
    Raise,
    Link(Link),
    Update(UpdateStatus),
}

/// How the connection thread reaches the window's main thread.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Hands a batch of messages to the main thread.
    pub post: fn(Vec<Msg>),
    /// The player quit on purpose: the window closes with it.
    pub quit: fn(),
}

pub enum Backend {
    Demo(RefCell<demo::Daemon>),
    Live(Live),
}

impl Backend {
    /// Demo commands are answered synchronously; the returned events are applied by the
    /// caller. Live commands are queued for the writer thread and return nothing.
    pub fn send(&self, cmd: Command) -> Vec<Event> {
        match self {
            Backend::Demo(d) => d.borrow_mut().apply(cmd),
            Backend::Live(l) => {
                enqueue(&l.tx, Out::Command(cmd));
                Vec::new()
            }
        }
    }

    /// Asks the player to check for, or install, updates. The demo has no player to ask.
    pub fn request_update(&self, req: UpdateRequest) {
        if let Backend::Live(l) = self {
            enqueue(&l.tx, Out::Update(req));
        }
    }

    /// Asks the player to quit, and stops reconnecting, which would start a new one. False
    /// for the demo, which has no player.
    pub fn stop_player(&self) -> bool {
        match self {
            Backend::Demo(_) => false,
            Backend::Live(l) => {
                STOPPING.store(true, Ordering::Relaxed);
                enqueue(&l.tx, Out::Command(Command::Shutdown));
                true
            }
        }
    }
}

/// Set by `stop_player`: a lost connection then closes the window instead of reconnecting.
static STOPPING: AtomicBool = AtomicBool::new(false);

type Slot = Arc<Mutex<Option<Arc<Client>>>>;

/// What the writer thread sends.
enum Out {
    Command(Command),
    Update(UpdateRequest),
}

pub struct Live {
    tx: Sender<Out>,
}

impl Live {
    /// Starts the connection and writer threads. Incoming messages go to `hooks.post`, one
    /// call per drained batch.
    pub fn start(hooks: Hooks) -> Live {
        let slot: Slot = Arc::default();
        let (tx, rx) = bounded::<Out>(COMMAND_QUEUE);
        let writer = Arc::clone(&slot);
        thread::Builder::new()
            .name("requiem-gui-writer".into())
            .spawn(move || {
                for out in rx {
                    let client = writer
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    // Commands issued while disconnected are dropped; the snapshot sent on
                    // reconnect shows the player's real state.
                    let Some(client) = client else { continue };
                    let sent = match out {
                        Out::Command(cmd) => client.send(cmd),
                        Out::Update(UpdateRequest::Check) => client.check_for_updates(),
                        Out::Update(UpdateRequest::Install) => client.install_update(),
                        Out::Update(UpdateRequest::SetAuto(on)) => client.set_auto_update(on),
                    };
                    if let Err(e) = sent {
                        eprintln!("requiem-gui: send failed: {e}");
                    }
                }
            })
            .expect("spawn writer thread");
        thread::Builder::new()
            .name("requiem-gui-conn".into())
            .spawn(move || connection(daemon_path(), slot, hooks))
            .expect("spawn connection thread");
        Live { tx }
    }
}

/// Never blocks the main thread: a full queue drops the command.
fn enqueue<T>(tx: &Sender<T>, item: T) {
    match tx.try_send(item) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => debug(|| "command queue full; dropped a command".into()),
        // The writer outlives the UI; it is gone only at exit.
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// A stderr line when `REQUIEM_LOG=debug`, as in the other crates.
pub fn debug(msg: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var("REQUIEM_LOG").is_ok_and(|v| v == "debug")) {
        eprintln!("requiem-gui: {}", msg());
    }
}

fn daemon_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| beside(&exe))
        .filter(|p| cfg!(target_os = "macos") || p.exists())
        .or_else(|| on_path("requiemd", &std::env::var_os("PATH")?))
        .unwrap_or_else(|| PathBuf::from("requiemd"))
}

/// The first executable `name` in `path`'s directories, as a shell would find it. Relative
/// directories are skipped: the daemon is spawned by absolute path.
fn on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// In an app bundle, the daemon is the helper app `Contents/Helpers/requiemd.app`; elsewhere
/// (dev builds, and Linux, where it is installed beside the window) it sits in the
/// executable's directory. On Linux a missing sibling leaves `requiemd` to `PATH`.
fn beside(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let contents = dir.parent()?;
    Some(if in_bundle(exe) {
        contents.join("Helpers/requiemd.app/Contents/MacOS/requiemd")
    } else {
        dir.join("requiemd")
    })
}

/// `exe` is an app's main executable, `<name>.app/Contents/MacOS/<exe>`.
fn in_bundle(exe: &Path) -> bool {
    let dir = exe.parent();
    let contents = dir.and_then(Path::parent);
    let app = contents.and_then(Path::parent);
    dir.and_then(Path::file_name).is_some_and(|n| n == "MacOS")
        && contents
            .and_then(Path::file_name)
            .is_some_and(|n| n == "Contents")
        && app.and_then(Path::extension).is_some_and(|e| e == "app")
}

/// Whether this window runs from an app, and so shows self-update. Dev builds, and Linux,
/// where Requiem does not update itself, show nothing.
pub fn shows_updates() -> bool {
    static IN_APP: OnceLock<bool> = OnceLock::new();
    *IN_APP.get_or_init(|| std::env::current_exe().is_ok_and(|exe| in_bundle(&exe)))
}

fn connection(daemon: PathBuf, slot: Slot, hooks: Hooks) {
    let mut attempt = 0;
    let mut out_of_date = false;
    loop {
        let link = match Client::connect_or_spawn(&daemon, Role::Gui, CONNECT_TIMEOUT) {
            Ok((client, events)) => {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(client));
                attempt = 0;
                out_of_date = false;
                (hooks.post)(vec![Msg::Link(Link::Connected)]);
                let mut reason = "connection closed".to_owned();
                while let Ok(first) = events.recv() {
                    let mut batch = Vec::new();
                    let mut lost = false;
                    let mut goodbye = false;
                    for inc in std::iter::once(first).chain(events.try_iter()) {
                        match inc {
                            Incoming::Event(ev) => {
                                // The toast stays plain; the cause goes here.
                                if let Event::ProviderBroken { provider, detail } = &ev {
                                    eprintln!("requiem-gui: {} broke: {detail}", provider.0);
                                }
                                batch.push(Msg::Event(ev));
                            }
                            Incoming::Raise => batch.push(Msg::Raise),
                            Incoming::Update(status) => {
                                if shows_updates() {
                                    batch.push(Msg::Update(status));
                                }
                            }
                            Incoming::Disconnected(r) => {
                                reason = r;
                                lost = true;
                            }
                            Incoming::Goodbye => goodbye = true,
                        }
                    }
                    if !batch.is_empty() {
                        (hooks.post)(batch);
                    }
                    if goodbye {
                        (hooks.quit)();
                        return;
                    }
                    if lost {
                        break;
                    }
                }
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
                eprintln!("requiem-gui: lost the player: {reason}");
                Link::Lost(LinkError::NotResponding)
            }
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                if !std::mem::replace(&mut out_of_date, true) {
                    eprintln!("requiem-gui: {e}");
                }
                Link::OutOfDate
            }
            Err(e) => {
                out_of_date = false;
                eprintln!("requiem-gui: cannot reach the player: {e}");
                Link::Lost(LinkError::from_io(e.kind()))
            }
        };
        if STOPPING.load(Ordering::Relaxed) {
            (hooks.quit)();
            return;
        }
        let delay = retry_delay(&link, attempt);
        (hooks.post)(vec![Msg::Link(link)]);
        thread::sleep(delay);
        attempt += 1;
    }
}

/// How long to wait before the next connect after `link`. An out-of-date player is polled at
/// the slowest pace until it quits and a current one can start.
fn retry_delay(link: &Link, attempt: usize) -> Duration {
    let slowest = BACKOFF.len() - 1;
    match link {
        Link::OutOfDate => BACKOFF[slowest],
        _ => BACKOFF[attempt.min(slowest)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_queue_drops_instead_of_blocking() {
        let (tx, rx) = bounded(1);
        enqueue(&tx, Command::Next);
        enqueue(&tx, Command::Prev);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Command::Next]);
        drop(rx);
        enqueue(&tx, Command::Next);
    }

    #[test]
    fn out_of_date_keeps_retrying_at_the_slowest_pace() {
        assert_eq!(retry_delay(&Link::OutOfDate, 0), Duration::from_secs(4));
        let lost = Link::Lost(LinkError::NotResponding);
        assert_eq!(retry_delay(&lost, 0), Duration::from_millis(500));
        assert_eq!(retry_delay(&lost, 9), Duration::from_secs(4));
    }

    #[test]
    fn daemon_is_the_helper_app_inside_a_bundle() {
        assert_eq!(
            beside(Path::new(
                "/Applications/Requiem.app/Contents/MacOS/requiem-gui"
            )),
            Some(PathBuf::from(
                "/Applications/Requiem.app/Contents/Helpers/requiemd.app/Contents/MacOS/requiemd"
            ))
        );
    }

    #[test]
    fn only_an_app_shows_updates() {
        assert!(in_bundle(Path::new(
            "/Applications/Requiem.app/Contents/MacOS/requiem-gui"
        )));
        assert!(!in_bundle(Path::new("/work/target/release/requiem-gui")));
        assert!(!in_bundle(Path::new("/requiem-gui")));
    }

    #[test]
    fn a_daemon_not_beside_the_window_is_found_on_path() {
        let root = std::env::temp_dir().join(format!("rqpath{}", std::process::id()));
        let (empty, plain, bin) = (root.join("empty"), root.join("plain"), root.join("bin"));
        for d in [&empty, &plain, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        // Not executable: skipped, as a shell would.
        std::fs::write(plain.join("requiemd"), b"").unwrap();
        let exe = bin.join("requiemd");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(["rel".as_ref(), empty.as_path(), &plain, &bin]).unwrap();
        let found = on_path("requiemd", &path);
        let none = on_path("requiemd", &std::env::join_paths([&empty, &plain]).unwrap());
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, Some(exe));
        assert_eq!(none, None);
    }

    #[test]
    fn daemon_is_a_sibling_in_dev_builds() {
        assert_eq!(
            beside(Path::new("/work/target/release/requiem-gui")),
            Some(PathBuf::from("/work/target/release/requiemd"))
        );
    }
}
