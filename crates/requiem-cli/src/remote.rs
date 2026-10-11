use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use requiem_core::{Command, Event};
use requiem_ipc::{Client, Incoming, Role};

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
        thread::Builder::new()
            .name("requiem-remote".into())
            .spawn(move || {
                for msg in incoming {
                    match msg {
                        Incoming::Event(ev) => {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Incoming::Raise => {}
                        // Stored before `tx` drops, which closes the event channel.
                        Incoming::Goodbye => {
                            *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                                Some("player quit".into());
                            return;
                        }
                        Incoming::Disconnected(reason) => {
                            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            client,
            events: Mutex::new(Some(rx)),
            gone,
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
}
