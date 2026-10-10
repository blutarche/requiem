//! The requiem GUI: a thin AppKit client of the player daemon `requiemd`.
//!
//! `vm`, `demo` and `args` are plain Rust and build on every OS; the window is macOS-only.

pub mod args;
pub mod demo;
pub mod vm;

#[cfg(target_os = "macos")]
pub mod app;
