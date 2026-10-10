//! View model: folds daemon events and connection status into `UiState`, and maps user
//! intents to `Command`s. No AppKit types, so it is unit-tested on every OS.

use std::ops::{BitOr, BitOrAssign};
use std::time::Duration;

use requiem_core::{Command, EnqueueMode, Event, PlayState, SeekTarget, Track};

pub const SEEK_STEP_SECS: i64 = 5;
pub const VOLUME_STEP: i16 = 10;
pub const TOAST_DURATION: Duration = Duration::from_secs(4);

/// Regions of the window that need refreshing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty(u16);

impl Dirty {
    pub const NONE: Dirty = Dirty(0);
    pub const RESULTS: Dirty = Dirty(1 << 0);
    /// The line under the search field: pending or failed.
    pub const SEARCH_STATUS: Dirty = Dirty(1 << 1);
    pub const QUEUE: Dirty = Dirty(1 << 2);
    /// Title, artists, total time.
    pub const NOW_PLAYING: Dirty = Dirty(1 << 3);
    /// The elapsed label; set only when its text changes.
    pub const ELAPSED: Dirty = Dirty(1 << 4);
    /// The seek bar; set on every position change; the bar skips sub-pixel moves.
    pub const SEEK: Dirty = Dirty(1 << 5);
    pub const STATE: Dirty = Dirty(1 << 6);
    pub const VOLUME: Dirty = Dirty(1 << 7);
    pub const TOAST: Dirty = Dirty(1 << 8);
    pub const CONNECTION: Dirty = Dirty(1 << 9);
    pub const ALL: Dirty = Dirty((1 << 10) - 1);

    pub fn contains(self, other: Dirty) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Dirty {
    type Output = Dirty;
    fn bitor(self, rhs: Dirty) -> Dirty {
        Dirty(self.0 | rhs.0)
    }
}

impl BitOrAssign for Dirty {
    fn bitor_assign(&mut self, rhs: Dirty) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// The first connect is in flight.
    Connecting,
    Connected,
    /// Lost or never made; the backend retries with backoff. Holds the reason.
    Reconnecting(String),
    /// The daemon speaks another protocol version; no retry.
    OutOfDate,
    /// `--demo`: fixed fake data, no daemon.
    Demo,
}

/// Connection changes reported by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connected,
    Lost(String),
    OutOfDate,
    Demo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    Idle,
    Pending,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub text: String,
    pub error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Search(String),
    /// Replace the queue with results `i..` and play the first.
    PlayResult(usize),
    AppendResult(usize),
    PlayQueue(usize),
    Toggle,
    Next,
    Prev,
    SeekTo(Duration),
    SeekBy(i64),
    SetVolume(u8),
    VolumeBy(i16),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    pub connection: Connection,
    /// The last query searched for.
    pub query: String,
    pub search: SearchStatus,
    pub results: Vec<Track>,
    pub queue: Vec<Track>,
    pub current: Option<usize>,
    pub now_playing: Option<Track>,
    pub duration: Option<Duration>,
    pub position: Duration,
    pub state: PlayState,
    pub volume: u8,
    /// At most one at a time; a new toast replaces the shown one.
    pub toast: Option<Toast>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            connection: Connection::Connecting,
            query: String::new(),
            search: SearchStatus::Idle,
            results: Vec::new(),
            queue: Vec::new(),
            current: None,
            now_playing: None,
            duration: None,
            position: Duration::ZERO,
            state: PlayState::Stopped,
            volume: 100,
            toast: None,
        }
    }
}

impl UiState {
    pub fn elapsed_text(&self) -> String {
        if self.state == PlayState::Buffering {
            return "buffering…".to_owned();
        }
        if self.now_playing.is_none() {
            return String::new();
        }
        fmt_time(self.position)
    }

    pub fn total_text(&self) -> String {
        match (&self.now_playing, self.duration) {
            (Some(_), Some(d)) => fmt_time(d),
            _ => String::new(),
        }
    }

