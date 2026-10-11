//! Shared engine state, the realtime callback and the two `Output` implementations.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, Sample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::{AudioError, CaptureBuffer};

const RETIRE_SLOTS: usize = 32;
/// Upper bound on frames rendered per callback pass, so the conversion scratch never grows.
const SCRATCH_FRAMES: usize = 4096;

pub(crate) const NO_GEN: u64 = u64::MAX;

/// One ring of decoded audio, handed from a decode thread to the callback.
pub(crate) struct Feed {
    pub id: u64,
    pub cons: Consumer<f32>,
    /// Set by the decode thread after its last push. The callback loads it before counting
    /// ring slots, so every sample pushed before the flag is visible.
    pub eof: Arc<AtomicBool>,
}

// `Handoff` moves feeds across threads through a raw pointer, which does not require `Send`.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Feed>();
};

/// Single-slot mailbox from the decode side to the callback. A feed put while another is still
/// pending supersedes it, so nothing piles up while the output is parked.
struct Handoff(AtomicPtr<Feed>);

impl Handoff {
    fn swap(&self, new: *mut Feed, order: Ordering) -> Option<Box<Feed>> {
        let old = self.0.swap(new, order);
        // SAFETY: the slot only ever holds null or a pointer from `Box::into_raw`, and the swap
        // that removes a pointer hands sole ownership of it to its caller.
        (!old.is_null()).then(|| unsafe { Box::from_raw(old) })
    }

    /// Publishes `feed`; returns the pending feed it superseded, which the callback never saw.
    fn put(&self, feed: Box<Feed>) -> Option<Box<Feed>> {
        self.swap(Box::into_raw(feed), Ordering::AcqRel)
    }

    /// Takes the pending feed. One atomic swap: no lock, allocation or free.
    fn take(&self) -> Option<Box<Feed>> {
        self.swap(ptr::null_mut(), Ordering::Acquire)
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Decode-thread side of the callback's handoff and retire ring.
pub(crate) struct Link {
    handoff: Arc<Handoff>,
    retire: Consumer<Box<Feed>>,
}

impl Link {
    /// Offers `feed` to the callback. A feed still pending from an earlier call was never
    /// adopted and is freed here.
    pub fn hand_over(&mut self, feed: Feed) {
        drop(self.handoff.put(Box::new(feed)));
    }

    /// Drops feeds the callback retired. Runs on non-realtime threads only.
    pub fn reap(&mut self) {
        while self.retire.pop().is_ok() {}
    }
}

pub(crate) struct Shared {
    pub link: Mutex<Link>,
    pub paused: AtomicBool,
    pub volume: AtomicU32,
    pub played: AtomicU64,
    pub underruns: AtomicU64,
    /// Id of the feed the callback currently reads; decode threads wait on it after a handover.
    pub active_ring: AtomicU64,
    pub next_ring_id: AtomicU64,
    pub rate: AtomicU32,
    /// Bumped whenever the output was reopened with a different sample rate.
    pub epoch: AtomicU64,
    /// Output is gone and could not be reopened.
    pub dead: AtomicBool,
    /// Set only while the output is stopped with no render in flight. The next render then
    /// takes any pending feed before reading, so the current ring can no longer add to `played`.
    pub parked: AtomicBool,
    /// Output clock relative to realtime, as f32 bits; only `CaptureOutput` runs faster.
    pub speed: AtomicU32,
    pub cur_gen: AtomicU64,
    pub ring_seconds: f32,
    pub debug: bool,
    /// Serialises sink calls against `load`/`stop` so a cancelled thread never emits late.
    pub emit_gate: Mutex<()>,
}

impl Shared {
    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Acquire)
    }

    pub fn speed(&self) -> f64 {
        f64::from(f32::from_bits(self.speed.load(Ordering::Relaxed)))
    }

    /// Samples (not frames) the ring must hold before playback starts or resumes.
    pub fn prime_samples(&self) -> usize {
        let secs = 0.5f32.min(self.ring_seconds / 2.0);
        (secs * self.rate() as f32) as usize * 2
    }

    pub fn log(&self, msg: impl FnOnce() -> String) {
        if self.debug {
            eprintln!("[requiem-audio] {}", msg());
        }
    }
}

