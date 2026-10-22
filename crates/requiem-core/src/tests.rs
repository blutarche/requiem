use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::testing::{EngineCall, FakeEngineHandle, FakeProvider, Gate, SignInScript, Step};
use crate::*;

const SECS: fn(u64) -> Duration = Duration::from_secs;

struct Rig {
    session: Session,
    provider: Arc<FakeProvider>,
    engine: FakeEngineHandle,
    rx: Receiver<Event>,
}

fn config() -> SessionConfig {
    SessionConfig {
        resolve_retry_delay: Duration::from_millis(5),
        ..SessionConfig::default()
    }
}

fn rig(config: SessionConfig) -> Rig {
    rig_with(FakeProvider::new("fake"), vec![], config)
}

/// `provider` is first; `others` follow in order.
fn rig_with(
    provider: Arc<FakeProvider>,
    others: Vec<Arc<FakeProvider>>,
    config: SessionConfig,
) -> Rig {
    let engine = FakeEngineHandle::new();
    let mut all: Vec<Arc<dyn Provider>> = vec![provider.clone()];
    all.extend(others.into_iter().map(|p| p as Arc<dyn Provider>));
    let session = Session::spawn(all, engine.factory(), config);
    let rx = session.subscribe();
    Rig {
        session,
        provider,
        engine,
        rx,
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

impl Rig {
    fn send(&self, cmd: Command) {
        self.session.send(cmd);
    }

    fn enqueue(&self, ids: &[&str]) {
        self.send(Command::Enqueue {
            tracks: tracks(ids),
            mode: EnqueueMode::Replace,
        });
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

    /// Reads events until `n` have matched `pred`, returning every event read.
    fn events_until(&self, n: usize, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        let mut matched = 0;
        while matched < n {
            let ev = self.wait_event(|_| true);
            matched += usize::from(pred(&ev));
            seen.push(ev);
        }
        seen
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
        self.engine
            .calls()
            .iter()
            .filter_map(EngineCall::load_payload)
            .collect()
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

    r.engine.emit(EngineEvent::Started {
        r#gen,
        duration: Some(SECS(200)),
    });
    let ev = r.wait_event(|e| matches!(e, Event::NowPlaying { .. }));
    assert_eq!(
        ev,
        Event::NowPlaying {
            track: track("a"),
            duration: Some(SECS(200))
        }
    );
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
    r.provider
        .script_resolve("a", vec![Step::ok(()).gated(&gate)]);
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
    r.provider
        .script_resolve("b", vec![Step::ok(()).gated(&gate)]);
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
    r.engine
        .wait_calls(|c| c.contains(&EngineCall::Seek(Duration::ZERO)));
    assert_eq!(r.loads(), ["b"]);

    r.engine.emit(EngineEvent::Position { r#gen, at: SECS(2) });
    r.wait_event(|e| matches!(e, Event::Position(d) if *d == SECS(2)));
    r.send(Command::Prev);
    let (gen2, payload) = r.engine.wait_load(2);
    assert_eq!(payload, "a");
    r.start(gen2, None);

    let seeks = |r: &Rig| {
        r.engine
            .calls()
            .iter()
            .filter(|c| matches!(c, EngineCall::Seek(_)))
            .count()
    };
    assert_eq!(seeks(&r), 1);
    r.send(Command::Prev);
    r.engine.wait_calls(|c| {
        c.iter()
            .filter(|c| matches!(c, EngineCall::Seek(_)))
            .count()
            == 2
    });
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
    r.provider.script_resolve(
        "a",
        vec![Step::err(ProviderError::Unplayable("gone".into()))],
    );
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.wait_event(is_failed("a"));
    assert_eq!(r.engine.wait_load(1).1, "b");
}

#[test]
fn client_broken_reports_and_stops() {
    let r = rig(config());
    r.provider.script_resolve(
        "a",
        vec![Step::err(ProviderError::ClientBroken("403".into()))],
    );
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
    r.provider
        .script_resolve("a", vec![Step::err(ProviderError::Parse("odd".into()))]);
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
        r.engine.emit(EngineEvent::Failed {
            r#gen,
            kind,
            detail: "x".into(),
        });
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
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::ProviderBroken { .. }))
    );
}

#[test]
fn stream_failures_report_provider_broken() {
    use FailureKind::*;
    let seen = fail_three([Stream, Decode, Stream]);
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::ProviderBroken { .. }))
    );
}

#[test]
fn failed_preresolve_is_not_a_strike() {
    let r = rig(SessionConfig {
        max_consecutive_failures: 2,
        ..config()
    });
    r.provider
        .script_resolve("b", vec![Step::err(ProviderError::Unplayable("x".into()))]);
    let r#gen = r.play_started(&["a", "b", "c"]);
    r.provider.wait_resolve_calls("b", 1);

    r.engine.emit(EngineEvent::Failed {
        r#gen,
        kind: FailureKind::Decode,
        detail: "x".into(),
    });
    r.wait_event(is_failed("a"));
    let (gen_b, payload) = r.engine.wait_load(2);
    assert_eq!(payload, "b");
    assert_eq!(r.provider.resolve_calls("b"), 2);

    r.engine.emit(EngineEvent::Failed {
        r#gen: gen_b,
        kind: FailureKind::Decode,
        detail: "x".into(),
    });
    r.wait_event(is_failed("b"));
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    assert_eq!(r.loads(), ["a", "b"]);
}

#[test]
fn searches_answered_out_of_order() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider
        .script_search("slow", vec![Step::ok(tracks(&["s"])).gated(&gate)]);
    r.provider
        .script_search("fast", vec![Step::ok(tracks(&["f"]))]);
    r.send(Command::Search {
        req: 1,
        query: "slow".into(),
    });
    r.provider.wait_search_calls(1);
    r.send(Command::Search {
        req: 2,
        query: "fast".into(),
    });

    let ev = r.wait_event(|e| matches!(e, Event::SearchResults { .. }));
    assert_eq!(
        ev,
        Event::SearchResults {
            req: 2,
            tracks: tracks(&["f"])
        }
    );
    gate.release();
    let ev = r.wait_event(|e| matches!(e, Event::SearchResults { .. }));
    assert_eq!(
        ev,
        Event::SearchResults {
            req: 1,
            tracks: tracks(&["s"])
        }
    );
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

    r.engine.emit(EngineEvent::Position {
        r#gen,
        at: SECS(10),
    });
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
    assert_eq!(
        r.wait_event(|e| matches!(e, Event::Volume(_))),
        Event::Volume(100)
    );
}

#[test]
fn backlogged_subscriber_skips_position_only() {
    let r = rig(SessionConfig {
        subscriber_position_backlog: 1,
        ..config()
    });
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
    r.provider
        .script_search("q", vec![Step::ok(vec![]).delayed(SECS(5))]);
    r.send(Command::Search {
        req: 1,
        query: "q".into(),
    });
    r.provider.wait_search_calls(1);
    let t = Instant::now();
    r.session.shutdown();
    assert!(t.elapsed() < SECS(1), "shutdown took {:?}", t.elapsed());
    assert_eq!(r.engine.calls().last(), Some(&EngineCall::Stop));
}

