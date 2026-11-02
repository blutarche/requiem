//! Self-update from signed GitHub releases, as `docs/self-update-design.md` describes.
//! Supports writable macOS app bundles and Linux tarball installations.

pub mod b64;
mod failure;
pub mod fetch;
pub mod key;
pub mod linux;
pub mod linux_manifest;
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

pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));
pub const UNSUPPORTED: &str = "Self-update is unavailable on this platform";
