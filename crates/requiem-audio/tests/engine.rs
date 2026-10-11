use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use requiem_audio::{AudioEngine, AudioOptions, CaptureBuffer, OutputChoice};
use requiem_core::{AudioSource, ByteSource, Engine, EngineEvent, FailureKind, FormatHint};

const RATE: u32 = 48_000;
const THRESHOLD: f32 = 0.01;
/// Seconds the last `Position` may fall short of the end: one 500 ms interval plus wake slack.
const LAST_POSITION_TOLERANCE: f64 = 0.6;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).unwrap()
}

struct MemSource(Cursor<Vec<u8>>);

impl Read for MemSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
impl Seek for MemSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}
impl ByteSource for MemSource {
    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}

fn mem(bytes: Vec<u8>, hint: FormatHint) -> AudioSource {
    AudioSource::Encoded {
        bytes: Box::new(MemSource(Cursor::new(bytes))),
        hint,
    }
}

fn opus() -> AudioSource {
    mem(fixture("sine440_48k.webm"), FormatHint::WebmOpus)
}

/// Runs `on_read(total_bytes_read_before)` before each read; it may sleep, fail or cap the read.
struct HookSource<F> {
    inner: MemSource,
    total: u64,
    on_read: F,
}

enum ReadPlan {
    Full,
    Cap(usize),
    Fail,
}

