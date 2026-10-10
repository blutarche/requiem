use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::testing::{EngineCall, FakeEngineHandle, FakeProvider, Gate, Step};
use crate::*;

const SECS: fn(u64) -> Duration = Duration::from_secs;

struct Rig {
    session: Session,
    provider: Arc<FakeProvider>,
    engine: FakeEngineHandle,
    rx: Receiver<Event>,
}

fn config() -> SessionConfig {
    SessionConfig { resolve_retry_delay: Duration::from_millis(5), ..SessionConfig::default() }
}

fn rig(config: SessionConfig) -> Rig {
    let provider = FakeProvider::new("fake");
    let engine = FakeEngineHandle::new();
    let session = Session::spawn(vec![provider.clone()], engine.factory(), config);
    let rx = session.subscribe();
    Rig { session, provider, engine, rx }
}

fn track(id: &str) -> Track {
    Track {
        id: TrackId { provider: ProviderId("fake".into()), id: id.into() },
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

impl Rig {
    fn send(&self, cmd: Command) {
        self.session.send(cmd);
    }

    fn enqueue(&self, ids: &[&str]) {
        self.send(Command::Enqueue { tracks: tracks(ids), mode: EnqueueMode::Replace });
    }

    /// Reads events until `pred` matches one, returning it.
    fn wait_event(&self, pred: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + SECS(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(ev) if pred(&ev) => return ev,
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for event"),
            }
        }
    }

    /// Plays `ids` from index 0 and reports the first track as started.
    fn play_started(&self, ids: &[&str]) -> u64 {
        self.enqueue(ids);
        self.send(Command::PlayIndex(0));
        let (r#gen, _) = self.engine.wait_load(1);
        self.start(r#gen, None);
        r#gen
    }

    fn start(&self, r#gen: u64, duration: Option<Duration>) {
        self.engine.emit(EngineEvent::Started { r#gen, duration });
        self.wait_event(|e| matches!(e, Event::State(PlayState::Playing)));
    }

    /// Round-trips the inbox so earlier messages have been handled.
    fn barrier(&self) {
        self.send(Command::SetVolume(100));
        self.wait_event(|e| matches!(e, Event::Volume(_)));
    }

    /// Lets detached workers finish and post their results, then drains the inbox.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(50));
        self.barrier();
    }

    fn loads(&self) -> Vec<String> {
        self.engine.calls().iter().filter_map(EngineCall::load_payload).collect()
    }
}

fn is_failed(id: &'static str) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::TrackFailed { track, .. } if track.id.id == id)
}

#[test]
fn replace_play_start_then_preresolves_next() {
    let r = rig(config());
    r.enqueue(&["a", "b"]);
    r.wait_event(|e| matches!(e, Event::QueueChanged { current: None, .. }));
    r.send(Command::PlayIndex(0));
    let (r#gen, payload) = r.engine.wait_load(1);
    assert_eq!(payload, "a");
    assert_eq!(r.provider.resolve_calls("b"), 0);

    r.engine.emit(EngineEvent::Started { r#gen, duration: Some(SECS(200)) });
    let ev = r.wait_event(|e| matches!(e, Event::NowPlaying { .. }));
    assert_eq!(ev, Event::NowPlaying { track: track("a"), duration: Some(SECS(200)) });
    r.wait_event(|e| matches!(e, Event::State(PlayState::Playing)));
    r.provider.wait_resolve_calls("b", 1);
}

#[test]
fn ended_advances_using_preresolved_source() {
    let r = rig(config());
    let r#gen = r.play_started(&["a", "b"]);
    r.provider.wait_resolve_calls("b", 1);
    r.engine.emit(EngineEvent::Ended { r#gen });
    let (gen2, payload) = r.engine.wait_load(2);
    assert_eq!(payload, "b");
    assert!(gen2 > r#gen);
    assert_eq!(r.provider.resolve_calls("b"), 1);
}

#[test]
fn late_start_resolve_after_next_is_dropped() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_resolve("a", vec![Step::ok(()).gated(&gate)]);
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.provider.wait_resolve_calls("a", 1);
    r.send(Command::Next);
    let (_, payload) = r.engine.wait_load(1);
    assert_eq!(payload, "b");

    gate.release();
    r.settle();
    assert_eq!(r.loads(), ["b"]);
}

#[test]
fn late_preresolve_after_replace_is_dropped() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_resolve("b", vec![Step::ok(()).gated(&gate)]);
    r.play_started(&["a", "b"]);
    r.provider.wait_resolve_calls("b", 1);

    // Same track id at the same index, but in a newer queue.
    r.enqueue(&["c", "b"]);
    gate.release();
    r.settle();
    r.send(Command::PlayIndex(1));
    let (_, payload) = r.engine.wait_load(2);
    assert_eq!(payload, "b");
    assert_eq!(r.provider.resolve_calls("b"), 2);
}

#[test]
fn old_gen_ended_does_not_advance() {
    let r = rig(config());
    let gen1 = r.play_started(&["a", "b", "c"]);
    r.send(Command::Next);
    r.engine.wait_load(2);
    r.engine.emit(EngineEvent::Ended { r#gen: gen1 });
    r.settle();
    assert_eq!(r.loads(), ["a", "b"]);
}

#[test]
fn prev_rules() {
    let r = rig(config());
    r.enqueue(&["a", "b", "c"]);
    r.send(Command::PlayIndex(1));
    let (r#gen, _) = r.engine.wait_load(1);
    r.start(r#gen, None);

    r.engine.emit(EngineEvent::Position { r#gen, at: SECS(5) });
    r.wait_event(|e| matches!(e, Event::Position(_)));
    r.send(Command::Prev);
    r.engine.wait_calls(|c| c.contains(&EngineCall::Seek(Duration::ZERO)));
    assert_eq!(r.loads(), ["b"]);

    r.engine.emit(EngineEvent::Position { r#gen, at: SECS(2) });
    r.wait_event(|e| matches!(e, Event::Position(d) if *d == SECS(2)));
    r.send(Command::Prev);
    let (gen2, payload) = r.engine.wait_load(2);
    assert_eq!(payload, "a");
    r.start(gen2, None);

    let seeks = |r: &Rig| {
        r.engine.calls().iter().filter(|c| matches!(c, EngineCall::Seek(_))).count()
    };
    assert_eq!(seeks(&r), 1);
    r.send(Command::Prev);
    r.engine.wait_calls(|c| c.iter().filter(|c| matches!(c, EngineCall::Seek(_))).count() == 2);
    assert_eq!(r.loads(), ["b", "a"]);
}

#[test]
fn next_past_end_stops() {
    let r = rig(config());
    r.play_started(&["a"]);
    r.send(Command::Next);
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    assert!(r.engine.calls().contains(&EngineCall::Stop));
}

#[test]
fn unplayable_fails_and_skips() {
    let r = rig(config());
    r.provider.script_resolve("a", vec![Step::err(ProviderError::Unplayable("gone".into()))]);
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.wait_event(is_failed("a"));
    assert_eq!(r.engine.wait_load(1).1, "b");
}

#[test]
fn client_broken_reports_and_stops() {
    let r = rig(config());
    r.provider.script_resolve("a", vec![Step::err(ProviderError::ClientBroken("403".into()))]);
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    let ev = r.wait_event(|e| matches!(e, Event::ProviderBroken { .. }));
    assert!(matches!(ev, Event::ProviderBroken { provider, .. } if provider.0 == "fake"));
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    r.settle();
    assert!(r.loads().is_empty());
}

#[test]
fn parse_twice_in_a_row_is_provider_broken() {
    let r = rig(config());
    let parse = || vec![Step::err(ProviderError::Parse("shape".into()))];
    r.provider.script_resolve("a", parse());
    r.provider.script_resolve("b", parse());
    r.enqueue(&["a", "b", "c"]);
    r.send(Command::PlayIndex(0));
    r.wait_event(is_failed("a"));
    r.wait_event(|e| matches!(e, Event::ProviderBroken { .. }));
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    r.settle();
    assert!(r.loads().is_empty());
}

#[test]
fn parse_once_then_success_continues() {
    let r = rig(config());
    r.provider.script_resolve("a", vec![Step::err(ProviderError::Parse("odd".into()))]);
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.wait_event(is_failed("a"));
    let (r#gen, payload) = r.engine.wait_load(1);
    assert_eq!(payload, "b");
    r.start(r#gen, None);
    while let Ok(ev) = r.rx.try_recv() {
        assert!(!matches!(ev, Event::ProviderBroken { .. }));
    }
}

#[test]
fn network_error_retries_once_then_fails() {
    let r = rig(config());
    let net = || Step::err(ProviderError::Network("timeout".into()));
    r.provider.script_resolve("a", vec![net(), net(), net()]);
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.wait_event(is_failed("a"));
    assert_eq!(r.engine.wait_load(1).1, "b");
    assert_eq!(r.provider.resolve_calls("a"), 2);
}

fn fail_three(kinds: [FailureKind; 3]) -> Vec<Event> {
    let r = rig(config());
    r.enqueue(&["a", "b", "c", "d", "e"]);
    r.send(Command::PlayIndex(0));
    for (i, kind) in kinds.into_iter().enumerate() {
        let (r#gen, _) = r.engine.wait_load(i + 1);
        r.engine.emit(EngineEvent::Failed { r#gen, kind, detail: "x".into() });
    }
    let mut seen = Vec::new();
    loop {
        let ev = r.wait_event(|_| true);
        let done = ev == Event::State(PlayState::Stopped);
        seen.push(ev);
        if done {
            break;
        }
    }
    assert!(r.engine.calls().contains(&EngineCall::Stop));
    assert_eq!(r.loads().len(), 3);
    seen
}

#[test]
fn three_engine_failures_stop() {
    use FailureKind::*;
    let seen = fail_three([Decode, Decode, Stream]);
    assert!(!seen.iter().any(|e| matches!(e, Event::ProviderBroken { .. })));
}

#[test]
fn stream_failures_report_provider_broken() {
    use FailureKind::*;
    let seen = fail_three([Stream, Decode, Stream]);
    assert!(seen.iter().any(|e| matches!(e, Event::ProviderBroken { .. })));
}

#[test]
fn failed_preresolve_is_not_a_strike() {
    let r = rig(SessionConfig { max_consecutive_failures: 2, ..config() });
    r.provider.script_resolve("b", vec![Step::err(ProviderError::Unplayable("x".into()))]);
    let r#gen = r.play_started(&["a", "b", "c"]);
    r.provider.wait_resolve_calls("b", 1);

    r.engine.emit(EngineEvent::Failed { r#gen, kind: FailureKind::Decode, detail: "x".into() });
    r.wait_event(is_failed("a"));
    let (gen_b, payload) = r.engine.wait_load(2);
    assert_eq!(payload, "b");
    assert_eq!(r.provider.resolve_calls("b"), 2);

    r.engine.emit(EngineEvent::Failed { r#gen: gen_b, kind: FailureKind::Decode, detail: "x".into() });
    r.wait_event(is_failed("b"));
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    assert_eq!(r.loads(), ["a", "b"]);
}

#[test]
fn searches_answered_out_of_order() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_search("slow", vec![Step::ok(tracks(&["s"])).gated(&gate)]);
    r.provider.script_search("fast", vec![Step::ok(tracks(&["f"]))]);
    r.send(Command::Search { req: 1, query: "slow".into() });
    r.provider.wait_search_calls(1);
    r.send(Command::Search { req: 2, query: "fast".into() });

    let ev = r.wait_event(|e| matches!(e, Event::SearchResults { .. }));
    assert_eq!(ev, Event::SearchResults { req: 2, tracks: tracks(&["f"]) });
    gate.release();
    let ev = r.wait_event(|e| matches!(e, Event::SearchResults { .. }));
    assert_eq!(ev, Event::SearchResults { req: 1, tracks: tracks(&["s"]) });
}

#[test]
fn transport_commands_map_to_engine_and_events() {
    let r = rig(config());
    r.enqueue(&["a"]);
    r.send(Command::PlayIndex(0));
    let (r#gen, _) = r.engine.wait_load(1);
    r.start(r#gen, Some(SECS(100)));

    let state = |r: &Rig, s| r.wait_event(|e| *e == Event::State(s));
    r.send(Command::Pause);
    state(&r, PlayState::Paused);
    r.send(Command::TogglePause);
    state(&r, PlayState::Playing);
    r.send(Command::TogglePause);
    state(&r, PlayState::Paused);
    r.send(Command::Resume);
    state(&r, PlayState::Playing);

    r.engine.emit(EngineEvent::Position { r#gen, at: SECS(10) });
    r.wait_event(|e| matches!(e, Event::Position(_)));
    r.send(Command::Seek(SeekTarget::Relative(30)));
    r.send(Command::Seek(SeekTarget::Relative(-100)));
    r.send(Command::Seek(SeekTarget::Relative(1000)));
    r.send(Command::Seek(SeekTarget::Absolute(SECS(7))));
    r.send(Command::SetVolume(150));
    r.send(Command::SetVolume(50));
    r.wait_event(|e| *e == Event::Volume(50));

    let calls = r.engine.calls();
    let tail: Vec<_> = calls[1..].to_vec();
    assert_eq!(
        tail,
        [
            EngineCall::Pause,
            EngineCall::Resume,
            EngineCall::Pause,
            EngineCall::Resume,
            EngineCall::Seek(SECS(40)),
            EngineCall::Seek(Duration::ZERO),
            EngineCall::Seek(SECS(100)),
            EngineCall::Seek(SECS(7)),
            EngineCall::SetVolume(1.0),
            EngineCall::SetVolume(0.5),
        ]
    );
}

#[test]
fn volume_events_clamp() {
    let r = rig(config());
    r.send(Command::SetVolume(150));
    assert_eq!(r.wait_event(|e| matches!(e, Event::Volume(_))), Event::Volume(100));
}

#[test]
fn backlogged_subscriber_skips_position_only() {
    let r = rig(SessionConfig { subscriber_position_backlog: 1, ..config() });
    let slow = r.session.subscribe();
    let r#gen = r.play_started(&["a"]);

    r.engine.emit(EngineEvent::Position { r#gen, at: SECS(1) });
    r.wait_event(|e| matches!(e, Event::Position(_)));
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));

    let got: Vec<Event> = slow.try_iter().collect();
    assert!(!got.iter().any(|e| matches!(e, Event::Position(_))));
    assert!(got.contains(&Event::State(PlayState::Paused)));
}

#[test]
fn shutdown_returns_while_provider_is_blocked() {
    let r = rig(config());
    r.provider.script_search("q", vec![Step::ok(vec![]).delayed(SECS(5))]);
    r.send(Command::Search { req: 1, query: "q".into() });
    r.provider.wait_search_calls(1);
    let t = Instant::now();
    r.session.shutdown();
    assert!(t.elapsed() < SECS(1), "shutdown took {:?}", t.elapsed());
    assert_eq!(r.engine.calls().last(), Some(&EngineCall::Stop));
}