pub(crate) fn new_shared(ring_seconds: f32, debug: bool) -> (Arc<Shared>, Callback) {
    let handoff = Arc::new(Handoff(AtomicPtr::new(ptr::null_mut())));
    let (ret_p, ret_c) = RingBuffer::new(RETIRE_SLOTS);
    let shared = Arc::new(Shared {
        link: Mutex::new(Link {
            handoff: handoff.clone(),
            retire: ret_c,
        }),
        paused: AtomicBool::new(false),
        volume: AtomicU32::new(1.0f32.to_bits()),
        played: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
        active_ring: AtomicU64::new(0),
        next_ring_id: AtomicU64::new(0),
        rate: AtomicU32::new(0),
        epoch: AtomicU64::new(0),
        dead: AtomicBool::new(false),
        parked: AtomicBool::new(true),
        speed: AtomicU32::new(1.0f32.to_bits()),
        cur_gen: AtomicU64::new(NO_GEN),
        ring_seconds,
        debug,
        emit_gate: Mutex::new(()),
    });
    let cb = Callback {
        shared: shared.clone(),
        handoff,
        retire: ret_p,
        cur: None,
        primed: false,
    };
    (shared, cb)
}

/// Realtime audio callback state. `render` takes no locks, allocates nothing, sends on no
/// channel and never calls the event sink.
pub(crate) struct Callback {
    shared: Arc<Shared>,
    handoff: Arc<Handoff>,
    retire: Producer<Box<Feed>>,
    /// Boxed so adopting and retiring a feed only move a pointer.
    cur: Option<Box<Feed>>,
    primed: bool,
}

impl Callback {
    /// Moves a feed to the retire ring so the ring buffer and its `Arc`s are freed off the
    /// callback thread. `push` moves into a preallocated slot and does not allocate. If the
    /// retire ring is full (a reaper has not run for `RETIRE_SLOTS` swaps) the feed is dropped
    /// inline, which can free memory; that is accepted over blocking or leaking.
    fn retire(&mut self, feed: Box<Feed>) {
        if let Err(PushError::Full(f)) = self.retire.push(feed) {
            drop(f);
        }
    }

    /// Fills `out` with interleaved stereo f32.
    pub fn render(&mut self, out: &mut [f32]) {
        if let Some(feed) = self.handoff.take() {
            let id = feed.id;
            if let Some(old) = self.cur.replace(feed) {
                self.retire(old);
            }
            self.primed = false;
            self.shared.active_ring.store(id, Ordering::Release);
        }

        let Some(feed) = self.cur.as_mut() else {
            out.fill(0.0);
            return;
        };
        if self.shared.paused.load(Ordering::Relaxed) {
            out.fill(0.0);
            return;
        }

        let eof = feed.eof.load(Ordering::Acquire);
        if !self.primed {
            if eof || feed.cons.slots() >= self.shared.prime_samples() {
                self.primed = true;
            } else {
                out.fill(0.0);
                return;
            }
        }

        let vol = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let want = out.len() & !1;
        let n = feed.cons.slots().min(want) & !1;
        if n > 0
            && let Ok(chunk) = feed.cons.read_chunk(n)
        {
            let (a, b) = chunk.as_slices();
            for (o, s) in out.iter_mut().zip(a.iter().chain(b.iter())) {
                *o = *s * vol;
            }
            chunk.commit_all();
            self.shared
                .played
                .fetch_add((n / 2) as u64, Ordering::Relaxed);
        }
        out[n..].fill(0.0);
        if n < want && !eof {
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
            self.primed = false;
        }
    }
}

pub(crate) trait Output {
    fn sample_rate(&self) -> u32;
    /// Reopens the output after a stream error, running only if it was running before.
    fn recover(&mut self) -> Result<(), AudioError>;
    /// Starts or stops calling the callback. Once a stop returns, no render is in flight.
    fn set_running(&mut self, run: bool) -> Result<(), AudioError>;
}

pub(crate) enum SupMsg {
    StreamError(String),
    Play,
    Pause,
    Shutdown,
}

#[derive(Default)]
struct CaptureState {
    run: bool,
    stop: bool,
}

type CaptureCtl = Arc<(Mutex<CaptureState>, Condvar)>;

