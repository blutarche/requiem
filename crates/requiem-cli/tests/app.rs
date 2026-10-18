use std::io::{self, Cursor, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use requiem_cli::app::{self, App, Flow, Format};
use requiem_core::testing::{EngineCall, FakeEngineHandle, FakeProvider, Gate, SignInScript, Step};
use requiem_core::{
    AccountState, Collection, CollectionId, CollectionKind, EngineEvent, Item, Page, ProviderError,
    ProviderId, SeekTarget, Session, SessionConfig, Shelf, Track, TrackId,
};

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

struct Rig {
    session: Session,
    provider: Arc<FakeProvider>,
    engine: FakeEngineHandle,
    out: Buf,
}

fn rig() -> Rig {
    let provider = FakeProvider::new("fake");
    provider.set_has_home(true);
    rig_with(provider)
}

/// For a provider whose starting state the test sets before the session reads it.
fn rig_with(provider: Arc<FakeProvider>) -> Rig {
    let engine = FakeEngineHandle::new();
    let session = Session::spawn(
        vec![provider.clone()],
        engine.factory(),
        SessionConfig::default(),
    );
    Rig {
        session,
        provider,
        engine,
        out: Buf::default(),
    }
}

fn track(id: &str) -> Track {
    Track {
        id: TrackId {
            provider: ProviderId("fake".into()),
            id: id.into(),
        },
        title: format!("Title {id}"),
        artists: vec!["A".into(), "B".into()],
        album: Some("Album".into()),
        duration: Some(Duration::from_secs(125)),
        artwork_url: None,
    }
}

impl Rig {
    fn app(&self, script: bool) -> App<'_> {
        App::new(&self.session, self.out.clone(), script)
    }
}

impl Rig {
    fn json_app(&self) -> App<'_> {
        App::with_format(&self.session, self.out.clone(), true, Format::Json)
    }

    fn json_lines(&self) -> Vec<serde_json::Value> {
        self.out
            .text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l}: {e}")))
            .collect()
    }
}

fn of_type<'a>(lines: &'a [serde_json::Value], ty: &str) -> Vec<&'a serde_json::Value> {
    lines.iter().filter(|l| l["type"] == ty).collect()
}

fn exec_all(app: &App<'_>, lines: &str) -> Flow {
    for l in lines.lines() {
        match app.exec(l) {
            Flow::Continue => {}
            other => return other,
        }
    }
    Flow::Continue
}

