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
    let provider = FakeProvider::new("fake");
    let engine = FakeEngineHandle::new();
    let session = Session::spawn(vec![provider.clone()], engine.factory(), config);
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
    let r = rig(config());
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
    let r = rig(config());
    let gate = Gate::new();
    r.provider.script_browse(
        Page::Library,
        vec![Step::err(ProviderError::SignInRequired("expired".into())).gated(&gate)],
    );
    r.send(Command::Browse {
        req: 1,
        page: Page::Library,
    });
    r.provider.set_account(signed_in());
    gate.release();
    r.wait_event(|e| matches!(e, Event::BrowseFailed { .. }));
    r.wait_event(is_account(signed_in()));
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
    let ev = r.wait_event(|e| matches!(e, Event::SignInFailed { .. }));
    assert_eq!(
        ev,
        Event::SignInFailed {
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
    r.wait_event(|e| matches!(e, Event::SignInFailed { .. }));
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
