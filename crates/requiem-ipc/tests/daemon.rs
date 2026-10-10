//! The daemon's shape: a `Server` over a real `Session`, driven by real `Client`s through
//! the public API, with the socket directory taken from `REQUIEM_SOCKET_DIR`.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use requiem_core::testing::{FakeEngineHandle, FakeProvider, Step};
use requiem_core::{
    Command, EngineEvent, EnqueueMode, Event, PlayState, ProviderId, Session, SessionConfig, Track,
    TrackId,
};
use requiem_ipc::{Client, ClientCounts, Incoming, Role, Server, ServerHooks, socket_path};

const WAIT: Duration = Duration::from_secs(5);
const SECS: fn(u64) -> Duration = Duration::from_secs;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn track(id: &str) -> Track {
    Track {
        id: TrackId {
            provider: ProviderId("fake".into()),
            id: id.into(),
        },
        title: id.into(),
        artists: vec![],
        album: None,
        duration: None,
        artwork_url: None,
    }
}

fn take(rx: &Receiver<Incoming>, n: usize) -> Vec<Event> {
    (0..n)
        .map(|_| match rx.recv_timeout(WAIT).expect("timed out") {
            Incoming::Event(ev) => ev,
            other => panic!("expected an event, got {other:?}"),
        })
        .collect()
}

fn wait_disconnected(rx: &Receiver<Incoming>) {
    loop {
        if let Incoming::Disconnected(_) = rx.recv_timeout(WAIT).expect("still connected") {
            return;
        }
    }
}

/// Skips other messages up to `Goodbye`, which must be the last.
fn wait_goodbye(rx: &Receiver<Incoming>) {
    loop {
        match rx.recv_timeout(WAIT).expect("still connected") {
            Incoming::Goodbye => break,
            Incoming::Disconnected(why) => panic!("disconnected without a Goodbye: {why}"),
            _ => {}
        }
    }
    assert!(rx.recv_timeout(WAIT).is_err_and(|e| e.is_disconnected()));
}

fn wait_clients(server: &Server, want: ClientCounts) {
    let deadline = Instant::now() + WAIT;
    while server.clients() != want {
        assert!(Instant::now() < deadline, "timed out waiting for {want:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn daemon_flow() {
    let tmp = TempDir(env::temp_dir().join(format!("rqd{}", process::id())));
    fs::create_dir(&tmp.0).unwrap();
    let dir = tmp.0.join("s");
    // SAFETY: the only test in this binary, so no other thread reads the environment.
    unsafe { env::set_var("REQUIEM_SOCKET_DIR", &dir) };
    let path = socket_path().unwrap();
    assert_eq!(path, dir.join("daemon.sock"));

    let provider = FakeProvider::new("fake");
    let engine = FakeEngineHandle::new();
    let found: Vec<Track> = ["a", "b", "c"].map(track).into();
    provider.script_search("song", vec![Step::ok(found.clone())]);
    let session = Session::spawn(
        vec![provider.clone()],
        engine.factory(),
        SessionConfig::default(),
    );
    let (quit_tx, quit_rx) = unbounded();
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || {
            let _ = quit_tx.send(());
        }),
    };
    let server = Server::start(session, hooks).unwrap();

    let (gui, gui_rx) = Client::connect(&path, Role::Gui).unwrap();
    assert_eq!(
        take(&gui_rx, 3),
        [
            Event::QueueChanged {
                tracks: vec![],
                current: None,
            },
            Event::State(PlayState::Stopped),
            Event::Volume(100),
        ]
    );

    gui.send(Command::Search {
        req: 1,
        query: "song".into(),
    })
    .unwrap();
    assert_eq!(
        take(&gui_rx, 1),
        [Event::SearchResults {
            req: 1,
            tracks: found.clone(),
        }]
    );
    gui.send(Command::Enqueue {
        tracks: found.clone(),
        mode: EnqueueMode::Replace,
    })
    .unwrap();
    gui.send(Command::PlayIndex(1)).unwrap();
    assert_eq!(
        take(&gui_rx, 2),
        [
            Event::QueueChanged {
                tracks: found.clone(),
                current: None,
            },
            Event::QueueChanged {
                tracks: found.clone(),
                current: Some(1),
            },
        ]
    );
    let (r#gen, payload) = engine.wait_load(1);
    assert_eq!(payload, "b");
    engine.emit(EngineEvent::Started {
        r#gen,
        duration: Some(SECS(200)),
    });
    engine.emit(EngineEvent::Position { r#gen, at: SECS(5) });
    gui.send(Command::SetVolume(40)).unwrap();
    let now_playing = Event::NowPlaying {
        track: track("b"),
        duration: Some(SECS(200)),
    };
    assert_eq!(
        take(&gui_rx, 4),
        [
            now_playing.clone(),
            Event::State(PlayState::Playing),
            Event::Position(SECS(5)),
            Event::Volume(40),
        ]
    );

    // A client connecting later rebuilds the same state from the snapshot.
    let (cli, cli_rx) = Client::connect(&path, Role::Cli).unwrap();
    let snapshot = |state| {
        vec![
            Event::QueueChanged {
                tracks: found.clone(),
                current: Some(1),
            },
            now_playing.clone(),
            Event::State(state),
            Event::Position(SECS(5)),
            Event::Volume(40),
        ]
    };
    assert_eq!(take(&cli_rx, 5), snapshot(PlayState::Playing));
    assert_eq!(server.clients(), ClientCounts { gui: 1, cli: 1 });

    cli.send(Command::Pause).unwrap();
    for rx in [&gui_rx, &cli_rx] {
        assert_eq!(take(rx, 1), [Event::State(PlayState::Paused)]);
    }

    // The GUI goes away and comes back.
    drop(gui);
    wait_disconnected(&gui_rx);
    wait_clients(&server, ClientCounts { gui: 0, cli: 1 });
    let (_gui, gui_rx) = Client::connect(&path, Role::Gui).unwrap();
    assert_eq!(take(&gui_rx, 5), snapshot(PlayState::Paused));

    // The CLI's `shutdown-daemon`.
    cli.send(Command::Shutdown).unwrap();
    quit_rx.recv_timeout(WAIT).unwrap();
    server.shutdown();
    wait_goodbye(&gui_rx);
    wait_goodbye(&cli_rx);
    assert!(!path.exists());
}