fn gen_of_first_load(r: &Rig) -> u64 {
    r.engine.wait_load(1);
    match &r.engine.calls()[..] {
        [EngineCall::Load { r#gen, .. }, ..] => *r#gen,
        c => panic!("unexpected calls {c:?}"),
    }
}

#[test]
fn search_prints_numbered_results() {
    let r = rig();
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1"), track("t2")])]);
    let app = r.app(true);
    exec_all(&app, "search q\nawait results 5");
    let text = r.out.text();
    assert!(
        text.contains("  1. A, B — Title t1 [Album] (2:05) · fake\n"),
        "{text}"
    );
    assert!(
        text.contains("  2. A, B — Title t2 [Album] (2:05) · fake\n"),
        "{text}"
    );
}

#[test]
fn stale_search_results_are_ignored() {
    let r = rig();
    let gate = Gate::new();
    r.provider
        .script_search("old", vec![Step::ok(vec![track("old1")]).gated(&gate)]);
    r.provider
        .script_search("new", vec![Step::ok(vec![track("new1")])]);
    let app = r.app(true);
    exec_all(&app, "search old\nsearch new\nawait results 5");
    gate.release();
    thread::sleep(Duration::from_millis(200));
    exec_all(&app, "play 1");
    let text = r.out.text();
    assert!(!text.contains("old1"), "{text}");
    assert_eq!(r.engine.wait_load(1).1, "new1");
}

#[test]
fn play_and_add_rejected_before_results_and_out_of_range() {
    let r = rig();
    let gate = Gate::new();
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1")]).gated(&gate)]);
    let app = r.app(true);
    exec_all(&app, "play 1\nadd 1\nsearch q\nplay 1");
    gate.release();
    exec_all(&app, "await results 5\nplay 2\nadd 0\nadd x");
    let text = r.out.text();
    assert_eq!(
        text.matches("error: no search results yet").count(),
        3,
        "{text}"
    );
    assert!(text.contains("error: no result 2\n"), "{text}");
    assert!(text.contains("error: no result 0\n"), "{text}");
    assert!(text.contains("error: no result x\n"), "{text}");
    assert!(r.engine.calls().is_empty());
}

#[test]
fn play_sends_exactly_the_chosen_result() {
    let r = rig();
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1"), track("t2")])]);
    let app = r.app(true);
    exec_all(&app, "search q\nawait results 5\nplay 2");
    assert_eq!(r.engine.wait_load(1).1, "t2");
    r.engine.emit(EngineEvent::Started {
        r#gen: gen_of_first_load(&r),
        duration: None,
    });
    exec_all(&app, "await nowplaying 5\nqueue");
    let text = r.out.text();
    assert!(text.contains("queue: 1 tracks, current 1"), "{text}");
    assert!(text.contains("> 1. A, B — Title t2"), "{text}");
}

#[test]
fn seek_arguments_map_to_targets() {
    assert_eq!(app::parse_seek("+10"), Some(SeekTarget::Relative(10)));
    assert_eq!(app::parse_seek("-10"), Some(SeekTarget::Relative(-10)));
    assert_eq!(
        app::parse_seek("30"),
        Some(SeekTarget::Absolute(Duration::from_secs(30)))
    );
    assert_eq!(app::parse_seek("x"), None);
    assert_eq!(app::parse_seek(""), None);
}

#[test]
fn volume_out_of_range_is_rejected() {
    let r = rig();
    let app = r.app(true);
    exec_all(&app, "vol 150\nvol -1\nvol 50\nsleep 0.2");
    let text = r.out.text();
    assert_eq!(
        text.matches("error: usage: vol <0-100>").count(),
        2,
        "{text}"
    );
    assert!(text.contains("volume: 50\n"), "{text}");
    assert!(!text.contains("volume: 100"), "{text}");
}

#[test]
fn await_playing_returns_after_started() {
    let r = rig();
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1")])]);
    let app = r.app(true);
    exec_all(&app, "search q\nawait results 5\nplay 1");
    let g = gen_of_first_load(&r);
    let engine = r.engine.clone();
    let emitter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        engine.emit(EngineEvent::Started {
            r#gen: g,
            duration: Some(Duration::from_secs(125)),
        });
    });
    assert_eq!(exec_all(&app, "await playing 5\nstatus"), Flow::Continue);
    emitter.join().unwrap();
    let text = r.out.text();
    assert!(text.contains("state: playing\n"), "{text}");
    assert!(
        text.contains("status: playing | A, B — Title t1 | 0:00 / 2:05 | volume 100"),
        "{text}"
    );
}

#[test]
fn await_timeout_exits_2_in_script_mode_only() {
    let r = rig();
    assert_eq!(
        exec_all(&r.app(true), "await playing 0.1\nquit"),
        Flow::Exit(2)
    );
    assert!(
        r.out
            .text()
            .contains("error: timed out waiting for playing\n")
    );
    assert_eq!(
        exec_all(&r.app(false), "await playing 0.1\nquit"),
        Flow::Quit
    );
}

#[test]
fn mark_prints_unix_millis() {
    let r = rig();
    exec_all(&r.app(true), "mark start");
    let text = r.out.text();
    let ms: u128 = text
        .strip_prefix("MARK start ")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("bad mark line {text:?}"));
    assert!(ms > 1_700_000_000_000);
}

#[test]
fn unknown_command_exits_2_in_script_mode_only() {
    let r = rig();
    assert_eq!(
        exec_all(&r.app(true), "# c\n\n  frobnicate \nquit"),
        Flow::Exit(2)
    );
    assert!(r.out.text().contains("error: unknown command frobnicate\n"));
    assert_eq!(exec_all(&r.app(false), "frobnicate\nquit"), Flow::Quit);
}

#[test]
fn run_returns_zero_on_eof() {
    let r = rig();
    let code = app::run(&r.session, Cursor::new("help\n"), r.out.clone(), true).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn shutdown_daemon_needs_a_connection() {
    let r = rig();
    let app = r.app(true);
    assert_eq!(app.exec("shutdown-daemon"), Flow::Continue);
    assert!(
        r.out.text().contains("error: not connected to a daemon\n"),
        "{}",
        r.out.text()
    );
}

struct SlowRemote;

impl app::Player for SlowRemote {
    fn send(&self, _: requiem_core::Command) {}

    fn subscribe(&self) -> crossbeam_channel::Receiver<requiem_core::Event> {
        let (tx, rx) = crossbeam_channel::unbounded();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            tx.send(requiem_core::Event::State(requiem_core::PlayState::Playing))
                .unwrap();
            tx.send(requiem_core::Event::Volume(5)).unwrap();
            thread::sleep(Duration::from_secs(5));
        });
        rx
    }

    fn remote(&self) -> bool {
        true
    }
}

