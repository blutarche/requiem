//! `--connect` mode against an in-process `requiem_ipc::Server`. All tests share one socket
//! directory, so they run one at a time.

use std::io::{self, Cursor, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, unbounded};
use requiem_cli::app::{self, App, Flow, Format, Player};
use requiem_cli::remote::RemotePlayer;
use requiem_core::testing::{FakeEngineHandle, FakeProvider, Step};
use requiem_core::{EngineEvent, ProviderId, Session, SessionConfig, Track, TrackId};
use requiem_ipc::{
    Client, PROTOCOL, Role, Server, ServerHooks, UpdateRequest, UpdateState, UpdateStatus,
};

static SERIAL: Mutex<()> = Mutex::new(());
static DIR: Once = Once::new();

/// Holds the serial lock; points `REQUIEM_SOCKET_DIR` at a private directory.
fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    DIR.call_once(|| {
        let dir = std::env::temp_dir().join(format!("rq-connect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        // SAFETY: every test takes the serial lock first, and the lock holder is the only
        // thread in this process that reads the environment.
        unsafe { std::env::set_var("REQUIEM_SOCKET_DIR", &dir) };
    });
    guard
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Daemon {
    server: Server,
    provider: Arc<FakeProvider>,
    engine: FakeEngineHandle,
    shutdowns: Receiver<()>,
    updates: Receiver<UpdateRequest>,
}

fn daemon() -> Daemon {
    let provider = FakeProvider::new("fake");
    let engine = FakeEngineHandle::new();
    let session = Session::spawn(
        vec![provider.clone()],
        engine.factory(),
        SessionConfig::default(),
    );
    let (tx, shutdowns) = bounded(1);
    let (update_tx, updates) = unbounded();
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || {
            let _ = tx.try_send(());
        }),
        on_update: Box::new(move |req| {
            let _ = update_tx.send(req);
        }),
    };
    let server = Server::start(session, hooks, status(UpdateState::Idle)).expect("start server");
    Daemon {
        server,
        provider,
        engine,
        shutdowns,
        updates,
    }
}

fn status(state: UpdateState) -> UpdateStatus {
    UpdateStatus {
        current: "0.1.0".into(),
        auto: true,
        state,
    }
}

fn ready() -> UpdateState {
    UpdateState::Ready {
        version: "0.2.0".into(),
    }
}

/// Runs `script` against the daemon while `answer` plays the daemon's updater.
fn run_update(d: Daemon, script: &str, answer: impl FnOnce(Daemon) + Send) -> (u8, String) {
    let player = connect();
    let out = Buf::default();
    let code = thread::scope(|scope| {
        scope.spawn(move || answer(d));
        app::run(&player, Cursor::new(script.to_owned()), out.clone(), true).unwrap()
    });
    (code, out.text())
}

fn expect_request(d: &Daemon, want: UpdateRequest) {
    let got = d.updates.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(got, want);
}

fn connect() -> RemotePlayer {
    // The server is already listening, so the nonexistent executable is never spawned.
    RemotePlayer::connect_or_spawn(Path::new("/nonexistent/requiemd"), Duration::from_secs(3))
        .expect("connect")
}

fn track(id: &str) -> Track {
    Track {
        id: TrackId {
            provider: ProviderId("fake".into()),
            id: id.into(),
        },
        title: format!("Title {id}"),
        artists: vec!["A".into()],
        album: None,
        duration: Some(Duration::from_secs(125)),
        artwork_url: None,
    }
}

fn exec_all(app: &App<'_>, lines: &str) {
    for l in lines.lines() {
        assert_eq!(app.exec(l), Flow::Continue, "{l}");
    }
}

