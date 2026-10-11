//! Audio engine: decode, resample and output.

mod decode;
mod output;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use requiem_core::{AudioSource, Engine, EngineEvent, EngineEventSink, FailureKind};
use rtrb::PushError;

use crate::decode::{Cmd, Job};
use crate::output::{
    CaptureOutput, DeviceOutput, Feed, NO_GEN, Output, Shared, SupMsg, new_shared,
};

/// How long `stop` waits for the output to take the silent feed. Shorter than the decode
/// thread's handover wait because `stop` runs on the caller's thread, and a miss only keeps
/// the old ring until the next load.
const STOP_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// Interleaved stereo f32 samples captured by `OutputChoice::Capture`.
#[derive(Clone, Default)]
pub struct CaptureBuffer(Arc<Mutex<Vec<f32>>>);

impl CaptureBuffer {
    pub fn samples(&self) -> Vec<f32> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn frames(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len() / 2
    }

    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

pub enum OutputChoice {
    Device,
    /// Pulls samples at `speed`x realtime into memory; for tests and headless runs.
    Capture {
        sample_rate: u32,
        speed: f32,
        sink: CaptureBuffer,
    },
}

pub struct AudioOptions {
    pub output: OutputChoice,
    pub ring_seconds: f32,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            output: OutputChoice::Device,
            ring_seconds: 3.0,
        }
    }
}

#[derive(Debug)]
pub struct AudioError(pub String);

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AudioError {}

struct Live {
    cancel: Arc<AtomicBool>,
    cmds: Sender<Cmd>,
}

pub struct AudioEngine {
    shared: Arc<Shared>,
    sink: EngineEventSink,
    live: Option<Live>,
    sup_tx: Sender<SupMsg>,
    supervisor: Option<JoinHandle<()>>,
}

impl AudioEngine {
    pub fn new(sink: EngineEventSink, opts: AudioOptions) -> Result<AudioEngine, AudioError> {
        let debug = std::env::var("REQUIEM_LOG").is_ok_and(|v| v == "debug");
        let ring_seconds = if opts.ring_seconds.is_finite() && opts.ring_seconds > 0.0 {
            opts.ring_seconds
        } else {
            3.0
        };
        let (shared, cb) = new_shared(ring_seconds, debug);
        let (sup_tx, sup_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel();

        let sh = shared.clone();
        let tx = sup_tx.clone();
        let sup_sink = sink.clone();
        let supervisor = std::thread::Builder::new()
            .name("requiem-output".into())
            .spawn(move || {
                // The cpal stream is created and dropped on this thread only.
                let mut output: Box<dyn Output> = match opts.output {
                    OutputChoice::Device => match DeviceOutput::open(cb, sh.clone(), tx) {
                        Ok(o) => Box::new(o),
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    },
                    OutputChoice::Capture {
                        sample_rate,
                        speed,
                        sink,
                    } => Box::new(CaptureOutput::open(cb, &sh, sample_rate, speed, sink)),
                };
                let _ = init_tx.send(Ok(()));
                supervise(output.as_mut(), &sup_rx, &sh, &sup_sink);
            })
            .map_err(|e| AudioError(format!("spawn output thread: {e}")))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(AudioEngine {
                shared,
                sink,
                live: None,
                sup_tx,
                supervisor: Some(supervisor),
            }),
            Ok(Err(e)) => {
                let _ = supervisor.join();
                Err(e)
            }
            Err(_) => Err(AudioError("output thread exited during startup".into())),
        }
    }