fn last_queue(r: &Rig, pred: impl Fn(&Event) -> bool) -> (Vec<String>, Option<usize>) {
    match r.wait_event(pred) {
        Event::QueueChanged { tracks, current } => {
            (tracks.into_iter().map(|t| t.id.id).collect(), current)
        }
        e => panic!("unexpected {e:?}"),
    }
}

fn queue_is(ids: &[&str], cur: Option<usize>) -> impl Fn(&Event) -> bool {
    let ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    move |e| {
        matches!(e, Event::QueueChanged { tracks, current }
            if *current == cur && tracks.iter().map(|t| t.id.id.clone()).eq(ids.iter().cloned()))
    }
}

#[test]
fn remove_before_current_shifts_index_and_keeps_playing() {
    let r = rig(config());
    r.play_started(&["a", "b", "c"]);
    r.send(Command::PlayIndex(2));
    let (g, _) = r.engine.wait_load(2);
    r.start(g, None);
    r.send(Command::Remove(0));
    r.wait_event(queue_is(&["b", "c"], Some(1)));
    r.barrier();
    assert_eq!(r.loads(), ["a", "c"]);
    assert!(!r.engine.calls().contains(&EngineCall::Stop));
}

#[test]
fn remove_after_current_keeps_current_and_playing() {
    let r = rig(config());
    r.play_started(&["a", "b", "c"]);
    r.send(Command::Remove(2));
    r.wait_event(queue_is(&["a", "b"], Some(0)));
    r.barrier();
    assert_eq!(r.loads(), ["a"]);
    assert!(!r.engine.calls().contains(&EngineCall::Stop));
}

#[test]
fn remove_current_while_playing_loads_successor() {
    let r = rig(config());
    r.play_started(&["a", "b", "c"]);
    r.send(Command::Remove(0));
    r.wait_event(queue_is(&["b", "c"], Some(0)));
    assert_eq!(r.engine.wait_load(2).1, "b");
}

#[test]
fn remove_last_current_while_playing_stops() {
    let r = rig(config());
    r.play_started(&["a", "b"]);
    r.send(Command::PlayIndex(1));
    let (g, _) = r.engine.wait_load(2);
    r.start(g, None);
    r.send(Command::Remove(1));
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    r.wait_event(queue_is(&["a"], None));
    assert!(r.engine.calls().contains(&EngineCall::Stop));
    assert_eq!(r.loads(), ["a", "b"]);
}

#[test]
fn remove_current_while_paused_stops_before_successor() {
    let r = rig(config());
    r.play_started(&["a", "b", "c"]);
    r.send(Command::PlayIndex(1));
    let (g, _) = r.engine.wait_load(2);
    r.start(g, None);
    r.send(Command::Pause);
    r.wait_event(|e| matches!(e, Event::State(PlayState::Paused)));
    r.send(Command::Remove(1));
    r.wait_event(queue_is(&["a", "c"], Some(0)));
    assert!(r.engine.calls().contains(&EngineCall::Stop));
    r.send(Command::Next);
    assert_eq!(r.engine.wait_load(3).1, "c");
}

#[test]
fn remove_out_of_range_is_a_noop() {
    let r = rig(config());
    r.enqueue(&["a"]);
    r.wait_event(queue_is(&["a"], None));
    r.send(Command::Remove(1));
    r.send(Command::Remove(9));
    r.send(Command::Move { from: 0, to: 1 });
    r.send(Command::Move { from: 1, to: 0 });
    r.send(Command::Move { from: 0, to: 0 });
    r.send(Command::SetVolume(50));
    // The next event is Volume, so none of the above emitted QueueChanged.
    assert_eq!(r.wait_event(|_| true), Event::Volume(50));
}

#[test]
fn move_keeps_current_on_the_same_track() {
    let r = rig(config());
    r.play_started(&["a", "b", "c", "d"]);
    r.send(Command::Move { from: 0, to: 2 });
    assert_eq!(
        last_queue(&r, queue_is(&["b", "c", "a", "d"], Some(2))).1,
        Some(2)
    );
    r.send(Command::Move { from: 3, to: 0 });
    r.wait_event(queue_is(&["d", "b", "c", "a"], Some(3)));
    r.send(Command::Move { from: 3, to: 1 });
    r.wait_event(queue_is(&["d", "a", "b", "c"], Some(1)));
    r.send(Command::Move { from: 2, to: 0 });
    r.wait_event(queue_is(&["b", "d", "a", "c"], Some(2)));
    r.send(Command::Move { from: 0, to: 3 });
    r.wait_event(queue_is(&["d", "a", "c", "b"], Some(1)));
    r.barrier();
    assert_eq!(r.loads(), ["a"]);
    assert!(!r.engine.calls().contains(&EngineCall::Stop));
}

#[test]
fn move_drops_the_stale_preresolve() {
    let r = rig(config());
    r.play_started(&["a", "b", "c"]);
    r.provider.wait_resolve_calls("b", 1);
    r.send(Command::Move { from: 2, to: 1 });
    r.wait_event(queue_is(&["a", "c", "b"], Some(0)));
    r.provider.wait_resolve_calls("c", 1);
    r.engine.emit(EngineEvent::Ended {
        r#gen: r.engine.wait_load(1).0,
    });
    assert_eq!(r.engine.wait_load(2).1, "c");
}

#[test]
fn clear_stops_and_empties() {
    let r = rig(config());
    r.play_started(&["a", "b"]);
    r.send(Command::Clear);
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    r.wait_event(queue_is(&[], None));
    assert!(r.engine.calls().contains(&EngineCall::Stop));
}

#[test]
fn seek_reports_the_new_position_even_while_paused() {
    let r = rig(config());
    r.enqueue(&["a"]);
    r.send(Command::PlayIndex(0));
    let (r#gen, _) = r.engine.wait_load(1);
    r.start(r#gen, Some(SECS(100)));
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));

    r.send(Command::Seek(SeekTarget::Absolute(SECS(42))));
    r.wait_event(|e| *e == Event::Position(SECS(42)));
    r.send(Command::Seek(SeekTarget::Absolute(SECS(500))));
    r.wait_event(|e| *e == Event::Position(SECS(100)));
}

