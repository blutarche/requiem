//! The requiem GUI: a thin client of the player daemon `requiemd`.
//!
//! `vm`, `view`, `search_keys`, `demo`, `args` and `backend` are plain Rust and build on
//! every OS; the window is AppKit on macOS (`app`) and GTK 4 on Linux (`gtk`).

pub mod args;
pub mod backend;
pub mod demo;
pub mod search_keys;
pub mod view;
pub mod vm;

#[cfg(target_os = "macos")]
pub mod app;
#[cfg(target_os = "linux")]
pub mod gtk;
