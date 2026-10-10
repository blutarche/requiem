use std::io::{self, Cursor, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use requiem_cli::app::{self, App, Flow};
use requiem_core::testing::{EngineCall, FakeEngineHandle, FakeProvider, Gate, Step};
use requiem_core::{EngineEvent, ProviderId, SeekTarget, Session, SessionConfig, Track, TrackId};

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
        text.contains("  1. A, B — Title t1 [Album] (2:05)\n"),
        "{text}"
    );
    assert!(
        text.contains("  2. A, B — Title t2 [Album] (2:05)\n"),
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