#[test]
fn play_while_stopped_starts_after_the_cursor_or_from_the_top() {
    let r = rig(config());
    r.enqueue(&["a", "b"]);
    r.send(Command::TogglePause);
    let (r#gen, payload) = r.engine.wait_load(1);
    assert_eq!(payload, "a");
    r.start(r#gen, None);

    r.send(Command::Next);
    let (r#gen, _) = r.engine.wait_load(2);
    r.start(r#gen, None);
    r.send(Command::Next);
    r.wait_event(|e| *e == Event::State(PlayState::Stopped));
    r.send(Command::Resume);
    let (_, payload) = r.engine.wait_load(3);
    assert_eq!(payload, "a");
}

impl Rig {
    /// Events emitted before a barrier.
    fn drain(&self) -> Vec<Event> {
        self.send(Command::SetVolume(100));
        let mut evs = Vec::new();
        loop {
            match self.wait_event(|_| true) {
                Event::Volume(_) => return evs,
                ev => evs.push(ev),
            }
        }
    }

    /// Waits for the nth (1-based) load; returns its generation and whether it was paused.
    fn nth_load(&self, n: usize) -> (u64, bool) {
        self.engine.wait_load(n);
        self.engine
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                EngineCall::Load { r#gen, paused, .. } => Some((r#gen, paused)),
                _ => None,
            })
            .nth(n - 1)
            .unwrap()
    }
}

fn no_state(evs: &[Event]) -> bool {
    !evs.iter().any(|e| matches!(e, Event::State(_)))
}

#[test]
fn pause_while_loading_holds_through_start() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider
        .script_resolve("a", vec![Step::ok(()).gated(&gate)]);
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.wait_event(|e| *e == Event::State(PlayState::Buffering));
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));
    gate.release();

    let (r#gen, paused) = r.nth_load(1);
    assert!(paused, "{:?}", r.engine.calls());
    r.engine.emit(EngineEvent::Started {
        r#gen,
        duration: Some(SECS(100)),
    });
    r.wait_event(|e| {
        *e == Event::NowPlaying {
            track: track("a"),
            duration: Some(SECS(100)),
        }
    });
    let evs = r.drain();
    assert!(no_state(&evs), "{evs:?}");
    r.provider.wait_resolve_calls("b", 1);

    r.send(Command::Seek(SeekTarget::Absolute(SECS(30))));
    r.wait_event(|e| *e == Event::Position(SECS(30)));
    r.send(Command::Resume);
    r.wait_event(|e| *e == Event::State(PlayState::Playing));
    let calls = r.engine.calls();
    assert_eq!(
        calls[calls.len() - 2..],
        [EngineCall::Seek(SECS(30)), EngineCall::Resume]
    );
}

#[test]
fn pause_while_skipping_holds_through_start() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider
        .script_resolve("b", vec![Step::ok(()).gated(&gate)]);
    r.play_started(&["a", "b"]);
    r.provider.wait_resolve_calls("b", 1);
    r.send(Command::Next);
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));
    gate.release();

    let (r#gen, paused) = r.nth_load(2);
    assert!(paused, "{:?}", r.engine.calls());
    r.engine.emit(EngineEvent::Started {
        r#gen,
        duration: None,
    });
    r.wait_event(|e| matches!(e, Event::NowPlaying { track, .. } if track.id.id == "b"));
    let evs = r.drain();
    assert!(no_state(&evs), "{evs:?}");
    r.send(Command::TogglePause);
    r.wait_event(|e| *e == Event::State(PlayState::Playing));
}

#[test]
fn resume_while_loading_buffers_until_started() {
    let r = rig(config());
    let gate_a = Gate::new();
    let gate_b = Gate::new();
    r.provider
        .script_resolve("a", vec![Step::ok(()).gated(&gate_a)]);
    r.provider
        .script_resolve("b", vec![Step::ok(()).gated(&gate_b)]);

    // Resumed before the load: the engine still holds nothing of this track.
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));
    r.send(Command::Resume);
    r.wait_event(|e| *e == Event::State(PlayState::Buffering));
    gate_a.release();
    let (gen_a, paused) = r.nth_load(1);
    assert!(!paused);
    assert!(!r.engine.calls().contains(&EngineCall::Resume));
    r.start(gen_a, None);

    // Resumed after a paused load, before the engine reports it started.
    r.provider.wait_resolve_calls("b", 1);
    r.send(Command::Next);
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));
    gate_b.release();
    let (gen_b, paused) = r.nth_load(2);
    assert!(paused);
    r.send(Command::Resume);
    r.wait_event(|e| *e == Event::State(PlayState::Buffering));
    assert_eq!(r.engine.calls().last(), Some(&EngineCall::Resume));
    r.start(gen_b, None);
}

#[test]
fn pause_while_loading_holds_through_a_failed_track() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_resolve(
        "a",
        vec![Step::err(ProviderError::Unplayable("gone".into())).gated(&gate)],
    );
    r.enqueue(&["a", "b"]);
    r.send(Command::PlayIndex(0));
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));
    gate.release();
    r.wait_event(is_failed("a"));
    let (r#gen, paused) = r.nth_load(1);
    assert_eq!(r.loads(), ["b"]);
    assert!(paused);
    r.engine.emit(EngineEvent::Started {
        r#gen,
        duration: None,
    });
    r.wait_event(|e| matches!(e, Event::NowPlaying { .. }));
    let evs = r.drain();
    assert!(no_state(&evs), "{evs:?}");
}

