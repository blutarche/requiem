use std::fmt;
use std::io::{Read, Seek};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackId {
    pub provider: ProviderId,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub artwork_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatHint {
    WebmOpus,
    Mp4Aac,
}

/// Seekable bytes with a known length. Symphonia's mkv reader fails on truncated
/// WebM, so sources must be seekable and length-aware.
/// Must do no I/O until the first read: pre-resolved sources hold no sockets.
pub trait ByteSource: Read + Seek + Send + Sync {
    fn byte_len(&self) -> Option<u64>;
}

pub enum AudioSource {
    Encoded {
        bytes: Box<dyn ByteSource>,
        hint: FormatHint,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Timeout, connect or DNS failure: retryable.
    Network(String),
    /// This track only: removed, private, restricted, live.
    Unplayable(String),
    /// The client path no longer works: needs a requiem update.
    ClientBroken(String),
    /// Response shape changed.
    Parse(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, msg) = match self {
            Self::Network(m) => ("network error", m),
            Self::Unplayable(m) => ("unplayable", m),
            Self::ClientBroken(m) => ("client broken", m),
            Self::Parse(m) => ("parse error", m),
        };
        write!(f, "{kind}: {msg}")
    }
}

impl std::error::Error for ProviderError {}

pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError>;
    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    Stream,
    Decode,
    Output,
}

/// Every event carries the load generation it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineEvent {
    Started { r#gen: u64, duration: Option<Duration> },
    Position { r#gen: u64, at: Duration },
    Buffering { r#gen: u64, on: bool },
    Ended { r#gen: u64 },
    Failed { r#gen: u64, kind: FailureKind, detail: String },
}

pub type EngineEventSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Every method only enqueues; none may block.
pub trait Engine: Send {
    fn load(&mut self, r#gen: u64, src: AudioSource);
    fn pause(&mut self);
    fn resume(&mut self);
    fn seek(&mut self, to: Duration);
    /// `v` is in `0.0..=1.0`.
    fn set_volume(&mut self, v: f32);
    fn stop(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnqueueMode {
    Replace,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeekTarget {
    Absolute(Duration),
    /// Signed seconds from the current position.
    Relative(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayState {
    Playing,
    Paused,
    Buffering,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Search { req: u64, query: String },
    Enqueue { tracks: Vec<Track>, mode: EnqueueMode },
    PlayIndex(usize),
    Next,
    Prev,
    Pause,
    Resume,
    TogglePause,
    Seek(SeekTarget),
    SetVolume(u8),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    SearchResults { req: u64, tracks: Vec<Track> },
    SearchFailed { req: u64, error: String },
    QueueChanged { tracks: Vec<Track>, current: Option<usize> },
    NowPlaying { track: Track, duration: Option<Duration> },
    Position(Duration),
    State(PlayState),
    Volume(u8),
    TrackFailed { track: Track, error: String },
    ProviderBroken { provider: ProviderId, detail: String },
}
