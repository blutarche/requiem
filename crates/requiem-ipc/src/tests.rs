use std::env;
use std::fs::{self, Permissions};
use std::io::{self, Cursor, Write};
use std::iter;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, unbounded};
use requiem_core::testing::{EngineCall, FakeEngineHandle, FakeProvider, Gate, Step};
use requiem_core::{
    AccountState, Command, EngineEvent, EnqueueMode, Event, PlayState, ProviderError, ProviderId,
    Session, SessionConfig, Track, TrackId,
};

use crate::frame::{self, Deadline, MAX_FRAME};
use crate::hub::{self, HubMsg};
use crate::path::{SOCKET_NAME, SUN_PATH_LEN, check_sun_path, default_dir, prepare_dir};
use crate::{
    Client, ClientCounts, ClientMsg, Incoming, Instance, PROTOCOL, Role, ServeError, Server,
    ServerHooks, ServerMsg, UpdateRequest, UpdateState, UpdateStatus,
};
use crate::{server, sys};

const WAIT: Duration = Duration::from_secs(5);
const SECS: fn(u64) -> Duration = Duration::from_secs;
/// Events in the join snapshot of a stopped player with an empty queue.
const IDLE_SNAPSHOT: usize = 4;

/// The update status a test server starts with.
fn idle_update() -> UpdateStatus {
    UpdateStatus {
        current: "test".into(),
        auto: true,
        state: UpdateState::Idle,
    }
}

/// The fake provider's account as every snapshot carries it.
fn account() -> Event {
    Event::Account {
        provider: ProviderId("fake".into()),
        state: AccountState::SignedOut,
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        // Short, so socket paths under it fit in sun_path.
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("rq{}-{n}", process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    server: Server,
    provider: Arc<FakeProvider>,
    engine: FakeEngineHandle,
    shutdowns: Receiver<()>,
    requests: Receiver<UpdateRequest>,
    dir: PathBuf,
    path: PathBuf,
    _tmp: TempDir,
}

fn start(
    dir: &Path,
    provider: &Arc<FakeProvider>,
    engine: &FakeEngineHandle,
) -> Result<(Server, Receiver<()>, Receiver<UpdateRequest>), ServeError> {
    let session = Session::spawn(
        vec![provider.clone()],
        engine.factory(),
        SessionConfig::default(),
    );
    let (tx, rx) = unbounded();
    let (req_tx, req_rx) = unbounded();
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || {
            let _ = tx.send(());
        }),
        on_update: Box::new(move |req| {
            let _ = req_tx.send(req);
        }),
    };
    Instance::lock_in(dir)?
        .serve(session, hooks, idle_update())
        .map(|s| (s, rx, req_rx))
}

fn rig() -> Rig {
    let tmp = TempDir::new();
    let dir = tmp.0.join("s");
    prepare_dir(&dir).unwrap();
    let provider = FakeProvider::new("fake");
    let engine = FakeEngineHandle::new();
    let (server, shutdowns, requests) = start(&dir, &provider, &engine).unwrap();
    Rig {
        server,
        provider,
        engine,
        shutdowns,
        requests,
        path: dir.join(SOCKET_NAME),
        dir,
        _tmp: tmp,
    }
}

impl Rig {
    /// Past the update status that every client gets right after `Welcome`.
    fn client(&self, role: Role) -> (Client, Receiver<Incoming>) {
        let (client, rx) = Client::connect(&self.path, role).unwrap();
        expect_update(&rx, &idle_update());
        (client, rx)
    }

    /// A connection that has sent `Hello` and read nothing yet.
    fn raw(&self, role: Role) -> UnixStream {
        let s = UnixStream::connect(&self.path).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        hello(&s, PROTOCOL, role);
        s
    }

