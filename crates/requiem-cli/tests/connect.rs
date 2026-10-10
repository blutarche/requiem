//! `--connect` mode against an in-process `requiem_ipc::Server`. All tests share one socket
//! directory, so they run one at a time.

use std::io::{self, Cursor, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use requiem_cli::app::{self, App, Flow, Player};
use requiem_cli::remote::RemotePlayer;
use requiem_core::testing::{FakeEngineHandle, FakeProvider, Step};
use requiem_core::{EngineEvent, ProviderId, Session, SessionConfig, Track, TrackId};
use requiem_ipc::{Client, PROTOCOL, Role, Server, ServerHooks};

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
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || {
            let _ = tx.try_send(());
        }),
    };
    let server = Server::start(session, hooks).expect("start server");
    Daemon {
        server,
        provider,
        engine,
        shutdowns,
    }
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
    assert!(text.contains("  1. A — Title t1 (2:05)\n"), "{text}");
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
