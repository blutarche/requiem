//! Daemon IPC: protocol types, framing, the socket location, and the `Client` and `Server` ends.

mod client;
mod frame;
mod hub;
mod path;
mod proto;
mod server;
mod sys;

#[cfg(test)]
mod tests;

pub use client::{Client, Incoming};
pub use path::{socket_dir, socket_path};
pub use proto::{ClientMsg, PROTOCOL, Role, ServerMsg};
pub use server::{ClientCounts, ServeError, Server, ServerHooks};
