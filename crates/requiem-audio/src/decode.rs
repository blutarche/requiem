//! Per-load decode thread: probe, decode, resample, push into the ring.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use requiem_core::{
    AudioSource, ByteSource, EngineEvent, EngineEventSink, FailureKind, FormatHint,
};
use rtrb::{Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::{Error as SymError, SeekErrorKind};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::output::{Feed, Shared};

const MAX_DECODE_ERRORS: u32 = 50;
const RESAMPLE_CHUNK: usize = 1024;
const POSITION_EVERY: Duration = Duration::from_millis(250);
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Output time added to computed waits so the callback has moved past the target on waking.
const WAKE_SLACK: Duration = Duration::from_millis(15);

pub(crate) enum Cmd {
    Seek(Duration),
    /// Playback resumed: re-evaluate now rather than at the end of a paused wait.
    Wake,
}

pub(crate) struct Job {
    pub r#gen: u64,
    pub src: AudioSource,
    pub cancel: Arc<AtomicBool>,
    pub cmds: Receiver<Cmd>,
    pub shared: Arc<Shared>,
    pub sink: EngineEventSink,
}

enum Exit {
    Cancelled,
    Fail(FailureKind, String),
}

type R<T> = Result<T, Exit>;

/// Symphonia reports a truncated stream as `UnexpectedEof`; any other I/O error is a stream
/// failure wherever it happens (probe, read or seek).
fn classify(e: SymError) -> Exit {
    match e {
        SymError::IoError(io) if io.kind() != io::ErrorKind::UnexpectedEof => {
            Exit::Fail(FailureKind::Stream, io.to_string())
        }
        other => Exit::Fail(FailureKind::Decode, other.to_string()),
    }
}

fn is_eof(e: &SymError) -> bool {
    matches!(e, SymError::IoError(io) if io.kind() == io::ErrorKind::UnexpectedEof)
}

struct Source(Box<dyn ByteSource>);

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl MediaSource for Source {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.0.byte_len()
    }
}