impl<F: FnMut(u64) -> ReadPlan + Send + Sync> Read for HookSource<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let end = match (self.on_read)(self.total) {
            ReadPlan::Full => buf.len(),
            ReadPlan::Cap(n) => buf.len().min(n),
            ReadPlan::Fail => return Err(io::Error::other("injected failure")),
        };
        let n = self.inner.read(&mut buf[..end])?;
        self.total += n as u64;
        Ok(n)
    }
}
impl<F: Send + Sync> Seek for HookSource<F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}
impl<F: FnMut(u64) -> ReadPlan + Send + Sync> ByteSource for HookSource<F> {
    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

fn hooked(
    bytes: Vec<u8>,
    hint: FormatHint,
    on_read: impl FnMut(u64) -> ReadPlan + Send + Sync + 'static,
) -> AudioSource {
    AudioSource::Encoded {
        bytes: Box::new(HookSource {
            inner: MemSource(Cursor::new(bytes)),
            total: 0,
            on_read,
        }),
        hint,
    }
}

type Log = Arc<Mutex<Vec<(Instant, EngineEvent)>>>;

struct Harness {
    engine: AudioEngine,
    capture: CaptureBuffer,
    log: Log,
}

fn harness(speed: f32) -> Harness {
    let capture = CaptureBuffer::default();
    let log: Log = Arc::default();
    let l = log.clone();
    let engine = AudioEngine::new(
        Arc::new(move |ev| l.lock().unwrap().push((Instant::now(), ev))),
        AudioOptions {
            output: OutputChoice::Capture {
                sample_rate: RATE,
                speed,
                sink: capture.clone(),
            },
            ring_seconds: 1.0,
        },
    )
    .unwrap();
    Harness {
        engine,
        capture,
        log,
    }
}

impl Harness {
    fn events(&self) -> Vec<EngineEvent> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.1.clone())
            .collect()
    }

    fn wait(&self, what: &str, secs: u64, mut pred: impl FnMut(&[EngineEvent]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let ev = self.events();
            if pred(&ev) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timeout waiting for {what}: {ev:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_ended(&self, g: u64) {
        self.wait("Ended", 30, |ev| {
            ev.iter()
                .any(|e| matches!(e, EngineEvent::Ended { r#gen: x } if *x == g))
        });
    }

    fn wait_started(&self, g: u64) {
        self.wait("Started", 10, |ev| {
            ev.iter()
                .any(|e| matches!(e, EngineEvent::Started { r#gen: x, .. } if *x == g))
        });
    }

    fn wait_failed(&self, g: u64) -> (FailureKind, String) {
        self.wait("Failed", 30, |ev| {
            ev.iter()
                .any(|e| matches!(e, EngineEvent::Failed { r#gen: x, .. } if *x == g))
        });
        self.events()
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::Failed {
                    r#gen,
                    kind,
                    detail,
                } if r#gen == g => Some((kind, detail)),
                _ => None,
            })
            .unwrap()
    }

    fn wait_capture_frames(&self, frames: usize, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while self.capture.frames() < frames {
            assert!(
                Instant::now() < deadline,
                "capture stalled at {}",
                self.capture.frames()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn left(samples: &[f32]) -> Vec<f32> {
    samples.chunks_exact(2).map(|f| f[0]).collect()
}

/// First and last frame above the silence threshold.
fn active_span(l: &[f32]) -> (usize, usize) {
    let first = l
        .iter()
        .position(|s| s.abs() > THRESHOLD)
        .expect("no audio captured");
    let last = l.iter().rposition(|s| s.abs() > THRESHOLD).unwrap();
    (first, last)
}

fn freq(l: &[f32], rate: u32) -> f64 {
    let crossings = l
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f64 / 2.0 / (l.len() as f64 / f64::from(rate))
}

fn secs(frames: usize) -> f64 {
    frames as f64 / f64::from(RATE)
}

fn near(actual: f64, expected: f64, tol: f64) -> bool {
    (actual - expected).abs() <= tol
}

fn positions(ev: &[EngineEvent], g: u64) -> Vec<f64> {
    ev.iter()
        .filter_map(|e| match e {
            EngineEvent::Position { r#gen: x, at } if *x == g => Some(at.as_secs_f64()),
            _ => None,
        })
        .collect()
}

fn gen_of(e: &EngineEvent) -> u64 {
    match e {
        EngineEvent::Started { r#gen, .. }
        | EngineEvent::Position { r#gen, .. }
        | EngineEvent::Buffering { r#gen, .. }
        | EngineEvent::Ended { r#gen }
        | EngineEvent::Failed { r#gen, .. } => *r#gen,
    }
}

fn check_tone(h: &Harness, expect_secs: f64) {
    let l = left(&h.capture.samples());
    let (first, last) = active_span(&l);
    let len = secs(last - first);
    assert!(near(len, expect_secs, expect_secs * 0.02), "duration {len}");
    let f = freq(&l[first..last], RATE);
    assert!(near(f, 440.0, 440.0 * 0.02), "frequency {f}");
}

#[test]
fn opus_plays_to_end() {
    let mut h = harness(10.0);
    h.engine.load(7, opus(), false);
    h.wait_ended(7);
    let ev = h.events();
    assert!(ev.iter().all(|e| gen_of(e) == 7));
    assert!(
        !ev.iter()
            .any(|e| matches!(e, EngineEvent::Buffering { .. })),
        "{ev:?}"
    );
    match &ev[0] {
        EngineEvent::Started {
            duration: Some(d), ..
        } => {
            assert!(near(d.as_secs_f64(), 6.0, 0.1), "duration {d:?}")
        }
        other => panic!("first event {other:?}"),
    }
    assert!(matches!(ev.last(), Some(EngineEvent::Ended { .. })));
    let pos = positions(&ev, 7);
    assert!(pos.len() >= 8, "positions {pos:?}");
    assert!(pos.windows(2).all(|w| w[0] <= w[1]));
    assert!(near(*pos.last().unwrap(), 6.0, LAST_POSITION_TOLERANCE));
    check_tone(&h, 6.0);
}

#[test]
fn aac_is_resampled_to_output_rate() {
    // Slower capture: the unoptimised resampler must keep ahead of the callback.
    let mut h = harness(4.0);
    h.engine.load(
        1,
        mem(fixture("sine440_44k.m4a"), FormatHint::Mp4Aac),
        false,
    );
    h.wait_ended(1);
    check_tone(&h, 6.0);
}

#[test]
fn seek_right_after_start() {
    let mut h = harness(4.0);
    h.engine.load(1, opus(), false);
    h.wait_started(1);
    let before_seek = h.capture.frames();
    h.engine.seek(Duration::from_secs(4));
    h.wait_ended(1);
    let l = left(&h.capture.samples());
    let l = &l[before_seek..];
    let (first, last) = active_span(l);
    let remaining = secs(last - first);
    assert!(near(remaining, 2.0, 0.2), "remaining {remaining}");
    let after: Vec<f64> = positions(&h.events(), 1)
        .into_iter()
        .filter(|p| *p >= 1.0)
        .collect();
    assert!(after.len() >= 2, "positions {after:?}");
    assert!(after.iter().all(|p| *p >= 4.0), "positions {after:?}");
    assert!(near(*after.last().unwrap(), 6.0, LAST_POSITION_TOLERANCE));
}

#[test]
fn output_is_parked_until_load() {
    let h = harness(10.0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.capture.frames(), 0, "output rendered before any load");
}

#[test]
fn pause_parks_output_and_freezes_position() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.wait_capture_frames(RATE as usize, 20);
    h.engine.pause();
    std::thread::sleep(Duration::from_millis(50));
    let f0 = h.capture.frames();
    let pos0 = positions(&h.events(), 1);
    // 2 s of output time at 10x realtime.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.capture.frames(), f0, "output rendered while paused");
    let pos1 = positions(&h.events(), 1);
    assert_eq!(pos0.len(), pos1.len(), "position advanced while paused");
    h.engine.resume();
    h.wait_capture_frames(f0 + RATE as usize / 10, 5);
    h.wait_ended(1);
    let pos2 = positions(&h.events(), 1);
    assert!(*pos2.last().unwrap() > *pos0.last().unwrap());
    assert!(near(*pos2.last().unwrap(), 6.0, LAST_POSITION_TOLERANCE));
    check_tone(&h, 6.0);
}

#[test]
fn paused_load_starts_without_playing() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), true);
    h.wait_started(1);
    match &h.events()[0] {
        EngineEvent::Started {
            duration: Some(d), ..
        } => assert!(near(d.as_secs_f64(), 6.0, 0.1), "duration {d:?}"),
        other => panic!("first event {other:?}"),
    }
    // 2 s of output time at 10x realtime.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.capture.frames(), 0, "output rendered while paused");
    assert!(positions(&h.events(), 1).is_empty(), "{:?}", h.events());
    h.engine.resume();
    h.wait_ended(1);
    check_tone(&h, 6.0);
}

#[test]
fn paused_load_after_a_paused_track_stays_silent() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.wait_capture_frames(RATE as usize, 20);
    h.engine.pause();
    std::thread::sleep(Duration::from_millis(50));
    let f0 = h.capture.frames();
    h.engine.load(2, opus(), true);
    h.wait_started(2);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.capture.frames(), f0, "output rendered while paused");
    assert!(positions(&h.events(), 2).is_empty(), "{:?}", h.events());
    h.engine.resume();
    h.wait_ended(2);
    let l = left(&h.capture.samples());
    let (first, last) = active_span(&l[f0..]);
    let len = secs(last - first);
    assert!(near(len, 6.0, 0.12), "duration {len}");
}

fn slow_probe(ms: u64) -> AudioSource {
    let mut first = true;
    hooked(
        fixture("sine440_48k.webm"),
        FormatHint::WebmOpus,
        move |_| {
            if std::mem::take(&mut first) {
                std::thread::sleep(Duration::from_millis(ms));
            }
            ReadPlan::Full
        },
    )
}

/// Frames rendered, and those audible, between loading gen 2 over a paused gen 1 and gen 2's
/// `Started`. With `paused_load`, gen 2 loads paused and is resumed while it probes.
fn old_track_bleed(paused_load: bool) -> (usize, usize) {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.wait_capture_frames(RATE as usize, 20);
    h.engine.pause();
    std::thread::sleep(Duration::from_millis(100));
    let f0 = h.capture.frames();
    h.engine.load(2, slow_probe(300), paused_load);
    if paused_load {
        std::thread::sleep(Duration::from_millis(20));
        h.engine.resume();
    }
    h.wait_started(2);
    let f1 = h.capture.frames();
    let l = left(&h.capture.samples());
    let audible = l[f0..f1].iter().filter(|s| s.abs() > THRESHOLD).count();
    (f1 - f0, audible)
}

#[test]
fn unpaused_load_after_pause_does_not_replay_paused_track() {
    let (n, a) = old_track_bleed(false);
    assert_eq!(a, 0, "old track audible for {a} of {n} frames");
}

#[test]
fn resume_during_paused_load_does_not_replay_paused_track() {
    let (n, a) = old_track_bleed(true);
    assert_eq!(a, 0, "old track audible for {a} of {n} frames");
}

#[test]
fn position_continues_across_pause() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.wait("positions", 10, |ev| {
        positions(ev, 1).iter().any(|p| *p >= 2.0)
    });
    h.engine.pause();
    std::thread::sleep(Duration::from_millis(50));
    let before = positions(&h.events(), 1);
    // 3 s of output time at 10x realtime: counting it would jump the position by ~3 s.
    std::thread::sleep(Duration::from_millis(300));
    h.engine.resume();
    h.wait_ended(1);
    let pos = positions(&h.events(), 1);
    let (last, next) = (before[before.len() - 1], pos[before.len()]);
    assert!(next > last && next - last < 1.0, "{last} -> {next}");
    assert!(pos.windows(2).all(|w| w[0] <= w[1]), "{pos:?}");
    assert!(
        near(*pos.last().unwrap(), 6.0, LAST_POSITION_TOLERANCE),
        "{pos:?}"
    );
}

#[test]
fn seek_while_paused_wakes_decoder_and_lands() {
    let mut h = harness(4.0);
    let reads: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let r = reads.clone();
    h.engine.load(
        1,
        hooked(
            fixture("sine440_48k.webm"),
            FormatHint::WebmOpus,
            move |_| {
                r.lock().unwrap().push(Instant::now());
                ReadPlan::Full
            },
        ),
        false,
    );
    h.wait("positions", 10, |ev| !positions(ev, 1).is_empty());
    h.engine.pause();
    std::thread::sleep(Duration::from_millis(100));
    let n = positions(&h.events(), 1).len();
    let t = Instant::now();
    h.engine.seek(Duration::from_secs(4));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        reads.lock().unwrap().iter().any(|at| *at > t),
        "decoder did not act on a seek while paused"
    );
    let f0 = h.capture.frames();
    h.engine.resume();
    h.wait_ended(1);
    let pos = positions(&h.events(), 1);
    assert!(
        pos.len() > n && pos[n..].iter().all(|p| *p >= 4.0),
        "{pos:?}"
    );
    let l = left(&h.capture.samples());
    let (first, last) = active_span(&l[f0..]);
    let remaining = secs(last - first);
    assert!(near(remaining, 2.0, 0.2), "remaining {remaining}");
}

#[test]
fn many_seeks_while_paused_are_each_acted_on() {
    let mut h = harness(4.0);
    let reads: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let r = reads.clone();
    h.engine.load(
        1,
        hooked(
            fixture("sine440_48k.webm"),
            FormatHint::WebmOpus,
            move |_| {
                r.lock().unwrap().push(Instant::now());
                ReadPlan::Full
            },
        ),
        false,
    );
    h.wait("positions", 10, |ev| !positions(ev, 1).is_empty());
    h.engine.pause();
    std::thread::sleep(Duration::from_millis(100));
    let n = positions(&h.events(), 1).len();
    // A parked output adopts no ring, so every seek here hands over one it never takes.
    for i in 0..12u64 {
        let quiet = Instant::now() + Duration::from_secs(5);
        while reads
            .lock()
            .unwrap()
            .last()
            .is_some_and(|at| at.elapsed() < Duration::from_millis(50))
        {
            assert!(
                Instant::now() < quiet,
                "decoder never settled before seek {i}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let t = Instant::now();
        h.engine.seek(Duration::from_millis(1250 + 250 * i));
        let deadline = t + Duration::from_secs(1);
        while !reads.lock().unwrap().iter().any(|at| *at > t) {
            assert!(
                Instant::now() < deadline,
                "decoder did not act on seek {i} while paused"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let f0 = h.capture.frames();
    h.engine.resume();
    h.wait_ended(1);
    let pos = positions(&h.events(), 1);
    assert!(
        pos.len() > n && pos[n..].iter().all(|p| *p >= 4.0),
        "{pos:?}"
    );
    let l = left(&h.capture.samples());
    let (first, last) = active_span(&l[f0..]);
    let remaining = secs(last - first);
    assert!(near(remaining, 2.0, 0.2), "remaining {remaining}");
}

fn rms_of_play(volume: f32) -> f64 {
    let mut h = harness(10.0);
    h.engine.set_volume(volume);
    h.engine.load(1, opus(), false);
    h.wait_ended(1);
    let l = left(&h.capture.samples());
    let (first, _) = active_span(&l);
    let mid = &l[first + RATE as usize..first + 5 * RATE as usize];
    (mid.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / mid.len() as f64).sqrt()
}

#[test]
fn volume_scales_rms() {
    let full = rms_of_play(1.0);
    let half = rms_of_play(0.5);
    let ratio = half / full;
    assert!(near(ratio, 0.5, 0.025), "ratio {ratio}");
}

#[test]
fn newer_load_silences_older_gen() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.engine.load(2, opus(), false);
    let at_load = h.events().len();
    h.wait_ended(2);
    let ev = h.events();
    assert!(ev[at_load..].iter().all(|e| gen_of(e) == 2), "{ev:?}");
    let started = ev
        .iter()
        .position(|e| matches!(e, EngineEvent::Started { r#gen: 2, .. }))
        .unwrap();
    assert!(ev[started..].iter().all(|e| gen_of(e) == 2));
}

#[test]
fn blocked_read_does_not_delay_next_load() {
    let mut h = harness(10.0);
    let mut first = true;
    h.engine.load(
        1,
        hooked(
            fixture("sine440_48k.webm"),
            FormatHint::WebmOpus,
            move |_| {
                if std::mem::take(&mut first) {
                    std::thread::sleep(Duration::from_secs(3));
                }
                ReadPlan::Full
            },
        ),
        false,
    );
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    h.engine.load(2, opus(), false);
    h.wait_started(2);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    h.wait_ended(2);
    std::thread::sleep(Duration::from_millis(3200));
    assert!(h.events().iter().all(|e| gen_of(e) == 2));
}

#[test]
fn io_error_fails_as_stream() {
    let mut h = harness(10.0);
    h.engine.load(
        1,
        hooked(fixture("sine440_48k.webm"), FormatHint::WebmOpus, |total| {
            if total > 20_000 {
                ReadPlan::Fail
            } else {
                ReadPlan::Full
            }
        }),
        false,
    );
    let (kind, detail) = h.wait_failed(1);
    assert_eq!(kind, FailureKind::Stream, "{detail}");
}

#[test]
fn garbage_fails_as_decode() {
    let mut h = harness(10.0);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let junk: Vec<u8> = (0..65_536)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    h.engine.load(1, mem(junk, FormatHint::WebmOpus), false);
    let (kind, detail) = h.wait_failed(1);
    assert_eq!(kind, FailureKind::Decode, "{detail}");
}

#[test]
fn slow_source_reports_buffering() {
    let mut h = harness(4.0);
    let mut slow_reads = 0;
    h.engine.load(
        1,
        // The 6 s fixture is ~10 KB/s; 512 B per 100 ms is well below realtime.
        hooked(
            fixture("sine440_48k.webm"),
            FormatHint::WebmOpus,
            move |total| {
                if total < 16 * 1024 || slow_reads >= 25 {
                    return ReadPlan::Full;
                }
                slow_reads += 1;
                std::thread::sleep(Duration::from_millis(100));
                ReadPlan::Cap(512)
            },
        ),
        false,
    );
    h.wait_ended(1);
    let flags: Vec<bool> = h
        .events()
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Buffering { on, .. } => Some(*on),
            _ => None,
        })
        .collect();
    assert!(flags.len() >= 2, "{flags:?}");
    assert!(
        flags.windows(2).all(|w| w[0] != w[1]),
        "flapping: {flags:?}"
    );
    assert!(flags[0] && !flags[flags.len() - 1], "{flags:?}");
}

#[test]
fn stop_goes_silent_and_parks_output() {
    let mut h = harness(4.0);
    h.engine.load(1, opus(), false);
    h.wait("positions", 10, |ev| positions(ev, 1).len() >= 2);
    h.engine.stop();
    let n_events = h.events().len();
    let f0 = h.capture.frames();
    std::thread::sleep(Duration::from_millis(50));
    let f1 = h.capture.frames();
    // 1 s of output time at 4x realtime.
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(h.capture.frames(), f1, "output rendered after stop");
    assert_eq!(h.events().len(), n_events, "events after stop");
    let l = left(&h.capture.samples());
    assert!(
        l[(f0 + 2_000).min(l.len())..].iter().all(|s| *s == 0.0),
        "audio after stop"
    );
}

#[test]
fn load_after_stop_plays() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.wait("positions", 10, |ev| positions(ev, 1).len() >= 2);
    h.engine.stop();
    std::thread::sleep(Duration::from_millis(50));
    let f0 = h.capture.frames();
    h.engine.load(2, opus(), false);
    h.wait_ended(2);
    let l = left(&h.capture.samples());
    let (first, last) = active_span(&l[f0..]);
    let len = secs(last - first);
    assert!(near(len, 6.0, 0.12), "duration {len}");
    assert!(near(
        *positions(&h.events(), 2).last().unwrap(),
        6.0,
        LAST_POSITION_TOLERANCE
    ));
}

#[test]
fn drop_is_quick() {
    let mut h = harness(10.0);
    h.engine.load(1, opus(), false);
    h.wait_started(1);
    let t = Instant::now();
    drop(h.engine);
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
}

/// Prints whether a real output device could be opened; sandboxes may block CoreAudio.
#[test]
fn device_open_smoke() {
    match AudioEngine::new(Arc::new(|_| {}), AudioOptions::default()) {
        Ok(_) => eprintln!("device output: opened"),
        Err(e) => eprintln!("device output: unavailable ({e})"),
    }
}

#[test]
#[ignore]
fn real_files() {
    for (var, hint) in [
        ("REQUIEM_REAL_WEBM", FormatHint::WebmOpus),
        ("REQUIEM_REAL_M4A", FormatHint::Mp4Aac),
    ] {
        let Ok(path) = std::env::var(var) else {
            eprintln!("{var} unset, skipping");
            continue;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut h = harness(10.0);
        h.engine.load(1, mem(bytes, hint), false);
        h.wait_started(1);
        h.wait_capture_frames(10 * RATE as usize, 120);
        h.engine.seek(Duration::from_secs(60));
        let f = h.capture.frames();
        h.wait_capture_frames(f + 5 * RATE as usize, 120);
        let ev = h.events();
        assert!(
            !ev.iter().any(|e| matches!(e, EngineEvent::Failed { .. })),
            "{var}: {ev:?}"
        );
        let pos = positions(&ev, 1);
        assert!(
            pos.iter().any(|p| *p >= 60.0),
            "{var}: never reached 60 s: {pos:?}"
        );
        eprintln!("{var}: ok, last position {:?}", pos.last());
    }
}