#[test]
fn playing_a_track_ends_a_pause() {
    let r = rig(config());
    r.play_started(&["a", "b"]);
    r.send(Command::Pause);
    r.wait_event(|e| *e == Event::State(PlayState::Paused));
    r.send(Command::PlayIndex(1));
    r.wait_event(|e| *e == Event::State(PlayState::Buffering));
    let (r#gen, paused) = r.nth_load(2);
    assert!(!paused);
    r.start(r#gen, None);
}

type PersistLog = Arc<std::sync::Mutex<Vec<PersistState>>>;

fn persist_rig() -> (Rig, PersistLog) {
    let log = PersistLog::default();
    let sink_log = log.clone();
    let r = rig(SessionConfig {
        persist: Some(Arc::new(move |s| sink_log.lock().unwrap().push(s))),
        ..config()
    });
    (r, log)
}

fn last_persist(log: &PersistLog) -> Option<PersistState> {
    log.lock().unwrap().last().cloned()
}

fn wait_persist(log: &PersistLog, pred: impl Fn(&PersistState) -> bool) -> PersistState {
    let deadline = Instant::now() + SECS(5);
    loop {
        if let Some(s) = last_persist(log).filter(&pred) {
            return s;
        }
        assert!(Instant::now() < deadline, "timed out waiting for persist");
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Rig {
    fn cue(&self, i: usize) {
        self.send(Command::Cue(i));
        self.wait_event(|e| matches!(e, Event::QueueChanged { current: Some(c), .. } if *c == i));
    }

    fn resume_loads(&self, n: usize) -> String {
        self.send(Command::Resume);
        self.engine.wait_load(n).1
    }
}

#[test]
fn cue_then_resume_plays_the_cued_index() {
    let r = rig(config());
    r.enqueue(&["a", "b", "c"]);
    r.cue(1);
    assert_eq!(r.resume_loads(1), "b");
}

#[test]
fn cue_is_ignored_while_playing() {
    let r = rig(config());
    r.play_started(&["a", "b", "c"]);
    while r.rx.try_recv().is_ok() {}
    r.send(Command::Cue(2));
    r.barrier();
    assert!(
        !r.rx
            .try_iter()
            .any(|e| matches!(e, Event::QueueChanged { .. }))
    );
    r.send(Command::Next);
    assert_eq!(r.engine.wait_load(2).1, "b");
}

#[test]
fn cue_out_of_range_is_ignored() {
    let r = rig(config());
    r.enqueue(&["a", "b"]);
    r.wait_event(|e| matches!(e, Event::QueueChanged { .. }));
    r.send(Command::Cue(2));
    r.barrier();
    assert!(
        !r.rx
            .try_iter()
            .any(|e| matches!(e, Event::QueueChanged { .. }))
    );
    assert_eq!(r.resume_loads(1), "a");
}

#[test]
fn replace_and_clear_drop_the_cue() {
    let (r, log) = persist_rig();
    r.enqueue(&["a", "b", "c"]);
    r.cue(2);
    r.enqueue(&["x", "y"]);
    wait_persist(&log, |s| s.tracks.len() == 2 && s.resume == Some(0));
    r.cue(1);
    r.send(Command::Clear);
    wait_persist(&log, |s| s.tracks.is_empty() && s.resume.is_none());
    r.enqueue(&["p", "q"]);
    assert_eq!(r.resume_loads(1), "p");
}

#[test]
fn play_index_and_stop_drop_the_cue() {
    let (r, log) = persist_rig();
    r.enqueue(&["a", "b", "c"]);
    r.cue(2);
    r.send(Command::PlayIndex(0));
    r.engine.wait_load(1);
    r.send(Command::Next);
    r.send(Command::Next);
    r.send(Command::Next);
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    r.barrier();
    // Stopped past the end with the cursor on the last track: Play wraps to the top.
    assert_eq!(last_persist(&log).unwrap().resume, Some(0));
}

#[test]
fn move_keeps_the_cue_on_the_same_track() {
    let r = rig(config());
    r.enqueue(&["a", "b", "c"]);
    r.cue(0);
    r.send(Command::Move { from: 0, to: 2 });
    r.wait_event(|e| {
        matches!(
            e,
            Event::QueueChanged {
                current: Some(2),
                ..
            }
        )
    });
    assert_eq!(r.resume_loads(1), "a");
}

#[test]
fn remove_before_the_cued_track_keeps_the_cue() {
    let r = rig(config());
    r.enqueue(&["a", "b", "c"]);
    r.cue(2);
    r.send(Command::Remove(0));
    r.wait_event(|e| {
        matches!(
            e,
            Event::QueueChanged {
                current: Some(1),
                ..
            }
        )
    });
    assert_eq!(r.resume_loads(1), "c");
}

#[test]
fn remove_of_the_cued_track_cues_its_successor() {
    let r = rig(config());
    r.enqueue(&["a", "b", "c"]);
    r.cue(1);
    r.send(Command::Remove(1));
    r.wait_event(|e| {
        matches!(
            e,
            Event::QueueChanged {
                current: Some(1),
                ..
            }
        )
    });
    assert_eq!(r.resume_loads(1), "c");
}

#[test]
fn remove_of_the_cued_last_track_clears_the_cue() {
    let r = rig(config());
    r.enqueue(&["a", "b", "c"]);
    r.cue(2);
    r.send(Command::Remove(2));
    r.wait_event(|e| matches!(e, Event::QueueChanged { current: None, .. }));
    assert_eq!(r.resume_loads(1), "a");
}

#[test]
fn persist_resume_while_playing_and_paused() {
    let (r, log) = persist_rig();
    r.enqueue(&["a", "b", "c"]);
    r.send(Command::PlayIndex(1));
    let (r#gen, _) = r.engine.wait_load(1);
    wait_persist(&log, |s| s.resume == Some(1));
    r.start(r#gen, None);
    r.send(Command::SetVolume(40));
    let s = wait_persist(&log, |s| s.volume == 40);
    assert_eq!((s.resume, s.tracks.len()), (Some(1), 3));
    // A stopped session would resume at 2.
    r.send(Command::Pause);
    r.wait_event(|e| matches!(e, Event::State(PlayState::Paused)));
    r.barrier();
    assert_eq!(last_persist(&log).unwrap().resume, Some(1));
}

#[test]
fn persist_resume_when_stopped_after_the_queue_ended() {
    let (r, log) = persist_rig();
    let g1 = r.play_started(&["a", "b"]);
    r.engine.emit(EngineEvent::Ended { r#gen: g1 });
    let (g2, _) = r.engine.wait_load(2);
    r.engine.emit(EngineEvent::Ended { r#gen: g2 });
    r.wait_event(|e| matches!(e, Event::State(PlayState::Stopped)));
    r.barrier();
    assert_eq!(last_persist(&log).unwrap().resume, Some(0));
}

#[test]
fn persist_resume_when_stopped_after_a_cue() {
    let (r, log) = persist_rig();
    r.enqueue(&["a", "b", "c"]);
    wait_persist(&log, |s| s.resume == Some(0));
    r.cue(2);
    wait_persist(&log, |s| s.resume == Some(2));
}

#[test]
fn persist_resume_for_an_empty_queue() {
    let (r, log) = persist_rig();
    r.enqueue(&["a"]);
    wait_persist(&log, |s| s.resume == Some(0));
    r.send(Command::Clear);
    let s = wait_persist(&log, |s| s.tracks.is_empty());
    assert_eq!((s.resume, s.volume), (None, 100));
}

fn fake_id() -> ProviderId {
    ProviderId("fake".into())
}

fn is_account(state: AccountState) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::Account { provider, state: s } if *provider == fake_id() && *s == state)
}

fn pending() -> AccountState {
    AccountState::Pending {
        verification_url: "https://example.test/device".into(),
        user_code: "ABCD-EFGH".into(),
        expires_at_unix: 1_900_000_000,
    }
}

fn signed_in() -> AccountState {
    AccountState::SignedIn {
        name: Some("Me".into()),
    }
}

#[test]
fn a_new_subscriber_first_hears_every_account() {
    let r = rig(config());
    r.provider.set_account(signed_in());
    // The session read the account when it started.
    let rx = r.session.subscribe();
    assert_eq!(
        rx.try_recv().unwrap(),
        Event::Account {
            provider: fake_id(),
            state: AccountState::SignedOut,
        }
    );
}

#[test]
fn browse_answers_under_the_request_id() {
    let provider = FakeProvider::new("fake");
    provider.set_has_home(true);
    provider.set_account(signed_in());
    let r = rig_with(provider, vec![], config());
    let shelf = Shelf {
        title: "Shelf".into(),
        items: vec![Item::Track(track("a"))],
    };
    r.provider
        .script_browse(Page::Home, vec![Step::ok(vec![shelf.clone()])]);
    r.provider.script_browse(
        Page::Library,
        vec![Step::err(ProviderError::SignInRequired("sign in".into()))],
    );
    r.send(Command::Browse {
        req: 7,
        page: Page::Home,
    });
    let ev = r.wait_event(|e| matches!(e, Event::BrowseResults { .. }));
    assert_eq!(
        ev,
        Event::BrowseResults {
            req: 7,
            page: Page::Home,
            shelves: vec![shelf],
        }
    );
    r.send(Command::Browse {
        req: 8,
        page: Page::Library,
    });
    let ev = r.wait_event(|e| matches!(e, Event::BrowseFailed { .. }));
    assert_eq!(
        ev,
        Event::BrowseFailed {
            req: 8,
            error: "sign-in required: sign in".into(),
            sign_in_required: true,
        }
    );
}

#[test]
fn browse_of_another_providers_collection_fails() {
    let r = rig(config());
    r.send(Command::Browse {
        req: 1,
        page: Page::Collection(CollectionId {
            provider: ProviderId("other".into()),
            id: "x".into(),
        }),
    });
    let ev = r.wait_event(|e| matches!(e, Event::BrowseFailed { .. }));
    assert!(
        matches!(
            ev,
            Event::BrowseFailed {
                req: 1,
                sign_in_required: false,
                ..
            }
        ),
        "{ev:?}"
    );
    assert!(r.provider.browse_calls().is_empty());
}

#[test]
fn a_browse_that_signs_the_provider_out_announces_it() {
    let provider = FakeProvider::new("fake");
    provider.set_account(signed_in());
    let r = rig_with(provider, vec![], config());
    let gate = Gate::new();
    r.provider.script_browse(
        Page::Library,
        vec![Step::err(ProviderError::SignInRequired("expired".into())).gated(&gate)],
    );
    r.send(Command::Browse {
        req: 1,
        page: Page::Library,
    });
    r.provider.set_account(AccountState::SignedOut);
    gate.release();
    r.wait_event(|e| matches!(e, Event::BrowseFailed { .. }));
    r.wait_event(is_account(AccountState::SignedOut));
}

#[test]
fn sign_in_reports_pending_then_signed_in() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: Some(gate.clone()),
        outcome: Ok(signed_in()),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(pending()));
    // A second request repeats the code instead of starting another flow.
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(pending()));
    assert_eq!(r.provider.sign_in_calls(), 1);
    gate.release();
    r.wait_event(is_account(signed_in()));
    assert_eq!(r.provider.sign_in_calls(), 1);
    // Announced once: the next event is the volume sent after it.
    r.send(Command::SetVolume(55));
    let next = r.wait_event(|e| matches!(e, Event::Account { .. } | Event::Volume(_)));
    assert_eq!(next, Event::Volume(55));
}

#[test]
fn failed_sign_in_says_why_then_reports_the_state() {
    let r = rig(config());
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: None,
        outcome: Err(ProviderError::SignInRequired("access was denied".into())),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(pending()));
    let ev = r.wait_event(|e| matches!(e, Event::AccountError { .. }));
    assert_eq!(
        ev,
        Event::AccountError {
            provider: fake_id(),
            error: "access was denied".into(),
        }
    );
    r.wait_event(is_account(AccountState::SignedOut));
}

#[test]
fn an_unchanged_state_after_a_failed_sign_in_is_still_announced() {
    let r = rig(config());
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(|e| matches!(e, Event::AccountError { .. }));
    r.wait_event(is_account(AccountState::SignedOut));
}

#[test]
fn sign_in_while_signed_in_only_repeats_the_state() {
    let r = rig(config());
    r.provider.set_account(signed_in());
    r.provider.script_sign_in(SignInScript {
        pending: None,
        gate: None,
        outcome: Ok(signed_in()),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(signed_in()));
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(signed_in()));
    assert_eq!(r.provider.sign_in_calls(), 1);
}

#[test]
fn sign_out_reports_signed_out() {
    let r = rig(config());
    r.provider.set_account(signed_in());
    r.provider.script_sign_in(SignInScript {
        pending: None,
        gate: None,
        outcome: Ok(signed_in()),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(signed_in()));
    r.send(Command::SignOut {
        provider: fake_id(),
    });
    r.wait_event(is_account(AccountState::SignedOut));
    assert_eq!(r.provider.sign_out_calls(), 1);
}

#[test]
fn an_unchanged_state_after_a_sign_out_is_still_announced() {
    let r = rig(config());
    r.wait_event(is_account(AccountState::SignedOut));
    r.send(Command::SignOut {
        provider: fake_id(),
    });
    r.wait_event(is_account(AccountState::SignedOut));
    assert_eq!(r.provider.sign_out_calls(), 1);
}

fn cancelled() -> ProviderError {
    ProviderError::SignInRequired("sign-in was cancelled".into())
}

#[test]
fn a_sign_in_that_a_sign_out_ends_is_no_error() {
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: Some(gate.clone()),
        outcome: Err(cancelled()),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    r.wait_event(is_account(pending()));
    r.send(Command::SignOut {
        provider: fake_id(),
    });
    r.provider.wait_sign_out_calls(1);
    gate.release();
    // The sign-out and the ended sign-in each announce the state, then the volume follows.
    let mut seen = r.events_until(2, is_account(AccountState::SignedOut));
    r.send(Command::SetVolume(55));
    seen.extend(r.events_until(1, |e| *e == Event::Volume(55)));
    assert!(
        !seen.iter().any(|e| matches!(e, Event::AccountError { .. })),
        "{seen:?}"
    );

    // The next sign-in's failure is an error again.
    r.provider.script_sign_in(SignInScript {
        pending: None,
        gate: None,
        outcome: Err(ProviderError::SignInRequired("access was denied".into())),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    let ev = r.wait_event(|e| matches!(e, Event::AccountError { .. }));
    assert_eq!(
        ev,
        Event::AccountError {
            provider: fake_id(),
            error: "access was denied".into(),
        }
    );
}

#[test]
fn a_sign_out_with_no_sign_in_running_hides_no_later_error() {
    let r = rig(config());
    r.send(Command::SignOut {
        provider: fake_id(),
    });
    r.wait_event(is_account(AccountState::SignedOut));
    r.provider.script_sign_in(SignInScript {
        pending: Some(pending()),
        gate: None,
        outcome: Err(cancelled()),
    });
    r.send(Command::SignIn {
        provider: fake_id(),
    });
    let ev = r.wait_event(|e| matches!(e, Event::AccountError { .. }));
    assert_eq!(
        ev,
        Event::AccountError {
            provider: fake_id(),
            error: "sign-in was cancelled".into(),
        }
    );
}

const CLIENT: &str = r#"{"installed":{"client_id":"id","client_secret":"s"}}"#;

fn import(r: &Rig) {
    r.send(Command::ImportSignInClient {
        provider: fake_id(),
        json: CLIENT.into(),
    });
}

#[test]
fn an_imported_clients_json_never_shows_in_debug() {
    let json = r#"{"client_id":"LEAKME-id","client_secret":"LEAKME-secret"}"#;
    let cmd = Command::ImportSignInClient {
        provider: fake_id(),
        json: json.into(),
    };
    for shown in [format!("{cmd:?}"), format!("{cmd:#?}")] {
        assert!(!shown.contains("LEAKME"), "{shown}");
        assert!(
            shown.contains(&format!("<redacted, {} bytes>", json.len())),
            "{shown}"
        );
    }
}

#[test]
fn an_imported_client_announces_the_account() {
    let provider = FakeProvider::new("fake");
    provider.set_account(AccountState::Unavailable("no client".into()));
    let r = rig_with(provider, vec![], config());
    import(&r);
    r.wait_event(is_account(AccountState::SignedOut));
    assert_eq!(r.provider.imported(), [CLIENT]);
    // An import that leaves the state as it was is still answered.
    import(&r);
    r.wait_event(is_account(AccountState::SignedOut));
    assert_eq!(r.provider.imported().len(), 2);
}

#[test]
fn a_failed_import_says_why() {
    let provider = FakeProvider::new("fake");
    provider.set_account(AccountState::Unavailable("no client".into()));
    let r = rig_with(provider, vec![], config());
    r.provider.fail_import(ProviderError::SignInRequired(
        "not a Google OAuth client".into(),
    ));
    import(&r);
    let mut seen = r.events_until(1, |e| matches!(e, Event::AccountError { .. }));
    assert_eq!(
        seen.pop(),
        Some(Event::AccountError {
            provider: fake_id(),
            error: "not a Google OAuth client".into(),
        })
    );
    // The state follows, unchanged.
    let next = r.wait_event(|_| true);
    assert_eq!(
        next,
        Event::Account {
            provider: fake_id(),
            state: AccountState::Unavailable("no client".into()),
        }
    );
}

#[test]
fn an_import_that_stood_but_could_not_finish_says_why_then_its_state() {
    let provider = FakeProvider::new("fake");
    provider.set_account(AccountState::SignedIn { name: None });
    let r = rig_with(provider, vec![], config());
    // As a provider that kept the new client and signed out, but whose revoke went unconfirmed.
    r.provider.set_account(AccountState::SignedOut);
    let unconfirmed = "signed out on this computer, but Google did not confirm";
    r.provider
        .fail_import(ProviderError::ClientBroken(unconfirmed.into()));
    import(&r);
    let mut seen = r.events_until(1, |e| matches!(e, Event::AccountError { .. }));
    seen.push(r.wait_event(|_| true));
    assert_eq!(
        seen[seen.len() - 2..],
        [
            Event::AccountError {
                provider: fake_id(),
                error: unconfirmed.into(),
            },
            Event::Account {
                provider: fake_id(),
                state: AccountState::SignedOut,
            },
        ]
    );
}

struct NoAccounts;

impl Provider for NoAccounts {
    fn id(&self) -> ProviderId {
        ProviderId("plain".into())
    }

    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Track>, ProviderError> {
        Ok(Vec::new())
    }

    fn resolve(&self, _track: &TrackId) -> Result<AudioSource, ProviderError> {
        Err(ProviderError::Unplayable("no tracks".into()))
    }
}

#[test]
fn a_provider_with_no_client_to_import_says_so() {
    let engine = FakeEngineHandle::new();
    let session = Session::spawn(vec![Arc::new(NoAccounts)], engine.factory(), config());
    let rx = session.subscribe();
    let plain = ProviderId("plain".into());
    session.send(Command::ImportSignInClient {
        provider: plain.clone(),
        json: CLIENT.into(),
    });
    let ev = std::iter::from_fn(|| rx.recv_timeout(SECS(5)).ok())
        .find(|e| matches!(e, Event::AccountError { .. }));
    assert_eq!(
        ev,
        Some(Event::AccountError {
            provider: plain,
            error: "this provider has no client to import".into(),
        })
    );
}

fn named_track(provider: &str, id: &str) -> Track {
    let mut t = track(id);
    t.id.provider = ProviderId(provider.into());
    t
}

fn pair() -> (Arc<FakeProvider>, Arc<FakeProvider>) {
    (FakeProvider::new("first"), FakeProvider::new("second"))
}

fn search_event(r: &Rig) -> Event {
    r.send(Command::Search {
        req: 1,
        query: "q".into(),
    });
    r.wait_event(|e| matches!(e, Event::SearchResults { .. } | Event::SearchFailed { .. }))
}

fn named_tracks(provider: &str, ids: &[&str]) -> Vec<Track> {
    ids.iter().map(|id| named_track(provider, id)).collect()
}

#[test]
fn search_takes_the_providers_in_turn_in_provider_order() {
    let (a, b) = pair();
    let gate = Gate::new();
    a.script_search(
        "q",
        vec![Step::ok(named_tracks("first", &["a1", "a2"])).gated(&gate)],
    );
    b.script_search("q", vec![Step::ok(named_tracks("second", &["b1", "b2"]))]);
    let r = rig_with(a, vec![b.clone()], config());
    r.send(Command::Search {
        req: 1,
        query: "q".into(),
    });
    // The second provider answers first, yet the first provider's tracks still lead.
    b.wait_search_calls(1);
    gate.release();
    let ev = r.wait_event(|e| matches!(e, Event::SearchResults { .. }));
    assert_eq!(
        ev,
        Event::SearchResults {
            req: 1,
            tracks: vec![
                named_track("first", "a1"),
                named_track("second", "b1"),
                named_track("first", "a2"),
                named_track("second", "b2")
            ],
        }
    );
}

#[test]
fn search_follows_the_shorter_list_with_the_rest_of_the_longer() {
    let (a, b) = pair();
    a.script_search(
        "q",
        vec![Step::ok(named_tracks("first", &["a1", "a2", "a3"]))],
    );
    b.script_search("q", vec![Step::ok(named_tracks("second", &["b1"]))]);
    let r = rig_with(a, vec![b], config());
    assert_eq!(
        search_event(&r),
        Event::SearchResults {
            req: 1,
            tracks: vec![
                named_track("first", "a1"),
                named_track("second", "b1"),
                named_track("first", "a2"),
                named_track("first", "a3")
            ],
        }
    );
}

#[test]
fn search_takes_the_others_in_turn_when_one_fails() {
    let (a, b) = pair();
    let c = FakeProvider::new("third");
    a.script_search("q", vec![Step::ok(named_tracks("first", &["a1", "a2"]))]);
    b.script_search("q", vec![Step::err(ProviderError::Network("down".into()))]);
    c.script_search("q", vec![Step::ok(named_tracks("third", &["c1", "c2"]))]);
    let r = rig_with(a, vec![b, c], config());
    assert_eq!(
        search_event(&r),
        Event::SearchResults {
            req: 1,
            tracks: vec![
                named_track("first", "a1"),
                named_track("third", "c1"),
                named_track("first", "a2"),
                named_track("third", "c2")
            ],
        }
    );
}

#[test]
fn search_survives_one_failing_provider() {
    let (a, b) = pair();
    a.script_search("q", vec![Step::err(ProviderError::Network("down".into()))]);
    b.script_search("q", vec![Step::ok(vec![named_track("second", "b1")])]);
    let r = rig_with(a, vec![b], config());
    assert_eq!(
        search_event(&r),
        Event::SearchResults {
            req: 1,
            tracks: vec![named_track("second", "b1")],
        }
    );
}

#[test]
fn search_skips_a_provider_that_needs_sign_in() {
    let (a, b) = pair();
    a.script_search(
        "q",
        vec![Step::err(ProviderError::SignInRequired("sign in".into()))],
    );
    b.script_search("q", vec![Step::ok(vec![named_track("second", "b1")])]);
    let r = rig_with(a, vec![b], config());
    assert_eq!(
        search_event(&r),
        Event::SearchResults {
            req: 1,
            tracks: vec![named_track("second", "b1")],
        }
    );
}

#[test]
fn search_fails_with_the_first_real_error_when_none_succeeds() {
    let (a, b) = pair();
    a.script_search(
        "q",
        vec![Step::err(ProviderError::SignInRequired("sign in".into()))],
    );
    b.script_search("q", vec![Step::err(ProviderError::Network("down".into()))]);
    let r = rig_with(a, vec![b], config());
    assert_eq!(
        search_event(&r),
        Event::SearchFailed {
            req: 1,
            error: "network error: down".into(),
        }
    );
}

#[test]
fn search_fails_with_a_sign_in_error_when_all_need_it() {
    let (a, b) = pair();
    a.script_search(
        "q",
        vec![Step::err(ProviderError::SignInRequired("first".into()))],
    );
    b.script_search(
        "q",
        vec![Step::err(ProviderError::SignInRequired("second".into()))],
    );
    let r = rig_with(a, vec![b.clone()], config());
    assert_eq!(
        search_event(&r),
        Event::SearchFailed {
            req: 1,
            error: "sign-in required: first".into(),
        }
    );
    assert_eq!(b.search_calls(), 1);
}

#[test]
fn library_comes_only_from_signed_in_providers_in_order() {
    let (a, b) = pair();
    let c = FakeProvider::new("third");
    let shelf = |t: &str| Shelf {
        title: t.into(),
        items: vec![],
    };
    a.set_account(signed_in());
    c.set_account(signed_in());
    a.script_browse(Page::Library, vec![Step::ok(vec![shelf("a")])]);
    b.script_browse(Page::Library, vec![Step::ok(vec![shelf("b")])]);
    c.script_browse(Page::Library, vec![Step::ok(vec![shelf("c")])]);
    let r = rig_with(a, vec![b.clone(), c], config());
    r.send(Command::Browse {
        req: 1,
        page: Page::Library,
    });
    let ev = r.wait_event(|e| matches!(e, Event::BrowseResults { .. }));
    assert_eq!(
        ev,
        Event::BrowseResults {
            req: 1,
            page: Page::Library,
            shelves: vec![shelf("a"), shelf("c")],
        }
    );
    assert!(b.browse_calls().is_empty());
}

#[test]
fn library_with_nobody_signed_in_asks_for_sign_in() {
    let (a, b) = pair();
    let r = rig_with(a.clone(), vec![b.clone()], config());
    r.send(Command::Browse {
        req: 1,
        page: Page::Library,
    });
    let ev = r.wait_event(|e| matches!(e, Event::BrowseFailed { .. }));
    assert!(
        matches!(
            ev,
            Event::BrowseFailed {
                req: 1,
                sign_in_required: true,
                ..
            }
        ),
        "{ev:?}"
    );
    assert!(a.browse_calls().is_empty() && b.browse_calls().is_empty());
}

fn track_shelf(title: &str, provider: &str, ids: &[&str]) -> Shelf {
    Shelf {
        title: title.into(),
        items: ids
            .iter()
            .map(|id| Item::Track(named_track(provider, id)))
            .collect(),
    }
}

fn library_event(r: &Rig) -> Event {
    r.send(Command::Browse {
        req: 1,
        page: Page::Library,
    });
    r.wait_event(|e| matches!(e, Event::BrowseResults { .. } | Event::BrowseFailed { .. }))
}

fn mixed_shelf(title: &str, items: &[(&str, &str)]) -> Shelf {
    Shelf {
        title: title.into(),
        items: items
            .iter()
            .map(|(p, id)| Item::Track(named_track(p, id)))
            .collect(),
    }
}

#[test]
fn library_takes_the_providers_entries_in_turn_in_one_shelf() {
    let (a, b) = pair();
    for p in [&a, &b] {
        p.set_account(signed_in());
    }
    // Each provider's shelves count as one list, in their order; the longer list's rest
    // follows.
    a.script_browse(
        Page::Library,
        vec![Step::ok(vec![
            track_shelf("Liked", "first", &["a1"]),
            track_shelf("Playlists", "first", &["a2"]),
        ])],
    );
    b.script_browse(
        Page::Library,
        vec![Step::ok(vec![track_shelf(
            "Playlists",
            "second",
            &["b1", "b2", "b3"],
        )])],
    );
    let r = rig_with(a, vec![b], config());
    assert_eq!(
        library_event(&r),
        Event::BrowseResults {
            req: 1,
            page: Page::Library,
            shelves: vec![mixed_shelf(
                "Library",
                &[
                    ("first", "a1"),
                    ("second", "b1"),
                    ("first", "a2"),
                    ("second", "b2"),
                    ("second", "b3")
                ]
            )],
        }
    );
}

#[test]
fn library_keeps_the_shelves_of_the_only_provider_with_entries() {
    let (a, b) = pair();
    let c = FakeProvider::new("third");
    for p in [&a, &b, &c] {
        p.set_account(signed_in());
    }
    a.script_browse(
        Page::Library,
        vec![Step::err(ProviderError::Network("down".into()))],
    );
    b.script_browse(
        Page::Library,
        vec![Step::ok(vec![track_shelf(
            "Spotify",
            "second",
            &["b1", "b2"],
        )])],
    );
    c.script_browse(
        Page::Library,
        vec![Step::ok(vec![track_shelf("Playlists", "third", &[])])],
    );
    let r = rig_with(a, vec![b, c], config());
    assert_eq!(
        library_event(&r),
        Event::BrowseResults {
            req: 1,
            page: Page::Library,
            shelves: vec![
                track_shelf("Spotify", "second", &["b1", "b2"]),
                track_shelf("Playlists", "third", &[])
            ],
        }
    );
}

#[test]
fn library_takes_the_others_in_turn_when_one_fails() {
    let (a, b) = pair();
    let c = FakeProvider::new("third");
    for p in [&a, &b, &c] {
        p.set_account(signed_in());
    }
    a.script_browse(
        Page::Library,
        vec![Step::ok(vec![track_shelf(
            "Spotify",
            "first",
            &["a1", "a2"],
        )])],
    );
    b.script_browse(
        Page::Library,
        vec![Step::err(ProviderError::Network("down".into()))],
    );
    c.script_browse(
        Page::Library,
        vec![Step::ok(vec![track_shelf("Playlists", "third", &["c1"])])],
    );
    let r = rig_with(a, vec![b, c], config());
    assert_eq!(
        library_event(&r),
        Event::BrowseResults {
            req: 1,
            page: Page::Library,
            shelves: vec![mixed_shelf(
                "Library",
                &[("first", "a1"), ("third", "c1"), ("first", "a2")]
            )],
        }
    );
}

fn home_event(r: &Rig) -> Event {
    r.send(Command::Browse {
        req: 1,
        page: Page::Home,
    });
    r.wait_event(|e| matches!(e, Event::BrowseResults { .. } | Event::BrowseFailed { .. }))
}

#[test]
fn home_comes_from_signed_in_providers_with_a_home_in_order() {
    let (a, b) = pair();
    let (c, d) = (FakeProvider::new("third"), FakeProvider::new("fourth"));
    for p in [&a, &c, &d] {
        p.set_has_home(true);
    }
    for p in [&a, &b, &d] {
        p.set_account(signed_in());
    }
    a.script_browse(
        Page::Home,
        vec![Step::ok(vec![track_shelf("First", "first", &["a1"])])],
    );
    d.script_browse(
        Page::Home,
        vec![Step::ok(vec![track_shelf("Fourth", "fourth", &["d1"])])],
    );
    let r = rig_with(a, vec![b.clone(), c.clone(), d], config());
    assert_eq!(
        home_event(&r),
        Event::BrowseResults {
            req: 1,
            page: Page::Home,
            shelves: vec![
                track_shelf("First", "first", &["a1"]),
                track_shelf("Fourth", "fourth", &["d1"])
            ],
        }
    );
    // Signed in without a Home, and with a Home but signed out.
    assert!(b.browse_calls().is_empty() && c.browse_calls().is_empty());
}

#[test]
fn home_shelves_that_share_a_title_alternate_their_items() {
    let (a, b) = pair();
    for p in [&a, &b] {
        p.set_has_home(true);
        p.set_account(signed_in());
    }
    a.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf("Liked songs", "first", &["a1", "a2", "a3"]),
            track_shelf("Made for you", "first", &["m1"]),
            track_shelf("Your playlists", "first", &["p1"]),
        ])],
    );
    b.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf("Liked songs", "second", &["b1"]),
            track_shelf("Only second", "second", &["x1"]),
            track_shelf("Your playlists", "second", &["q1", "q2", "q3"]),
        ])],
    );
    let r = rig_with(a, vec![b], config());
    let Event::BrowseResults { shelves, .. } = home_event(&r) else {
        panic!("Home failed");
    };
    let mixed = |title: &str, items: &[(&str, &str)]| Shelf {
        title: title.into(),
        items: items
            .iter()
            .map(|(p, id)| Item::Track(named_track(p, id)))
            .collect(),
    };
    assert_eq!(
        shelves,
        [
            mixed(
                "Liked songs",
                &[
                    ("first", "a1"),
                    ("second", "b1"),
                    ("first", "a2"),
                    ("first", "a3")
                ]
            ),
            track_shelf("Made for you", "first", &["m1"]),
            track_shelf("Only second", "second", &["x1"]),
            mixed(
                "Your playlists",
                &[
                    ("first", "p1"),
                    ("second", "q1"),
                    ("second", "q2"),
                    ("second", "q3")
                ]
            ),
        ]
    );
}

