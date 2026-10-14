//! Self-update is macOS only: the player says so and makes no request.

use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use requiem_ipc::{UpdateRequest, UpdateState, UpdateStatus};

pub struct Updater {
    statuses: Sender<UpdateStatus>,
}

impl Updater {
    pub fn start(
        _stop: impl Fn() + Send + Sync + 'static,
    ) -> (Arc<Updater>, Receiver<UpdateStatus>) {
        let (statuses, rx) = unbounded();
        (Arc::new(Updater { statuses }), rx)
    }

    pub fn status(&self) -> UpdateStatus {
        UpdateStatus {
            current: env!("CARGO_PKG_VERSION").into(),
            auto: false,
            state: UpdateState::Unsupported,
        }
    }

    /// Every request is ignored; the status is republished so the asker gets an answer.
    pub fn request(&self, _req: UpdateRequest) {
        let _ = self.statuses.send(self.status());
    }
}
