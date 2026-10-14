//! Self-update is macOS only: the player says so and makes no request.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use requiem_ipc::{UpdateRequest, UpdateState, UpdateStatus};

/// Only a macOS app has an installed player to start instead.
pub fn exec_newer_player(_exe: &Path, _reexeced: bool) -> Result<(), String> {
    Ok(())
}

pub struct Updater {
    statuses: Sender<UpdateStatus>,
}

impl Updater {
    pub fn start(
        _exe: Option<&Path>,
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