fn titles_of(shelves: &[Shelf]) -> Vec<&str> {
    shelves.iter().map(|s| s.title.as_str()).collect()
}

#[test]
fn home_weaves_shared_shelves_and_alternates_the_rest() {
    let (a, b) = pair();
    for p in [&a, &b] {
        p.set_has_home(true);
        p.set_account(signed_in());
    }
    a.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf(HOME_LIKED, "first", &["s1", "s2"]),
            track_shelf(HOME_MIXES, "first", &["sm1", "sm2"]),
            track_shelf(HOME_PLAYLISTS, "first", &["sp1", "sp2"]),
        ])],
    );
    b.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf(HOME_LIKED, "second", &["y1", "y2"]),
            track_shelf(HOME_MIXES, "second", &["ym1", "ym2"]),
            track_shelf("Listen again", "second", &["l1"]),
            track_shelf("Quick picks", "second", &["k1"]),
            track_shelf("Forgotten favorites", "second", &["f1"]),
            track_shelf(HOME_PLAYLISTS, "second", &["yp1", "yp2"]),
        ])],
    );
    let r = rig_with(a, vec![b], config());
    let Event::BrowseResults { shelves, .. } = home_event(&r) else {
        panic!("Home failed");
    };
    assert_eq!(
        titles_of(&shelves),
        [
            HOME_LIKED,
            HOME_MIXES,
            HOME_PLAYLISTS,
            "Listen again",
            "Quick picks",
            "Forgotten favorites"
        ]
    );
    assert_eq!(
        shelves[..3],
        [
            mixed_shelf(
                HOME_LIKED,
                &[
                    ("first", "s1"),
                    ("second", "y1"),
                    ("first", "s2"),
                    ("second", "y2")
                ]
            ),
            mixed_shelf(
                HOME_MIXES,
                &[
                    ("first", "sm1"),
                    ("second", "ym1"),
                    ("first", "sm2"),
                    ("second", "ym2")
                ]
            ),
            mixed_shelf(
                HOME_PLAYLISTS,
                &[
                    ("first", "sp1"),
                    ("second", "yp1"),
                    ("first", "sp2"),
                    ("second", "yp2")
                ]
            ),
        ]
    );
}