#[test]
fn remote_run_waits_for_the_join_snapshot() {
    let out = Buf::default();
    let code = app::run(&SlowRemote, Cursor::new("status\n"), out.clone(), true).unwrap();
    let text = out.text();
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("status: playing") && text.contains("volume 5"),
        "{text}"
    );
}

fn started_queue(r: &Rig, app: &App<'_>) {
    r.provider.script_search(
        "q",
        vec![Step::ok(vec![track("t1"), track("t2"), track("t3")])],
    );
    exec_all(app, "search q\nawait results 5\nplay 1\nadd 2\nadd 3");
    wait_out(r, "queue: 3 tracks, current 1", 1);
}

/// Polls the output until `needle` has been printed `n` times.
fn wait_out(r: &Rig, needle: &str, n: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while r.out.text().matches(needle).count() < n {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {needle:?}: {}",
            r.out.text()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn remove_move_clear_map_one_based_positions() {
    let r = rig();
    let app = r.app(true);
    started_queue(&r, &app);
    r.engine.wait_load(1);
    exec_all(&app, "move 3 1");
    wait_out(&r, "queue: 3 tracks, current 2", 1);
    exec_all(&app, "queue");
    let text = r.out.text();
    assert!(
        text.contains("  1. A, B — Title t3 [Album] (2:05)\n"),
        "{text}"
    );
    assert!(
        text.contains("> 2. A, B — Title t1 [Album] (2:05)\n"),
        "{text}"
    );
    exec_all(&app, "remove 1");
    wait_out(&r, "queue: 2 tracks, current 1", 2);
    exec_all(&app, "clear");
    wait_out(&r, "queue: 0 tracks, current none", 1);
    exec_all(&app, "queue");
    assert!(r.out.text().contains("queue empty"), "{}", r.out.text());
}

#[test]
fn jump_plays_the_one_based_queue_position() {
    let r = rig();
    let app = r.app(true);
    started_queue(&r, &app);
    r.engine.wait_load(1);
    exec_all(&app, "jump 3\njump 4\njump x");
    assert_eq!(r.engine.wait_load(2).1, "t3");
    wait_out(&r, "queue: 3 tracks, current 3", 1);
    assert_eq!(
        r.out.text().matches("error: no queue position").count(),
        2,
        "{}",
        r.out.text()
    );
}

#[test]
fn remove_and_move_reject_bad_arguments() {
    let r = rig();
    let app = r.app(true);
    started_queue(&r, &app);
    r.engine.wait_load(1);
    let before = r.out.text();
    for l in [
        "remove 0", "remove 4", "remove x", "remove", "move 1 4", "move 0 1",
    ] {
        assert!(matches!(app.exec(l), Flow::Continue), "{l}");
    }
    assert!(matches!(app.exec("move 1"), Flow::Continue));
    assert!(matches!(app.exec("move 1 2 3"), Flow::Continue));
    let text = r.out.text()[before.len()..].to_owned();
    assert_eq!(
        text.matches("error: no queue position").count(),
        6,
        "{text}"
    );
    assert_eq!(
        text.matches("error: usage: move <from> <to>").count(),
        2,
        "{text}"
    );
    exec_all(&app, "queue");
    assert!(r.out.text().contains("queue: 3 tracks"), "{}", r.out.text());
}

#[test]
fn json_search_results_are_one_object_each() {
    let r = rig();
    let mut t2 = track("t2");
    t2.album = None;
    t2.duration = None;
    t2.artwork_url = Some("https://img/t2.jpg".into());
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1"), t2])]);
    let app = r.json_app();
    exec_all(&app, "search q\nawait results 5");
    let lines = r.json_lines();
    assert_eq!(of_type(&lines, "ack")[0]["command"], "search");
    let results = of_type(&lines, "result");
    assert_eq!(results.len(), 2);
    assert_eq!(
        *results[0],
        serde_json::json!({
            "type": "result",
            "index": 1,
            "id": {"provider": "fake", "id": "t1"},
            "title": "Title t1",
            "artists": ["A", "B"],
            "album": "Album",
            "duration_ms": 125000,
            "artwork_url": null,
        })
    );
    assert_eq!(results[1]["index"], 2);
    assert_eq!(results[1]["album"], serde_json::Value::Null);
    assert_eq!(results[1]["duration_ms"], serde_json::Value::Null);
    assert_eq!(results[1]["artwork_url"], "https://img/t2.jpg");
}

#[test]
fn json_status_and_errors() {
    let r = rig();
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1")])]);
    let app = r.json_app();
    exec_all(&app, "status\nplay 1\nsearch q\nawait results 5\nplay 1");
    r.engine.wait_load(1);
    r.engine.emit(EngineEvent::Started {
        r#gen: gen_of_first_load(&r),
        duration: Some(Duration::from_secs(125)),
    });
    exec_all(&app, "await playing 5\nstatus");
    let lines = r.json_lines();
    let status = of_type(&lines, "status");
    assert_eq!(status[0]["state"], "stopped");
    assert_eq!(status[0]["track"], serde_json::Value::Null);
    assert_eq!(status[1]["state"], "playing");
    assert_eq!(status[1]["track"]["title"], "Title t1");
    assert_eq!(status[1]["duration_ms"], 125000);
    assert_eq!(status[1]["volume"], 100);
    assert_eq!(
        *of_type(&lines, "error")[0],
        serde_json::json!({"type": "error", "message": "no search results yet"})
    );
}

#[test]
fn json_queue_marks_the_current_track() {
    let r = rig();
    r.provider
        .script_search("q", vec![Step::ok(vec![track("t1")])]);
    let app = r.json_app();
    exec_all(&app, "queue");
    assert_eq!(of_type(&r.json_lines(), "queue_empty").len(), 1);
    exec_all(&app, "search q\nawait results 5\nplay 1");
    r.engine.wait_load(1);
    r.engine.emit(EngineEvent::Started {
        r#gen: gen_of_first_load(&r),
        duration: None,
    });
    exec_all(&app, "await nowplaying 5\nqueue");
    let lines = r.json_lines();
    let changed = of_type(&lines, "queue_changed");
    assert_eq!(changed.last().unwrap()["count"], 1);
    assert_eq!(changed.last().unwrap()["current"], 1);
    let items = of_type(&lines, "queue_item");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["index"], 1);
    assert_eq!(items[0]["current"], true);
    assert_eq!(items[0]["title"], "Title t1");
    assert_eq!(of_type(&lines, "ack")[1]["command"], "play");
}

#[test]
fn play_track_enqueues_exactly_the_given_track() {
    let r = rig();
    let app = r.json_app();
    let json = r#"{"type":"result","index":7,"id":{"provider":"fake","id":"zz"},"title":"Picked","artists":["X"],"album":null,"duration_ms":1500,"artwork_url":"https://a/b.jpg"}"#;
    exec_all(&app, &format!("play-track {json}"));
    assert_eq!(r.engine.wait_load(1).1, "zz");
    r.engine.emit(EngineEvent::Started {
        r#gen: gen_of_first_load(&r),
        duration: None,
    });
    exec_all(&app, "await nowplaying 5\nqueue");
    let lines = r.json_lines();
    let items = of_type(&lines, "queue_item");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0]["id"],
        serde_json::json!({"provider": "fake", "id": "zz"})
    );
    assert_eq!(items[0]["title"], "Picked");
    assert_eq!(items[0]["duration_ms"], 1500);
    assert_eq!(items[0]["artwork_url"], "https://a/b.jpg");
    assert_eq!(of_type(&lines, "ack")[0]["command"], "play-track");
}