    fn wait_clients(&self, want: ClientCounts) {
        let deadline = Instant::now() + WAIT;
        while self.server.clients() != want {
            assert!(Instant::now() < deadline, "timed out waiting for {want:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Replaces the queue, starts its first track and returns the load generation.
    fn play(&self, ids: &[&str]) -> u64 {
        self.server.send(Command::Enqueue {
            tracks: tracks(ids),
            mode: EnqueueMode::Replace,
        });
        self.server.send(Command::PlayIndex(0));
        let (r#gen, _) = self.engine.wait_load(1);
        self.engine.emit(EngineEvent::Started {
            r#gen,
            duration: Some(SECS(180)),
        });
        r#gen
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

fn tracks(ids: &[&str]) -> Vec<Track> {
    ids.iter().map(|i| track(i)).collect()
}

fn hello(s: &UnixStream, protocol: u32, role: Role) {
    let msg = ClientMsg::Hello {
        protocol,
        role,
        version: "test".into(),
    };
    frame::write(&mut &*s, &msg).unwrap();
}

fn read_msg(s: &UnixStream) -> ServerMsg {
    frame::read(&mut &*s)
        .unwrap()
        .expect("connection closed early")
}

fn next_event(rx: &Receiver<Incoming>) -> Event {
    match rx
        .recv_timeout(WAIT)
        .expect("timed out waiting for a message")
    {
        Incoming::Event(ev) => ev,
        other => panic!("expected an event, got {other:?}"),
    }
}

fn expect_update(rx: &Receiver<Incoming>, want: &UpdateStatus) {
    let got = rx
        .recv_timeout(WAIT)
        .expect("timed out waiting for a message");
    assert_eq!(got, Incoming::Update(want.clone()));
}

fn take(rx: &Receiver<Incoming>, n: usize) -> Vec<Event> {
    (0..n).map(|_| next_event(rx)).collect()
}

fn wait_sub(rx: &Receiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = rx.recv_timeout(WAIT).expect("timed out waiting for event");
        if pred(&ev) {
            return ev;
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
    assert_eq!(rx.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected));
}

/// Skips other messages up to `Disconnected`, which must be the last.
fn wait_disconnected(rx: &Receiver<Incoming>) {
    loop {
        match rx.recv_timeout(WAIT).expect("still connected") {
            Incoming::Disconnected(_) => break,
            Incoming::Goodbye => panic!("Goodbye on a dropped connection"),
            _ => {}
        }
    }
    assert_eq!(rx.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected));
}

/// A client of a fake daemon on a socketpair that answers `Welcome`, sends `then` and hangs
/// up without closing anything else.
fn client_of(then: Vec<ServerMsg>) -> (Client, Receiver<Incoming>) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let daemon = thread::spawn(move || {
        let _: ClientMsg = frame::read(&mut &theirs).unwrap().unwrap();
        let welcome = ServerMsg::Welcome {
            protocol: PROTOCOL,
            version: "test".into(),
        };
        for msg in iter::once(welcome).chain(then) {
            frame::write(&mut &theirs, &msg).unwrap();
        }
    });
    let connected = Client::handshake(ours, Role::Gui).unwrap();
    daemon.join().unwrap();
    connected
}

#[test]
fn frame_round_trip() {
    let client_msgs = [
        ClientMsg::Hello {
            protocol: PROTOCOL,
            role: Role::Gui,
            version: "1.2.3".into(),
        },
        ClientMsg::Command(Command::Search {
            req: 7,
            query: "naïve café".into(),
        }),
        ClientMsg::Command(Command::Enqueue {
            tracks: tracks(&["a", "b"]),
            mode: EnqueueMode::Append,
        }),
        ClientMsg::Ping(u64::MAX),
        ClientMsg::CheckForUpdates,
        ClientMsg::InstallUpdate,
        ClientMsg::SetAutoUpdate(false),
    ];
    let mut buf = Vec::new();
    for m in &client_msgs {
        frame::write(&mut buf, m).unwrap();
    }
    let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
    assert_eq!(
        buf[4..4 + len],
        serde_json::to_vec(&client_msgs[0]).unwrap()
    );
    let mut r = Cursor::new(buf);
    for m in &client_msgs {
        assert_eq!(
            frame::read::<_, ClientMsg>(&mut r).unwrap().as_ref(),
            Some(m)
        );
    }
    assert_eq!(frame::read::<_, ClientMsg>(&mut r).unwrap(), None);

    let server_msgs = [
        ServerMsg::Welcome {
            protocol: PROTOCOL,
            version: "1.2.3".into(),
        },
        ServerMsg::Incompatible { supported: 9 },
        ServerMsg::Event(Event::QueueChanged {
            tracks: tracks(&["a"]),
            current: Some(0),
        }),
        ServerMsg::Event(Event::Position(Duration::from_millis(1500))),
        ServerMsg::Raise,
        ServerMsg::Pong(3),
        ServerMsg::Update(UpdateStatus {
            current: "0.1.0".into(),
            auto: false,
            state: UpdateState::Failed {
                version: "0.2.0".into(),
                reason: "the new version did not start".into(),
            },
        }),
    ];
    let mut buf = Vec::new();
    for m in &server_msgs {
        frame::write(&mut buf, m).unwrap();
    }
    let mut r = Cursor::new(buf);
    for m in &server_msgs {
        assert_eq!(
            frame::read::<_, ServerMsg>(&mut r).unwrap().as_ref(),
            Some(m)
        );
    }
    assert_eq!(frame::read::<_, ServerMsg>(&mut r).unwrap(), None);
}

#[test]
fn oversize_frames_are_rejected() {
    // Refused from the header alone, before the body is read.
    let header = ((MAX_FRAME + 1) as u32).to_be_bytes();
    let err = frame::read_body(&mut Cursor::new(header)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);

    let header = (MAX_FRAME as u32).to_be_bytes();
    let err = frame::read_body(&mut Cursor::new(header)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

    let big = ClientMsg::Command(Command::Search {
        req: 1,
        query: "x".repeat(MAX_FRAME),
    });
    assert_eq!(
        frame::encode(&big).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );

    let err = frame::read_body(&mut Cursor::new([0u8, 0])).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn oversize_frame_closes_the_connection() {
    let rig = rig();
    let s = rig.raw(Role::Cli);
    assert!(matches!(read_msg(&s), ServerMsg::Welcome { .. }));
    rig.wait_clients(ClientCounts { gui: 0, cli: 1 });
    (&s).write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
        .unwrap();
    rig.wait_clients(ClientCounts::default());
    while frame::read::<_, ServerMsg>(&mut &s).unwrap().is_some() {}
}

#[test]
fn handshake_welcome_then_snapshot() {
    let rig = rig();
    let s = rig.raw(Role::Gui);
    assert_eq!(
        read_msg(&s),
        ServerMsg::Welcome {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
        }
    );
    assert_eq!(read_msg(&s), ServerMsg::Update(idle_update()));
    let snapshot = [
        Event::QueueChanged {
            tracks: vec![],
            current: None,
        },
        Event::State(PlayState::Stopped),
        account(),
        Event::Volume(100),
    ];
    for ev in &snapshot {
        assert_eq!(read_msg(&s), ServerMsg::Event(ev.clone()));
    }
    frame::write(&mut &s, &ClientMsg::Ping(42)).unwrap();
    assert_eq!(read_msg(&s), ServerMsg::Pong(42));

    let (_c, rx) = rig.client(Role::Cli);
    assert_eq!(take(&rx, IDLE_SNAPSHOT), snapshot);
    assert_eq!(rig.server.clients(), ClientCounts { gui: 1, cli: 1 });
}

#[test]
fn version_mismatch_gets_incompatible() {
    let rig = rig();
    let s = UnixStream::connect(&rig.path).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    hello(&s, PROTOCOL + 1, Role::Gui);
    assert_eq!(
        read_msg(&s),
        ServerMsg::Incompatible {
            supported: PROTOCOL
        }
    );
    assert_eq!(frame::read::<_, ServerMsg>(&mut &s).unwrap(), None);

    // A future Hello whose other fields no longer parse still learns the supported version.
    let s = UnixStream::connect(&rig.path).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let future =
        serde_json::json!({"Hello": {"protocol": PROTOCOL + 1, "role": "Tray", "build": 7}});
    frame::write(&mut &s, &future).unwrap();
    assert_eq!(
        read_msg(&s),
        ServerMsg::Incompatible {
            supported: PROTOCOL
        }
    );

    // Anything but `Hello` first is dropped without a reply.
    let s = UnixStream::connect(&rig.path).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    frame::write(&mut &s, &ClientMsg::Ping(1)).unwrap();
    assert_eq!(frame::read::<_, ServerMsg>(&mut &s).unwrap(), None);
    assert_eq!(rig.server.clients(), ClientCounts::default());
}

/// One handshake over a socket pair, with `parent` standing in for this process's spawner.
fn handshake_with(protocol: u32, role: Role, parent: i32) -> (Option<Role>, Option<ServerMsg>) {
    let (client, server) = UnixStream::pair().unwrap();
    client.set_read_timeout(Some(WAIT)).unwrap();
    hello(&client, protocol, role);
    let got = server::handshake_as(&server, parent);
    drop(server);
    (got, frame::read(&mut &client).unwrap())
}

#[test]
fn an_older_cli_from_our_spawner_gets_a_welcome_in_its_protocol() {
    let me = process::id() as i32;
    let (role, reply) = handshake_with(PROTOCOL - 1, Role::Cli, me);
    assert_eq!(role, None);
    assert_eq!(
        reply,
        Some(ServerMsg::Welcome {
            protocol: PROTOCOL - 1,
            version: env!("CARGO_PKG_VERSION").into(),
        })
    );

    // The exact JSON the older daemons' restart check reads, and the connection is closed.
    let (client, server) = UnixStream::pair().unwrap();
    client.set_read_timeout(Some(WAIT)).unwrap();
    let old = serde_json::json!({"Hello": {"protocol": 4, "role": "Cli", "version": "0.3.0"}});
    frame::write(&mut &client, &old).unwrap();
    assert_eq!(server::handshake_as(&server, me), None);
    drop(server);
    let welcome: serde_json::Value = frame::read(&mut &client).unwrap().unwrap();
    assert_eq!(
        welcome,
        serde_json::json!({"Welcome": {"protocol": 4, "version": env!("CARGO_PKG_VERSION")}})
    );
    assert_eq!(frame::read::<_, ServerMsg>(&mut &client).unwrap(), None);
}

#[test]
fn an_older_cli_from_anyone_else_gets_incompatible() {
    let other = process::id() as i32 + 1;
    let (role, reply) = handshake_with(PROTOCOL - 1, Role::Cli, other);
    assert_eq!(role, None);
    assert_eq!(
        reply,
        Some(ServerMsg::Incompatible {
            supported: PROTOCOL
        })
    );
}

#[test]
fn an_older_gui_from_our_spawner_gets_incompatible() {
    let me = process::id() as i32;
    let (role, reply) = handshake_with(PROTOCOL - 1, Role::Gui, me);
    assert_eq!(role, None);
    assert_eq!(
        reply,
        Some(ServerMsg::Incompatible {
            supported: PROTOCOL
        })
    );

    // A role that no longer decodes is not a `Cli`.
    let (client, server) = UnixStream::pair().unwrap();
    client.set_read_timeout(Some(WAIT)).unwrap();
    let odd = serde_json::json!({"Hello": {"protocol": PROTOCOL - 1, "role": "Tray"}});
    frame::write(&mut &client, &odd).unwrap();
    assert_eq!(server::handshake_as(&server, me), None);
    assert_eq!(
        read_msg(&client),
        ServerMsg::Incompatible {
            supported: PROTOCOL
        }
    );
}

#[test]
fn a_current_hello_is_unchanged_whoever_spawned_us() {
    let me = process::id() as i32;
    for parent in [me, me + 1] {
        let (client, server) = UnixStream::pair().unwrap();
        hello(&client, PROTOCOL, Role::Cli);
        assert_eq!(server::handshake_as(&server, parent), Some(Role::Cli));
    }
}

#[test]
fn client_reports_incompatible_daemon_as_unsupported() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let daemon = thread::spawn(move || {
        let hello: ClientMsg = frame::read(&mut &theirs).unwrap().unwrap();
        assert!(matches!(
            hello,
            ClientMsg::Hello {
                protocol: PROTOCOL,
                role: Role::Gui,
                ..
            }
        ));
        let reply = ServerMsg::Incompatible {
            supported: PROTOCOL + 1,
        };
        frame::write(&mut &theirs, &reply).unwrap();
    });
    let Err(err) = Client::handshake(ours, Role::Gui) else {
        panic!("handshake with an incompatible daemon succeeded");
    };
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    daemon.join().unwrap();
}

#[test]
fn snapshot_replays_state_in_order() {
    let rig = rig();
    let sub = rig.server.subscribe();
    let r#gen = rig.play(&["a", "b"]);
    rig.engine.emit(EngineEvent::Position {
        r#gen,
        at: SECS(42),
    });
    rig.server.send(Command::SetVolume(30));
    wait_sub(&sub, |e| *e == Event::Volume(30));

    let playing = vec![
        Event::QueueChanged {
            tracks: tracks(&["a", "b"]),
            current: Some(0),
        },
        Event::NowPlaying {
            track: track("a"),
            duration: Some(SECS(180)),
        },
        Event::State(PlayState::Playing),
        Event::Position(SECS(42)),
        account(),
        Event::Volume(30),
    ];
    let (_c, rx) = rig.client(Role::Gui);
    assert_eq!(take(&rx, 6), playing);
    rig.engine.emit(EngineEvent::Position {
        r#gen,
        at: SECS(43),
    });
    assert_eq!(next_event(&rx), Event::Position(SECS(43)));

    let sub2 = rig.server.subscribe();
    let got: Vec<Event> = (0..6).map(|_| sub2.recv_timeout(WAIT).unwrap()).collect();
    assert_eq!(got[..3], playing[..3]);
    assert_eq!(got[3], Event::Position(SECS(43)));
    assert_eq!(got[4..], playing[4..]);

    // Stopped means no track is loaded: no NowPlaying or Position.
    rig.server.send(Command::Next);
    rig.server.send(Command::Next);
    wait_sub(&sub, |e| *e == Event::State(PlayState::Stopped));
    let (_c2, rx2) = rig.client(Role::Cli);
    assert_eq!(
        take(&rx2, IDLE_SNAPSHOT),
        [
            Event::QueueChanged {
                tracks: tracks(&["a", "b"]),
                current: Some(1),
            },
            Event::State(PlayState::Stopped),
            account(),
            Event::Volume(30),
        ]
    );
}

#[test]
fn snapshot_then_live_has_no_gap_or_overlap() {
    let rig = rig();
    let sub = rig.server.subscribe();
    let server = &rig.server;
    thread::scope(|s| {
        s.spawn(|| {
            for v in 0..100 {
                server.send(Command::SetVolume(v));
                thread::sleep(Duration::from_millis(2));
            }
        });
        wait_sub(&sub, |e| *e == Event::Volume(20));
        let (_c, rx) = rig.client(Role::Cli);
        assert!(matches!(next_event(&rx), Event::QueueChanged { .. }));
        assert_eq!(next_event(&rx), Event::State(PlayState::Stopped));
        assert_eq!(next_event(&rx), account());
        let Event::Volume(first) = next_event(&rx) else {
            panic!("snapshot has no Volume");
        };
        assert!(first >= 20);
        for want in first + 1..100 {
            assert_eq!(next_event(&rx), Event::Volume(want));
        }
    });
}

#[test]
fn search_results_go_only_to_the_requester_under_its_own_id() {
    let rig = rig();
    let (gate_a, gate_b) = (Gate::new(), Gate::new());
    let p = &rig.provider;
    p.script_search("qa", vec![Step::ok(tracks(&["a1"])).gated(&gate_a)]);
    p.script_search("qb", vec![Step::ok(tracks(&["b1", "b2"])).gated(&gate_b)]);
    p.script_search("qf", vec![Step::err(ProviderError::Network("down".into()))]);
    p.script_search("qs", vec![Step::ok(tracks(&["s1"]))]);
    let (a, arx) = rig.client(Role::Gui);
    let (b, brx) = rig.client(Role::Cli);
    let sub = rig.server.subscribe();
    take(&arx, IDLE_SNAPSHOT);
    take(&brx, IDLE_SNAPSHOT);

    // Same local id from both, both in flight at once.
    let search = |query: &str, req| Command::Search {
        req,
        query: query.into(),
    };
    a.send(search("qa", 1)).unwrap();
    b.send(search("qb", 1)).unwrap();
    p.wait_search_calls(2);
    gate_b.release();
    assert_eq!(
        next_event(&brx),
        Event::SearchResults {
            req: 1,
            tracks: tracks(&["b1", "b2"]),
        }
    );
    gate_a.release();
    assert_eq!(
        next_event(&arx),
        Event::SearchResults {
            req: 1,
            tracks: tracks(&["a1"]),
        }
    );

    a.send(search("qf", 2)).unwrap();
    assert_eq!(
        next_event(&arx),
        Event::SearchFailed {
            req: 2,
            error: "network error: down".into(),
        }
    );

    rig.server.send(search("qs", 9));
    wait_sub(&sub, |e| {
        *e == Event::SearchResults {
            req: 9,
            tracks: tracks(&["s1"]),
        }
    });

    // Every search result was emitted before this broadcast; none reached the wrong client.
    rig.server.send(Command::SetVolume(55));
    assert_eq!(next_event(&arx), Event::Volume(55));
    assert_eq!(next_event(&brx), Event::Volume(55));
}

#[test]
fn browse_results_go_only_to_the_requester_and_accounts_to_everyone() {
    let rig = rig();
    let shelf = requiem_core::Shelf {
        title: "Shelf".into(),
        items: vec![requiem_core::Item::Track(track("h1"))],
    };
    let p = &rig.provider;
    let page = requiem_core::Page::Collection(requiem_core::CollectionId {
        provider: ProviderId("fake".into()),
        id: "c".into(),
    });
    p.script_browse(page.clone(), vec![Step::ok(vec![shelf.clone()])]);
    let (a, arx) = rig.client(Role::Gui);
    let (b, brx) = rig.client(Role::Cli);
    take(&arx, IDLE_SNAPSHOT);
    take(&brx, IDLE_SNAPSHOT);

    a.send(Command::Browse {
        req: 3,
        page: page.clone(),
    })
    .unwrap();
    assert_eq!(
        next_event(&arx),
        Event::BrowseResults {
            req: 3,
            page,
            shelves: vec![shelf],
        }
    );
    b.send(Command::Browse {
        req: 3,
        page: requiem_core::Page::Library,
    })
    .unwrap();
    assert_eq!(
        next_event(&brx),
        Event::BrowseFailed {
            req: 3,
            error: "sign-in required: sign in to browse your library".into(),
            sign_in_required: true,
        }
    );

    p.script_sign_in(requiem_core::testing::SignInScript {
        pending: None,
        gate: None,
        outcome: Ok(AccountState::SignedIn { name: None }),
    });
    b.send(Command::SignIn {
        provider: ProviderId("fake".into()),
    })
    .unwrap();
    let signed_in = Event::Account {
        provider: ProviderId("fake".into()),
        state: AccountState::SignedIn { name: None },
    };
    assert_eq!(next_event(&arx), signed_in);
    assert_eq!(next_event(&brx), signed_in);
    let (_c, crx) = rig.client(Role::Cli);
    let snapshot = take(&crx, IDLE_SNAPSHOT);
    assert_eq!(snapshot[2], signed_in);
}

#[test]
fn a_changed_page_is_told_to_every_client() {
    let rig = rig();
    let p = &rig.provider;
    let shelf = |id: &str| requiem_core::Shelf {
        title: "Playlists".into(),
        items: vec![requiem_core::Item::Track(track(id))],
    };
    p.script_browse(
        requiem_core::Page::Library,
        vec![Step::ok(vec![shelf("old")]), Step::ok(vec![shelf("new")])],
    );
    p.script_sign_in(requiem_core::testing::SignInScript {
        pending: None,
        gate: None,
        outcome: Ok(AccountState::SignedIn { name: None }),
    });
    let (a, arx) = rig.client(Role::Gui);
    let (_b, brx) = rig.client(Role::Cli);
    take(&arx, IDLE_SNAPSHOT);
    take(&brx, IDLE_SNAPSHOT);
    a.send(Command::SignIn {
        provider: ProviderId("fake".into()),
    })
    .unwrap();
    next_event(&arx);
    next_event(&brx);
    a.send(Command::Browse {
        req: 1,
        page: requiem_core::Page::Library,
    })
    .unwrap();
    assert!(matches!(next_event(&arx), Event::BrowseResults { .. }));

    p.say_changed(requiem_core::Page::Library);
    let changed = Event::PageChanged {
        page: requiem_core::Page::Library,
        shelves: vec![shelf("new")],
    };
    assert_eq!(next_event(&arx), changed);
    assert_eq!(next_event(&brx), changed);
}

#[test]
fn an_imported_clients_json_is_a_plain_string_on_the_wire() {
    let json = r#"{"client_id":"id","client_secret":"s"}"#;
    let cmd = Command::ImportSignInClient {
        provider: ProviderId("fake".into()),
        json: json.into(),
    };
    assert_eq!(
        serde_json::to_value(&cmd).unwrap(),
        serde_json::json!({"ImportSignInClient": {"provider": "fake", "json": json}})
    );
}

#[test]
fn an_imported_client_reaches_the_provider() {
    let rig = rig();
    let (a, arx) = rig.client(Role::Gui);
    take(&arx, IDLE_SNAPSHOT);
    // As large as a provider takes.
    let json = format!(
        r#"{{"client_id":"id","pad":"{}"}}"#,
        "x".repeat(64 * 1024 - 32)
    );
    a.send(Command::ImportSignInClient {
        provider: ProviderId("fake".into()),
        json: json.clone().into(),
    })
    .unwrap();
    assert_eq!(next_event(&arx), account());
    assert_eq!(rig.provider.imported(), [json]);
}

#[test]
fn slow_client_loses_positions_then_its_connection() {
    let rig = rig();
    let sub = rig.server.subscribe();
    let r#gen = rig.play(&["a"]);
    wait_sub(&sub, |e| *e == Event::State(PlayState::Playing));
    let slow = rig.raw(Role::Cli);
    let stuck = rig.raw(Role::Cli);
    rig.wait_clients(ClientCounts { gui: 0, cli: 2 });

    // One frame far larger than any socket buffer blocks both writers mid-write, so
    // everything after it stays in the hub's per-client queues.
    let filler: Vec<Track> = (0..3000)
        .map(|i| Track {
            title: "x".repeat(400),
            ..track(&format!("f{i}"))
        })
        .collect();
    rig.server.send(Command::Enqueue {
        tracks: filler,
        mode: EnqueueMode::Append,
    });
    let is_filler =
        |e: &Event| matches!(e, Event::QueueChanged { tracks, .. } if tracks.len() == 3001);
    wait_sub(&sub, is_filler);

    for v in 0..100 {
        rig.server.send(Command::SetVolume(v));
    }
    wait_sub(&sub, |e| *e == Event::Volume(99));
    for s in 1000..1010 {
        rig.engine
            .emit(EngineEvent::Position { r#gen, at: SECS(s) });
    }
    wait_sub(&sub, |e| *e == Event::Position(SECS(1009)));
    rig.server.send(Command::Pause);
    wait_sub(&sub, |e| *e == Event::State(PlayState::Paused));
    assert_eq!(rig.server.clients(), ClientCounts { gui: 0, cli: 2 });

    // With 100 queued, the Positions were dropped; nothing else was.
    let mut volumes = None;
    loop {
        let ServerMsg::Event(ev) = read_msg(&slow) else {
            continue;
        };
        match ev {
            ev if is_filler(&ev) => volumes = Some(0),
            Event::Volume(_) => volumes = volumes.map(|n| n + 1),
            Event::Position(at) => assert!(at < SECS(1000), "backlogged {at:?} delivered"),
            Event::State(PlayState::Paused) => break,
            _ => {}
        }
    }
    assert_eq!(volumes, Some(100));

    // `stuck` never reads, so its queue fills and the hub cuts it off; `slow` keeps up.
    thread::spawn(move || while let Ok(Some(_)) = frame::read::<_, ServerMsg>(&mut &slow) {});
    for v in 0..300 {
        rig.server.send(Command::SetVolume((v % 100) as u8));
    }
    rig.wait_clients(ClientCounts { gui: 0, cli: 1 });
    loop {
        match frame::read::<_, ServerMsg>(&mut &stuck) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
                break;
            }
        }
    }
}

#[test]
fn raise_reaches_gui_clients_only() {
    let rig = rig();
    assert!(!rig.server.raise_gui());
    let (_cli, cli_rx) = rig.client(Role::Cli);
    assert!(!rig.server.raise_gui());
    let (_gui, gui_rx) = rig.client(Role::Gui);
    assert_eq!(rig.server.clients(), ClientCounts { gui: 1, cli: 1 });
    assert!(rig.server.raise_gui());
    take(&gui_rx, IDLE_SNAPSHOT);
    assert_eq!(gui_rx.recv_timeout(WAIT).unwrap(), Incoming::Raise);

    rig.server.send(Command::SetVolume(5));
    take(&cli_rx, IDLE_SNAPSHOT);
    assert_eq!(next_event(&cli_rx), Event::Volume(5));
}

#[test]
fn shutdown_command_runs_the_hook_and_leaves_the_session_to_the_daemon() {
    let rig = rig();
    let (c, rx) = rig.client(Role::Cli);
    take(&rx, IDLE_SNAPSHOT);
    c.send(Command::Shutdown).unwrap();
    rig.shutdowns.recv_timeout(WAIT).unwrap();
    c.send(Command::SetVolume(7)).unwrap();
    assert_eq!(next_event(&rx), Event::Volume(7));
    assert!(!rig.engine.calls().contains(&EngineCall::Stop));

    let path = rig.path.clone();
    rig.server.shutdown();
    wait_goodbye(&rx);
    assert!(!path.exists());
    assert!(rig.engine.calls().contains(&EngineCall::Stop));
}

#[test]
fn second_server_reports_already_running() {
    let rig = rig();
    let second = start(
        &rig.dir,
        &FakeProvider::new("fake"),
        &FakeEngineHandle::new(),
    );
    assert!(matches!(second, Err(ServeError::AlreadyRunning)));
    let (_c, rx) = rig.client(Role::Cli);
    assert!(matches!(next_event(&rx), Event::QueueChanged { .. }));

    // The lock goes with the first server.
    rig.server.shutdown();
    let (again, ..) = start(
        &rig.dir,
        &FakeProvider::new("fake"),
        &FakeEngineHandle::new(),
    )
    .unwrap();
    again.shutdown();
}

#[test]
fn stale_socket_is_replaced() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("s");
    prepare_dir(&dir).unwrap();
    let path = dir.join(SOCKET_NAME);
    drop(UnixListener::bind(&path).unwrap());
    assert_eq!(
        UnixStream::connect(&path).unwrap_err().kind(),
        io::ErrorKind::ConnectionRefused
    );

    let (server, ..) = start(&dir, &FakeProvider::new("fake"), &FakeEngineHandle::new()).unwrap();
    let (_c, rx) = Client::connect(&path, Role::Cli).unwrap();
    expect_update(&rx, &idle_update());
    assert!(matches!(next_event(&rx), Event::QueueChanged { .. }));
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    server.shutdown();
    assert!(!path.exists());
}

#[test]
fn socket_dir_is_created_private_and_validated() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("s");
    prepare_dir(&dir).unwrap();
    let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&dir), 0o700);
    prepare_dir(&dir).unwrap();

    // Ours, so any group or other bits are removed rather than refused.
    for loose in [0o770, 0o720, 0o702, 0o777, 0o755, 0o701] {
        fs::set_permissions(&dir, Permissions::from_mode(loose)).unwrap();
        prepare_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700, "from {loose:o}");
    }

