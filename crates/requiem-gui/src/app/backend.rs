//! Where commands go and events come from: the fixed demo state, or `requiemd` over IPC.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Sender, unbounded};
use dispatch2::DispatchQueue;
use requiem_core::{Command, Event};
use requiem_ipc::{Client, Incoming, Role};

use super::ui;
use crate::demo;
use crate::vm::Link;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
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
                // The writer outlives the UI; a send error can only happen at exit.
                let _ = l.tx.send(cmd);
                Vec::new()
            }
        }
    }
}

type Slot = Arc<Mutex<Option<Arc<Client>>>>;

pub struct Live {
    tx: Sender<Command>,
}

impl Live {
    /// Starts the connection and writer threads. Incoming messages reach `Ui::receive` on
    /// the main thread, one dispatch per drained batch.
    pub fn start() -> Live {
        let slot: Slot = Arc::default();
        let (tx, rx) = unbounded::<Command>();
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

fn daemon_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("requiemd")))
        .unwrap_or_else(|| PathBuf::from("requiemd"))
}

fn connection(daemon: PathBuf, slot: Slot) {
    let mut attempt = 0;
    loop {
        match Client::connect_or_spawn(&daemon, Role::Gui, CONNECT_TIMEOUT) {
            Ok((client, events)) => {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(client));
                attempt = 0;
                post(vec![Msg::Link(Link::Connected)]);
                let mut reason = "connection closed".to_owned();
                while let Ok(first) = events.recv() {
                    let mut batch = Vec::new();
                    let mut lost = false;
                    for inc in std::iter::once(first).chain(events.try_iter()) {
                        match inc {
                            Incoming::Event(ev) => batch.push(Msg::Event(ev)),
                            Incoming::Raise => batch.push(Msg::Raise),
                            Incoming::Disconnected(r) => {
                                reason = r;
                                lost = true;
                            }
                        }
                    }
                    if !batch.is_empty() {
                        post(batch);
                    }
                    if lost {
                        break;
                    }
                }
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
                post(vec![Msg::Link(Link::Lost(reason))]);
            }
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                eprintln!("requiem-gui: {e}");
                post(vec![Msg::Link(Link::OutOfDate)]);
                return;
            }
            Err(e) => {
                eprintln!("requiem-gui: cannot reach the player: {e}");
                post(vec![Msg::Link(Link::Lost(e.to_string()))]);
            }
        }
        thread::sleep(BACKOFF[attempt.min(BACKOFF.len() - 1)]);
        attempt += 1;
    }
}

fn post(batch: Vec<Msg>) {
    DispatchQueue::main().exec_async(move || {
        ui::with(|ui| ui.receive(batch));
    });
}