#[test]
fn search_play_await_playing_and_status() {
    let _g = serial();
    let d = daemon();
    d.provider
        .script_search("q", vec![Step::ok(vec![track("t1")])]);
    let player = connect();
    let out = Buf::default();
    let app = App::new(&player, out.clone(), true);

    exec_all(&app, "search q\nawait results 5\nplay 1");
    let (r#gen, id) = d.engine.wait_load(1);
    assert_eq!(id, "t1");
    d.engine.emit(EngineEvent::Started {
        r#gen,
        duration: Some(Duration::from_secs(125)),
    });
    exec_all(&app, "await playing 5\nstatus");

    let text = out.text();
    assert!(text.contains("  1. A — Title t1 (2:05) · fake\n"), "{text}");
    assert!(
        text.contains("status: playing | A — Title t1 | 0:00 / 2:05 | volume 100\n"),
        "{text}"
    );
    drop(app);
    drop(player);
    d.server.shutdown();
}

#[test]
fn shutdown_daemon_runs_the_hook_and_ends_the_session() {
    let _g = serial();
    let d = daemon();
    let player = connect();
    let out = Buf::default();
    let code = app::run(
        &player,
        Cursor::new("shutdown-daemon\nstatus\n"),
        out.clone(),
        true,
    )
    .unwrap();
    assert_eq!(code, 0);
    d.shutdowns
        .recv_timeout(Duration::from_secs(5))
        .expect("on_shutdown ran");
    assert!(!out.text().contains("status:"), "{}", out.text());
    // What the daemon does next.
    d.server.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.disconnected().is_none() {
        assert!(Instant::now() < deadline, "player never saw the daemon go");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(player.disconnected().as_deref(), Some("player quit"));
}

#[test]
fn quit_leaves_the_daemon_running() {
    let _g = serial();
    let d = daemon();
    let player = connect();
    let code = app::run(&player, Cursor::new("quit\n"), Buf::default(), true).unwrap();
    assert_eq!(code, 0);
    drop(player);
    assert!(
        d.shutdowns
            .recv_timeout(Duration::from_millis(300))
            .is_err()
    );
    // A fresh client still gets served.
    let again = connect();
    let out = Buf::default();
    exec_all(&App::new(&again, out.clone(), true), "status");
    assert!(out.text().contains("status: stopped"), "{}", out.text());
    drop(again);
    d.server.shutdown();
}

#[test]
fn daemon_quit_mid_script_exits_2() {
    let _g = serial();
    let d = daemon();
    let player = connect();
    let out = Buf::default();
    let server = d.server;
    let killer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        server.shutdown();
    });
    let code = app::run(
        &player,
        Cursor::new("await playing 20\nstatus\n"),
        out.clone(),
        true,
    )
    .unwrap();
    killer.join().unwrap();
    assert_eq!(code, 2);
    let text = out.text();
    assert!(
        text.contains("error: player disconnected: player quit\n"),
        "{text}"
    );
    assert!(!text.contains("status:"), "{text}");
}

#[test]
fn disconnect_mid_script_exits_2() {
    let _g = serial();
    // A daemon that completes the handshake, then drops the connection without a Goodbye.
    let path = requiem_ipc::socket_dir().unwrap().join("fake.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let fake = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut len = [0; 4];
        s.read_exact(&mut len).unwrap();
        s.read_exact(&mut vec![0; u32::from_be_bytes(len) as usize])
            .unwrap();
        let welcome = format!(r#"{{"Welcome":{{"protocol":{PROTOCOL},"version":"test"}}}}"#);
        s.write_all(&u32::try_from(welcome.len()).unwrap().to_be_bytes())
            .unwrap();
        s.write_all(welcome.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(300));
    });
    let (client, incoming) = Client::connect(&path, Role::Cli).unwrap();
    let player = RemotePlayer::new(client, incoming).unwrap();
    let out = Buf::default();
    let code = app::run(
        &player,
        Cursor::new("await playing 20\nstatus\n"),
        out.clone(),
        true,
    )
    .unwrap();
    fake.join().unwrap();
    assert_eq!(code, 2);
    let text = out.text();
    assert!(
        text.contains("error: player disconnected: daemon closed the connection\n"),
        "{text}"
    );
    assert!(!text.contains("status:"), "{text}");
}

#[test]
fn update_prints_the_status() {
    let _g = serial();
    let (code, text) = run_update(daemon(), "update\n", |d| d.server.shutdown());
    assert_eq!(code, 0);
    assert!(
        text.contains("Requiem 0.1.0\nautomatic checks: on\n"),
        "{text}"
    );
}