    /// Playback progress in `0.0..=1.0`.
    pub fn progress(&self) -> f64 {
        match self.duration {
            Some(d) if !d.is_zero() && self.now_playing.is_some() => {
                (self.position.as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0)
            }
            _ => 0.0,
        }
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.state, PlayState::Playing | PlayState::Buffering)
    }

    pub fn banner(&self) -> Option<Banner> {
        match self.connection {
            Connection::Reconnecting(_) => Some(Banner {
                text: "Player disconnected — reconnecting…",
                error: false,
            }),
            Connection::OutOfDate => Some(Banner {
                text: "Player is out of date — quit it from the menu bar",
                error: true,
            }),
            _ => None,
        }
    }

    /// The results area's empty state, when there are no results. Its action focuses the
    /// search field.
    pub fn results_empty(&self) -> Option<EmptyText> {
        if !self.results.is_empty() {
            return None;
        }
        Some(if self.search == SearchStatus::Done {
            EmptyText {
                heading: "No results".to_owned(),
                sub: format!("Nothing matched “{}”", self.query),
                action: "Edit search",
            }
        } else {
            EmptyText {
                heading: "Search YouTube Music".to_owned(),
                sub: "Type a song, artist or album".to_owned(),
                action: "Start a search",
            }
        })
    }

    pub fn search_status_text(&self) -> Option<(String, bool)> {
        match &self.search {
            SearchStatus::Pending => Some(("Searching…".to_owned(), false)),
            SearchStatus::Failed(e) => Some((format!("Search failed: {e}"), true)),
            SearchStatus::Idle | SearchStatus::Done => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Banner {
    pub text: &'static str,
    pub error: bool,
}

/// An empty state: heading, subtext and the label of its primary action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyText {
    pub heading: String,
    pub sub: String,
    pub action: &'static str,
}

#[derive(Debug, Default)]
pub struct Vm {
    ui: UiState,
    last_req: u64,
    pending_req: Option<u64>,
    last_toast: u64,
    dirty: Dirty,
}

impl Vm {
    pub fn new() -> Self {
        Self {
            dirty: Dirty::ALL,
            ..Self::default()
        }
    }

    pub fn ui(&self) -> &UiState {
        &self.ui
    }

    /// Returns the regions changed since the last call, and clears them.
    pub fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::SearchResults { req, tracks } => {
                if self.pending_req == Some(req) {
                    self.pending_req = None;
                    self.ui.results = tracks;
                    self.ui.search = SearchStatus::Done;
                    self.dirty |= Dirty::RESULTS | Dirty::SEARCH_STATUS;
                }
            }
            Event::SearchFailed { req, error } => {
                if self.pending_req == Some(req) {
                    self.pending_req = None;
                    self.ui.search = SearchStatus::Failed(error);
                    self.dirty |= Dirty::RESULTS | Dirty::SEARCH_STATUS;
                }
            }
            Event::QueueChanged { tracks, current } => {
                self.ui.queue = tracks;
                self.ui.current = current;
                self.dirty |= Dirty::QUEUE;
            }
            Event::NowPlaying { track, duration } => {
                self.ui.now_playing = Some(track);
                self.ui.duration = duration;
                self.set_position(Duration::ZERO);
                self.dirty |= Dirty::NOW_PLAYING | Dirty::ELAPSED | Dirty::SEEK;
            }
            Event::Position(at) => self.set_position(at),
            Event::State(state) => self.set_state(state),
            Event::Volume(v) => self.set_volume(v),
            Event::TrackFailed { track, error } => {
                self.toast(format!("Couldn't play “{}”: {error}", track.title), true);
            }
            Event::ProviderBroken { provider, detail } => {
                self.toast(
                    format!(
                        "{} stopped working ({detail}); requiem may need an update",
                        provider_name(&provider.0)
                    ),
                    true,
                );
            }
        }
    }

    pub fn link(&mut self, link: Link) {
        let was = std::mem::replace(&mut self.ui.connection, Connection::Connecting);
        self.ui.connection = match link {
            Link::Connected => {
                if matches!(was, Connection::Reconnecting(_)) {
                    self.toast("Player reconnected".to_owned(), false);
                }
                // The snapshot that follows rebuilds player state, but omits `NowPlaying`
                // when nothing is loaded, so stale state must not survive the reconnect.
                self.reset_player();
                if self.pending_req.take().is_some() {
                    self.ui.search = SearchStatus::Idle;
                    self.dirty |= Dirty::SEARCH_STATUS;
                }
                Connection::Connected
            }
            Link::Lost(reason) => {
                if was == Connection::Connected {
                    self.toast("Player disconnected — reconnecting".to_owned(), true);
                }
                if self.pending_req.take().is_some() {
                    self.ui.search = SearchStatus::Failed("player disconnected".to_owned());
                    self.dirty |= Dirty::SEARCH_STATUS;
                }
                Connection::Reconnecting(reason)
            }
            Link::OutOfDate => Connection::OutOfDate,
            Link::Demo => Connection::Demo,
        };
        if self.ui.connection != was {
            self.dirty |= Dirty::CONNECTION;
        }
    }

    pub fn intent(&mut self, intent: Intent) -> Vec<Command> {
        match intent {
            Intent::Search(query) => {
                let query = query.trim();
                if query.is_empty() {
                    return Vec::new();
                }
                self.last_req += 1;
                self.pending_req = Some(self.last_req);
                self.ui.query = query.to_owned();
                self.ui.search = SearchStatus::Pending;
                self.dirty |= Dirty::SEARCH_STATUS;
                vec![Command::Search {
                    req: self.last_req,
                    query: query.to_owned(),
                }]
            }
            Intent::PlayResult(i) => match self.ui.results.get(i..) {
                Some(rest) if !rest.is_empty() => vec![
                    Command::Enqueue {
                        tracks: rest.to_vec(),
                        mode: EnqueueMode::Replace,
                    },
                    Command::PlayIndex(0),
                ],
                _ => Vec::new(),
            },
            Intent::AppendResult(i) => match self.ui.results.get(i) {
                Some(t) => vec![Command::Enqueue {
                    tracks: vec![t.clone()],
                    mode: EnqueueMode::Append,
                }],
                None => Vec::new(),
            },
            Intent::PlayQueue(i) if i < self.ui.queue.len() => vec![Command::PlayIndex(i)],
            Intent::PlayQueue(_) => Vec::new(),
            Intent::Toggle => vec![Command::TogglePause],
            Intent::Next => vec![Command::Next],
            Intent::Prev => vec![Command::Prev],
            Intent::SeekTo(to) => {
                if self.ui.now_playing.is_none() {
                    return Vec::new();
                }
                let to = self.ui.duration.map_or(to, |d| to.min(d));
                self.set_position(to);
                vec![Command::Seek(SeekTarget::Absolute(to))]
            }
            Intent::SeekBy(secs) => {
                if self.ui.now_playing.is_none() {
                    return Vec::new();
                }
                // Mirrors the session's arithmetic so the bar moves before the daemon answers.
                let step = Duration::from_secs(secs.unsigned_abs());
                let to = if secs >= 0 {
                    self.ui.position.saturating_add(step)
                } else {
                    self.ui.position.saturating_sub(step)
                };
                let to = self.ui.duration.map_or(to, |d| to.min(d));
                self.set_position(to);
                vec![Command::Seek(SeekTarget::Relative(secs))]
            }
            Intent::SetVolume(v) => self.volume_command(v.min(100)),
            Intent::VolumeBy(delta) => {
                let v = (i16::from(self.ui.volume) + delta).clamp(0, 100);
                self.volume_command(v as u8)
            }
        }
    }

    pub fn dismiss_toast(&mut self, id: u64) {
        if self.ui.toast.as_ref().is_some_and(|t| t.id == id) {
            self.ui.toast = None;
            self.dirty |= Dirty::TOAST;
        }
    }

    fn volume_command(&mut self, v: u8) -> Vec<Command> {
        if v == self.ui.volume {
            return Vec::new();
        }
        self.set_volume(v);
        vec![Command::SetVolume(v)]
    }

    fn toast(&mut self, text: String, error: bool) {
        self.last_toast += 1;
        self.ui.toast = Some(Toast {
            id: self.last_toast,
            text,
            error,
        });
        self.dirty |= Dirty::TOAST;
    }

    fn set_position(&mut self, at: Duration) {
        if at == self.ui.position {
            return;
        }
        if at.as_secs() != self.ui.position.as_secs() {
            self.dirty |= Dirty::ELAPSED;
        }
        self.ui.position = at;
        self.dirty |= Dirty::SEEK;
    }

    fn set_state(&mut self, state: PlayState) {
        if state == self.ui.state {
            return;
        }
        self.ui.state = state;
        self.dirty |= Dirty::STATE | Dirty::ELAPSED;
        // The session forgets the track on stop and never says so with an event.
        if state == PlayState::Stopped && self.ui.now_playing.is_some() {
            self.ui.now_playing = None;
            self.ui.duration = None;
            self.set_position(Duration::ZERO);
            self.dirty |= Dirty::NOW_PLAYING | Dirty::SEEK;
        }
    }

    fn set_volume(&mut self, v: u8) {
        if v != self.ui.volume {
            self.ui.volume = v;
            self.dirty |= Dirty::VOLUME;
        }
    }

    fn reset_player(&mut self) {
        self.ui.queue.clear();
        self.ui.current = None;
        self.ui.now_playing = None;
        self.ui.duration = None;
        self.ui.position = Duration::ZERO;
        self.ui.state = PlayState::Stopped;
        self.dirty |=
            Dirty::QUEUE | Dirty::NOW_PLAYING | Dirty::ELAPSED | Dirty::SEEK | Dirty::STATE;
    }
}