#[test]
fn add_track_appends_without_playing_in_text_mode() {
    let r = rig();
    let app = r.app(true);
    let json = r#"{"id":{"provider":"fake","id":"aa"},"title":"Added","artists":["X","Y"]}"#;
    exec_all(&app, &format!("add-track {json}\nadd-track {json}"));
    wait_out(&r, "queue: 2 tracks, current none", 1);
    exec_all(&app, "queue");
    assert!(
        r.out.text().contains("  2. X, Y — Added\n"),
        "{}",
        r.out.text()
    );
    assert!(r.engine.calls().is_empty());
}

#[test]
fn malformed_track_json_errors_and_leaves_the_queue_alone() {
    let r = rig();
    let app = r.json_app();
    for l in [
        "play-track",
        "play-track {not json",
        r#"add-track {"title":"no id","artists":[]}"#,
        r#"add-track {"id":{"provider":"fake","id":"a"},"title":"t","artists":[1]}"#,
    ] {
        assert!(matches!(app.exec(l), Flow::Continue), "{l}");
    }
    exec_all(&app, "queue");
    let lines = r.json_lines();
    assert_eq!(of_type(&lines, "error").len(), 4);
    assert_eq!(of_type(&lines, "queue_empty").len(), 1);
    assert!(of_type(&lines, "ack").is_empty());
    assert!(r.engine.calls().is_empty());
}

