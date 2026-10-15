//! Spotify through librespot. librespot runs in a helper process, `requiemd --spotify-helper
//! <socket>`: it needs tokio, and under `panic = "abort"` one of its panics would otherwise
//! take the daemon down. The daemon's [`SpotifyProvider`] talks to the helper over a Unix
//! socket with plain blocking I/O; see `wire` for the protocol.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

mod backend;
mod client;
mod creds;
mod keys;
mod logger;
mod server;
mod source;
mod sys;
mod wire;

#[cfg(test)]
mod live;
#[cfg(test)]
mod testing;

pub use client::{SpotifyOptions, SpotifyProvider};
pub use server::{Backend, Stream, serve};
pub use wire::Pending;

/// requiemd's argument for helper mode, followed by the socket path.
pub const HELPER_ARG: &str = "--spotify-helper";
pub const PROVIDER_ID: &str = "spotify";
const HELPER_WORKERS: usize = 2;

/// The Spotify sign-in file, when there is a usable location.
pub fn credentials_file() -> Option<PathBuf> {
    creds::from_env()
}

/// Parses the Spotify sign-in file at `path` without changing it; a missing file passes.
pub fn check_credentials_file(path: &Path) -> Result<(), String> {
    creds::check(path)
}

/// `requiemd --spotify-helper <socket>`: serves librespot on `socket` until stdin closes.
pub fn run_helper(socket: &Path) -> ExitCode {
    logger::init();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(HELPER_WORKERS)
        .thread_name("spotify-rt")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[spotify] helper: {e}");
            return ExitCode::from(1);
        }
    };
    let tmp_dir = socket
        .parent()
        .map_or_else(std::env::temp_dir, Path::to_path_buf);
    let backend = backend::Librespot::new(rt, credentials_file(), tmp_dir);
    serve(socket, Arc::new(backend))
}
