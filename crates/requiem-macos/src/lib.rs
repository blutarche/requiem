//! macOS menu-bar status item, Now Playing and media keys for the player daemon, and the
//! artwork loader it shares with the GUI.
//!
//! On other OSes the types still exist and `run` drains updates until `stop`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use requiem_core::PlayState;

/// Artwork bookkeeping without a toolkit, shared with the Linux window.
#[doc(hidden)]
pub mod art;
#[cfg(target_os = "macos")]
mod macos;
mod view;

/// Artwork loading and the cover view.
#[cfg(target_os = "macos")]
pub use macos::art as artwork;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub use macos::mini_player_snapshot;
pub use view::ViewTracker;

/// What the status item, the mini player and Now Playing show. `artists` is joined for
/// display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerView {
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub position: Duration,
    pub state: PlayState,
    pub artwork_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    Toggle,
    Next,
    Prev,
    Seek(Duration),
    SetVolume(u8),
    OpenGui,
    CheckForUpdates,
    RestartToUpdate,
    Quit,
}

/// Something for `run` to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiUpdate {
    /// What is playing, as [`ViewTracker`] reports it.
    View(Option<PlayerView>),
    /// The engine's position; send it only while [`wants_positions`].
    Position(Duration),
    Volume(u8),
    /// Whether the menu offers **Check for Updates…**, and the version **Restart to Update**
    /// installs, if one is ready.
    Updates {
        check: bool,
        ready: Option<String>,
    },
}

/// Set while the mini player is open.
static WANTS_POSITIONS: AtomicBool = AtomicBool::new(false);

/// Whether `run` shows every position, which it does only while the mini player is open.
/// Otherwise the view's position, which [`ViewTracker`] corrects on seeks, is enough.
pub fn wants_positions() -> bool {
    WANTS_POSITIONS.load(Ordering::Relaxed)
}

/// Runs the AppKit loop on the calling (main) thread until `stop` fires.
/// `updates` are applied in order, except that of the views pending at a [`wake`] only the
/// latest is applied, and positions before it are dropped.
///
/// On macOS the loop reads `updates` and `stop` only when woken: call [`wake`] after every
/// send on either. A disconnected `stop` counts as fired. On macOS this panics off the main
/// thread. On other OSes it drains `updates` so publishers stay connected until `stop`.
pub fn run(updates: Receiver<UiUpdate>, actions: Sender<UiAction>, stop: Receiver<()>) {
    #[cfg(target_os = "macos")]
    macos::run(updates, actions, stop);
    #[cfg(not(target_os = "macos"))]
    {
        let _actions = actions;
        loop {
            crossbeam_channel::select! {
                recv(stop) -> _ => return,
                recv(updates) -> update => {
                    if update.is_err() {
                        let _ = stop.recv();
                        return;
                    }
                }
            }
        }
    }
}

/// Has `run` read `updates` and `stop` soon. Callable from any thread, before or during
/// `run`.
pub fn wake() {
    #[cfg(target_os = "macos")]
    macos::wake();
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use std::thread;

    use crossbeam_channel::unbounded;

    use super::*;

    #[test]
    fn run_returns_when_stop_fires() {
        let (_updates_tx, updates) = unbounded();
        let (actions, _actions_rx) = unbounded();
        let (stop_tx, stop) = unbounded();
        let handle = thread::spawn(move || run(updates, actions, stop));
        stop_tx.send(()).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn run_drains_updates_until_stopped() {
        let (updates_tx, updates) = crossbeam_channel::bounded(0);
        let (actions, _actions_rx) = unbounded();
        let (stop_tx, stop) = unbounded();
        let handle = thread::spawn(move || run(updates, actions, stop));

        for volume in [40, 80] {
            updates_tx
                .send_timeout(UiUpdate::Volume(volume), Duration::from_secs(5))
                .expect("the platform loop must keep draining updates");
        }
        assert!(!handle.is_finished(), "only stop ends the platform loop");
        stop_tx.send(()).unwrap();
        handle.join().unwrap();
    }
}