fn playlist(id: &str, title: &str) -> Collection {
    Collection {
        id: CollectionId {
            provider: ProviderId("fake".into()),
            id: id.into(),
        },
        kind: CollectionKind::Playlist,
        title: title.into(),
        subtitle: Some("by me".into()),
        artwork_url: Some("https://img/p.jpg".into()),
    }
}

/// Home: two tracks on one shelf and a playlist on the next; the playlist holds `t3`.
fn script_home(r: &Rig) {
    let list = playlist("p1", "Mix");
    r.provider.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            Shelf {
                title: "Quick picks".into(),
                items: vec![Item::Track(track("t1")), Item::Track(track("t2"))],
            },
            Shelf {
                title: "Empty".into(),
                items: vec![],
            },
            Shelf {
                title: "Playlists".into(),
                items: vec![Item::Collection(list.clone())],
            },
        ])],
    );
    r.provider.script_browse(
        Page::Collection(list.id),
        vec![Step::ok(vec![Shelf {
            title: "Mix".into(),
            items: vec![Item::Track(track("t3"))],
        }])],
    );
}

#[test]
fn text_mode_never_prints_terminal_escapes_from_remote_titles() {
    let r = rig();
    let mut t = track("t1");
    t.title = "a\x1b]0;pwned\x07b\x1b[2Jc\u{9b}31m\x7f\r".into();
    r.provider.script_browse(
        Page::Home,
        vec![Step::ok(vec![Shelf {
            title: "Sh\x1b[2Jelf".into(),
            items: vec![Item::Track(t)],
        }])],
    );
    let app = r.app(true);
    exec_all(&app, "home\nawait results 1");
    let text = r.out.text();
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
    assert!(text.contains("a\u{fffd}]0;pwned\u{fffd}b"), "{text:?}");
}

#[test]
fn home_numbers_items_across_shelves_and_open_lists_a_playlist() {
    let r = rig();
    script_home(&r);
    let app = r.app(true);
    exec_all(&app, "home\nawait results 5");
    let text = r.out.text();
    assert!(
        text.contains(
            "Quick picks:\n  1. A, B — Title t1 [Album] (2:05)\n  2. A, B — Title t2 [Album] (2:05)\n\
             Playlists:\n  3. [playlist] Mix — by me\n"
        ),
        "{text}"
    );
    assert!(!text.contains("Empty"), "{text}");

    exec_all(&app, "play 3\nopen 1\nopen 4\nplay 2");
    let text = r.out.text();
    assert!(
        text.contains("error: result 3 is a playlist; use open 3\n"),
        "{text}"
    );
    assert!(
        text.contains("error: result 1 is a track; use play 1\n"),
        "{text}"
    );
    assert!(text.contains("error: no result 4\n"), "{text}");
    assert_eq!(r.engine.wait_load(1).1, "t2");

    // `open` starts a new listing, numbered from 1.
    exec_all(&app, "open 3\nawait results 5\nadd 1\nplay 1");
    assert!(
        r.out
            .text()
            .contains("Mix:\n  1. A, B — Title t3 [Album] (2:05)\n"),
        "{}",
        r.out.text()
    );
    assert_eq!(r.engine.wait_load(2).1, "t3");
    assert_eq!(
        r.provider.browse_calls(),
        [Page::Home, Page::Collection(playlist("p1", "Mix").id)]
    );
}

