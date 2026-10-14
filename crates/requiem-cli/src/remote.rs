use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use requiem_core::{Command, Event};
use requiem_ipc::{Client, Incoming, Role, UpdateRequest, UpdateStatus};

use crate::app::Player;

/// Where a CLI at `cli_exe` finds `requiemd`: next to itself, else the helper app inside the
/// bundle it ships in. Falls back to the sibling when neither exists, so a spawn failure names it.
pub fn daemon_exe(cli_exe: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let sibling = cli_exe.with_file_name("requiemd");
    if exists(&sibling) {
        return sibling;
    }
    if let Some(dir) = cli_exe.parent() {
        let bundled = dir.join("../Helpers/requiemd.app/Contents/MacOS/requiemd");
        if exists(&bundled) {
            return bundled;
        }
    }
    sibling
}

/// A daemon reached over IPC.
pub struct RemotePlayer {
    client: Client,
    events: Mutex<Option<Receiver<Event>>>,
    gone: Arc<Mutex<Option<String>>>,
    updates: Arc<(Mutex<Updates>, Condvar)>,
}

/// Every self-update status the daemon sent, in order: a quick check's `Checking` and the
/// state after it may arrive together, and `update check` needs both.
#[derive(Default)]
struct Updates {
    all: Vec<UpdateStatus>,
    /// The connection has ended.
    ended: bool,
}

impl RemotePlayer {
    /// Connects to the running daemon, starting `daemon_exe` if none is.
    pub fn connect_or_spawn(daemon_exe: &Path, timeout: Duration) -> io::Result<Self> {
        let (client, incoming) = Client::connect_or_spawn(daemon_exe, Role::Cli, timeout)?;
        Self::new(client, incoming)
    }

    pub fn new(client: Client, incoming: Receiver<Incoming>) -> io::Result<Self> {
        let (tx, rx) = unbounded();
        let gone = Arc::new(Mutex::new(None));
        let slot = gone.clone();
        let updates = Arc::new((Mutex::new(Updates::default()), Condvar::new()));
        let statuses = updates.clone();
        let note = move |status: Option<UpdateStatus>| {
            let (lock, changed) = &*statuses;
            let mut u = lock.lock().unwrap_or_else(PoisonError::into_inner);
            match status {
                Some(s) => u.all.push(s),
                None => u.ended = true,
            }
            changed.notify_all();
        };
        thread::Builder::new()
            .name("requiem-remote".into())
            .spawn(move || {
                for msg in incoming {
                    match msg {
                        Incoming::Event(ev) => {
                            if tx.send(ev).is_err() {
                                break;
                            }
                        }
                        Incoming::Raise => {}
                        Incoming::Update(status) => note(Some(status)),
                        // Stored before `tx` drops, which closes the event channel.
                        Incoming::Goodbye => {
                            *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                                Some("player quit".into());
                            break;
                        }
                        Incoming::Disconnected(reason) => {
                            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
                            break;
                        }
                    }
                }
                note(None);
            })?;
        Ok(Self {
            client,
            events: Mutex::new(Some(rx)),
            gone,
            updates,
        })
    }
}

impl Player for RemotePlayer {
    /// Send errors are dropped; a dead connection is reported through `disconnected`.
    fn send(&self, cmd: Command) {
        let _ = self.client.send(cmd);
    }

    /// Single-use: later calls get an already-closed channel.
    fn subscribe(&self) -> Receiver<Event> {
        let taken = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        taken.unwrap_or_else(|| unbounded().1)
    }

    fn remote(&self) -> bool {
        true
    }

    fn disconnected(&self) -> Option<String> {
        self.gone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update_status(&self) -> Option<(UpdateStatus, usize)> {
        let u = self
            .updates
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        u.all.last().map(|s| (s.clone(), u.all.len()))
    }

    fn next_update(&self, seen: usize, timeout: Duration) -> Option<UpdateStatus> {
        let (lock, changed) = &*self.updates;
        let deadline = Instant::now() + timeout;
        let mut u = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while u.all.len() <= seen && !u.ended {
            let left = deadline.checked_duration_since(Instant::now())?;
            u = changed
                .wait_timeout(u, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        u.all.get(seen).cloned()
    }

    /// Send errors are dropped, as for `send`.
    fn request_update(&self, req: UpdateRequest) {
        let _ = match req {
            UpdateRequest::Check => self.client.check_for_updates(),
            UpdateRequest::Install => self.client.install_update(),
            UpdateRequest::SetAuto(on) => self.client.set_auto_update(on),
        };
    }
}
