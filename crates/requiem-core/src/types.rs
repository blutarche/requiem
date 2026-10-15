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

/// The id format is private to the provider that issued it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CollectionId {
    pub provider: ProviderId,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionKind {
    Playlist,
    Album,
}

/// A playlist or album that browses to its tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub kind: CollectionKind,
    pub title: String,
    pub subtitle: Option<String>,
    pub artwork_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Item {
    Track(Track),
    Collection(Collection),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<Item>,
}

/// `Home` goes to the first provider with `has_home()`; `Library` to every signed-in provider,
/// merged in provider order; a collection to the provider that issued it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Page {
    Home,
    Library,
    Collection(CollectionId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountState {
    /// Sign-in can't start; the text says what to set up.
    Unavailable(String),
    SignedOut,
    /// A device-code sign-in is waiting for the user to enter `user_code` at `verification_url`.
    Pending {
        verification_url: String,
        user_code: String,
        expires_at_unix: u64,
    },
    SignedIn {
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatHint {
    WebmOpus,
    Mp4Aac,
    OggVorbis,
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
    /// The request needs a signed-in account: clients offer sign-in instead of an error.
    SignInRequired(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, msg) = match self {
            Self::Network(m) => ("network error", m),
            Self::Unplayable(m) => ("unplayable", m),
            Self::ClientBroken(m) => ("client broken", m),
            Self::Parse(m) => ("parse error", m),
            Self::SignInRequired(m) => ("sign-in required", m),
        };
        write!(f, "{kind}: {msg}")
    }
}

impl std::error::Error for ProviderError {}

pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError>;
    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError>;

    /// Blocking; called on a worker thread.
    fn browse(&self, _page: &Page) -> Result<Vec<Shelf>, ProviderError> {
        Ok(Vec::new())
    }

    /// Whether `Page::Home` is meaningful for this provider.
    fn has_home(&self) -> bool {
        false
    }

    /// Called on the session thread: must not block.
    fn account(&self) -> AccountState {
        AccountState::Unavailable("this provider has no accounts".into())
    }

    /// Blocks until the sign-in ends, reporting `Pending` and then `SignedIn` through
    /// `on_state`. Called on a worker thread.
    fn sign_in(&self, _on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        Err(ProviderError::Unplayable(
            "this provider has no accounts".into(),
        ))
    }

    /// Ends a pending sign-in too. Blocking; called on a worker thread. An `Err` still means
    /// signed out locally; it says what the provider could not confirm remotely.
    fn sign_out(&self) -> Result<(), ProviderError> {
        Ok(())
    }
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
    Started {
        r#gen: u64,
        duration: Option<Duration>,
    },
    Position {
        r#gen: u64,
        at: Duration,
    },
    Buffering {
        r#gen: u64,
        on: bool,
    },
    Ended {
        r#gen: u64,
    },
    Failed {
        r#gen: u64,
        kind: FailureKind,
        detail: String,
    },
}

pub type EngineEventSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Every method only enqueues and none may block, except that `stop` may wait, bounded, for the
/// output to adopt the silent feed.
pub trait Engine: Send {
    /// With `paused`, the track is decoded and reported `Started` but plays only on `resume`.
    fn load(&mut self, r#gen: u64, src: AudioSource, paused: bool);
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
    Search {
        req: u64,
        query: String,
    },
    Enqueue {
        tracks: Vec<Track>,
        mode: EnqueueMode,
    },
    PlayIndex(usize),
    /// 0-based queue index.
    Remove(usize),
    /// 0-based; `to` is the track's index after the move.
    Move {
        from: usize,
        to: usize,
    },
    Clear,
    /// Only while stopped: marks `usize` as the track Play starts. The daemon sends it on
    /// restore and clients never do, so `PROTOCOL` is unchanged.
    Cue(usize),
    Next,
    Prev,
    Pause,
    Resume,
    TogglePause,
    Seek(SeekTarget),
    SetVolume(u8),
    Shutdown,
    /// Answered with `BrowseResults` or `BrowseFailed` under the same `req`.
    Browse {
        req: u64,
        page: Page,
    },
    /// While a sign-in is pending, re-announces it rather than starting another.
    SignIn {
        provider: ProviderId,
    },
    SignOut {
        provider: ProviderId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    SearchResults {
        req: u64,
        tracks: Vec<Track>,
    },
    SearchFailed {
        req: u64,
        error: String,
    },
    QueueChanged {
        tracks: Vec<Track>,
        current: Option<usize>,
    },
    NowPlaying {
        track: Track,
        duration: Option<Duration>,
    },
    Position(Duration),
    State(PlayState),
    Volume(u8),
    TrackFailed {
        track: Track,
        error: String,
    },
    ProviderBroken {
        provider: ProviderId,
        detail: String,
    },
    BrowseResults {
        req: u64,
        page: Page,
        shelves: Vec<Shelf>,
    },
    BrowseFailed {
        req: u64,
        error: String,
        /// Signing in would let the request succeed.
        sign_in_required: bool,
    },
    /// Sent on every change, and to each new subscriber for every provider.
    Account {
        provider: ProviderId,
        state: AccountState,
    },
    /// Why a sign-in ended without `SignedIn`, or what a sign-out could not finish. The
    /// `Account` that follows has the new state.
    AccountError {
        provider: ProviderId,
        error: String,
    },
}
