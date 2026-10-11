//! Where commands go and events come from: the fixed demo state, or `requiemd` over IPC.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError, bounded};
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use requiem_core::{Command, Event};
use requiem_ipc::{Client, Incoming, Role};

use super::ui;
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
                enqueue(&l.tx, cmd);
                Vec::new()
            }
        }
    }

    /// Asks the player to quit, and stops reconnecting, which would start a new one. False
    /// for the demo, which has no player.
    pub fn stop_player(&self) -> bool {
        match self {
            Backend::Demo(_) => false,
            Backend::Live(l) => {
                STOPPING.store(true, Ordering::Relaxed);
                enqueue(&l.tx, Command::Shutdown);
                true
            }
        }
    }
}

/// Set by `stop_player`: a lost connection then closes the window instead of reconnecting.
static STOPPING: AtomicBool = AtomicBool::new(false);

type Slot = Arc<Mutex<Option<Arc<Client>>>>;

pub struct Live {
    tx: Sender<Command>,
}

impl Live {
    /// Starts the connection and writer threads. Incoming messages reach `Ui::receive` on
    /// the main thread, one dispatch per drained batch.
    pub fn start() -> Live {
        let slot: Slot = Arc::default();
        let (tx, rx) = bounded::<Command>(COMMAND_QUEUE);
        let writer = Arc::clone(&slot);
        thread::Builder::new()
            .name("requiem-gui-writer".into())
            .spawn(move || {
                for cmd in rx {
                    let client = writer
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    // Commands issued while disconnected are dropped; the snapshot sent on
                    // reconnect shows the player's real state.
                    if let Some(client) = client
                        && let Err(e) = client.send(cmd)
                    {
                        eprintln!("requiem-gui: send failed: {e}");
                    }
                }
            })
            .expect("spawn writer thread");
        thread::Builder::new()
            .name("requiem-gui-conn".into())
            .spawn(move || connection(daemon_path(), slot))
            .expect("spawn connection thread");
        Live { tx }
    }
}

/// Never blocks the main thread: a full queue drops the command.
fn enqueue(tx: &Sender<Command>, cmd: Command) {
    match tx.try_send(cmd) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => debug(|| "command queue full; dropped a command".into()),
        // The writer outlives the UI; it is gone only at exit.
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// A stderr line when `REQUIEM_LOG=debug`, as in the other crates.
fn debug(msg: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var("REQUIEM_LOG").is_ok_and(|v| v == "debug")) {
        eprintln!("requiem-gui: {}", msg());
    }
}

fn daemon_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| beside(&exe))
        .unwrap_or_else(|| PathBuf::from("requiemd"))
}

/// In an app bundle, the daemon is the helper app `Contents/Helpers/requiemd.app`; elsewhere
/// (dev builds) it sits in the executable's directory.
fn beside(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let contents = dir.parent()?;
    let app = contents.parent()?;
    let in_bundle = dir.file_name().is_some_and(|n| n == "MacOS")
        && contents.file_name().is_some_and(|n| n == "Contents")
        && app.extension().is_some_and(|e| e == "app");
    Some(if in_bundle {
        contents.join("Helpers/requiemd.app/Contents/MacOS/requiemd")
    } else {
        dir.join("requiemd")
    })
}

fn connection(daemon: PathBuf, slot: Slot) {
    let mut attempt = 0;
    let mut out_of_date = false;
    loop {
        let link = match Client::connect_or_spawn(&daemon, Role::Gui, CONNECT_TIMEOUT) {
            Ok((client, events)) => {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(client));
                attempt = 0;
                out_of_date = false;
                post(vec![Msg::Link(Link::Connected)]);
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
                            Incoming::Disconnected(r) => {
                                reason = r;
                                lost = true;
                            }
                            Incoming::Goodbye => goodbye = true,
                        }
                    }
                    if !batch.is_empty() {
                        post(batch);
                    }
                    if goodbye {
                        quit();
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
            quit();
            return;
        }
        let delay = retry_delay(&link, attempt);
        post(vec![Msg::Link(link)]);
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

/// The player quit on purpose (menu-bar Quit, or "Quit and Stop Music"): close the window
/// with it rather than respawn it.
fn quit() {
    DispatchQueue::main().exec_async(|| {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        NSApplication::sharedApplication(mtm).terminate(None);
    });
}

fn post(batch: Vec<Msg>) {
    DispatchQueue::main().exec_async(move || {
        ui::with(|ui| ui.receive(batch));
    });
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
                "/Applications/requiem.app/Contents/MacOS/requiem-gui"
            )),
            Some(PathBuf::from(
                "/Applications/requiem.app/Contents/Helpers/requiemd.app/Contents/MacOS/requiemd"
            ))
        );
    }

    #[test]
    fn daemon_is_a_sibling_in_dev_builds() {
        assert_eq!(
            beside(Path::new("/work/target/release/requiem-gui")),
            Some(PathBuf::from("/work/target/release/requiemd"))
        );
    }
}
