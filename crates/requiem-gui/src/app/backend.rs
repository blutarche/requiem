//! The shared backend, reached from AppKit's main dispatch queue.

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

use crate::backend::Hooks;
pub use crate::backend::{Backend, Live, Msg, shows_updates};

use super::ui;

/// Incoming messages reach `Ui::receive` on the main thread, one dispatch per drained batch.
pub const HOOKS: Hooks = Hooks { post, quit };

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