#[test]
fn requiem_update_prints_its_answer_alone() {
    let _g = serial();
    let d = daemon();
    let player = connect();
    let out = Buf::default();
    let code = app::run_update(&player, "update", out.clone(), Format::Text);
    d.server.shutdown();
    assert_eq!(code, 0);
    assert_eq!(out.text(), "Requiem 0.1.0\nautomatic checks: on\n");
}

#[test]
fn requiem_update_json_prints_its_answer_alone() {
    let _g = serial();
    let d = daemon();
    let player = connect();
    let out = Buf::default();
    let code = app::run_update(&player, "update", out.clone(), Format::Json);
    d.server.shutdown();
    assert_eq!(code, 0);
    let text = out.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    let json: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(json["type"], "update", "{text}");
}

#[test]
fn update_check_waits_through_the_download_for_the_result() {
    let _g = serial();
    let (code, text) = run_update(daemon(), "update check\n", |d| {
        expect_request(&d, UpdateRequest::Check);
        d.server.set_update(status(UpdateState::Checking));
        d.server.set_update(status(UpdateState::Downloading {
            version: "0.2.0".into(),
        }));
        d.server.set_update(status(ready()));
        thread::sleep(Duration::from_millis(300));
        d.server.shutdown();
    });
    assert_eq!(code, 0);
    assert!(text.contains("downloading Requiem 0.2.0\n"), "{text}");
    assert!(text.contains("Requiem 0.2.0 is ready."), "{text}");
}

#[test]
fn update_check_reports_a_failed_check() {
    let _g = serial();
    let (code, text) = run_update(daemon(), "update check\n", |d| {
        expect_request(&d, UpdateRequest::Check);
        d.server.set_update(status(UpdateState::Checking));
        d.server.set_update(status(UpdateState::Error {
            reason: "the update check failed: offline".into(),
        }));
        thread::sleep(Duration::from_millis(300));
        d.server.shutdown();
    });
    assert_eq!(code, 1);
    assert!(
        text.contains("Requiem 0.1.0: the update check failed: offline\n"),
        "{text}"
    );
}

#[test]
fn update_json_names_an_error_and_its_reason() {
    let _g = serial();
    let d = daemon();
    d.server.set_update(status(UpdateState::Error {
        reason: "the update check failed: offline".into(),
    }));
    let player = connect();
    let out = Buf::default();
    let code = app::run_update(&player, "update", out.clone(), Format::Json);
    d.server.shutdown();
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(out.text().trim()).unwrap();
    assert_eq!(
        (&json["state"], &json["reason"]),
        (
            &serde_json::json!("error"),
            &serde_json::json!("the update check failed: offline")
        )
    );
}

#[test]
fn update_check_prints_an_ignored_request_as_the_status() {
    let _g = serial();
    let (code, text) = run_update(daemon(), "update check\n", |d| {
        expect_request(&d, UpdateRequest::Check);
        d.server.set_update(status(UpdateState::UpToDate));
        thread::sleep(Duration::from_millis(300));
        d.server.shutdown();
    });
    assert_eq!(code, 0);
    assert!(text.contains("Requiem 0.1.0 is up to date."), "{text}");
}

#[test]
fn update_install_without_a_ready_update_fails() {
    let _g = serial();
    let (code, text) = run_update(daemon(), "update install\n", |d| {
        expect_request(&d, UpdateRequest::Install);
        d.server.set_update(status(UpdateState::Idle));
        thread::sleep(Duration::from_millis(300));
        d.server.shutdown();
    });
    assert_eq!(code, 1);
    assert!(
        text.contains("error: no update is ready to install"),
        "{text}"
    );
}

#[test]
fn update_install_ends_when_the_player_stops_to_restart() {
    let _g = serial();
    let d = daemon();
    d.server.set_update(status(ready()));
    let (code, text) = run_update(d, "update install\nstatus\n", |d| {
        expect_request(&d, UpdateRequest::Install);
        d.server.shutdown();
    });
    assert_eq!(code, 0);
    assert!(text.contains("Requiem is restarting on 0.2.0\n"), "{text}");
    assert!(!text.contains("status:"), "{text}");
}