pub(crate) fn spawn(job: Job) {
    let name = format!("requiem-decode-{}", job.r#gen);
    let _ = std::thread::Builder::new()
        .name(name)
        .spawn(move || run(job));
}

fn run(job: Job) {
    let Job {
        r#gen,
        src,
        cancel,
        cmds,
        shared,
        sink,
    } = job;
    let emit = {
        let cancel = cancel.clone();
        let shared = shared.clone();
        Box::new(move |ev: EngineEvent| {
            let _gate = shared
                .emit_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !cancel.load(Ordering::Acquire) {
                sink(ev);
            }
        }) as Box<dyn Fn(EngineEvent) + Send>
    };
    let mut p = Player::new(r#gen, cancel, cmds, shared, emit);
    match p.play(src) {
        Ok(()) | Err(Exit::Cancelled) => {}
        Err(Exit::Fail(kind, detail)) => {
            p.shared
                .log(|| format!("gen {gen} failed: {kind:?}: {detail}", gen = p.gen_));
            (p.emit)(EngineEvent::Failed {
                r#gen: p.gen_,
                kind,
                detail,
            });
        }
    }
}

struct Player {
    gen_: u64,
    cancel: Arc<AtomicBool>,
    cmds: Receiver<Cmd>,
    shared: Arc<Shared>,
    emit: Box<dyn Fn(EngineEvent) + Send>,
    fmt: Option<Box<dyn FormatReader>>,
    dec: Option<Box<dyn AudioDecoder>>,
    track_id: u32,
    time_base: Option<symphonia::core::units::TimeBase>,
    in_rate: u32,
    out_rate: u32,
    rs: Option<Fft<f32>>,
    /// Output frames still to drop to cancel the resampler's startup delay.
    rs_skip: usize,
    rs_in: u64,
    rs_out: u64,
    /// Interleaved stereo at `in_rate` waiting for a full resampler chunk.
    pending: Vec<f32>,
    rs_buf: Vec<f32>,
    tmp: Vec<f32>,
    stereo: Vec<f32>,
    prod: Option<Producer<f32>>,
    /// Ring capacity in samples.
    ring_cap: usize,
    eof_flag: Arc<AtomicBool>,
    started: bool,
    eof: bool,
    /// The ring filled up; writing resumes once it drains to `refill_level`.
    full: bool,
    pending_seek: Option<Duration>,
    /// Source frames to drop after an accurate seek that landed before the target.
    skip_frames: u64,
    base_time: f64,
    base_played: u64,
    last_emit_played: u64,
    epoch: u64,
    seen_underruns: u64,
    buffering: bool,
    errors: u32,
}

impl Player {
    fn new(
        gen_: u64,
        cancel: Arc<AtomicBool>,
        cmds: Receiver<Cmd>,
        shared: Arc<Shared>,
        emit: Box<dyn Fn(EngineEvent) + Send>,
    ) -> Self {
        Self {
            gen_,
            cancel,
            cmds,
            shared,
            emit,
            fmt: None,
            dec: None,
            track_id: 0,
            time_base: None,
            in_rate: 0,
            out_rate: 0,
            rs: None,
            rs_skip: 0,
            rs_in: 0,
            rs_out: 0,
            pending: Vec::new(),
            rs_buf: Vec::new(),
            tmp: Vec::new(),
            stereo: Vec::new(),
            prod: None,
            ring_cap: 0,
            eof_flag: Arc::new(AtomicBool::new(false)),
            started: false,
            eof: false,
            full: false,
            pending_seek: None,
            skip_frames: 0,
            base_time: 0.0,
            base_played: 0,
            last_emit_played: 0,
            epoch: 0,
            seen_underruns: 0,
            buffering: false,
            errors: 0,
        }
    }

    fn play(&mut self, src: AudioSource) -> R<()> {
        let AudioSource::Encoded { bytes, hint } = src;
        if self.shared.dead.load(Ordering::Acquire) {
            return Err(Exit::Fail(
                FailureKind::Output,
                "audio output unavailable".into(),
            ));
        }
        let mut codecs = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut codecs);
        codecs.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();

        let mut h = Hint::new();
        h.with_extension(match hint {
            FormatHint::WebmOpus => "webm",
            FormatHint::Mp4Aac => "m4a",
        });
        let mss =
            MediaSourceStream::new(Box::new(Source(bytes)), MediaSourceStreamOptions::default());
        let fmt = symphonia::default::get_probe()
            .probe(
                &h,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(classify)?;
        let track = fmt
            .default_track(TrackType::Audio)
            .ok_or_else(|| Exit::Fail(FailureKind::Decode, "no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| Exit::Fail(FailureKind::Decode, "no audio codec parameters".into()))?;
        let dec = codecs
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(classify)?;
        self.track_id = track.id;
        self.time_base = track.time_base;
        let media = fmt.media_info();
        let duration = [
            (track.time_base, track.duration),
            (media.time_base, media.duration),
        ]
        .into_iter()
        .find_map(|(tb, d)| tb?.calc_duration(d?))
        .map(|t| Duration::from_secs_f64(t.as_secs_f64().max(0.0)));
        self.fmt = Some(fmt);
        self.dec = Some(dec);
        self.out_rate = self.shared.rate();
        self.epoch = self.shared.epoch.load(Ordering::Acquire);

        loop {
            self.service()?;
            if self.started
                && let Some(t) = self.pending_seek.take()
            {
                self.seek(t)?;
                continue;
            }
            if self.eof {
                if self.drained() {
                    self.checked()?;
                    (self.emit)(EngineEvent::Ended { r#gen: self.gen_ });
                    return Ok(());
                }
                self.wait(self.idle_wait())?;
                continue;
            }

            let packet = match self.fmt.as_mut().unwrap().next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.finish_input()?;
                    continue;
                }
                Err(e) if is_eof(&e) => {
                    self.finish_input()?;
                    continue;
                }
                Err(e) => return Err(classify(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = self.dec.as_mut().unwrap().decode(&packet);
            let buf = match decoded {
                Ok(b) => b,
                Err(SymError::DecodeError(msg)) => {
                    self.errors += 1;
                    if self.errors > MAX_DECODE_ERRORS {
                        return Err(Exit::Fail(
                            FailureKind::Decode,
                            format!("{MAX_DECODE_ERRORS}+ consecutive decode errors: {msg}"),
                        ));
                    }
                    continue;
                }
                Err(e) => return Err(classify(e)),
            };
            self.errors = 0;
            let rate = buf.spec().rate();
            to_stereo(&buf, &mut self.tmp, &mut self.stereo);
            if self.stereo.is_empty() {
                continue;
            }

            if !self.started {
                self.in_rate = rate;
                self.build_resampler()?;
                self.new_ring()?;
                self.base_time = 0.0;
                self.started = true;
                self.checked()?;
                (self.emit)(EngineEvent::Started {
                    r#gen: self.gen_,
                    duration,
                });
            } else if rate != self.in_rate {
                return Err(Exit::Fail(
                    FailureKind::Decode,
                    format!("sample rate changed mid-stream: {} -> {rate}", self.in_rate),
                ));
            }
            self.consume()?;
        }
    }

    fn checked(&self) -> R<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(Exit::Cancelled)
        } else {
            Ok(())
        }
    }

    fn position_secs(&self) -> f64 {
        let played = self.shared.played.load(Ordering::Relaxed);
        self.base_time
            + played.saturating_sub(self.base_played) as f64 / f64::from(self.out_rate.max(1))
    }

    fn fill_samples(&self) -> usize {
        self.prod.as_ref().map_or(0, |p| self.ring_cap - p.slots())
    }

    fn drained(&self) -> bool {
        self.fill_samples() == 0
    }

    /// Samples at or below which a full ring takes writes again. Refills come in bursts of
    /// 0.5 s, so a slow network read still has most of the ring to play from. The burst is at
    /// most half the ring: at `ring_cap` the thread would wake every `WAKE_SLACK`.
    fn refill_level(&self) -> usize {
        let half_second = self.out_rate as usize; // 0.5 s of stereo samples
        self.ring_cap - half_second.min(self.ring_cap / 2)
    }

    fn position_every(&self) -> u64 {
        u64::from(self.out_rate.max(1)) * POSITION_EVERY.as_millis() as u64 / 1000
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Seek(to) => self.pending_seek = Some(to),
            Cmd::Wake => {}
        }
    }

    /// Blocks until a command arrives or `timeout`, if any, passes. The engine drops the sender
    /// when it cancels this thread, which ends the wait at once.
    fn wait(&mut self, timeout: Option<Duration>) -> R<()> {
        let cmd = match timeout {
            Some(timeout) => match self.cmds.recv_timeout(timeout) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => return Err(Exit::Cancelled),
            },
            None => self.cmds.recv().map_err(|_| Exit::Cancelled)?,
        };
        self.on_cmd(cmd);
        Ok(())
    }

    /// Time until the next Position is due or the ring next needs attention: refill at
    /// `refill_level`, or drained after EOF.
    ///
    /// `None` while paused: resume, seek and cancel each reach this thread as a command, and an
    /// output reopen at another rate is picked up by `service` once resume wakes it.
    fn idle_wait(&self) -> Option<Duration> {
        if self.shared.paused.load(Ordering::Relaxed) {
            return None;
        }
        let played = self.shared.played.load(Ordering::Relaxed);
        let to_position = self
            .position_every()
            .saturating_sub(played.saturating_sub(self.last_emit_played));
        let fill = self.fill_samples();
        let to_ring = if self.eof {
            fill
        } else {
            fill.saturating_sub(self.refill_level())
        } / 2;
        let frames = to_position.min(to_ring as u64);
        let secs = frames as f64 / f64::from(self.out_rate.max(1)) + WAKE_SLACK.as_secs_f64();
        Some(Duration::from_secs_f64(secs / self.shared.speed()))
    }

    /// Cancel check, queued commands, output reopen, position and buffering reports.
    fn service(&mut self) -> R<()> {
        self.checked()?;
        while let Ok(cmd) = self.cmds.try_recv() {
            self.on_cmd(cmd);
        }
        if !self.started {
            return Ok(());
        }

        let epoch = self.shared.epoch.load(Ordering::Acquire);
        if epoch != self.epoch {
            // The output now runs at another rate: resume from where playback is.
            self.epoch = epoch;
            let here = Duration::from_secs_f64(self.position_secs());
            self.out_rate = self.shared.rate();
            self.rs = None;
            self.pending_seek.get_or_insert(here);
        }

        let paused = self.shared.paused.load(Ordering::Relaxed);
        let underruns = self.shared.underruns.load(Ordering::Relaxed);
        if underruns != self.seen_underruns {
            self.seen_underruns = underruns;
            if !paused && !self.eof && !self.buffering {
                self.buffering = true;
                self.checked()?;
                (self.emit)(EngineEvent::Buffering {
                    r#gen: self.gen_,
                    on: true,
                });
            }
        }
        if self.buffering && (self.eof || self.fill_samples() >= self.shared.prime_samples()) {
            self.buffering = false;
            self.checked()?;
            (self.emit)(EngineEvent::Buffering {
                r#gen: self.gen_,
                on: false,
            });
        }

        let played = self.shared.played.load(Ordering::Relaxed);
        if !paused && played.saturating_sub(self.last_emit_played) >= self.position_every() {
            self.last_emit_played = played;
            self.checked()?;
            (self.emit)(EngineEvent::Position {
                r#gen: self.gen_,
                at: Duration::from_secs_f64(self.position_secs()),
            });
        }
        Ok(())
    }

    /// Installs a fresh empty ring in the callback and waits until it is active, so that the
    /// position base is read after the previous ring stopped playing.
    fn new_ring(&mut self) -> R<()> {
        let frames = (self.shared.ring_seconds * self.out_rate as f32) as usize;
        self.ring_cap = frames.max(1024) * 2;
        let (prod, cons) = RingBuffer::new(self.ring_cap);
        let id = self.shared.next_ring_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.eof_flag = Arc::new(AtomicBool::new(false));
        let mut feed = Some(Feed {
            id,
            cons,
            eof: self.eof_flag.clone(),
        });
        loop {
            {
                let mut link = self
                    .shared
                    .link
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                link.reap();
                // Checked under the lock so a cancelled thread can never replace a newer feed.
                self.checked()?;
                match link.handoff.push(feed.take().unwrap()) {
                    Ok(()) => break,
                    Err(rtrb::PushError::Full(f)) => feed = Some(f),
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.prod = Some(prod);
        self.eof = false;
        self.full = false;

        let deadline = Instant::now() + ACK_TIMEOUT;
        while self.shared.active_ring.load(Ordering::Acquire) != id
            && Instant::now() < deadline
            && !self.shared.dead.load(Ordering::Relaxed)
            && !self.shared.parked.load(Ordering::Acquire)
        {
            self.checked()?;
            std::thread::sleep(Duration::from_millis(1));
        }
        self.base_played = self.shared.played.load(Ordering::Relaxed);
        self.last_emit_played = self.base_played;
        Ok(())
    }

    fn build_resampler(&mut self) -> R<()> {
        self.rs = None;
        self.rs_skip = 0;
        self.rs_in = 0;
        self.rs_out = 0;
        self.pending.clear();
        if self.in_rate == self.out_rate || self.out_rate == 0 {
            return Ok(());
        }
        let rs = Fft::<f32>::new(
            self.in_rate as usize,
            self.out_rate as usize,
            RESAMPLE_CHUNK,
            2,
            FixedSync::Input,
        )
        .map_err(|e| Exit::Fail(FailureKind::Decode, format!("resampler: {e}")))?;
        self.rs_skip = rs.output_delay();
        self.rs_buf = vec![0.0; rs.output_frames_max() * 2];
        self.rs = Some(rs);
        Ok(())
    }

    /// Feeds `self.stereo` (source rate) towards the ring.
    fn consume(&mut self) -> R<()> {
        let mut data = std::mem::take(&mut self.stereo);
        if self.skip_frames > 0 {
            let drop_frames = (self.skip_frames as usize).min(data.len() / 2);
            data.drain(..drop_frames * 2);
            self.skip_frames -= drop_frames as u64;
        }
        let res = if self.rs.is_some() {
            self.pending.extend_from_slice(&data);
            self.resample(false)
        } else {
            self.push(&data).map(|_| ())
        };
        data.clear();
        self.stereo = data;
        res
    }

    /// Runs full resampler chunks; with `flush`, also the partial tail and the delay.
    fn resample(&mut self, flush: bool) -> R<()> {
        let mut idle = 0;
        loop {
            let Some(rs) = self.rs.as_mut() else {
                return Ok(());
            };
            let need = rs.input_frames_next();
            let have = self.pending.len() / 2;
            if !flush && have < need {
                return Ok(());
            }
            let target = (self.rs_in as f64 * rs.resample_ratio()).ceil() as u64;
            if flush && (self.rs_out >= target || idle >= 3) {
                return Ok(());
            }
            let take = have.min(need);
            if take == 0 {
                idle += 1;
            }
            let input = InterleavedSlice::new(&self.pending[..take * 2], 2, take).unwrap();
            let out_frames = rs.output_frames_max();
            let mut output = InterleavedSlice::new_mut(&mut self.rs_buf, 2, out_frames).unwrap();
            let partial = Indexing::new().partial_len(take);
            let idx = if take < need { Some(&partial) } else { None };
            let (used, produced) = rs
                .process_into_buffer(&input, &mut output, idx)
                .map_err(|e| Exit::Fail(FailureKind::Decode, format!("resample: {e}")))?;
            // A zero-padded tail reports the full chunk as used.
            let used = used.min(take);
            self.pending.drain(..used * 2);
            self.rs_in += used as u64;

            let start = self.rs_skip.min(produced);
            self.rs_skip -= start;
            let mut end = produced;
            if flush {
                // The tail chunk is zero-padded; keep only what the real input accounts for.
                let room = target.saturating_sub(self.rs_out) as usize;
                end = end.min(start + room);
            }
            self.rs_out += (end - start) as u64;
            let buf = std::mem::take(&mut self.rs_buf);
            let pushed = self.push(&buf[start * 2..end * 2]);
            self.rs_buf = buf;
            if !pushed? {
                return Ok(());
            }
        }
    }

    /// Writes into the ring, waiting while it is full. `Ok(false)` means a seek interrupted
    /// the write and the rest was discarded.
    fn push(&mut self, data: &[f32]) -> R<bool> {
        let mut off = 0;
        while off < data.len() {
            self.service()?;
            if self.pending_seek.is_some() {
                return Ok(false);
            }
            if self.full && self.fill_samples() <= self.refill_level() {
                self.full = false;
            }
            let prod = self.prod.as_mut().expect("ring exists once started");
            let n = if self.full {
                0
            } else {
                prod.slots().min(data.len() - off) & !1
            };
            if n == 0 {
                self.full = true;
                self.wait(self.idle_wait())?;
                continue;
            }
            let mut chunk = prod.write_chunk(n).expect("n <= free slots");
            let (a, b) = chunk.as_mut_slices();
            let (head, tail) = data[off..off + n].split_at(a.len());
            a.copy_from_slice(head);
            b.copy_from_slice(tail);
            chunk.commit_all();
            off += n;
        }
        Ok(true)
    }

    fn finish_input(&mut self) -> R<()> {
        if self.rs.is_some() {
            self.resample(true)?;
        }
        self.eof = true;
        self.eof_flag.store(true, Ordering::Release);
        Ok(())
    }

    fn seek_past_end(&mut self, to: Duration) -> R<()> {
        self.base_time = to.as_secs_f64();
        self.finish_input()
    }

    fn seek(&mut self, to: Duration) -> R<()> {
        self.new_ring()?;
        self.pending.clear();
        self.stereo.clear();
        if self.rs.is_none() && self.in_rate != self.out_rate && self.out_rate != 0 {
            self.build_resampler()?;
        } else if let Some(rs) = self.rs.as_mut() {
            rs.reset();
            self.rs_skip = rs.output_delay();
            self.rs_in = 0;
            self.rs_out = 0;
        }
        let ms = to.as_millis().min(i64::MAX as u128) as i64;
        let result = self.fmt.as_mut().unwrap().seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from_millis(ms),
                track_id: Some(self.track_id),
            },
        );
        self.dec.as_mut().unwrap().reset();
        self.errors = 0;
        match result {
            Ok(seeked) => {
                let actual = self
                    .time_base
                    .and_then(|tb| tb.calc_time(seeked.actual_ts))
                    .map_or(to.as_secs_f64(), |t| t.as_secs_f64());
                let target = to.as_secs_f64();
                // Accurate seeks land on a packet at or before the target.
                let gap = (target - actual).max(0.0);
                self.skip_frames = (gap * f64::from(self.in_rate)) as u64;
                self.base_time = if gap > 0.0 { target } else { actual };
            }
            Err(SymError::SeekError(SeekErrorKind::OutOfRange)) => self.seek_past_end(to)?,
            Err(e) if is_eof(&e) => self.seek_past_end(to)?,
            Err(e) => return Err(classify(e)),
        }
        Ok(())
    }
}

/// Interleaved stereo f32: mono is duplicated, extra channels beyond the first two dropped.
fn to_stereo(buf: &GenericAudioBufferRef<'_>, tmp: &mut Vec<f32>, out: &mut Vec<f32>) {
    let ch = buf.spec().channels().count().max(1);
    buf.copy_to_vec_interleaved::<f32>(tmp);
    out.clear();
    match ch {
        1 => out.extend(tmp.iter().flat_map(|&s| [s, s])),
        2 => out.extend_from_slice(tmp),
        _ => out.extend(tmp.chunks_exact(ch).flat_map(|f| [f[0], f[1]])),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::output::new_shared;

    const RATE: u32 = 48_000;

    fn shared() -> Arc<Shared> {
        let (shared, _cb) = new_shared(3.0, false);
        shared.rate.store(RATE, Ordering::Release);
        shared
    }

    fn player(cmds: Receiver<Cmd>, shared: Arc<Shared>) -> Player {
        Player::new(
            1,
            Arc::default(),
            cmds,
            shared,
            Box::new(|_: EngineEvent| {}),
        )
    }

    /// A player whose ring holds `samples`, at `RATE`.
    fn with_ring(samples: usize) -> Player {
        let (_tx, rx) = mpsc::channel();
        let mut p = player(rx, shared());
        p.out_rate = RATE;
        p.ring_cap = samples;
        p
    }

    #[test]
    fn refill_leaves_half_a_second_of_room() {
        let p = with_ring(3 * RATE as usize * 2);
        assert_eq!(p.refill_level(), p.ring_cap - RATE as usize);
    }

    #[test]
    fn refill_burst_is_at_most_half_a_small_ring() {
        assert_eq!(with_ring(2048).refill_level(), 1024);
    }

    #[test]
    fn paused_wait_lasts_until_a_command() {
        let shared = shared();
        shared.paused.store(true, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let mut p = player(rx, shared);
            for _ in 0..2 {
                let ended = p.wait(p.idle_wait());
                done_tx.send(ended.is_ok()).unwrap();
            }
        });
        assert!(
            done.recv_timeout(Duration::from_millis(1500)).is_err(),
            "paused wait ended without a command"
        );
        tx.send(Cmd::Wake).unwrap();
        assert_eq!(done.recv_timeout(Duration::from_secs(1)), Ok(true));
        drop(tx);
        assert_eq!(done.recv_timeout(Duration::from_secs(1)), Ok(false));
        waiter.join().unwrap();
    }
}