#[test]
fn home_of_one_provider_comes_back_unchanged() {
    let (a, b) = pair();
    a.set_has_home(true);
    a.set_account(signed_in());
    b.set_account(signed_in());
    let home = vec![
        track_shelf("One", "first", &["a1", "a2"]),
        track_shelf("Two", "first", &["b1"]),
        track_shelf("Three", "first", &["c1"]),
    ];
    a.script_browse(Page::Home, vec![Step::ok(home.clone())]);
    let r = rig_with(a, vec![b], config());
    let Event::BrowseResults { shelves, .. } = home_event(&r) else {
        panic!("Home failed");
    };
    assert_eq!(shelves, home);
}

#[test]
fn home_of_three_providers_takes_their_shelves_in_turn() {
    let (a, b) = pair();
    let c = FakeProvider::new("third");
    for p in [&a, &b, &c] {
        p.set_has_home(true);
        p.set_account(signed_in());
    }
    a.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf("A1", "first", &["a1"]),
            track_shelf("A2", "first", &["a2"]),
            track_shelf("Shared", "first", &["as"]),
        ])],
    );
    b.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf("B1", "second", &["b1"]),
            track_shelf("Shared", "second", &["bs"]),
        ])],
    );
    c.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf("C1", "third", &["c1"]),
            track_shelf("C2", "third", &["c2"]),
            track_shelf("Shared", "third", &["cs"]),
        ])],
    );
    let r = rig_with(a, vec![b, c], config());
    let Event::BrowseResults { shelves, .. } = home_event(&r) else {
        panic!("Home failed");
    };
    assert_eq!(
        titles_of(&shelves),
        ["A1", "B1", "C1", "A2", "Shared", "C2"]
    );
    assert_eq!(
        shelves[4],
        mixed_shelf(
            "Shared",
            &[("first", "as"), ("second", "bs"), ("third", "cs")]
        )
    );
}

