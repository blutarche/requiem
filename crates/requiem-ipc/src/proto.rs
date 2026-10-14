use requiem_core::{Command, Event};
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 4;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Gui,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        protocol: u32,
        role: Role,
        version: String,
    },
    Command(Command),
    Ping(u64),
    /// Checks now, whatever the schedule, and clears retry counts.
    CheckForUpdates,
    /// Restart to Update.
    InstallUpdate,
    SetAutoUpdate(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        protocol: u32,
        version: String,
    },
    Incompatible {
        supported: u32,
    },
    Event(Event),
    Raise,
    Pong(u64),
    /// The daemon is stopping on purpose; it closes the connection after this.
    Goodbye,
    /// Sent right after `Welcome` and on every change.
    Update(UpdateStatus),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// The running player's version.
    pub current: String,
    /// Check for Updates Automatically.
    pub auto: bool,
    pub state: UpdateState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Downloading {
        version: String,
    },
    /// Staged, waiting for Restart to Update or the player's exit.
    Ready {
        version: String,
    },
    /// After a bad release or a third failed try; `reason` is for the banner.
    Failed {
        version: String,
        reason: String,
    },
    NotUpdatable {
        reason: String,
    },
    Unsupported,
}

/// Reads only the protocol number, so a `Hello` from another protocol version still gets
/// `Incompatible` even if the rest of its shape has changed.
#[derive(Deserialize)]
pub(crate) enum HelloProbe {
    Hello { protocol: u32 },
}
