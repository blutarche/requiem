use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use requiem_audio::{AudioEngine, AudioOptions};
use requiem_core::{
    Command, EngineEventSink, EnqueueMode, Event, PersistSink, Session, SessionConfig,
};
use requiem_ipc::{ServeError, Server, ServerHooks, UpdateState, UpdateStatus};
use requiem_provider_ytmusic::{YtMusicOptions, YtMusicProvider};

use idle::{Activity, IdleTimer};

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
mod persist;
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
    let state_path = persist::state_path();
    let restored = state_path.as_deref().and_then(persist::load);
    // Nothing is sent to the writer before the server is up, so an early return writes nothing.
    let (persist_sink, writer) = match state_path {
        Some(path) => {
            let (tx, handle) = persist::spawn_writer(path, restored.clone());
            let sink: PersistSink = Arc::new(move |state| {
                let _ = tx.send(state);
            });
            (Some(sink), Some(handle))
        }
        None => (None, None),
    };
    let session = Session::spawn(
        vec![provider],
        move |sink| {
            let _ = slot.set(sink);
            Box::new(engine)
        },
        SessionConfig {
            persist: persist_sink,
            ..SessionConfig::default()
        },
    );

    // The hub's state mirror assumes a fresh Session: nothing may be sent to it before
    // `Server::start`.
    let (stop_tx, stop_rx) = bounded::<()>(1);
    let hook_tx = stop_tx.clone();
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || request_stop(&hook_tx)),
        on_update: Box::new(|_| {}),
    };
    let update = UpdateStatus {
        current: env!("CARGO_PKG_VERSION").into(),
        auto: false,
        state: UpdateState::Unsupported,
    };
    let server = match Server::start(session, hooks, update) {
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

    if let Some(state) = restored {
        if !state.tracks.is_empty() {
            server.send(Command::Enqueue {
                tracks: state.tracks,
                mode: EnqueueMode::Replace,
            });
        }
        server.send(Command::SetVolume(state.volume));
        if let Some(i) = state.resume {
            server.send(Command::Cue(i));
        }
    }

    let activity = Arc::new(Mutex::new(Activity::new()));
    watch_state(server.subscribe(), activity.clone());
    let (done_tx, done_rx) = bounded::<()>(0);
    thread::scope(|scope| {
        scope.spawn(|| idle_exit(&server, &activity, idle_limit(), &stop_tx, &done_rx));
        ui::run_platform(&stop_tx, stop_rx, &server);
        drop(done_tx);
    });
    debug!("shutting down");
    server.shutdown();
    // Shutting the Session down drops its sink and so closes the writer's channel.
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    ExitCode::SUCCESS
}

/// Asks the platform UI to return. The macOS loop sees `stop` only when woken.
fn request_stop(stop: &Sender<()>) {
    let _ = stop.try_send(());
    requiem_macos::wake();
}

fn watch_state(events: Receiver<Event>, activity: Arc<Mutex<Activity>>) {
    let _ = thread::Builder::new()
        .name("requiemd-state".into())
        .spawn(move || {
            for ev in events {
                activity
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .observe(&ev);
            }
        });
}

/// Requests a stop once there have been no clients, no playback and no pending sign-in
/// for `limit`. Returns when `done` closes.
fn idle_exit(
    server: &Server,
    activity: &Mutex<Activity>,
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
        let quiet = activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle();
        let idle = counts.gui + counts.cli == 0 && quiet;
        if timer.expired(Instant::now(), idle) {
            debug!("idle for {limit:?}, exiting");
            request_stop(stop);
            return;
        }
    }
}
