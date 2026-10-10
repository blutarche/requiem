use requiem_core::{Command, Event};
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome { protocol: u32, version: String },
    Incompatible { supported: u32 },
    Event(Event),
    Raise,
    Pong(u64),
}

/// Reads only the protocol number, so a `Hello` from another protocol version still gets
/// `Incompatible` even if the rest of its shape has changed.
#[derive(Deserialize)]
pub(crate) enum HelloProbe {
    Hello { protocol: u32 },
}