#[test]
fn a_search_replaces_a_browse_listing() {
    let r = rig();
    script_home(&r);
    r.provider
        .script_search("q", vec![Step::ok(vec![track("s1")])]);
    let app = r.app(true);
    exec_all(
        &app,
        "home\nawait results 5\nsearch q\nopen 3\nawait results 5\nplay 1",
    );
    let text = r.out.text();
    // `open 3` ran before the search returned, against no listing.
    assert!(text.contains("error: no search results yet\n"), "{text}");
    assert_eq!(r.engine.wait_load(1).1, "s1");
}

#[test]
fn json_browse_prints_shelves_and_items() {
    let r = rig();
    script_home(&r);
    let app = r.json_app();
    exec_all(&app, "home\nawait results 5");
    let lines = r.json_lines();
    assert_eq!(of_type(&lines, "ack")[0]["command"], "home");
    let shelves: Vec<&serde_json::Value> = of_type(&lines, "shelf");
    assert_eq!(
        shelves,
        [
            &serde_json::json!({"type": "shelf", "title": "Quick picks"}),
            &serde_json::json!({"type": "shelf", "title": "Playlists"}),
        ]
    );
    let items = of_type(&lines, "item");
    assert_eq!(items.len(), 3);
    assert_eq!(
        *items[0],
        serde_json::json!({
            "type": "item",
            "index": 1,
            "kind": "track",
            "id": {"provider": "fake", "id": "t1"},
            "title": "Title t1",
            "subtitle": "A, B",
            "artists": ["A", "B"],
            "album": "Album",
            "duration_ms": 125000,
            "artwork_url": null,
        })
    );
    assert_eq!(
        *items[2],
        serde_json::json!({
            "type": "item",
            "index": 3,
            "kind": "playlist",
            "id": {"provider": "fake", "id": "p1"},
            "title": "Mix",
            "subtitle": "by me",
            "artwork_url": "https://img/p.jpg",
        })
    );
    // A track item line plays as is.
    exec_all(&app, &format!("play-track {}", items[1]));
    assert_eq!(r.engine.wait_load(1).1, "t2");
}

#[test]
fn library_that_needs_sign_in_says_so() {
    let provider = FakeProvider::new("fake");
    provider.set_account(AccountState::SignedIn { name: None });
    let r = rig_with(provider);
    r.provider.script_browse(
        Page::Library,
        vec![Step::err(ProviderError::SignInRequired(
            "sign in to Google to see your library".into(),
        ))],
    );
    let app = r.json_app();
    exec_all(&app, "library");
    wait_out(&r, "browse_failed", 1);
    let lines = r.json_lines();
    assert_eq!(
        *of_type(&lines, "browse_failed")[0],
        serde_json::json!({
            "type": "browse_failed",
            "error": "sign-in required: sign in to Google to see your library",
            "sign_in_required": true,
        })
    );
}

fn pending() -> AccountState {
    AccountState::Pending {
        verification_url: "https://www.google.com/device".into(),
        user_code: "GQVQ-JKEC".into(),
        expires_at_unix: 4_000_000_000,
    }
}

#[test]
fn signin_prints_the_code_and_waits_for_the_outcome() {
    let r = rig();
    let gate = Gate::new();
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: Some(gate.clone()),
        outcome: Ok(AccountState::SignedIn {
            name: Some("Me".into()),
        }),
    });
    let flow = thread::scope(|s| {
        let waiter = s.spawn(|| r.app(true).exec("signin"));
        wait_out(
            &r,
            "Go to https://www.google.com/device and enter GQVQ-JKEC\n",
            1,
        );
        assert!(!waiter.is_finished());
        gate.release();
        waiter.join().unwrap()
    });
    assert_eq!(flow, Flow::Continue);
    let text = r.out.text();
    assert!(
        text.ends_with(
            "Go to https://www.google.com/device and enter GQVQ-JKEC\nsigned in as Me\n"
        ),
        "{text}"
    );
    let app = r.app(true);
    exec_all(&app, "account");
    assert!(r.out.text().ends_with("fake: signed in as Me\n"));

    // Signing in again only repeats the state.
    assert_eq!(app.exec("signin"), Flow::Continue);
    assert!(r.out.text().ends_with("signed in as Me\n"));
    assert_eq!(r.provider.sign_in_calls(), 1);
}

