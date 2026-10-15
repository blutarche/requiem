use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use requiem_audio::{AudioEngine, AudioOptions};
use requiem_core::{
    Command, EngineEventSink, EnqueueMode, Event, PersistSink, Session, SessionConfig,
};
use requiem_ipc::{Instance, ServeError, Server, ServerHooks};
use requiem_provider_spotify::{SpotifyOptions, SpotifyProvider};
use requiem_provider_ytmusic::{YtMusicOptions, YtMusicProvider};

use idle::{Activity, IdleTimer};
use update::Updater;

const USAGE: &str = "usage: requiemd [--prefer-aac] [--help]";
/// Run by an updating player on a staged copy; see `requiem_update::stage::self_test`.
const SELF_TEST_ARG: &str = "--update-self-test";
/// Set on a player started in place of an older image (`update::exec_newer_player`), which
/// never does so again: a bundle whose Info.plist and binary disagree can't make it loop.
const REEXECED_ENV: &str = "REQUIEM_REEXECED";
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
#[cfg(target_os = "macos")]
mod update;
#[cfg(not(target_os = "macos"))]
#[path = "update_unsupported.rs"]
mod update;

fn idle_limit() -> Duration {
    let secs = std::env::var("REQUIEM_IDLE_EXIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_IDLE_EXIT_SECS);
    Duration::from_secs(secs)
}

fn main() -> ExitCode {
    let reexeced = std::env::var_os(REEXECED_ENV).is_some();
    // SAFETY: no other thread has started yet. Removed so players this one starts still check.
    unsafe { std::env::remove_var(REEXECED_ENV) };
    if std::env::args().nth(1).as_deref() == Some(SELF_TEST_ARG) {
        return self_test();
    }
    // Spawned by `SpotifyProvider`; it never takes the instance lock or opens audio.
    if std::env::args_os().nth(1).as_deref() == Some(requiem_provider_spotify::HELPER_ARG.as_ref())
    {
        return match std::env::args_os().nth(2) {
            Some(socket) => requiem_provider_spotify::run_helper(socket.as_ref()),
            None => {
                eprintln!(
                    "requiemd: {} needs a socket path",
                    requiem_provider_spotify::HELPER_ARG
                );
                ExitCode::from(2)
            }
        };
    }
    // Lets the live update test make a new version fail to start after the swap.
    #[cfg(debug_assertions)]
    if std::env::var("REQUIEM_TEST_FAIL_START_VERSION")
        .is_ok_and(|v| v == env!("CARGO_PKG_VERSION"))
    {
        eprintln!("requiemd: REQUIEM_TEST_FAIL_START_VERSION is this version; not starting");
        return ExitCode::from(1);
    }
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
    let exe = ui::own_exe();
    let instance = match Instance::lock() {
        Ok(i) => i,
        Err(ServeError::AlreadyRunning) => {
            eprintln!("requiemd: already running");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("requiemd: {e}");
            return ExitCode::from(1);
        }
    };
    // Restart to Update swaps under this lock, so any such swap has finished; nothing listens
    // yet, so no client meets an image the installed app has replaced.
    if let Some(exe) = &exe
        && let Err(e) = update::exec_newer_player(exe, reexeced)
    {
        eprintln!("requiemd: {e}");
        return ExitCode::from(1);
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
    let spotify = Arc::new(SpotifyProvider::new(SpotifyOptions {
        exe: exe.clone(),
        socket_dir: requiem_ipc::socket_dir().ok(),
        credentials: requiem_provider_spotify::credentials_file(),
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
        vec![spotify, provider],
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
    // `Instance::serve`.
    let (stop_tx, stop_rx) = bounded::<()>(1);
    let hook_tx = stop_tx.clone();
    let update_tx = stop_tx.clone();
    let (updater, statuses) = Updater::start(exe.as_deref(), move || request_stop(&update_tx));
    let hook_updater = updater.clone();
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || request_stop(&hook_tx)),
        on_update: Box::new(move |req| hook_updater.request(req)),
    };
    let server = match instance.serve(session, hooks, updater.status()) {
        Ok(s) => s,
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
        let on_update = |req| updater.request(req);
        ui::run_platform(
            &stop_tx,
            stop_rx,
            &server,
            statuses,
            &on_update,
            exe.as_deref(),
        );
        drop(done_tx);
    });
    debug!("shutting down");
    shut_down(server, writer, &updater);
    ExitCode::SUCCESS
}

/// `requiemd --update-self-test`: reads, without changing, the files a new version must
/// read. Binds no socket, opens no audio device and writes nothing.
fn self_test() -> ExitCode {
    let mut ok = true;
    if let Some(path) = persist::state_path()
        && let Err(e) = persist::read(&path)
    {
        eprintln!("requiemd: {}: {e}", path.display());
        ok = false;
    }
    if let Some(path) = requiem_provider_ytmusic::token_file()
        && let Err(e) = requiem_provider_ytmusic::check_token_file(&path)
    {
        eprintln!("requiemd: {e}");
        ok = false;
    }
    if let Some(path) = requiem_provider_spotify::credentials_file()
        && let Err(e) = requiem_provider_spotify::check_credentials_file(&path)
    {
        eprintln!("requiemd: {e}");
        ok = false;
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Stops serving, waits for the state writer, then installs a ready update.
fn shut_down(server: Server, writer: Option<JoinHandle<()>>, updater: &Updater) {
    // Shutting the Session down drops its sink and so closes the writer's channel.
    let join = |writer: Option<JoinHandle<()>>| {
        if let Some(writer) = writer {
            let _ = writer.join();
        }
    };
    #[cfg(target_os = "macos")]
    match updater.finish() {
        Some(update::Install::Restart(ready)) => {
            let gui = server.clients().gui > 0;
            let instance = server.shutdown_keeping_lock();
            join(writer);
            updater.restart(ready, instance, gui);
        }
        finished => {
            server.shutdown();
            join(writer);
            if let Some(update::Install::AtExit(ready)) = finished {
                updater.install_at_exit(ready);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = updater;
        server.shutdown();
        join(writer);
    }
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