pub(crate) struct CaptureOutput {
    rate: u32,
    ctl: CaptureCtl,
    thread: Option<JoinHandle<()>>,
}

impl CaptureOutput {
    pub fn open(
        mut cb: Callback,
        shared: &Shared,
        rate: u32,
        speed: f32,
        sink: CaptureBuffer,
    ) -> Self {
        shared.rate.store(rate, Ordering::Release);
        let speed = speed.max(0.01);
        shared.speed.store(speed.to_bits(), Ordering::Relaxed);
        let speed = f64::from(speed);
        let ctl = CaptureCtl::default();
        let ctl2 = ctl.clone();
        let thread = std::thread::Builder::new()
            .name("requiem-capture".into())
            .spawn(move || {
                let block_frames = (rate / 100).max(1) as usize;
                let period = Duration::from_secs_f64(0.01 / speed);
                let mut buf = vec![0.0f32; block_frames * 2];
                let mut next = Instant::now();
                let (lock, cv) = &*ctl2;
                loop {
                    {
                        // Rendering under the lock makes `set_running(false)` wait it out.
                        let mut st = lock.lock().unwrap();
                        if !st.run && !st.stop {
                            st = cv.wait_while(st, |s| !s.run && !s.stop).unwrap();
                            next = Instant::now();
                        }
                        if st.stop {
                            break;
                        }
                        cb.render(&mut buf);
                    }
                    sink.0.lock().unwrap().extend_from_slice(&buf);
                    next += period;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                }
            })
            .expect("spawn capture thread");
        Self {
            rate,
            ctl,
            thread: Some(thread),
        }
    }

    fn update(&self, f: impl FnOnce(&mut CaptureState)) {
        let (lock, cv) = &*self.ctl;
        f(&mut lock.lock().unwrap_or_else(PoisonError::into_inner));
        cv.notify_all();
    }
}

impl Output for CaptureOutput {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn recover(&mut self) -> Result<(), AudioError> {
        Ok(())
    }

    fn set_running(&mut self, run: bool) -> Result<(), AudioError> {
        self.update(|s| s.run = run);
        Ok(())
    }
}

impl Drop for CaptureOutput {
    fn drop(&mut self) {
        self.update(|s| s.stop = true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type CbSlot = Arc<Mutex<Option<Callback>>>;

/// Owns the callback inside the cpal closure. Dropping it (on the thread that drops the
/// stream, never the audio thread) returns the callback to the slot so a reopened stream can
/// reuse the same rings.
struct CbGuard {
    cb: Option<Callback>,
    slot: CbSlot,
    scratch: Vec<f32>,
    channels: usize,
}

impl CbGuard {
    fn fill<T: Sample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let Some(cb) = self.cb.as_mut() else {
            return;
        };
        let ch = self.channels;
        for block in data.chunks_mut(SCRATCH_FRAMES * ch) {
            let frames = block.len() / ch;
            let scratch = &mut self.scratch[..frames * 2];
            cb.render(scratch);
            for (dst, src) in block.chunks_exact_mut(ch).zip(scratch.chunks_exact(2)) {
                if ch == 1 {
                    dst[0] = T::from_sample((src[0] + src[1]) * 0.5);
                } else {
                    dst[0] = T::from_sample(src[0]);
                    dst[1] = T::from_sample(src[1]);
                    dst[2..].fill(T::EQUILIBRIUM);
                }
            }
        }
    }
}

impl Drop for CbGuard {
    fn drop(&mut self) {
        if let Some(cb) = self.cb.take()
            && let Ok(mut slot) = self.slot.lock()
        {
            *slot = Some(cb);
        }
    }
}

pub(crate) struct DeviceOutput {
    stream: Option<cpal::Stream>,
    playing: bool,
    rate: u32,
    slot: CbSlot,
    shared: Arc<Shared>,
    err_tx: Sender<SupMsg>,
}

impl DeviceOutput {
    pub fn open(
        cb: Callback,
        shared: Arc<Shared>,
        err_tx: Sender<SupMsg>,
    ) -> Result<Self, AudioError> {
        let mut out = Self {
            stream: None,
            playing: false,
            rate: 0,
            slot: Arc::new(Mutex::new(Some(cb))),
            shared,
            err_tx,
        };
        out.build()?;
        Ok(out)
    }

    fn build(&mut self) -> Result<(), AudioError> {
        let cb = self
            .slot
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| AudioError("output callback lost".into()))?;
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError("no default output device".into()))?;
        let supported = device
            .default_output_config()
            .map_err(|e| AudioError(format!("default output config: {e}")))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let channels = usize::from(config.channels).max(1);
        let guard = CbGuard {
            cb: Some(cb),
            slot: self.slot.clone(),
            scratch: vec![0.0; SCRATCH_FRAMES * 2],
            channels,
        };
        let stream = match format {
            SampleFormat::F32 => self.stream_of::<f32>(&device, config, guard),
            SampleFormat::I16 => self.stream_of::<i16>(&device, config, guard),
            SampleFormat::U16 => self.stream_of::<u16>(&device, config, guard),
            other => Err(AudioError(format!("unsupported sample format {other}"))),
        }?;
        self.shared
            .rate
            .store(config.sample_rate, Ordering::Release);
        if self.playing {
            stream
                .play()
                .map_err(|e| AudioError(format!("start stream: {e}")))?;
        }
        self.rate = config.sample_rate;
        self.stream = Some(stream);
        self.shared.log(|| {
            format!(
                "device output {} Hz, {} ch, {format}",
                config.sample_rate, config.channels
            )
        });
        Ok(())
    }

