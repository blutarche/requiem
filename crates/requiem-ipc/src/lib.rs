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

pub use client::{Client, Incoming, spawn_daemon};
pub use path::{log_path, socket_dir, socket_path};
pub use proto::{ClientMsg, PROTOCOL, Role, ServerMsg, UpdateState, UpdateStatus};
pub use server::{ClientCounts, Instance, ServeError, Server, ServerHooks, UpdateRequest};
