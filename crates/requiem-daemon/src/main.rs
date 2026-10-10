use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use requiem_audio::{AudioEngine, AudioOptions};
use requiem_core::{EngineEventSink, Event, PlayState, Session, SessionConfig};
use requiem_ipc::{ServeError, Server, ServerHooks};
use requiem_provider_ytmusic::{YtMusicOptions, YtMusicProvider};

use idle::IdleTimer;

const USAGE: &str = "usage: requiemd [--prefer-aac] [--help]";
const DEFAULT_IDLE_EXIT_SECS: u64 = 300;
const IDLE_POLL: Duration = Duration::from_secs(5);

fn debug_enabled() -> bool {
    std::env::var("REQUIEM_LOG").is_ok_and(|v| v == "debug")
}

macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::debug_enabled() {
            eprintln!("[requiemd] {}", format_args!($($arg)*));
        }
    };
}

mod idle;
mod ui;

fn idle_limit() -> Duration {
    let secs = std::env::var("REQUIEM_IDLE_EXIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_IDLE_EXIT_SECS);
    Duration::from_secs(secs)
}

fn main() -> ExitCode {
    let mut prefer_aac = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--prefer-aac" => prefer_aac = true,
            other => {
                eprintln!("requiemd: unknown argument {other}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    // The engine must exist before `Session::spawn`, whose factory cannot fail, so events
    // reach the session through a slot the factory fills.
    let slot: Arc<OnceLock<EngineEventSink>> = Arc::new(OnceLock::new());
    let forward: EngineEventSink = {
        let slot = slot.clone();
        Arc::new(move |ev| {
            if let Some(sink) = slot.get() {
                sink(ev);
            }
        })
    };
    let engine = match AudioEngine::new(forward, AudioOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("requiemd: no audio output: {e}");
            return ExitCode::from(1);
        }
    };
    let provider = Arc::new(YtMusicProvider::new(YtMusicOptions {
        prefer_aac,
        ..YtMusicOptions::default()
    }));
    let session = Session::spawn(
        vec![provider],
        move |sink| {
            let _ = slot.set(sink);
            Box::new(engine)
        },
        SessionConfig::default(),
    );

    // The hub's state mirror assumes a fresh Session: nothing may be sent to it before
    // `Server::start`.
    let (stop_tx, stop_rx) = bounded::<()>(1);
    let hook_tx = stop_tx.clone();
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || {
            let _ = hook_tx.try_send(());
        }),
    };
    let server = match Server::start(session, hooks) {
        Ok(s) => s,
        Err(ServeError::AlreadyRunning) => {
            eprintln!("requiemd: already running");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("requiemd: {e}");
            return ExitCode::from(1);
        }
    };
    debug!("listening");

    let stopped = Arc::new(AtomicBool::new(true));
    watch_state(server.subscribe(), stopped.clone());
    let (done_tx, done_rx) = bounded::<()>(0);
    thread::scope(|scope| {
        scope.spawn(|| idle_exit(&server, &stopped, idle_limit(), &stop_tx, &done_rx));
        ui::run_platform(&stop_tx, stop_rx, &server);
        drop(done_tx);
    });
    debug!("shutting down");
    server.shutdown();
    ExitCode::SUCCESS
}

/// Mirrors the playback state; the daemon starts `Stopped`.
fn watch_state(events: Receiver<Event>, stopped: Arc<AtomicBool>) {
    let _ = thread::Builder::new()
        .name("requiemd-state".into())
        .spawn(move || {
            for ev in events {
                if let Event::State(s) = ev {
                    stopped.store(s == PlayState::Stopped, Ordering::Release);
                }
            }
        });
}

/// Requests a stop once there have been no clients and no playback for `limit`.
/// Returns when `done` closes.
fn idle_exit(
    server: &Server,
    stopped: &AtomicBool,
    limit: Duration,
    stop: &Sender<()>,
    done: &Receiver<()>,
) {
    let mut timer = IdleTimer::new(limit);
    loop {
        match done.recv_timeout(IDLE_POLL) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        let counts = server.clients();
        let idle = counts.gui + counts.cli == 0 && stopped.load(Ordering::Acquire);
        if timer.expired(Instant::now(), idle) {
            debug!("idle for {limit:?}, exiting");
            let _ = stop.try_send(());
            return;
        }
    }
}
