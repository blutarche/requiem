//! Self-update from signed GitHub releases, as `docs/self-update-design.md` describes.
//! Installing is macOS only; elsewhere the player reports [`UNSUPPORTED`] and makes no
//! request.

pub mod b64;
mod failure;
pub mod fetch;
pub mod key;
mod lock;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod manifest;
pub mod policy;
pub mod stage;
pub mod state;
#[cfg(test)]
mod test_support;

pub use failure::{Failure, Kind, Reason};
pub use lock::DirLock;

pub const SUPPORTED: bool = cfg!(target_os = "macos");
pub const UNSUPPORTED: &str = "Self-update is macOS only";