    // Not ours: refused before anything is changed. Root owns everything, so skip as root.
    if sys::euid() != 0 {
        let err = prepare_dir(Path::new("/")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    let link = tmp.0.join("l");
    std::os::unix::fs::symlink(&dir, &link).unwrap();
    let err = prepare_dir(&link).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

    let file = tmp.0.join("f");
    fs::write(&file, b"").unwrap();
    let err = prepare_dir(&file).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

    assert!(prepare_dir(&tmp.0.join("missing/s")).is_err());
}

#[test]
fn socket_paths_must_fit_sun_path() {
    let at = |len: usize| PathBuf::from(format!("/{}", "d".repeat(len - 1)));
    assert!(check_sun_path(&at(SUN_PATH_LEN - 1)).is_ok());
    let err = check_sun_path(&at(SUN_PATH_LEN)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let Err(err) = Client::connect(&at(SUN_PATH_LEN), Role::Cli) else {
        panic!("connected through an over-long path");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn default_dir_is_per_user() {
    let dir = default_dir().unwrap();
    assert!(dir.is_absolute(), "{}", dir.display());
    #[cfg(target_os = "macos")]
    assert!(dir.ends_with("requiem"), "{}", dir.display());
    #[cfg(target_os = "linux")]
    match env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(xdg) if xdg.is_absolute() => assert_eq!(dir, xdg.join("requiem")),
        _ => assert_eq!(dir, PathBuf::from(format!("/tmp/requiem-{}", sys::euid()))),
    }
}

#[test]
fn handshake_reads_share_one_deadline() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    // Each byte arrives well within the deadline; the whole frame never does.
    let trickle = thread::spawn(move || {
        for b in frame::encode(&ClientMsg::Ping(1)).unwrap() {
            if (&theirs).write_all(&[b]).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    let started = Instant::now();
    let err = frame::read_body(&mut Deadline::new(&ours, Duration::from_millis(200))).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(ours);
    trickle.join().unwrap();
}

#[test]
fn connect_or_spawn_needs_an_absolute_daemon_path() {
    let Err(err) = Client::connect_or_spawn(Path::new("requiemd"), Role::Gui, WAIT) else {
        panic!("connected through a relative daemon path");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn goodbye_is_the_last_message() {
    let (_c, rx) = client_of(vec![ServerMsg::Event(Event::Volume(3)), ServerMsg::Goodbye]);
    assert_eq!(next_event(&rx), Event::Volume(3));
    wait_goodbye(&rx);
}

#[test]
fn dropped_connection_is_disconnected_without_goodbye() {
    let (_c, rx) = client_of(vec![ServerMsg::Event(Event::Volume(3))]);
    assert_eq!(next_event(&rx), Event::Volume(3));
    wait_disconnected(&rx);
}

#[test]
fn shutdown_says_goodbye_to_every_client_last() {
    let rig = rig();
    let (_gui, gui_rx) = rig.client(Role::Gui);
    let (_cli, cli_rx) = rig.client(Role::Cli);
    let raw = rig.raw(Role::Cli);
    rig.wait_clients(ClientCounts { gui: 1, cli: 2 });
    rig.server.shutdown();
    wait_goodbye(&gui_rx);
    wait_goodbye(&cli_rx);
    let mut last = None;
    while let Some(msg) = frame::read::<_, ServerMsg>(&mut &raw).unwrap() {
        last = Some(msg);
    }
    assert_eq!(last, Some(ServerMsg::Goodbye));
}

/// Over socketpairs, so it runs where binding a socket is not allowed.
#[test]
fn stop_says_goodbye_without_waiting_on_a_stuck_client() {
    let session = Session::spawn(
        vec![FakeProvider::new("fake")],
        FakeEngineHandle::new().factory(),
        SessionConfig::default(),
    );
    let hooks = ServerHooks {
        on_shutdown: Box::new(|| {}),
        on_update: Box::new(|_| {}),
    };
    let (hub, hub_thread) = hub::spawn(session, hooks, idle_update()).unwrap();
    let (sub_tx, sub) = unbounded();
    hub.send(HubMsg::Subscribe(sub_tx)).unwrap();

    let (ours, theirs) = UnixStream::pair().unwrap();
    server::admit(1, theirs, &hub);
    let (_reader, rx) = Client::handshake(ours, Role::Gui).unwrap();
    expect_update(&rx, &idle_update());
    let (stuck, theirs) = UnixStream::pair().unwrap();
    server::admit(2, theirs, &hub);
    stuck.set_read_timeout(Some(WAIT)).unwrap();
    hello(&stuck, PROTOCOL, Role::Cli);
    // Welcome, the update status and the snapshot.
    for _ in 0..IDLE_SNAPSHOT + 2 {
        read_msg(&stuck);
    }

    // One frame far larger than a socket buffer blocks the writer of a peer that stops
    // reading.
    let filler: Vec<Track> = (0..3000)
        .map(|i| Track {
            title: "x".repeat(400),
            ..track(&format!("f{i}"))
        })
        .collect();
    hub.send(HubMsg::Command {
        from: None,
        cmd: Command::Enqueue {
            tracks: filler,
            mode: EnqueueMode::Append,
        },
    })
    .unwrap();
    let is_filler =
        |e: &Event| matches!(e, Event::QueueChanged { tracks, .. } if tracks.len() == 3000);
    wait_sub(&sub, is_filler);
    take(&rx, IDLE_SNAPSHOT);
    assert!(is_filler(&next_event(&rx)));

    let started = Instant::now();
    hub.send(HubMsg::Stop).unwrap();
    hub_thread.join().unwrap();
    assert!(
        started.elapsed() < SECS(2),
        "stop took {:?}",
        started.elapsed()
    );
    wait_goodbye(&rx);
    // `stuck` gets what fit in its socket buffer, then the connection ends mid-frame.
    loop {
        match frame::read::<_, ServerMsg>(&mut &stuck) {
            Ok(Some(msg)) => assert_ne!(msg, ServerMsg::Goodbye),
            Ok(None) => break,
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
                break;
            }
        }
    }
}

#[test]
fn send_to_a_daemon_that_stopped_reading_times_out_and_disconnects() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let daemon = thread::spawn(move || {
        let _: ClientMsg = frame::read(&mut &theirs).unwrap().unwrap();
        let welcome = ServerMsg::Welcome {
            protocol: PROTOCOL,
            version: "test".into(),
        };
        frame::write(&mut &theirs, &welcome).unwrap();
        theirs
    });
    let (client, rx) = Client::handshake(ours, Role::Gui).unwrap();
    // Held open and never read from again.
    let _theirs = daemon.join().unwrap();

    // A clone stays alive, so `Client::drop` can't be what closes the connection.
    let client = Arc::new(client);
    let sender = client.clone();
    let (done_tx, done) = unbounded();
    thread::spawn(move || {
        let cmd = Command::Enqueue {
            tracks: (0..50)
                .map(|i| Track {
                    title: "x".repeat(400),
                    ..track(&format!("t{i}"))
                })
                .collect(),
            mode: EnqueueMode::Append,
        };
        let err = loop {
            if let Err(e) = sender.send(cmd.clone()) {
                break e;
            }
        };
        let _ = done_tx.send(err);
    });
    // A write that gets part of the frame out before timing out succeeds, so the send
    // can take two timeouts.
    let err = done.recv_timeout(2 * WAIT).expect("send still blocked");
    assert!(
        matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
        "{err}"
    );
    wait_disconnected(&rx);
}

#[test]
fn update_status_follows_welcome_and_every_change() {
    let rig = rig();
    let (_c, rx) = rig.client(Role::Gui);
    take(&rx, IDLE_SNAPSHOT);
    let ready = UpdateStatus {
        state: UpdateState::Ready {
            version: "0.2.0".into(),
        },
        ..idle_update()
    };
    rig.server.set_update(ready.clone());
    expect_update(&rx, &ready);

    let s = UnixStream::connect(&rig.path).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    hello(&s, PROTOCOL, Role::Cli);
    assert!(matches!(read_msg(&s), ServerMsg::Welcome { .. }));
    assert_eq!(read_msg(&s), ServerMsg::Update(ready));
    assert!(matches!(
        read_msg(&s),
        ServerMsg::Event(Event::QueueChanged { .. })
    ));
}

#[test]
fn update_requests_reach_the_hook() {
    let rig = rig();
    let (c, _rx) = rig.client(Role::Cli);
    c.check_for_updates().unwrap();
    c.set_auto_update(false).unwrap();
    c.install_update().unwrap();
    let got: Vec<_> = (0..3)
        .map(|_| rig.requests.recv_timeout(WAIT).unwrap())
        .collect();
    assert_eq!(
        got,
        [
            UpdateRequest::Check,
            UpdateRequest::SetAuto(false),
            UpdateRequest::Install
        ]
    );
}

#[test]
fn client_knows_the_daemons_version_and_pid() {
    let rig = rig();
    let (c, _rx) = rig.client(Role::Cli);
    assert_eq!(c.server_version(), env!("CARGO_PKG_VERSION"));
    assert_eq!(c.peer_pid().unwrap(), process::id() as i32);
}

#[test]
fn shutdown_keeping_lock_holds_off_another_server_until_dropped() {
    let rig = rig();
    let (_c, rx) = rig.client(Role::Cli);
    let lock = rig.server.shutdown_keeping_lock();
    wait_goodbye(&rx);
    assert!(!rig.path.exists());
    let again = || {
        start(
            &rig.dir,
            &FakeProvider::new("fake"),
            &FakeEngineHandle::new(),
        )
    };
    assert!(matches!(again(), Err(ServeError::AlreadyRunning)));
    drop(lock);
    let (server, ..) = again().unwrap();
    server.shutdown();
}

#[test]
fn an_instance_holds_off_another_daemon_before_it_listens() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("s");
    prepare_dir(&dir).unwrap();
    let instance = Instance::lock_in(&dir).unwrap();
    assert!(!dir.join(SOCKET_NAME).exists());
    assert!(matches!(
        Instance::lock_in(&dir),
        Err(ServeError::AlreadyRunning)
    ));
    drop(instance);
    let (server, ..) = start(&dir, &FakeProvider::new("fake"), &FakeEngineHandle::new()).unwrap();
    server.shutdown();
}

#[test]
fn the_instance_lock_is_closed_on_exec() {
    let rig = rig();
    let lock = rig.server.shutdown_keeping_lock();
    // SAFETY: F_GETFD only reads the flags of a descriptor `lock` owns.
    let flags = unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "an exec'd daemon can't lock again"
    );
}