/// `m:ss`, or `h:mm:ss` from an hour up.
pub fn fmt_time(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn artists(track: &Track) -> String {
    track.artists.join(", ")
}

fn provider_name(id: &str) -> &str {
    match id {
        "ytmusic" => "YouTube Music",
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteAction {
    PlayPause,
    Next,
    Previous,
    FocusSearch,
    ShowSearch,
    ShowQueue,
    VolumeUp,
    VolumeDown,
}

impl PaletteAction {
    pub const ALL: [PaletteAction; 8] = [
        PaletteAction::PlayPause,
        PaletteAction::Next,
        PaletteAction::Previous,
        PaletteAction::FocusSearch,
        PaletteAction::ShowSearch,
        PaletteAction::ShowQueue,
        PaletteAction::VolumeUp,
        PaletteAction::VolumeDown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaletteAction::PlayPause => "Play/Pause",
            PaletteAction::Next => "Next",
            PaletteAction::Previous => "Previous",
            PaletteAction::FocusSearch => "Focus search",
            PaletteAction::ShowSearch => "Show search",
            PaletteAction::ShowQueue => "Show queue",
            PaletteAction::VolumeUp => "Volume up",
            PaletteAction::VolumeDown => "Volume down",
        }
    }

    pub fn shortcut(self) -> &'static str {
        match self {
            PaletteAction::PlayPause => "Space",
            PaletteAction::FocusSearch => "⌘F",
            PaletteAction::ShowSearch => "⌘1",
            PaletteAction::ShowQueue => "⌘2",
            _ => "",
        }
    }

    /// Case-insensitive substring match on the label, in `ALL` order.
    pub fn filter(query: &str) -> Vec<PaletteAction> {
        let q = query.trim().to_lowercase();
        Self::ALL
            .into_iter()
            .filter(|a| a.label().to_lowercase().contains(&q))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use requiem_core::{ProviderId, TrackId};

    fn track(id: &str, secs: u64) -> Track {
        Track {
            id: TrackId {
                provider: ProviderId("test".into()),
                id: id.into(),
            },
            title: format!("Title {id}"),
            artists: vec![format!("Artist {id}"), "Guest".into()],
            album: Some(format!("Album {id}")),
            duration: Some(Duration::from_secs(secs)),
            artwork_url: None,
        }
    }

    fn tracks(n: usize) -> Vec<Track> {
        (0..n).map(|i| track(&i.to_string(), 200)).collect()
    }

    fn connected() -> Vm {
        let mut vm = Vm::new();
        vm.link(Link::Connected);
        vm.take_dirty();
        vm
    }

    /// A vm with `n` results from a completed search.
    fn with_results(n: usize) -> Vm {
        let mut vm = connected();
        let req = search_req(&mut vm, "q");
        vm.apply(Event::SearchResults {
            req,
            tracks: tracks(n),
        });
        vm.take_dirty();
        vm
    }

    fn search_req(vm: &mut Vm, q: &str) -> u64 {
        match vm.intent(Intent::Search(q.into())).as_slice() {
            [Command::Search { req, .. }] => *req,
            other => panic!("expected one search, got {other:?}"),
        }
    }

    fn playing(secs: u64) -> Vm {
        let mut vm = connected();
        vm.apply(Event::NowPlaying {
            track: track("np", secs),
            duration: Some(Duration::from_secs(secs)),
        });
        vm.apply(Event::State(PlayState::Playing));
        vm.take_dirty();
        vm
    }

    #[test]
    fn starts_connecting_with_everything_dirty() {
        let mut vm = Vm::new();
        assert_eq!(vm.ui().connection, Connection::Connecting);
        assert_eq!(vm.take_dirty(), Dirty::ALL);
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn search_sends_increasing_reqs_and_trims() {
        let mut vm = connected();
        assert_eq!(
            vm.intent(Intent::Search("  night drive ".into())),
            vec![Command::Search {
                req: 1,
                query: "night drive".into()
            }]
        );
        assert_eq!(vm.ui().query, "night drive");
        assert_eq!(vm.ui().search, SearchStatus::Pending);
        assert!(vm.take_dirty().contains(Dirty::SEARCH_STATUS));
        assert_eq!(search_req(&mut vm, "b"), 2);
        assert_eq!(search_req(&mut vm, "c"), 3);
    }

    #[test]
    fn empty_search_sends_nothing() {
        let mut vm = connected();
        assert!(vm.intent(Intent::Search("   ".into())).is_empty());
        assert_eq!(vm.ui().search, SearchStatus::Idle);
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn search_results_for_latest_req_are_shown() {
        let mut vm = connected();
        let req = search_req(&mut vm, "a");
        vm.take_dirty();
        vm.apply(Event::SearchResults {
            req,
            tracks: tracks(3),
        });
        assert_eq!(vm.ui().results.len(), 3);
        assert_eq!(vm.ui().search, SearchStatus::Done);
        let d = vm.take_dirty();
        assert!(d.contains(Dirty::RESULTS | Dirty::SEARCH_STATUS));
    }

    #[test]
    fn stale_search_results_are_ignored() {
        let mut vm = connected();
        let old = search_req(&mut vm, "a");
        let new = search_req(&mut vm, "b");
        vm.take_dirty();
        vm.apply(Event::SearchResults {
            req: old,
            tracks: tracks(5),
        });
        assert!(vm.ui().results.is_empty());
        assert_eq!(vm.ui().search, SearchStatus::Pending);
        assert!(vm.take_dirty().is_empty());
        vm.apply(Event::SearchResults {
            req: new,
            tracks: tracks(2),
        });
        assert_eq!(vm.ui().results.len(), 2);
        // A late answer to the old req after the new one landed is ignored too.
        vm.apply(Event::SearchResults {
            req: old,
            tracks: tracks(5),
        });
        assert_eq!(vm.ui().results.len(), 2);
    }

    #[test]
    fn search_failed_shows_error_and_keeps_results() {
        let mut vm = with_results(4);
        let req = search_req(&mut vm, "x");
        vm.take_dirty();
        vm.apply(Event::SearchFailed {
            req,
            error: "network error: timeout".into(),
        });
        assert_eq!(
            vm.ui().search,
            SearchStatus::Failed("network error: timeout".into())
        );
        assert_eq!(vm.ui().results.len(), 4);
        assert!(vm.take_dirty().contains(Dirty::SEARCH_STATUS));
        assert_eq!(
            vm.ui().search_status_text(),
            Some(("Search failed: network error: timeout".into(), true))
        );
    }

    #[test]
    fn stale_search_failure_is_ignored() {
        let mut vm = connected();
        let old = search_req(&mut vm, "a");
        let _new = search_req(&mut vm, "b");
        vm.take_dirty();
        vm.apply(Event::SearchFailed {
            req: old,
            error: "boom".into(),
        });
        assert_eq!(vm.ui().search, SearchStatus::Pending);
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn queue_changed_replaces_queue_and_current() {
        let mut vm = connected();
        vm.apply(Event::QueueChanged {
            tracks: tracks(8),
            current: Some(2),
        });
        assert_eq!(vm.ui().queue.len(), 8);
        assert_eq!(vm.ui().current, Some(2));
        assert_eq!(vm.take_dirty(), Dirty::QUEUE);
    }

    #[test]
    fn now_playing_sets_track_and_resets_position() {
        let mut vm = connected();
        vm.apply(Event::Position(Duration::from_secs(40)));
        vm.take_dirty();
        vm.apply(Event::NowPlaying {
            track: track("a", 225),
            duration: Some(Duration::from_secs(225)),
        });
        let ui = vm.ui();
        assert_eq!(ui.now_playing.as_ref().unwrap().title, "Title a");
        assert_eq!(ui.duration, Some(Duration::from_secs(225)));
        assert_eq!(ui.position, Duration::ZERO);
        assert_eq!(ui.total_text(), "3:45");
        let d = vm.take_dirty();
        assert!(d.contains(Dirty::NOW_PLAYING | Dirty::ELAPSED | Dirty::SEEK));
    }

    #[test]
    fn position_marks_elapsed_only_when_the_second_changes() {
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_millis(83_000)));
        assert_eq!(vm.take_dirty(), Dirty::ELAPSED | Dirty::SEEK);
        vm.apply(Event::Position(Duration::from_millis(83_250)));
        assert_eq!(vm.take_dirty(), Dirty::SEEK);
        vm.apply(Event::Position(Duration::from_millis(83_250)));
        assert!(vm.take_dirty().is_empty());
        vm.apply(Event::Position(Duration::from_millis(84_000)));
        assert_eq!(vm.take_dirty(), Dirty::ELAPSED | Dirty::SEEK);
        assert_eq!(vm.ui().elapsed_text(), "1:24");
    }

    #[test]
    fn progress_is_position_over_duration() {
        let mut vm = playing(200);
        vm.apply(Event::Position(Duration::from_secs(50)));
        assert!((vm.ui().progress() - 0.25).abs() < 1e-9);
        vm.apply(Event::Position(Duration::from_secs(500)));
        assert_eq!(vm.ui().progress(), 1.0);
        assert_eq!(connected().ui().progress(), 0.0);
    }

    #[test]
    fn state_change_marks_state_and_elapsed() {
        let mut vm = playing(225);
        vm.apply(Event::State(PlayState::Paused));
        assert_eq!(vm.ui().state, PlayState::Paused);
        assert_eq!(vm.take_dirty(), Dirty::STATE | Dirty::ELAPSED);
        vm.apply(Event::State(PlayState::Paused));
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn buffering_shows_in_elapsed_label() {
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_secs(10)));
        vm.apply(Event::State(PlayState::Buffering));
        assert_eq!(vm.ui().elapsed_text(), "buffering…");
        assert!(vm.ui().is_playing());
        vm.apply(Event::State(PlayState::Playing));
        assert_eq!(vm.ui().elapsed_text(), "0:10");
    }

    #[test]
    fn stopped_clears_now_playing() {
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_secs(10)));
        vm.take_dirty();
        vm.apply(Event::State(PlayState::Stopped));
        let ui = vm.ui();
        assert!(ui.now_playing.is_none());
        assert_eq!(ui.duration, None);
        assert_eq!(ui.position, Duration::ZERO);
        assert_eq!(ui.elapsed_text(), "");
        assert_eq!(ui.total_text(), "");
        let d = vm.take_dirty();
        assert!(d.contains(Dirty::STATE | Dirty::NOW_PLAYING | Dirty::ELAPSED | Dirty::SEEK));
    }

    #[test]
    fn volume_event_marks_only_on_change() {
        let mut vm = connected();
        vm.apply(Event::Volume(70));
        assert_eq!(vm.ui().volume, 70);
        assert_eq!(vm.take_dirty(), Dirty::VOLUME);
        vm.apply(Event::Volume(70));
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn track_failed_raises_error_toast() {
        let mut vm = connected();
        vm.apply(Event::TrackFailed {
            track: track("a", 1),
            error: "unplayable: private".into(),
        });
        let t = vm.ui().toast.clone().unwrap();
        assert!(t.error);
        assert_eq!(t.text, "Couldn't play “Title a”: unplayable: private");
        assert_eq!(vm.take_dirty(), Dirty::TOAST);
    }

    #[test]
    fn provider_broken_raises_error_toast() {
        let mut vm = connected();
        vm.apply(Event::ProviderBroken {
            provider: ProviderId("ytmusic".into()),
            detail: "3 consecutive failures".into(),
        });
        let t = vm.ui().toast.clone().unwrap();
        assert!(t.error);
        assert!(t.text.starts_with("YouTube Music stopped working"));
        assert!(t.text.contains("3 consecutive failures"));
    }

    #[test]
    fn new_toast_replaces_old_and_dismiss_matches_id() {
        let mut vm = connected();
        vm.apply(Event::TrackFailed {
            track: track("a", 1),
            error: "e".into(),
        });
        let first = vm.ui().toast.clone().unwrap().id;
        vm.apply(Event::TrackFailed {
            track: track("b", 1),
            error: "e".into(),
        });
        let second = vm.ui().toast.clone().unwrap();
        assert!(second.id > first);
        assert!(second.text.contains("Title b"));
        vm.take_dirty();
        // The first toast's timer firing late must not hide the second.
        vm.dismiss_toast(first);
        assert!(vm.ui().toast.is_some());
        assert!(vm.take_dirty().is_empty());
        vm.dismiss_toast(second.id);
        assert!(vm.ui().toast.is_none());
        assert_eq!(vm.take_dirty(), Dirty::TOAST);
    }

    #[test]
    fn play_result_replaces_queue_from_row_and_plays_first() {
        let mut vm = with_results(5);
        let cmds = vm.intent(Intent::PlayResult(2));
        assert_eq!(
            cmds,
            vec![
                Command::Enqueue {
                    tracks: vm.ui().results[2..].to_vec(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(0)
            ]
        );
        assert!(vm.intent(Intent::PlayResult(5)).is_empty());
        assert!(vm.intent(Intent::PlayResult(99)).is_empty());
    }

    #[test]
    fn append_result_enqueues_one_track() {
        let mut vm = with_results(5);
        assert_eq!(
            vm.intent(Intent::AppendResult(3)),
            vec![Command::Enqueue {
                tracks: vec![vm.ui().results[3].clone()],
                mode: EnqueueMode::Append
            }]
        );
        assert!(vm.intent(Intent::AppendResult(5)).is_empty());
    }

    #[test]
    fn play_queue_plays_index_in_range() {
        let mut vm = connected();
        vm.apply(Event::QueueChanged {
            tracks: tracks(3),
            current: None,
        });
        assert_eq!(vm.intent(Intent::PlayQueue(1)), vec![Command::PlayIndex(1)]);
        assert!(vm.intent(Intent::PlayQueue(3)).is_empty());
    }

    #[test]
    fn transport_intents_map_to_commands() {
        let mut vm = connected();
        assert_eq!(vm.intent(Intent::Toggle), vec![Command::TogglePause]);
        assert_eq!(vm.intent(Intent::Next), vec![Command::Next]);
        assert_eq!(vm.intent(Intent::Prev), vec![Command::Prev]);
    }

    #[test]
    fn seek_to_clamps_and_moves_position_now() {
        let mut vm = playing(225);
        assert_eq!(
            vm.intent(Intent::SeekTo(Duration::from_secs(100))),
            vec![Command::Seek(SeekTarget::Absolute(Duration::from_secs(
                100
            )))]
        );
        assert_eq!(vm.ui().position, Duration::from_secs(100));
        assert_eq!(vm.take_dirty(), Dirty::ELAPSED | Dirty::SEEK);
        assert_eq!(
            vm.intent(Intent::SeekTo(Duration::from_secs(900))),
            vec![Command::Seek(SeekTarget::Absolute(Duration::from_secs(
                225
            )))]
        );
    }

    #[test]
    fn seek_by_sends_relative_and_mirrors_session_clamping() {
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_secs(3)));
        assert_eq!(
            vm.intent(Intent::SeekBy(-SEEK_STEP_SECS)),
            vec![Command::Seek(SeekTarget::Relative(-5))]
        );
        assert_eq!(vm.ui().position, Duration::ZERO);
        vm.apply(Event::Position(Duration::from_secs(222)));
        assert_eq!(
            vm.intent(Intent::SeekBy(SEEK_STEP_SECS)),
            vec![Command::Seek(SeekTarget::Relative(5))]
        );
        assert_eq!(vm.ui().position, Duration::from_secs(225));
    }

    #[test]
    fn seek_without_a_track_sends_nothing() {
        let mut vm = connected();
        assert!(vm.intent(Intent::SeekTo(Duration::from_secs(1))).is_empty());
        assert!(vm.intent(Intent::SeekBy(5)).is_empty());
    }

    #[test]
    fn set_volume_clamps_updates_now_and_dedupes() {
        let mut vm = connected();
        vm.apply(Event::Volume(70));
        vm.take_dirty();
        assert_eq!(
            vm.intent(Intent::SetVolume(40)),
            vec![Command::SetVolume(40)]
        );
        assert_eq!(vm.ui().volume, 40);
        assert_eq!(vm.take_dirty(), Dirty::VOLUME);
        assert!(vm.intent(Intent::SetVolume(40)).is_empty());
        assert_eq!(
            vm.intent(Intent::SetVolume(250)),
            vec![Command::SetVolume(100)]
        );
    }

    #[test]
    fn volume_by_steps_and_clamps() {
        let mut vm = connected();
        vm.apply(Event::Volume(95));
        assert_eq!(
            vm.intent(Intent::VolumeBy(VOLUME_STEP)),
            vec![Command::SetVolume(100)]
        );
        assert!(vm.intent(Intent::VolumeBy(VOLUME_STEP)).is_empty());
        vm.apply(Event::Volume(5));
        assert_eq!(
            vm.intent(Intent::VolumeBy(-VOLUME_STEP)),
            vec![Command::SetVolume(0)]
        );
        // Two quick presses compound on the optimistic value.
        vm.apply(Event::Volume(50));
        vm.intent(Intent::VolumeBy(VOLUME_STEP));
        assert_eq!(
            vm.intent(Intent::VolumeBy(VOLUME_STEP)),
            vec![Command::SetVolume(70)]
        );
    }

    #[test]
    fn first_connect_resets_without_toast() {
        let mut vm = Vm::new();
        vm.take_dirty();
        vm.link(Link::Connected);
        assert_eq!(vm.ui().connection, Connection::Connected);
        assert!(vm.ui().toast.is_none());
        assert!(vm.ui().banner().is_none());
        assert!(vm.take_dirty().contains(Dirty::CONNECTION));
    }

    #[test]
    fn losing_connection_toasts_once_and_shows_banner() {
        let mut vm = playing(225);
        vm.link(Link::Lost("daemon closed the connection".into()));
        assert_eq!(
            vm.ui().connection,
            Connection::Reconnecting("daemon closed the connection".into())
        );
        let t = vm.ui().toast.clone().unwrap();
        assert!(t.error);
        assert_eq!(
            vm.ui().banner(),
            Some(Banner {
                text: "Player disconnected — reconnecting…",
                error: false
            })
        );
        assert!(vm.take_dirty().contains(Dirty::CONNECTION | Dirty::TOAST));
        // A failed retry reports Lost again; no second toast.
        vm.link(Link::Lost("connection refused".into()));
        assert_eq!(vm.ui().toast.as_ref().unwrap().id, t.id);
    }

    #[test]
    fn initial_connect_failure_shows_banner_without_toast() {
        let mut vm = Vm::new();
        vm.link(Link::Lost("not found".into()));
        assert!(vm.ui().banner().is_some());
        assert!(vm.ui().toast.is_none());
    }

    #[test]
    fn reconnect_toasts_and_clears_stale_player_state() {
        let mut vm = playing(225);
        vm.apply(Event::QueueChanged {
            tracks: tracks(3),
            current: Some(1),
        });
        vm.link(Link::Lost("gone".into()));
        vm.take_dirty();
        vm.link(Link::Connected);
        let ui = vm.ui();
        assert_eq!(ui.connection, Connection::Connected);
        assert!(ui.now_playing.is_none());
        assert!(ui.queue.is_empty());
        assert_eq!(ui.current, None);
        assert_eq!(ui.state, PlayState::Stopped);
        let t = ui.toast.clone().unwrap();
        assert!(!t.error);
        assert_eq!(t.text, "Player reconnected");
        let d = vm.take_dirty();
        assert!(d.contains(
            Dirty::CONNECTION | Dirty::QUEUE | Dirty::NOW_PLAYING | Dirty::STATE | Dirty::TOAST
        ));
    }

    #[test]
    fn snapshot_after_reconnect_rebuilds_state() {
        let mut vm = playing(225);
        vm.link(Link::Lost("gone".into()));
        vm.link(Link::Connected);
        let q = tracks(4);
        vm.apply(Event::QueueChanged {
            tracks: q.clone(),
            current: Some(3),
        });
        vm.apply(Event::NowPlaying {
            track: q[3].clone(),
            duration: q[3].duration,
        });
        vm.apply(Event::State(PlayState::Paused));
        vm.apply(Event::Volume(30));
        vm.apply(Event::Position(Duration::from_secs(61)));
        let ui = vm.ui();
        assert_eq!(ui.current, Some(3));
        assert_eq!(ui.now_playing.as_ref(), Some(&q[3]));
        assert_eq!(ui.state, PlayState::Paused);
        assert_eq!(ui.volume, 30);
        assert_eq!(ui.elapsed_text(), "1:01");
    }

    #[test]
    fn disconnect_fails_pending_search_and_reconnect_idles_it() {
        let mut vm = connected();
        let req = search_req(&mut vm, "a");
        vm.link(Link::Lost("gone".into()));
        assert_eq!(
            vm.ui().search,
            SearchStatus::Failed("player disconnected".into())
        );
        // The old daemon's answer can no longer arrive; a stray one is ignored.
        vm.apply(Event::SearchResults {
            req,
            tracks: tracks(1),
        });
        assert!(vm.ui().results.is_empty());

        let mut vm = connected();
        search_req(&mut vm, "a");
        vm.link(Link::Connected);
        assert_eq!(vm.ui().search, SearchStatus::Idle);
    }

    #[test]
    fn out_of_date_shows_error_banner() {
        let mut vm = Vm::new();
        vm.link(Link::OutOfDate);
        assert_eq!(vm.ui().connection, Connection::OutOfDate);
        assert_eq!(
            vm.ui().banner(),
            Some(Banner {
                text: "Player is out of date — quit it from the menu bar",
                error: true
            })
        );
    }

    #[test]
    fn demo_link_has_no_banner() {
        let mut vm = Vm::new();
        vm.link(Link::Demo);
        assert_eq!(vm.ui().connection, Connection::Demo);
        assert!(vm.ui().banner().is_none());
    }

    #[test]
    fn results_empty_state_texts() {
        let mut vm = connected();
        assert_eq!(
            vm.ui().results_empty(),
            Some(EmptyText {
                heading: "Search YouTube Music".into(),
                sub: "Type a song, artist or album".into(),
                action: "Start a search",
            })
        );
        let req = search_req(&mut vm, "zzz");
        vm.apply(Event::SearchResults {
            req,
            tracks: Vec::new(),
        });
        assert_eq!(
            vm.ui().results_empty(),
            Some(EmptyText {
                heading: "No results".into(),
                sub: "Nothing matched “zzz”".into(),
                action: "Edit search",
            })
        );
        assert_eq!(with_results(1).ui().results_empty(), None);
    }

    #[test]
    fn search_status_text_by_state() {
        let mut vm = connected();
        assert_eq!(vm.ui().search_status_text(), None);
        search_req(&mut vm, "a");
        assert_eq!(
            vm.ui().search_status_text(),
            Some(("Searching…".into(), false))
        );
    }

    #[test]
    fn fmt_time_formats() {
        assert_eq!(fmt_time(Duration::ZERO), "0:00");
        assert_eq!(fmt_time(Duration::from_millis(83_900)), "1:23");
        assert_eq!(fmt_time(Duration::from_secs(225)), "3:45");
        assert_eq!(fmt_time(Duration::from_secs(3600 + 62)), "1:01:02");
    }

    #[test]
    fn artists_joins_with_commas() {
        assert_eq!(artists(&track("a", 1)), "Artist a, Guest");
    }

    #[test]
    fn palette_filter_is_case_insensitive_substring() {
        assert_eq!(PaletteAction::filter(""), PaletteAction::ALL.to_vec());
        assert_eq!(
            PaletteAction::filter("VOL"),
            vec![PaletteAction::VolumeUp, PaletteAction::VolumeDown]
        );
        assert_eq!(
            PaletteAction::filter("show"),
            vec![PaletteAction::ShowSearch, PaletteAction::ShowQueue]
        );
        assert_eq!(
            PaletteAction::filter("search"),
            vec![PaletteAction::FocusSearch, PaletteAction::ShowSearch]
        );
        assert!(PaletteAction::filter("xyz").is_empty());
    }

    #[test]
    fn dirty_set_operations() {
        let d = Dirty::QUEUE | Dirty::TOAST;
        assert!(d.contains(Dirty::QUEUE));
        assert!(!d.contains(Dirty::RESULTS));
        assert!(!d.contains(Dirty::QUEUE | Dirty::RESULTS));
        assert!(Dirty::NONE.is_empty());
        assert!(Dirty::ALL.contains(d));
    }
}