    fn stream_of<T>(
        &self,
        device: &cpal::Device,
        config: cpal::StreamConfig,
        mut guard: CbGuard,
    ) -> Result<cpal::Stream, AudioError>
    where
        T: SizedSample + FromSample<f32>,
    {
        let tx = self.err_tx.clone();
        let shared = self.shared.clone();
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _| guard.fill(data),
                move |e| {
                    shared.log(|| format!("stream error: {e}"));
                    if !matches!(e.kind(), ErrorKind::Xrun | ErrorKind::RealtimeDenied) {
                        let _ = tx.send(SupMsg::StreamError(e.to_string()));
                    }
                },
                None,
            )
            .map_err(|e| AudioError(format!("build output stream: {e}")))
    }
}

impl Output for DeviceOutput {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn recover(&mut self) -> Result<(), AudioError> {
        // Dropping the stream returns the callback to the slot.
        self.stream = None;
        self.build()
    }

    /// The no-render-in-flight guarantee on stop rests on CoreAudio's `AudioOutputUnitStop`
    /// waiting out the current IO cycle when called off the IO thread.
    fn set_running(&mut self, run: bool) -> Result<(), AudioError> {
        self.playing = run;
        let Some(stream) = &self.stream else {
            return Ok(());
        };
        if run {
            stream
                .play()
                .map_err(|e| AudioError(format!("start stream: {e}")))
        } else {
            stream
                .pause()
                .map_err(|e| AudioError(format!("pause stream: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finished empty feed, and its `eof` flag for counting live references.
    fn feed(id: u64) -> (Feed, Arc<AtomicBool>) {
        let (_prod, cons) = RingBuffer::new(2);
        let eof = Arc::new(AtomicBool::new(true));
        (
            Feed {
                id,
                cons,
                eof: eof.clone(),
            },
            eof,
        )
    }

    #[test]
    fn handoff_keeps_only_the_newest_feed_and_frees_the_rest() {
        let (shared, mut cb) = new_shared(1.0, false);
        let mut eofs = Vec::new();
        for id in 1..=3 {
            let (f, eof) = feed(id);
            shared.link.lock().unwrap().hand_over(f);
            eofs.push(eof);
        }
        let live: Vec<usize> = eofs.iter().map(Arc::strong_count).collect();
        assert_eq!(live, [1, 1, 2]);
        cb.render(&mut [0.0; 2]);
        assert_eq!(shared.active_ring.load(Ordering::Acquire), 3);
        let (f, pending) = feed(4);
        shared.link.lock().unwrap().hand_over(f);
        drop(cb);
        drop(shared);
        assert_eq!(Arc::strong_count(&eofs[2]), 1);
        assert_eq!(Arc::strong_count(&pending), 1);
    }
}