#[test]
fn a_failed_signin_says_why_and_exits_2_in_script_mode() {
    let r = rig();
    for _ in 0..2 {
        r.provider.script_sign_in(SignInScript {
            pending: Some(pending()),
            gate: None,
            outcome: Err(ProviderError::SignInRequired("access was denied".into())),
        });
    }
    assert_eq!(r.app(true).exec("signin"), Flow::Exit(2));
    let text = r.out.text();
    assert!(
        text.contains("account error: access was denied\n"),
        "{text}"
    );
    assert!(text.ends_with("signed out\n"), "{text}");
    assert_eq!(r.app(false).exec("signin"), Flow::Continue);
}

#[test]
fn signin_without_a_client_prints_the_hint() {
    let provider = FakeProvider::new("fake");
    let hint = "no Google OAuth client: save the JSON of a \"TVs and Limited Input devices\" \
                client to /cfg/requiem/google-client.json";
    provider.set_account(AccountState::Unavailable(hint.into()));
    provider.script_sign_in(SignInScript {
        pending: None,
        gate: None,
        outcome: Err(ProviderError::SignInRequired(hint.into())),
    });
    let r = rig_with(provider);
    let app = r.json_app();
    exec_all(&app, "account");
    assert_eq!(app.exec("signin"), Flow::Exit(2));
    let lines = r.json_lines();
    assert_eq!(
        *of_type(&lines, "account")[0],
        serde_json::json!({
            "type": "account",
            "provider": "fake",
            "state": "unavailable",
            "detail": hint,
        })
    );
    assert_eq!(
        *of_type(&lines, "account_error")[0],
        serde_json::json!({"type": "account_error", "provider": "fake", "error": hint})
    );
}

#[test]
fn json_signin_and_signout_report_the_account() {
    let r = rig();
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: None,
        outcome: Ok(AccountState::SignedIn { name: None }),
    });
    let app = r.json_app();
    assert_eq!(app.exec("signin"), Flow::Continue);
    exec_all(&app, "signout");
    wait_out(&r, "signed_out", 1);
    let lines = r.json_lines();
    let accounts = of_type(&lines, "account");
    assert_eq!(
        *accounts[0],
        serde_json::json!({
            "type": "account",
            "provider": "fake",
            "state": "pending",
            "verification_url": "https://www.google.com/device",
            "user_code": "GQVQ-JKEC",
            "expires_at_unix": 4_000_000_000u64,
        })
    );
    assert_eq!(
        *accounts[1],
        serde_json::json!({"type": "account", "provider": "fake", "state": "signed_in", "name": null})
    );
    assert_eq!(accounts[2]["state"], "signed_out");
    assert_eq!(r.provider.sign_out_calls(), 1);
}

#[test]
fn a_signin_after_a_signout_starts_from_signed_out() {
    let provider = FakeProvider::new("fake");
    provider.set_account(AccountState::SignedIn {
        name: Some("Me".into()),
    });
    let r = rig_with(provider);
    let gate = Gate::new();
    r.provider.gate_sign_out(gate.clone());
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: None,
        outcome: Ok(AccountState::SignedIn {
            name: Some("New".into()),
        }),
    });
    let flow = thread::scope(|s| {
        let waiter = s.spawn(|| exec_all(&r.app(true), "signout\nsignin"));
        r.provider.wait_sign_out_calls(1);
        thread::sleep(Duration::from_millis(200));
        // A `signin` sent now would find the account still signed in and end on that.
        assert!(!waiter.is_finished(), "{}", r.out.text());
        assert_eq!(r.provider.sign_in_calls(), 0);
        gate.release();
        waiter.join().unwrap()
    });
    assert_eq!(flow, Flow::Continue);
    assert_eq!(r.provider.sign_in_calls(), 1);
    let text = r.out.text();
    assert!(
        text.ends_with(
            "signed out\nGo to https://www.google.com/device and enter GQVQ-JKEC\nsigned in as New\n"
        ),
        "{text}"
    );
}

#[test]
fn a_signin_naming_its_provider_waits_past_the_announced_state() {
    // The session announces `SignedOut` on subscribe, which a `signin` that came first would
    // take for its outcome. It depends on thread timing, so it is tried a number of times.
    for _ in 0..50 {
        let r = rig();
        r.provider.script_sign_in(SignInScript {
            pending: Some(pending()),
            gate: None,
            outcome: Ok(AccountState::SignedIn { name: None }),
        });
        assert_eq!(
            r.app(true).exec("signin fake"),
            Flow::Continue,
            "{}",
            r.out.text()
        );
        assert_eq!(r.provider.sign_in_calls(), 1);
    }
}
