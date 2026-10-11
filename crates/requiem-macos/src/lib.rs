//! macOS menu-bar status item, Now Playing and media keys for the player daemon.
//!
//! On other OSes the types still exist and `run` only waits for `stop`.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use requiem_core::PlayState;

#[cfg(target_os = "macos")]
mod macos;
mod view;

pub use view::ViewTracker;

/// What the status item and Now Playing show. `artists` is joined for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerView {
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub position: Duration,
    pub state: PlayState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    Toggle,
    Next,
    Prev,
    Seek(Duration),
    OpenGui,
    Quit,
}

/// Runs the AppKit loop on the calling (main) thread until `stop` fires.
/// `views` delivers updates; of those pending at a [`wake`], only the latest is applied.
///
/// On macOS the loop reads `views` and `stop` only when woken: call [`wake`] after every send
/// on either. A disconnected `stop` counts as fired. On macOS this panics off the main thread.
pub fn run(views: Receiver<Option<PlayerView>>, actions: Sender<UiAction>, stop: Receiver<()>) {
    #[cfg(target_os = "macos")]
    macos::run(views, actions, stop);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (views, actions);
        let _ = stop.recv();
    }
}

/// Has `run` read `views` and `stop` soon. Callable from any thread, before or during `run`.
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
        let (_views_tx, views) = unbounded();
        let (actions, _actions_rx) = unbounded();
        let (stop_tx, stop) = unbounded();
        let handle = thread::spawn(move || run(views, actions, stop));
        stop_tx.send(()).unwrap();
        handle.join().unwrap();
    }
}