    /// Cancels the live decode thread. Returns once any sink call it had in flight is done,
    /// so nothing from it can arrive afterwards.
    fn cancel_live(&mut self) {
        if let Some(live) = self.live.take() {
            live.cancel.store(true, Ordering::Release);
        }
        drop(
            self.shared
                .emit_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Replaces the callback's ring with an empty finished one: silence, no underruns. Returns
    /// the new feed's id, or `None` if the handoff stayed full.
    fn silence(&self) -> Option<u64> {
        for _ in 0..5 {
            let mut link = self
                .shared
                .link
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            link.reap();
            let (_prod, cons) = rtrb::RingBuffer::new(2);
            let id = self.shared.next_ring_id.fetch_add(1, Ordering::Relaxed) + 1;
            let feed = Feed {
                id,
                cons,
                eof: Arc::new(AtomicBool::new(true)),
            };
            match link.handoff.push(feed) {
                Ok(()) => return Some(id),
                Err(PushError::Full(_)) => {
                    drop(link);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        None
    }

    /// Waits, bounded, until the callback reads feed `id` and so has retired the ring before
    /// it. A parked or dead output renders nothing, so it is not waited for.
    fn await_adopted(&self, id: u64) {
        let deadline = Instant::now() + STOP_ACK_TIMEOUT;
        while self.shared.active_ring.load(Ordering::Acquire) != id
            && Instant::now() < deadline
            && !self.shared.dead.load(Ordering::Relaxed)
            && !self.shared.parked.load(Ordering::Acquire)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn supervise(
    output: &mut dyn Output,
    rx: &mpsc::Receiver<SupMsg>,
    shared: &Shared,
    sink: &EngineEventSink,
) {
    while let Ok(msg) = rx.recv() {
        match msg {
            SupMsg::Shutdown => return,
            SupMsg::StreamError(detail) => reopen(output, shared, sink, &detail),
            SupMsg::Play => {
                shared.parked.store(false, Ordering::Release);
                if let Err(e) = output.set_running(true) {
                    reopen(output, shared, sink, &e.0);
                }
            }
            SupMsg::Pause => {
                if let Err(e) = output.set_running(false) {
                    // Reopening drops the old stream, so no render is in flight afterwards.
                    reopen(output, shared, sink, &e.0);
                }
                shared.parked.store(true, Ordering::Release);
            }
        }
    }
}

fn reopen(output: &mut dyn Output, shared: &Shared, sink: &EngineEventSink, detail: &str) {
    let old_rate = output.sample_rate();
    match output.recover() {
        Ok(()) => {
            shared.log(|| format!("output reopened at {} Hz", output.sample_rate()));
            if output.sample_rate() != old_rate {
                shared.epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
        Err(e) => {
            shared.dead.store(true, Ordering::Release);
            let gen_ = shared.cur_gen.load(Ordering::Acquire);
            if gen_ != NO_GEN {
                let _gate = shared
                    .emit_gate
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                sink(EngineEvent::Failed {
                    r#gen: gen_,
                    kind: FailureKind::Output,
                    detail: format!("{detail}; reopen failed: {e}"),
                });
            }
        }
    }
}

impl Engine for AudioEngine {
    fn load(&mut self, r#gen: u64, src: AudioSource) {
        self.cancel_live();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        self.live = Some(Live {
            cancel: cancel.clone(),
            cmds: tx,
        });
        self.shared.cur_gen.store(r#gen, Ordering::Release);
        self.shared.paused.store(false, Ordering::Relaxed);
        // Sent before spawning so the output is normally running by the ring handover; a
        // handover that still finds it parked skips the ack wait (see `Shared::parked`).
        let _ = self.sup_tx.send(SupMsg::Play);
        decode::spawn(Job {
            r#gen,
            src,
            cancel,
            cmds: rx,
            shared: self.shared.clone(),
            sink: self.sink.clone(),
        });
    }

    fn pause(&mut self) {
        self.shared.paused.store(true, Ordering::Relaxed);
        let _ = self.sup_tx.send(SupMsg::Pause);
    }

    fn resume(&mut self) {
        self.shared.paused.store(false, Ordering::Relaxed);
        if let Some(live) = &self.live {
            let _ = self.sup_tx.send(SupMsg::Play);
            let _ = live.cmds.send(Cmd::Wake);
        }
    }

    fn seek(&mut self, to: Duration) {
        if let Some(live) = &self.live {
            let _ = live.cmds.send(Cmd::Seek(to));
        }
    }

    fn set_volume(&mut self, v: f32) {
        let v = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    fn stop(&mut self) {
        self.cancel_live();
        self.shared.cur_gen.store(NO_GEN, Ordering::Release);
        if let Some(id) = self.silence() {
            // Parking stops the callback, which would otherwise hold the old ring until the
            // next load.
            self.await_adopted(id);
        }
        let _ = self.sup_tx.send(SupMsg::Pause);
        self.shared
            .link
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reap();
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.cancel_live();
        let _ = self.sup_tx.send(SupMsg::Shutdown);
        if let Some(t) = self.supervisor.take() {
            let _ = t.join();
        }
    }
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<AudioEngine>();
};