#[test]
fn home_of_a_provider_without_shelves_drops_out() {
    let (a, b) = pair();
    for p in [&a, &b] {
        p.set_has_home(true);
        p.set_account(signed_in());
    }
    a.script_browse(Page::Home, vec![Step::ok(Vec::new())]);
    b.script_browse(
        Page::Home,
        vec![Step::ok(vec![
            track_shelf("One", "second", &["b1"]),
            track_shelf("Two", "second", &["b2"]),
        ])],
    );
    let r = rig_with(a, vec![b], config());
    let Event::BrowseResults { shelves, .. } = home_event(&r) else {
        panic!("Home failed");
    };
    assert_eq!(
        shelves,
        [
            track_shelf("One", "second", &["b1"]),
            track_shelf("Two", "second", &["b2"])
        ]
    );
}

#[test]
fn home_with_nobody_signed_in_asks_for_sign_in() {
    let (a, b) = pair();
    a.set_has_home(true);
    b.set_account(signed_in());
    let r = rig_with(a.clone(), vec![b.clone()], config());
    assert_eq!(
        home_event(&r),
        Event::BrowseFailed {
            req: 1,
            error: "sign-in required: sign in to see your music".into(),
            sign_in_required: true,
        }
    );
    assert!(a.browse_calls().is_empty() && b.browse_calls().is_empty());
}

#[test]
fn sign_in_and_out_of_an_unknown_provider_report_at_once() {
    let r = rig(config());
    let nope = ProviderId("nope".into());
    for cmd in [
        Command::SignIn {
            provider: nope.clone(),
        },
        Command::SignOut {
            provider: nope.clone(),
        },
        Command::ImportSignInClient {
            provider: nope.clone(),
            json: CLIENT.into(),
        },
    ] {
        r.send(cmd);
        let ev = r.wait_event(|e| matches!(e, Event::AccountError { .. }));
        assert_eq!(
            ev,
            Event::AccountError {
                provider: nope.clone(),
                error: "no provider nope".into(),
            }
        );
    }
}
