use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use requiem_core::Event;

use crate::model::{Action, Snapshot};
use crate::mpris::{self, Mpris};
use crate::tray::TrayHandle;

/// The tray and MPRIS service, whichever of them the session could register. Dropping it
/// unregisters both.
pub struct Desktop {
    shared: mpris::Shared,
    mpris: Option<Mpris>,
    tray: Option<TrayHandle>,
}

/// Registers on the session bus. `None`, after one line on stderr, when neither the tray nor
/// MPRIS could register (typically no session bus); the daemon then carries on headless.
/// `actions` gets what the user asks for; `Action::Quit` means stop the daemon.
pub fn start(actions: Sender<Action>) -> Option<Desktop> {
    let shared: mpris::Shared = Arc::new(Mutex::new(Snapshot::default()));
    let mpris = Mpris::start(shared.clone(), actions.clone());
    let tray = TrayHandle::start(actions);
    match (mpris, tray) {
        (Err(m), Err(t)) => {
            eprintln!("requiemd: no desktop integration (mpris: {m}; tray: {t})");
            None
        }
        (mpris, tray) => {
            if let Err(e) = &mpris {
                eprintln!("requiemd: no MPRIS: {e}");
            }
            if let Err(e) = &tray {
                eprintln!("requiemd: no tray icon: {e}");
            }
            Some(Desktop {
                shared,
                mpris: mpris.ok(),
                tray: tray.ok(),
            })
        }
    }
}

impl Desktop {
    pub fn apply(&self, ev: &Event) {
        let (snap, changed) = {
            let mut snap = mpris::lock(&self.shared);
            let changed = snap.apply(ev);
            if !changed.any() {
                return;
            }
            (snap.clone(), changed)
        };
        if let Some(m) = &self.mpris {
            m.publish(&snap, changed);
        }
        if let Some(t) = &self.tray {
            t.update(&snap);
        }
    }
}
