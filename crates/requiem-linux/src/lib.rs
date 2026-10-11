//! Linux desktop integration for the player daemon: a StatusNotifierItem tray icon and
//! MPRIS2 media control, both on the session bus and driven by the player's events.

pub mod model;

#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "linux")]
mod tray;

#[cfg(target_os = "linux")]
pub use desktop::{Desktop, start};
