//! View model: folds daemon events and connection status into `UiState`, and maps user
//! intents to `Command`s. No AppKit types, so it is unit-tested on every OS.

use std::io;
use std::ops::{BitOr, BitOrAssign};
use std::time::{Duration, Instant};

use requiem_core::{
    AccountState, Collection, Command, EnqueueMode, Event, Item, Page, PlayState, ProviderId,
    SeekTarget, Shelf, Track, TrackId,
};

pub const SEEK_STEP_SECS: i64 = 5;
pub const VOLUME_STEP: i16 = 10;
pub const TOAST_DURATION: Duration = Duration::from_secs(4);
/// How long a local seek or volume change outranks the daemon's echoes. Without it, events
/// the daemon sent before it saw the command snap the control back for a moment.
pub const ECHO_HOLD: Duration = Duration::from_millis(500);
/// A position this close to a local seek's target confirms it; engine ticks rarely land
/// exactly on it.
const SEEK_AGREES: Duration = Duration::from_secs(1);
/// No banner for a connection this young, so a connect or reconnect under a second does not
/// flash one.
pub const BANNER_GRACE: Duration = Duration::from_millis(800);
/// After this long without a connection, the banner adds the reason and what to do.
pub const STUCK_AFTER: Duration = Duration::from_secs(30);

// The old player's menu item may still be named "Quit requiem", so this names none.
const OUT_OF_DATE: &str = "An older requiem player is still running. Quit it from its \
                           menu-bar icon; this window reconnects on its own.";
const STUCK_HINT: &str = "If this persists, choose Quit and Stop Music from the requiem \
                          menu-bar icon, then reopen requiem.";

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
    /// The connection or its banner.
    pub const CONNECTION: Dirty = Dirty(1 << 9);
    pub const HOME: Dirty = Dirty(1 << 10);
    pub const LIBRARY: Dirty = Dirty(1 << 11);
    pub const COLLECTION: Dirty = Dirty(1 << 12);
    /// The account state, which the Library page and the palette show.
    pub const ACCOUNT: Dirty = Dirty(1 << 13);
    pub const ALL: Dirty = Dirty((1 << 14) - 1);

    pub fn contains(self, other: Dirty) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn intersects(self, other: Dirty) -> bool {
        self.0 & other.0 != 0
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
    /// Not connected yet; the backend retries with backoff.
    Connecting,
    Connected,
    /// Lost after a connection; the backend retries with backoff. Holds the reason.
    Reconnecting(LinkError),
    /// The daemon speaks another protocol version; the backend retries until it is gone.
    OutOfDate,
    /// `--demo`: fixed fake data, no daemon.
    Demo,
}

/// Connection changes reported by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connected,
    Lost(LinkError),
    OutOfDate,
    Demo,
}

/// Why the player can't be reached, as the banner puts it. The raw error goes to stderr only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// Nothing listens on the socket.
    NotRunning,
    /// A timeout, or the connection closed.
    NotResponding,
    Unreachable,
}

impl LinkError {
    pub fn from_io(kind: io::ErrorKind) -> LinkError {
        use io::ErrorKind as K;
        match kind {
            K::NotFound | K::ConnectionRefused => LinkError::NotRunning,
            K::TimedOut
            | K::UnexpectedEof
            | K::ConnectionReset
            | K::ConnectionAborted
            | K::BrokenPipe
            | K::NotConnected => LinkError::NotResponding,
            _ => LinkError::Unreachable,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            LinkError::NotRunning => "The player isn’t running.",
            LinkError::NotResponding => "The player stopped responding.",
            LinkError::Unreachable => "The player couldn’t be reached.",
        }
    }
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
    /// The label of the toast's action button, if it has one.
    pub action: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Search(String),
    /// Replace the queue with the tracks of `list` from item `from` on, and play the first.
    PlayFrom {
        list: List,
        from: usize,
    },
    /// Append item `index` of `list`.
    Append {
        list: List,
        index: usize,
    },
    AppendAll(List),
    PlayQueue(usize),
    Toggle,
    Next,
    Prev,
    SeekTo(Duration),
    SeekBy(i64),
    SetVolume(u8),
    VolumeBy(i16),
    /// The toast's Undo, or Cmd+Z: restores the queue a result replaced or Clear emptied.
    Undo,
    /// `selected` is the queue row selected now; the selection follows that track.
    EditQueue {
        edit: QueueEdit,
        selected: Option<usize>,
    },
    /// Shows `Place`, fetching its page if it has not loaded.
    Visit(Place),
    OpenCollection {
        collection: Collection,
        from: Place,
    },
    /// Fetches the current page again, if it is a browse page.
    Refresh,
    SignIn,
    SignOut,
}

/// Where the window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Home,
    Search,
    Library,
    Queue,
    /// A playlist or album opened from Home or Library.
    Collection,
}

/// A list of items an intent plays or appends from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Results,
    /// The tracks of the open collection.
    Collection,
    /// A Home shelf, by index. Its items may include collections, which the tracks skip.
    Shelf(usize),
}

/// A browse page that is fetched on its own and answered by its own req.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Home,
    Library,
    Collection,
}

impl Slot {
    const ALL: [Slot; 3] = [Slot::Home, Slot::Library, Slot::Collection];
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Load {
    /// Not requested yet, or dropped for a reconnect.
    #[default]
    Idle,
    Loading,
    Done,
    Failed {
        error: String,
        sign_in_required: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Browse {
    pub load: Load,
    pub shelves: Vec<Shelf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CollectionPage {
    pub collection: Collection,
    /// The page it was opened from, which Back returns to.
    pub from: Place,
    pub browse: Browse,
    /// Every track of the shelves, which the page lists.
    pub tracks: Vec<Track>,
}

/// What a browse page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageView {
    Loading,
    Failed(String),
    Ready,
}

/// What the Library page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryView {
    /// No account state yet.
    Loading,
    Unavailable(String),
    SignedOut,
    Pending {
        verification_url: String,
        user_code: String,
        expires_at_unix: u64,
    },
    SignedIn {
        name: Option<String>,
        page: PageView,
    },
}

impl Browse {
    pub fn view(&self) -> PageView {
        match &self.load {
            Load::Idle | Load::Loading => PageView::Loading,
            Load::Failed { error, .. } => PageView::Failed(error.clone()),
            Load::Done => PageView::Ready,
        }
    }
}

/// What the sign-in panel tells the user to open: the whole address, which is where the code
/// is entered, without its scheme.
pub fn sign_in_hint(url: &str) -> String {
    let shown = url.split_once("://").map_or(url, |(_, rest)| rest);
    format!(
        "On any device, go to {} and enter this code.",
        shown.trim_end_matches('/')
    )
}

/// "Code expires in 3 min": whole minutes, rounded up, so it never reads 0 while valid.
pub fn expires_text(expires_at_unix: u64, now_unix: u64) -> String {
    match expires_at_unix.saturating_sub(now_unix) {
        0 => "Code expired".to_owned(),
        left => format!("Code expires in {} min", left.div_ceil(60)),
    }
}

/// A change to the queue: to one row, by index, or to all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEdit {
    Remove(usize),
    MoveUp(usize),
    MoveDown(usize),
    Clear,
}

/// Which controls take input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls {
    /// Previous, play/pause and next: connected and the queue has tracks.
    pub transport: bool,
    /// The seek bar: connected and a track is loaded.
    pub seek: bool,
    /// Volume, and playing or adding rows: connected.
    pub player: bool,
}

/// The visible table's selection, as far as actions care.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Selection {
    /// A row is selected.
    pub row: bool,
    /// The selected row is a search result, so it can be added.
    pub result: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    pub connection: Connection,
    /// Shown above the pages; it trails the connection by `BANNER_GRACE`.
    pub banner: Option<Banner>,
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
    pub place: Place,
    pub home: Browse,
    pub library: Browse,
    pub collection: Option<CollectionPage>,
    /// The daemon's account states, by provider; the first provider's is the one shown.
    pub accounts: Vec<(ProviderId, AccountState)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            connection: Connection::Connecting,
            banner: None,
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
            place: Place::Search,
            home: Browse::default(),
            library: Browse::default(),
            collection: None,
            accounts: Vec::new(),
        }
    }
}

impl UiState {
    pub fn elapsed_text(&self) -> String {
        if self.state == PlayState::Buffering {
            return "Buffering…".to_owned();
        }
        if self.now_playing.is_none() {
            return String::new();
        }
        fmt_time(self.position)
    }

    /// The track's listed duration, as its row shows it, over the engine's measured one, so
    /// the bar and the row agree.
    pub fn total_text(&self) -> String {
        match &self.now_playing {
            Some(t) => t
                .duration
                .or(self.duration)
                .map(fmt_time)
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    /// The seek bar's value as VoiceOver reads it, "1:23 of 3:45"; empty with nothing loaded.
    pub fn seek_spoken(&self) -> String {
        if self.now_playing.is_none() {
            return String::new();
        }
        let at = fmt_time(self.position);
        match self.total_text() {
            total if total.is_empty() => at,
            total => format!("{at} of {total}"),
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

    /// Where a seek to `fraction` of the bar lands; `None` until the duration is known.
    pub fn seek_target(&self, fraction: f64) -> Option<Duration> {
        self.duration.map(|d| d.mul_f64(fraction.clamp(0.0, 1.0)))
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.state, PlayState::Playing | PlayState::Buffering)
    }

    pub fn controls(&self) -> Controls {
        let live = matches!(self.connection, Connection::Connected | Connection::Demo);
        Controls {
            transport: live && !self.queue.is_empty(),
            seek: live && self.now_playing.is_some(),
            player: live,
        }
    }

    /// Whether `action` would do anything. The menus enable their items, and the palette its
    /// rows, by this.
    pub fn can_run(&self, action: PaletteAction, selection: Selection) -> bool {
        let can = self.controls();
        match action {
            PaletteAction::PlayPause | PaletteAction::Next | PaletteAction::Previous => {
                can.transport
            }
            PaletteAction::PlaySelected => can.player && selection.row,
            PaletteAction::AddToQueue => can.player && selection.result,
            PaletteAction::VolumeUp | PaletteAction::VolumeDown => can.player,
            PaletteAction::ClearQueue => self.edit_command(QueueEdit::Clear).is_some(),
            PaletteAction::FocusSearch
            | PaletteAction::ShowHome
            | PaletteAction::ShowSearch
            | PaletteAction::ShowLibrary
            | PaletteAction::ShowQueue => true,
            PaletteAction::Refresh => can.player && self.refreshable(),
            PaletteAction::SignIn => can.player && self.library_view() == LibraryView::SignedOut,
            PaletteAction::SignOut => {
                can.player && matches!(self.account_state(), Some(AccountState::SignedIn { .. }))
            }
        }
    }

    /// The first provider's account state, which the Library page and Sign In act on.
    pub fn account_state(&self) -> Option<&AccountState> {
        self.accounts.first().map(|(_, s)| s)
    }

    /// The current place is a browse page that a fetch would fill.
    fn refreshable(&self) -> bool {
        match self.place {
            Place::Home => true,
            Place::Library => matches!(self.account_state(), Some(AccountState::SignedIn { .. })),
            Place::Collection => self.collection.is_some(),
            Place::Search | Place::Queue => false,
        }
    }

    pub fn library_view(&self) -> LibraryView {
        let Some(state) = self.account_state() else {
            return LibraryView::Loading;
        };
        match state {
            AccountState::Unavailable(text) => LibraryView::Unavailable(text.clone()),
            AccountState::Pending {
                verification_url,
                user_code,
                expires_at_unix,
            } => LibraryView::Pending {
                verification_url: verification_url.clone(),
                user_code: user_code.clone(),
                expires_at_unix: *expires_at_unix,
            },
            AccountState::SignedOut => LibraryView::SignedOut,
            AccountState::SignedIn { name } => match self.library.load {
                Load::Failed {
                    sign_in_required: true,
                    ..
                } => LibraryView::SignedOut,
                _ => LibraryView::SignedIn {
                    name: name.clone(),
                    page: self.library.view(),
                },
            },
        }
    }

    /// The playlists and albums of the Library, in shelf order: the rows of its list.
    pub fn library_collections(&self) -> Vec<&Collection> {
        self.library
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .filter_map(|i| match i {
                Item::Collection(c) => Some(c),
                Item::Track(_) => None,
            })
            .collect()
    }

    /// The tracks of `list` from item `from` on.
    fn list_tracks(&self, list: List, from: usize) -> Vec<Track> {
        match list {
            List::Results => self.results.get(from..).unwrap_or_default().to_vec(),
            List::Collection => self
                .collection
                .as_ref()
                .and_then(|c| c.tracks.get(from..))
                .unwrap_or_default()
                .to_vec(),
            List::Shelf(i) => self
                .home
                .shelves
                .get(i)
                .and_then(|s| s.items.get(from..))
                .unwrap_or_default()
                .iter()
                .filter_map(|item| match item {
                    Item::Track(t) => Some(t.clone()),
                    Item::Collection(_) => None,
                })
                .collect(),
        }
    }

    /// Item `index` of `list`, if it is a track.
    fn list_track(&self, list: List, index: usize) -> Option<Track> {
        match list {
            List::Shelf(i) => match self.home.shelves.get(i)?.items.get(index)? {
                Item::Track(t) => Some(t.clone()),
                Item::Collection(_) => None,
            },
            _ => self.list_tracks(list, index).into_iter().next(),
        }
    }

    /// Whether "Quit and Stop Music" has a player to stop.
    pub fn can_stop_player(&self) -> bool {
        self.connection == Connection::Connected
    }

    /// The command `edit` sends, or `None` when it does nothing: disconnected, a row out of
    /// range, a move past either end, or clearing an empty queue. The menus enable an edit
    /// exactly when this is `Some`, and the session ignores the rest without an answer.
    pub fn edit_command(&self, edit: QueueEdit) -> Option<Command> {
        if !self.controls().player {
            return None;
        }
        let len = self.queue.len();
        match edit {
            QueueEdit::Remove(i) => (i < len).then_some(Command::Remove(i)),
            QueueEdit::MoveUp(i) => {
                (i > 0 && i < len).then(|| Command::Move { from: i, to: i - 1 })
            }
            QueueEdit::MoveDown(i) => (i + 1 < len).then(|| Command::Move { from: i, to: i + 1 }),
            QueueEdit::Clear => (len > 0).then_some(Command::Clear),
        }
    }

    /// The Queue nav item's count; `None` when the queue is empty.
    pub fn queue_badge(&self) -> Option<String> {
        (!self.queue.is_empty()).then(|| self.queue.len().to_string())
    }

    /// The results area's empty state, when there are no results.
    pub fn results_empty(&self) -> Option<EmptyText> {
        if !self.results.is_empty() {
            return None;
        }
        Some(if self.search == SearchStatus::Done {
            EmptyText {
                heading: "No results".to_owned(),
                sub: format!("Nothing matched “{}”", self.query),
                action: Some("Edit search"),
            }
        } else {
            EmptyText {
                heading: "Search YouTube Music".to_owned(),
                sub: "Type a song, artist or album, then press Return".to_owned(),
                action: None,
            }
        })
    }

    pub fn search_status_text(&self) -> Option<(String, bool)> {
        match &self.search {
            SearchStatus::Pending => Some(("Searching…".to_owned(), false)),
            SearchStatus::Failed(e) => Some((
                format!("Search failed: {e}. Press Return to try again."),
                true,
            )),
            SearchStatus::Idle | SearchStatus::Done => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub text: String,
    pub error: bool,
}

/// An empty state: heading, subtext and its primary action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyText {
    pub heading: String,
    pub sub: String,
    /// The label of the button that focuses the search field, if there is one.
    pub action: Option<&'static str>,
}

#[derive(Debug, Default)]
pub struct Vm {
    ui: UiState,
    /// The newest req of search and browse alike, so no two share one.
    last_req: u64,
    pending_req: Option<u64>,
    /// The req each browse page waits on, by `Slot`. A page drops answers to any other req, but
    /// only its own newer fetch supersedes it: a slow Home answer is not lost to a Library load.
    pending_browse: [Option<u64>; 3],
    last_toast: u64,
    dirty: Dirty,
    seek_hold: Option<Hold<Duration>>,
    volume_hold: Option<Hold<u8>>,
    /// Commands raised by events or link changes rather than by an intent.
    outbox: Vec<Command>,
    /// When the current spell without a connection began; `None` while connected.
    offline_since: Option<Instant>,
    /// Why the last connect attempt before the first connection failed.
    connect_error: Option<LinkError>,
    /// The banner already shows the reason and the hint.
    stuck: bool,
    /// The queue a replace or clear left behind. It outlives its toast, until the queue
    /// changes again or the undo is used.
    undo: Option<Undo>,
    /// Where to seek once the track an undo restarted begins.
    restore: Option<Restore>,
    /// Where the selected queue row lands once the queue shows the user's edit.
    select_after: Option<usize>,
    /// The queue row to select now; the view takes it once.
    queue_selection: Option<usize>,
    /// The toast of a `TrackFailed` whose outcome the next event tells.
    failed_toast: Option<u64>,
}

#[derive(Debug)]
struct Undo {
    toast: u64,
    /// The Undo menu item's title.
    title: &'static str,
    /// The queue the change being undone leaves; any other queue ends the offer.
    after: Vec<TrackId>,
    /// The session has answered with `after`. Until then, the queue as it was is news sent
    /// before the change, not a change.
    landed: bool,
    tracks: Vec<Track>,
    /// The playing index and position, when a track was loaded.
    playing: Option<(usize, Duration)>,
    /// That track was paused.
    paused: bool,
}

#[derive(Debug)]
struct Restore {
    index: usize,
    track: TrackId,
    position: Duration,
}

/// A local value that wins over disagreeing echoes until `until`.
#[derive(Debug, Clone, Copy)]
struct Hold<T> {
    target: T,
    until: Instant,
}

impl<T> Hold<T> {
    fn new(target: T, now: Instant) -> Option<Self> {
        Some(Hold {
            target,
            until: now + ECHO_HOLD,
        })
    }
}

/// Whether to drop an echo: a hold is running and the echo disagrees with it. An agreeing
/// echo, or any echo after the deadline, ends the hold.
fn held<T>(hold: &mut Option<Hold<T>>, agrees: impl FnOnce(&T) -> bool, now: Instant) -> bool {
    match hold {
        Some(h) if now < h.until && !agrees(&h.target) => true,
        Some(_) => {
            *hold = None;
            false
        }
        None => false,
    }
}

impl Vm {
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    /// `new` with the clock passed in.
    pub fn new_at(now: Instant) -> Self {
        Self {
            dirty: Dirty::ALL,
            offline_since: Some(now),
            ..Self::default()
        }
    }

    pub fn ui(&self) -> &UiState {
        &self.ui
    }

    /// Returns the regions changed since the last call, and clears them. Until then changes
    /// accumulate, so a window that skips renders while hidden loses none.
    pub fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    /// The queue row to select after an edit, once the queue shows it; `None` leaves the
    /// selection alone.
    pub fn take_queue_selection(&mut self) -> Option<usize> {
        self.queue_selection.take()
    }

    /// The Undo menu item's title while there is a queue change to undo.
    pub fn undo_title(&self) -> Option<&'static str> {
        self.undo.as_ref().map(|u| u.title)
    }

    /// Commands to send that no intent returned: a search resent on reconnect, or the seek
    /// that finishes an undo.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    /// When the banner next changes on its own; call `tick` then.
    pub fn next_tick(&self) -> Option<Instant> {
        let since = self.offline_since?;
        if self.ui.banner.is_none() {
            Some(since + BANNER_GRACE)
        } else if !self.stuck {
            Some(since + STUCK_AFTER)
        } else {
            None
        }
    }

    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    /// `tick` with the clock passed in.
    pub fn tick_at(&mut self, now: Instant) {
        self.refresh_banner(now);
    }

    pub fn apply(&mut self, ev: Event) {
        self.apply_at(ev, Instant::now());
    }

    /// `apply` with the clock passed in.
    pub fn apply_at(&mut self, ev: Event, now: Instant) {
        // The session follows a `TrackFailed` at once with the outcome: the next track's
        // `QueueChanged`, or a stop.
        let failed = self.failed_toast.take();
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
                // The next queue shows the edit: the session answers an edit at once, and
                // `edit_command` sends none it would ignore.
                if let Some(row) = self.select_after.take() {
                    self.queue_selection = tracks.len().checked_sub(1).map(|last| row.min(last));
                }
                if let Some(u) = &mut self.undo {
                    // Ids, not tracks: the session may fill in details of the tracks it was sent.
                    let ids = || tracks.iter().map(|t| &t.id);
                    if u.after.iter().eq(ids()) {
                        u.landed = true;
                    } else if u.landed || !u.tracks.iter().map(|t| &t.id).eq(ids()) {
                        self.withdraw_undo();
                    }
                }
                self.ui.queue = tracks;
                self.ui.current = current;
                self.dirty |= Dirty::QUEUE;
                self.amend_failure(failed, "Skipped to the next track.");
            }
            Event::NowPlaying { track, duration } => {
                self.seek_hold = None;
                let restore = (self.restore.take())
                    .filter(|r| r.track == track.id && self.ui.current == Some(r.index));
                self.ui.now_playing = Some(track);
                self.ui.duration = duration;
                self.set_position(Duration::ZERO);
                self.dirty |= Dirty::NOW_PLAYING | Dirty::ELAPSED | Dirty::SEEK;
                if let Some(r) = restore {
                    let seek = self.seek_to(r.position, now);
                    self.outbox.extend(seek);
                }
            }
            Event::Position(at) => {
                if !held(&mut self.seek_hold, |&t| at.abs_diff(t) <= SEEK_AGREES, now) {
                    self.set_position(at);
                }
            }
            Event::State(state) => {
                if state == PlayState::Stopped {
                    self.amend_failure(failed, "Playback stopped.");
                }
                self.set_state(state);
            }
            Event::Volume(v) => {
                if !held(&mut self.volume_hold, |&t| t == v, now) {
                    self.set_volume(v);
                }
            }
            Event::TrackFailed { track, error } => {
                self.toast(format!("Couldn’t play “{}”: {error}", track.title), true);
                self.failed_toast = Some(self.last_toast);
            }
            Event::BrowseResults { req, shelves, .. } => {
                if let Some(slot) = self.take_browse(req) {
                    self.land(slot, shelves);
                }
            }
            Event::BrowseFailed {
                req,
                error,
                sign_in_required,
            } => {
                if let Some(slot) = self.take_browse(req) {
                    self.set_browse(
                        slot,
                        Browse {
                            load: Load::Failed {
                                error,
                                sign_in_required,
                            },
                            shelves: Vec::new(),
                        },
                    );
                }
            }
            Event::Account { provider, state } => self.set_account(provider, state),
            Event::AccountError { error, .. } => self.toast(error, true),
            // The detail is for stderr, which the backend writes it to.
            Event::ProviderBroken { provider, .. } => {
                self.toast(
                    format!(
                        "{} stopped working; requiem may need an update",
                        provider_name(&provider.0)
                    ),
                    true,
                );
            }
        }
    }

    pub fn link(&mut self, link: Link) {
        self.link_at(link, Instant::now());
    }

    /// `link` with the clock passed in.
    pub fn link_at(&mut self, link: Link, now: Instant) {
        let was = std::mem::replace(&mut self.ui.connection, Connection::Connecting);
        let connected = matches!(link, Link::Connected);
        self.ui.connection = match link {
            Link::Connected => {
                let outage = matches!(was, Connection::Reconnecting(_)) && self.ui.banner.is_some();
                if outage || was == Connection::OutOfDate {
                    self.toast("Player reconnected".to_owned(), false);
                }
                // The snapshot that follows rebuilds player state, but omits `NowPlaying`
                // when nothing is loaded, so stale state must not survive the reconnect.
                self.reset_player();
                self.offline_since = None;
                self.connect_error = None;
                if self.pending_req.is_some() {
                    self.resend_search();
                }
                Connection::Connected
            }
            Link::Lost(reason) => match was {
                Connection::Connecting => {
                    self.connect_error = Some(reason);
                    Connection::Connecting
                }
                Connection::Reconnecting(_) => Connection::Reconnecting(reason),
                _ => {
                    self.offline_since = Some(now);
                    self.restore = None;
                    Connection::Reconnecting(reason)
                }
            },
            Link::OutOfDate => {
                self.offline_since = None;
                Connection::OutOfDate
            }
            Link::Demo => {
                self.offline_since = None;
                Connection::Demo
            }
        };
        if self.ui.connection != was {
            self.dirty |= Dirty::CONNECTION;
        }
        if connected {
            self.reset_browse();
            let fetch = self.fetch_place();
            self.outbox.extend(fetch);
        }
        self.refresh_banner(now);
    }

    pub fn intent(&mut self, intent: Intent) -> Vec<Command> {
        self.intent_at(intent, Instant::now())
    }

    /// `intent` with the clock passed in. Intents for controls that `UiState::controls`
    /// disables send nothing.
    pub fn intent_at(&mut self, intent: Intent, now: Instant) -> Vec<Command> {
        let can = self.ui.controls();
        match intent {
            Intent::Search(query) => {
                let query = query.trim();
                if query.is_empty() {
                    return Vec::new();
                }
                self.ui.query = query.to_owned();
                self.search_command()
            }
            Intent::PlayFrom { list, from } if can.player => self.play_from(list, from),
            Intent::Append { list, index } if can.player => match self.ui.list_track(list, index) {
                Some(t) => enqueue_append(vec![t]),
                None => Vec::new(),
            },
            Intent::AppendAll(list) if can.player => enqueue_append(self.ui.list_tracks(list, 0)),
            Intent::PlayQueue(i) if can.player && i < self.ui.queue.len() => {
                vec![Command::PlayIndex(i)]
            }
            Intent::Toggle if can.transport => vec![Command::TogglePause],
            Intent::Next if can.transport => vec![Command::Next],
            Intent::Prev if can.transport => vec![Command::Prev],
            Intent::SeekTo(to) if can.seek => {
                let to = self.ui.duration.map_or(to, |d| to.min(d));
                self.seek_to(to, now)
            }
            Intent::SeekBy(secs) if can.seek => {
                // Mirrors the session's arithmetic so the bar moves before the daemon answers.
                let step = Duration::from_secs(secs.unsigned_abs());
                let to = if secs >= 0 {
                    self.ui.position.saturating_add(step)
                } else {
                    self.ui.position.saturating_sub(step)
                };
                let to = self.ui.duration.map_or(to, |d| to.min(d));
                self.set_position(to);
                self.seek_hold = Hold::new(to, now);
                vec![Command::Seek(SeekTarget::Relative(secs))]
            }
            Intent::SetVolume(v) if can.player => self.volume_command(v.min(100), now),
            Intent::VolumeBy(delta) if can.player => {
                let v = (i16::from(self.ui.volume) + delta).clamp(0, 100);
                self.volume_command(v as u8, now)
            }
            Intent::Undo if can.player => self.undo(),
            Intent::EditQueue { edit, selected } => {
                let Some(cmd) = self.ui.edit_command(edit) else {
                    return Vec::new();
                };
                self.select_after = selected.and_then(|s| follow(s, &cmd));
                if edit == QueueEdit::Clear {
                    let text = if self.ui.is_playing() {
                        "Queue cleared and playback stopped"
                    } else {
                        "Queue cleared"
                    };
                    self.offer_undo(text, "Undo Clear Queue", Vec::new());
                }
                // Showing the queue selects the playing row, so one stray Delete would drop the
                // track; any other row is cheap to re-add and undoing it would reload the track.
                if let QueueEdit::Remove(i) = edit
                    && self.ui.current == Some(i)
                    && let Some(track) = self.ui.queue.get(i)
                {
                    let text = format!("Removed “{}”", track.title);
                    let mut after: Vec<TrackId> =
                        self.ui.queue.iter().map(|t| t.id.clone()).collect();
                    after.remove(i);
                    self.offer_undo(&text, "Undo Remove from Queue", after);
                }
                vec![cmd]
            }
            Intent::Visit(place) => {
                self.ui.place = place;
                self.fetch_place()
            }
            Intent::OpenCollection { collection, from } => {
                self.ui.collection = Some(CollectionPage {
                    collection,
                    from,
                    browse: Browse::default(),
                    tracks: Vec::new(),
                });
                self.pending_browse[Slot::Collection as usize] = None;
                self.dirty |= Dirty::COLLECTION;
                self.ui.place = Place::Collection;
                self.fetch_place()
            }
            Intent::Refresh => {
                let slot = match self.ui.place {
                    Place::Home => Slot::Home,
                    Place::Library => Slot::Library,
                    Place::Collection => Slot::Collection,
                    Place::Search | Place::Queue => return Vec::new(),
                };
                self.fetch(slot).into_iter().collect()
            }
            Intent::SignIn if can.player => self
                .account_provider()
                .map(|provider| Command::SignIn { provider })
                .into_iter()
                .collect(),
            Intent::SignOut if can.player => self
                .account_provider()
                .map(|provider| Command::SignOut { provider })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn dismiss_toast(&mut self, id: u64) {
        if self.ui.toast.as_ref().is_some_and(|t| t.id == id) {
            self.ui.toast = None;
            self.dirty |= Dirty::TOAST;
        }
    }

    fn search_command(&mut self) -> Vec<Command> {
        self.last_req += 1;
        self.pending_req = Some(self.last_req);
        self.ui.search = SearchStatus::Pending;
        self.dirty |= Dirty::SEARCH_STATUS;
        vec![Command::Search {
            req: self.last_req,
            query: self.ui.query.clone(),
        }]
    }

    /// A search sent while disconnected was dropped, and one in flight when the connection
    /// went has no answer coming; both go again under a new req.
    fn resend_search(&mut self) {
        let cmds = self.search_command();
        self.outbox.extend(cmds);
    }

    fn play_from(&mut self, list: List, from: usize) -> Vec<Command> {
        let rest = self.ui.list_tracks(list, from);
        if rest.is_empty() {
            return Vec::new();
        }
        if !self.ui.queue.is_empty() {
            let after = rest.iter().map(|t| t.id.clone()).collect();
            self.offer_undo("Queue replaced", "Undo Replace Queue", after);
        }
        vec![
            Command::Enqueue {
                tracks: rest,
                mode: EnqueueMode::Replace,
            },
            Command::PlayIndex(0),
        ]
    }

    fn account_provider(&self) -> Option<ProviderId> {
        self.ui.accounts.first().map(|(p, _)| p.clone())
    }

    /// The slot that waits on `req`, which then waits on none.
    fn take_browse(&mut self, req: u64) -> Option<Slot> {
        Slot::ALL.into_iter().find(|&slot| {
            let waiting = &mut self.pending_browse[slot as usize];
            (*waiting == Some(req)).then(|| *waiting = None).is_some()
        })
    }

    fn land(&mut self, slot: Slot, shelves: Vec<Shelf>) {
        self.set_browse(
            slot,
            Browse {
                load: Load::Done,
                shelves,
            },
        );
    }

    fn set_browse(&mut self, slot: Slot, browse: Browse) {
        match slot {
            Slot::Home => {
                self.ui.home = browse;
                self.dirty |= Dirty::HOME;
            }
            Slot::Library => {
                self.ui.library = browse;
                self.dirty |= Dirty::LIBRARY;
            }
            Slot::Collection => {
                if let Some(page) = &mut self.ui.collection {
                    page.tracks = browse
                        .shelves
                        .iter()
                        .flat_map(|s| &s.items)
                        .filter_map(|i| match i {
                            Item::Track(t) => Some(t.clone()),
                            Item::Collection(_) => None,
                        })
                        .collect();
                    page.browse = browse;
                    self.dirty |= Dirty::COLLECTION;
                }
            }
        }
    }

    /// Requests `slot`'s page, replacing what it shows with a loading state. Nothing goes out
    /// while disconnected; the reconnect asks again. Library needs a signed-in account.
    fn fetch(&mut self, slot: Slot) -> Option<Command> {
        if !self.ui.controls().player {
            return None;
        }
        let page = match slot {
            Slot::Home => Page::Home,
            Slot::Library => {
                if !matches!(self.ui.account_state(), Some(AccountState::SignedIn { .. })) {
                    return None;
                }
                Page::Library
            }
            Slot::Collection => {
                Page::Collection(self.ui.collection.as_ref()?.collection.id.clone())
            }
        };
        self.last_req += 1;
        self.pending_browse[slot as usize] = Some(self.last_req);
        self.set_browse(
            slot,
            Browse {
                load: Load::Loading,
                shelves: Vec::new(),
            },
        );
        Some(Command::Browse {
            req: self.last_req,
            page,
        })
    }

    /// Fetches the page of the current place unless it has been requested already.
    fn fetch_place(&mut self) -> Vec<Command> {
        let (slot, load) = match self.ui.place {
            Place::Home => (Slot::Home, &self.ui.home.load),
            Place::Library => (Slot::Library, &self.ui.library.load),
            Place::Collection => match &self.ui.collection {
                Some(page) => (Slot::Collection, &page.browse.load),
                None => return Vec::new(),
            },
            Place::Search | Place::Queue => return Vec::new(),
        };
        if *load != Load::Idle {
            return Vec::new();
        }
        self.fetch(slot).into_iter().collect()
    }

    /// A connection starts over: nothing asked before it is answered, and the snapshot that
    /// follows tells the accounts again.
    fn reset_browse(&mut self) {
        self.pending_browse = [None; 3];
        self.ui.home = Browse::default();
        self.ui.library = Browse::default();
        self.ui.accounts.clear();
        if let Some(page) = &mut self.ui.collection {
            page.browse = Browse::default();
            page.tracks.clear();
        }
        self.dirty |= Dirty::HOME | Dirty::LIBRARY | Dirty::COLLECTION | Dirty::ACCOUNT;
    }

    fn set_account(&mut self, provider: ProviderId, state: AccountState) {
        let signed_in =
            |ui: &UiState| matches!(ui.account_state(), Some(AccountState::SignedIn { .. }));
        let was = signed_in(&self.ui);
        match self.ui.accounts.iter_mut().find(|(p, _)| *p == provider) {
            Some(entry) => entry.1 = state,
            None => self.ui.accounts.push((provider, state)),
        }
        self.dirty |= Dirty::ACCOUNT;
        let now = signed_in(&self.ui);
        if was && !now {
            self.pending_browse[Slot::Library as usize] = None;
            self.set_browse(Slot::Library, Browse::default());
        } else if now && !was {
            let fetch = self.fetch(Slot::Library);
            self.outbox.extend(fetch);
        }
    }

    /// Shows `text` with Undo, which restores the queue as it is now: its tracks, and the
    /// playing track, position and pause. `after` is the queue the change leaves. A track
    /// still loading has no position yet, so it restores from its start.
    fn offer_undo(&mut self, text: &str, title: &'static str, after: Vec<TrackId>) {
        let tracks = self.ui.queue.clone();
        let playing = self
            .ui
            .current
            .zip(self.ui.now_playing.as_ref())
            .map(|(c, np)| {
                let loaded = self.ui.queue.get(c).is_some_and(|t| t.id == np.id);
                (
                    c,
                    if loaded {
                        self.ui.position
                    } else {
                        Duration::ZERO
                    },
                )
            });
        self.toast_with(text.to_owned(), false, Some("Undo"));
        self.undo = Some(Undo {
            toast: self.last_toast,
            title,
            after,
            landed: false,
            tracks,
            playing,
            paused: self.ui.state == PlayState::Paused,
        });
    }

    /// Ends the offer, and its toast if that still shows.
    fn withdraw_undo(&mut self) {
        if let Some(u) = self.undo.take() {
            self.dismiss_toast(u.toast);
        }
    }

    /// Restores the queue: `Enqueue`, `PlayIndex` and any `Pause` now, since a pause sent
    /// while the track loads holds through its start, and the `Seek` once the restored track
    /// starts. A seek sent before then reaches the previous track's decoder, because the
    /// session loads the new one only after resolving it.
    fn undo(&mut self) -> Vec<Command> {
        let Some(undo) = self.undo.take() else {
            return Vec::new();
        };
        self.dismiss_toast(undo.toast);
        self.restore = undo.playing.and_then(|(index, position)| {
            let track = undo.tracks.get(index)?;
            (!position.is_zero()).then(|| Restore {
                index,
                track: track.id.clone(),
                position,
            })
        });
        let mut cmds = vec![Command::Enqueue {
            tracks: undo.tracks,
            mode: EnqueueMode::Replace,
        }];
        if let Some((index, _)) = undo.playing {
            cmds.push(Command::PlayIndex(index));
            if undo.paused {
                cmds.push(Command::Pause);
            }
        }
        cmds
    }

    fn seek_to(&mut self, to: Duration, now: Instant) -> Vec<Command> {
        self.set_position(to);
        self.seek_hold = Hold::new(to, now);
        vec![Command::Seek(SeekTarget::Absolute(to))]
    }

    fn volume_command(&mut self, v: u8, now: Instant) -> Vec<Command> {
        if v == self.ui.volume {
            return Vec::new();
        }
        self.set_volume(v);
        self.volume_hold = Hold::new(v, now);
        vec![Command::SetVolume(v)]
    }

    fn toast(&mut self, text: String, error: bool) {
        self.toast_with(text, error, None);
    }

    fn toast_with(&mut self, text: String, error: bool, action: Option<&'static str>) {
        self.last_toast += 1;
        self.ui.toast = Some(Toast {
            id: self.last_toast,
            text,
            error,
            action,
        });
        self.dirty |= Dirty::TOAST;
    }

    /// Adds what happened next to a `TrackFailed` toast that is still shown.
    fn amend_failure(&mut self, failed: Option<u64>, outcome: &str) {
        if let Some(t) = self.ui.toast.as_mut().filter(|t| Some(t.id) == failed) {
            // Two sentences now, so both end in a period.
            t.text = format!("{}. {outcome}", t.text);
            self.dirty |= Dirty::TOAST;
        }
    }

    fn refresh_banner(&mut self, now: Instant) {
        let waited = self.offline_since.map(|t| now.saturating_duration_since(t));
        let (text, reason) = match &self.ui.connection {
            Connection::Connecting => ("Connecting to player…", self.connect_error),
            Connection::Reconnecting(r) => ("Player disconnected — reconnecting…", Some(*r)),
            Connection::OutOfDate => (OUT_OF_DATE, None),
            Connection::Connected | Connection::Demo => ("", None),
        };
        self.stuck = waited.is_some_and(|w| w >= STUCK_AFTER);
        let banner = match waited {
            _ if self.ui.connection == Connection::OutOfDate => Some(Banner {
                text: text.to_owned(),
                error: true,
            }),
            Some(w) if w < BANNER_GRACE => None,
            Some(_) if self.stuck => Some(Banner {
                text: match reason {
                    Some(r) => format!("{text} {} {STUCK_HINT}", r.text()),
                    None => format!("{text} {STUCK_HINT}"),
                },
                error: false,
            }),
            Some(_) => Some(Banner {
                text: text.to_owned(),
                error: false,
            }),
            None => None,
        };
        if banner != self.ui.banner {
            self.ui.banner = banner;
            self.dirty |= Dirty::CONNECTION;
        }
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
        self.seek_hold = None;
        self.volume_hold = None;
        self.restore = None;
        self.select_after = None;
        self.queue_selection = None;
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

fn enqueue_append(tracks: Vec<Track>) -> Vec<Command> {
    if tracks.is_empty() {
        return Vec::new();
    }
    vec![Command::Enqueue {
        tracks,
        mode: EnqueueMode::Append,
    }]
}

/// Where row `selected` ends up after `cmd`, by the session's arithmetic; a removed row's
/// place goes to the row after it. `None` once the queue is cleared.
fn follow(selected: usize, cmd: &Command) -> Option<usize> {
    match *cmd {
        Command::Remove(i) if selected > i => Some(selected - 1),
        Command::Remove(_) => Some(selected),
        Command::Move { from, to } => Some(if selected == from {
            to
        } else if from < selected && selected <= to {
            selected - 1
        } else if to <= selected && selected < from {
            selected + 1
        } else {
            selected
        }),
        _ => None,
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
    PlaySelected,
    AddToQueue,
    FocusSearch,
    ShowHome,
    ShowSearch,
    ShowLibrary,
    ShowQueue,
    Refresh,
    SignIn,
    SignOut,
    VolumeUp,
    VolumeDown,
    ClearQueue,
}

impl PaletteAction {
    pub const ALL: [PaletteAction; 16] = [
        PaletteAction::PlayPause,
        PaletteAction::Next,
        PaletteAction::Previous,
        PaletteAction::PlaySelected,
        PaletteAction::AddToQueue,
        PaletteAction::FocusSearch,
        PaletteAction::ShowHome,
        PaletteAction::ShowSearch,
        PaletteAction::ShowLibrary,
        PaletteAction::ShowQueue,
        PaletteAction::Refresh,
        PaletteAction::SignIn,
        PaletteAction::SignOut,
        PaletteAction::VolumeUp,
        PaletteAction::VolumeDown,
        PaletteAction::ClearQueue,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaletteAction::PlayPause => "Play/Pause",
            PaletteAction::Next => "Next",
            PaletteAction::Previous => "Previous",
            PaletteAction::PlaySelected => "Play selected",
            PaletteAction::AddToQueue => "Add to queue",
            PaletteAction::FocusSearch => "Focus search",
            PaletteAction::ShowHome => "Show home",
            PaletteAction::ShowSearch => "Show search",
            PaletteAction::ShowLibrary => "Show library",
            PaletteAction::ShowQueue => "Show queue",
            PaletteAction::Refresh => "Refresh",
            PaletteAction::SignIn => "Sign in to Google",
            PaletteAction::SignOut => "Sign out of Google",
            PaletteAction::VolumeUp => "Volume up",
            PaletteAction::VolumeDown => "Volume down",
            PaletteAction::ClearQueue => "Clear queue",
        }
    }

    /// The key that runs the action outside the palette, as the menus show it.
    pub fn shortcut(self) -> &'static str {
        match self {
            PaletteAction::PlayPause => "Space",
            PaletteAction::Next => "⌘→",
            PaletteAction::Previous => "⌘←",
            PaletteAction::PlaySelected => "↩",
            PaletteAction::AddToQueue => "⌘↩",
            PaletteAction::FocusSearch => "⌘F",
            PaletteAction::ShowHome => "⌘1",
            PaletteAction::ShowSearch => "⌘2",
            PaletteAction::ShowLibrary => "⌘3",
            PaletteAction::ShowQueue => "⌘4",
            PaletteAction::Refresh => "⌘R",
            PaletteAction::SignIn | PaletteAction::SignOut => "",
            PaletteAction::VolumeUp => "⌘↑",
            PaletteAction::VolumeDown => "⌘↓",
            PaletteAction::ClearQueue => "",
        }
    }

    /// Sign In and Sign Out are listed only while they apply.
    pub fn is_account(self) -> bool {
        matches!(self, PaletteAction::SignIn | PaletteAction::SignOut)
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

    #[test]
    fn actions_run_only_when_they_would_act() {
        let runnable = |vm: &Vm, selection| {
            PaletteAction::ALL
                .into_iter()
                .filter(|&a| vm.ui().can_run(a, selection))
                .collect::<Vec<_>>()
        };
        let views = vec![
            PaletteAction::FocusSearch,
            PaletteAction::ShowHome,
            PaletteAction::ShowSearch,
            PaletteAction::ShowLibrary,
            PaletteAction::ShowQueue,
        ];
        assert_eq!(
            runnable(
                &Vm::new(),
                Selection {
                    row: true,
                    result: true
                }
            ),
            views
        );

        let mut vm = connected();
        let none = Selection::default();
        let mut idle = views.clone();
        idle.extend([PaletteAction::VolumeUp, PaletteAction::VolumeDown]);
        assert_eq!(runnable(&vm, none), idle);
        // Refresh fetches a browse page again, so it waits for one to be shown.
        vm.intent(Intent::Visit(Place::Home));
        assert!(runnable(&vm, none).contains(&PaletteAction::Refresh));
        vm.intent(Intent::Visit(Place::Search));

        vm.apply(Event::QueueChanged {
            tracks: tracks(2),
            current: Some(0),
        });
        // Without a selection, Play selected and Add to queue wait; with no account, so do the
        // sign-in actions, and Refresh waits for a browse page.
        assert_eq!(runnable(&vm, none).len(), PaletteAction::ALL.len() - 5);
        let queue_row = Selection {
            row: true,
            result: false,
        };
        assert!(vm.ui().can_run(PaletteAction::PlaySelected, queue_row));
        assert!(!vm.ui().can_run(PaletteAction::AddToQueue, queue_row));
        let result = Selection {
            row: true,
            result: true,
        };
        assert_eq!(runnable(&vm, result).len(), PaletteAction::ALL.len() - 3);
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

    /// Applies `ev` after any echo hold from earlier intents has run out.
    fn apply_later(vm: &mut Vm, ev: Event) {
        vm.apply_at(ev, Instant::now() + ECHO_HOLD);
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
            Some((
                "Search failed: network error: timeout. Press Return to try again.".into(),
                true
            ))
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
    fn seek_bar_value_is_spoken_as_time() {
        assert_eq!(connected().ui().seek_spoken(), "");
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_secs(83)));
        assert_eq!(vm.ui().seek_spoken(), "1:23 of 3:45");
        // No duration known: the time alone.
        let mut vm = connected();
        let t = Track {
            duration: None,
            ..track("x", 1)
        };
        vm.apply(Event::NowPlaying {
            track: t,
            duration: None,
        });
        assert_eq!(vm.ui().seek_spoken(), "0:00");
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
        assert_eq!(vm.ui().elapsed_text(), "Buffering…");
        // VoiceOver still hears the time.
        assert_eq!(vm.ui().seek_spoken(), "0:10 of 3:45");
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
        assert_eq!(t.text, "Couldn’t play “Title a”: unplayable: private");
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
        assert_eq!(
            t.text,
            "YouTube Music stopped working; requiem may need an update"
        );
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
        let cmds = vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 2,
        });
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
        assert!(
            vm.intent(Intent::PlayFrom {
                list: List::Results,
                from: 5
            })
            .is_empty()
        );
        assert!(
            vm.intent(Intent::PlayFrom {
                list: List::Results,
                from: 99
            })
            .is_empty()
        );
    }

    #[test]
    fn append_result_enqueues_one_track() {
        let mut vm = with_results(5);
        assert_eq!(
            vm.intent(Intent::Append {
                list: List::Results,
                index: 3
            }),
            vec![Command::Enqueue {
                tracks: vec![vm.ui().results[3].clone()],
                mode: EnqueueMode::Append
            }]
        );
        assert!(
            vm.intent(Intent::Append {
                list: List::Results,
                index: 5
            })
            .is_empty()
        );
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
        vm.apply(Event::QueueChanged {
            tracks: tracks(2),
            current: Some(0),
        });
        assert_eq!(vm.intent(Intent::Toggle), vec![Command::TogglePause]);
        assert_eq!(vm.intent(Intent::Next), vec![Command::Next]);
        assert_eq!(vm.intent(Intent::Prev), vec![Command::Prev]);
    }

    #[test]
    fn seek_target_scales_the_measured_duration() {
        let mut vm = playing(200);
        assert_eq!(vm.ui().seek_target(0.25), Some(Duration::from_secs(50)));
        assert_eq!(vm.ui().seek_target(1.5), Some(Duration::from_secs(200)));
        vm.apply(Event::State(PlayState::Stopped));
        assert_eq!(vm.ui().seek_target(0.5), None);
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
        apply_later(&mut vm, Event::Position(Duration::from_secs(222)));
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
        apply_later(&mut vm, Event::Volume(5));
        assert_eq!(
            vm.intent(Intent::VolumeBy(-VOLUME_STEP)),
            vec![Command::SetVolume(0)]
        );
        // Two quick presses compound on the optimistic value.
        apply_later(&mut vm, Event::Volume(50));
        vm.intent(Intent::VolumeBy(VOLUME_STEP));
        assert_eq!(
            vm.intent(Intent::VolumeBy(VOLUME_STEP)),
            vec![Command::SetVolume(70)]
        );
    }

    #[test]
    fn local_volume_ignores_stale_echoes_until_one_agrees() {
        let mut vm = connected();
        vm.apply(Event::Volume(70));
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        vm.intent_at(Intent::SetVolume(40), t0);
        vm.take_dirty();
        // Sent by the daemon before it saw the command.
        vm.apply_at(Event::Volume(70), ms(100));
        assert_eq!(vm.ui().volume, 40);
        assert!(vm.take_dirty().is_empty());
        vm.apply_at(Event::Volume(40), ms(150));
        // The hold ended with the agreeing echo; the daemon wins again.
        vm.apply_at(Event::Volume(55), ms(200));
        assert_eq!(vm.ui().volume, 55);
        assert_eq!(vm.take_dirty(), Dirty::VOLUME);
    }

    #[test]
    fn local_volume_hold_expires() {
        let mut vm = connected();
        vm.apply(Event::Volume(70));
        let t0 = Instant::now();
        vm.intent_at(Intent::VolumeBy(-VOLUME_STEP), t0);
        vm.apply_at(Event::Volume(70), t0 + ECHO_HOLD - Duration::from_millis(1));
        assert_eq!(vm.ui().volume, 60);
        vm.apply_at(Event::Volume(70), t0 + ECHO_HOLD);
        assert_eq!(vm.ui().volume, 70);
    }

    #[test]
    fn local_seek_ignores_stale_positions_until_one_agrees() {
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_secs(83)));
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        vm.intent_at(Intent::SeekTo(Duration::from_secs(150)), t0);
        vm.take_dirty();
        vm.apply_at(Event::Position(Duration::from_millis(83_250)), ms(100));
        assert_eq!(vm.ui().position, Duration::from_secs(150));
        assert!(vm.take_dirty().is_empty());
        // Within `SEEK_AGREES` of the target: accepted, and the hold ends.
        vm.apply_at(Event::Position(Duration::from_millis(150_200)), ms(200));
        assert_eq!(vm.ui().position, Duration::from_millis(150_200));
        vm.apply_at(Event::Position(Duration::from_secs(90)), ms(250));
        assert_eq!(vm.ui().position, Duration::from_secs(90));
    }

    #[test]
    fn local_seek_hold_expires_and_covers_relative_seeks() {
        let mut vm = playing(225);
        vm.apply(Event::Position(Duration::from_secs(83)));
        let t0 = Instant::now();
        vm.intent_at(Intent::SeekBy(SEEK_STEP_SECS), t0);
        assert_eq!(vm.ui().position, Duration::from_secs(88));
        vm.apply_at(
            Event::Position(Duration::from_secs(83)),
            t0 + ECHO_HOLD - Duration::from_millis(1),
        );
        assert_eq!(vm.ui().position, Duration::from_secs(88));
        vm.apply_at(Event::Position(Duration::from_secs(84)), t0 + ECHO_HOLD);
        assert_eq!(vm.ui().position, Duration::from_secs(84));
    }

    #[test]
    fn new_track_ends_the_seek_hold() {
        let mut vm = playing(225);
        let t0 = Instant::now();
        vm.intent_at(Intent::SeekTo(Duration::from_secs(150)), t0);
        vm.apply_at(
            Event::NowPlaying {
                track: track("next", 200),
                duration: Some(Duration::from_secs(200)),
            },
            t0,
        );
        vm.apply_at(Event::Position(Duration::from_secs(2)), t0);
        assert_eq!(vm.ui().position, Duration::from_secs(2));
    }

    #[test]
    fn first_connect_resets_without_toast() {
        let mut vm = Vm::new();
        vm.take_dirty();
        vm.link(Link::Connected);
        assert_eq!(vm.ui().connection, Connection::Connected);
        assert!(vm.ui().toast.is_none());
        assert!(vm.ui().banner.is_none());
        assert!(vm.take_dirty().contains(Dirty::CONNECTION));
    }

    #[test]
    fn losing_connection_raises_no_toast() {
        let mut vm = playing(225);
        vm.link(Link::Lost(LinkError::NotResponding));
        assert_eq!(
            vm.ui().connection,
            Connection::Reconnecting(LinkError::NotResponding)
        );
        assert!(vm.ui().toast.is_none());
        assert!(vm.take_dirty().contains(Dirty::CONNECTION));
    }

    #[test]
    fn reconnecting_banner_waits_out_the_grace() {
        let mut vm = playing(225);
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        vm.link_at(Link::Lost(LinkError::NotResponding), t0);
        assert_eq!(vm.ui().banner, None);
        assert_eq!(vm.next_tick(), Some(t0 + BANNER_GRACE));
        vm.take_dirty();
        vm.tick_at(ms(799));
        assert_eq!(vm.ui().banner, None);
        assert!(vm.take_dirty().is_empty());
        vm.tick_at(ms(800));
        assert_eq!(
            vm.ui().banner,
            Some(Banner {
                text: "Player disconnected — reconnecting…".into(),
                error: false
            })
        );
        assert_eq!(vm.take_dirty(), Dirty::CONNECTION);
        assert_eq!(vm.next_tick(), Some(t0 + STUCK_AFTER));
    }

    #[test]
    fn retries_keep_the_outage_clock_and_add_the_reason_after_30s() {
        let mut vm = playing(225);
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        vm.link_at(Link::Lost(LinkError::NotResponding), t0);
        // Each failed retry reports Lost again with the latest reason.
        vm.link_at(Link::Lost(LinkError::NotRunning), s(4));
        vm.link_at(Link::Lost(LinkError::NotRunning), s(29));
        assert_eq!(
            vm.ui().banner.as_ref().map(|b| b.text.as_str()),
            Some("Player disconnected — reconnecting…")
        );
        vm.tick_at(s(30));
        assert_eq!(
            vm.ui().banner,
            Some(Banner {
                text: "Player disconnected — reconnecting… The player isn’t running. If \
                       this persists, choose Quit and Stop Music from the requiem menu-bar \
                       icon, then reopen requiem."
                    .into(),
                error: false
            })
        );
        assert_eq!(vm.next_tick(), None);
        assert!(vm.ui().toast.is_none());
    }

    #[test]
    fn connecting_banner_after_the_grace_and_reason_after_30s() {
        let t0 = Instant::now();
        let mut vm = Vm::new_at(t0);
        assert_eq!(vm.ui().banner, None);
        vm.tick_at(t0 + BANNER_GRACE);
        assert_eq!(
            vm.ui().banner,
            Some(Banner {
                text: "Connecting to player…".into(),
                error: false
            })
        );
        // A failed first connect is still connecting: nothing was disconnected.
        vm.link_at(
            Link::Lost(LinkError::NotRunning),
            t0 + Duration::from_secs(3),
        );
        assert_eq!(vm.ui().connection, Connection::Connecting);
        vm.tick_at(t0 + STUCK_AFTER);
        assert_eq!(
            vm.ui().banner.as_ref().map(|b| b.text.as_str()),
            Some(
                "Connecting to player… The player isn’t running. If this persists, choose \
                 Quit and Stop Music from the requiem menu-bar icon, then reopen requiem."
            )
        );
        assert!(vm.ui().toast.is_none());
    }

    #[test]
    fn link_errors_are_put_in_plain_words() {
        use io::ErrorKind as K;
        for (kind, want) in [
            (K::ConnectionRefused, LinkError::NotRunning),
            (K::NotFound, LinkError::NotRunning),
            (K::TimedOut, LinkError::NotResponding),
            (K::UnexpectedEof, LinkError::NotResponding),
            (K::ConnectionReset, LinkError::NotResponding),
            (K::BrokenPipe, LinkError::NotResponding),
            (K::PermissionDenied, LinkError::Unreachable),
            (K::InvalidData, LinkError::Unreachable),
            (K::Other, LinkError::Unreachable),
        ] {
            assert_eq!(LinkError::from_io(kind), want, "{kind:?}");
        }
        assert_eq!(LinkError::NotRunning.text(), "The player isn’t running.");
        assert_eq!(
            LinkError::NotResponding.text(),
            "The player stopped responding."
        );
        assert_eq!(
            LinkError::Unreachable.text(),
            "The player couldn’t be reached."
        );
        // The stuck banner says it in those words.
        let mut vm = playing(225);
        let t0 = Instant::now();
        vm.link_at(Link::Lost(LinkError::Unreachable), t0);
        vm.tick_at(t0 + STUCK_AFTER);
        let text = vm.ui().banner.clone().unwrap().text;
        assert!(
            text.starts_with(
                "Player disconnected — reconnecting… The player couldn’t be reached. If this"
            ),
            "{text}"
        );
    }

    #[test]
    fn quick_connect_shows_no_banner_or_toast() {
        let t0 = Instant::now();
        let mut vm = Vm::new_at(t0);
        vm.link_at(Link::Connected, t0 + Duration::from_millis(50));
        assert_eq!(vm.ui().banner, None);
        assert_eq!(vm.next_tick(), None);

        let mut vm = playing(225);
        vm.link_at(Link::Lost(LinkError::NotResponding), t0);
        vm.link_at(Link::Connected, t0 + Duration::from_millis(700));
        assert_eq!(vm.ui().banner, None);
        assert!(vm.ui().toast.is_none());
    }

    #[test]
    fn reconnect_toasts_and_clears_stale_player_state() {
        let mut vm = playing(225);
        vm.apply(Event::QueueChanged {
            tracks: tracks(3),
            current: Some(1),
        });
        let t0 = Instant::now();
        vm.link_at(Link::Lost(LinkError::NotResponding), t0);
        vm.tick_at(t0 + Duration::from_secs(2));
        vm.take_dirty();
        vm.link_at(Link::Connected, t0 + Duration::from_secs(3));
        assert_eq!(vm.ui().banner, None);
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
        vm.link(Link::Lost(LinkError::NotResponding));
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
    fn pending_search_survives_disconnect_and_is_resent() {
        let mut vm = connected();
        let req = search_req(&mut vm, "a");
        vm.link(Link::Lost(LinkError::NotResponding));
        assert_eq!(vm.ui().search, SearchStatus::Pending);
        assert!(vm.take_commands().is_empty());
        vm.link(Link::Connected);
        assert_eq!(vm.ui().search, SearchStatus::Pending);
        let resent = vm.take_commands();
        assert_eq!(
            resent,
            vec![Command::Search {
                req: req + 1,
                query: "a".into()
            }]
        );
        // An answer to the old req is ignored.
        vm.apply(Event::SearchResults {
            req,
            tracks: tracks(1),
        });
        assert!(vm.ui().results.is_empty());
        vm.apply(Event::SearchResults {
            req: req + 1,
            tracks: tracks(2),
        });
        assert_eq!(vm.ui().results.len(), 2);
    }

    #[test]
    fn search_before_the_first_connect_is_sent_on_connect() {
        let mut vm = Vm::new();
        search_req(&mut vm, "night drive");
        vm.link(Link::Connected);
        assert_eq!(vm.ui().search, SearchStatus::Pending);
        assert_eq!(
            vm.take_commands(),
            vec![Command::Search {
                req: 2,
                query: "night drive".into()
            }]
        );
        assert!(vm.take_commands().is_empty());
    }

    #[test]
    fn out_of_date_shows_error_banner() {
        let mut vm = Vm::new();
        vm.take_dirty();
        vm.link(Link::OutOfDate);
        assert_eq!(vm.ui().connection, Connection::OutOfDate);
        assert_eq!(vm.take_dirty(), Dirty::CONNECTION);
        assert!(vm.ui().toast.is_none());
        assert_eq!(
            vm.ui().banner,
            Some(Banner {
                text: "An older requiem player is still running. Quit it from its menu-bar \
                       icon; this window reconnects on its own."
                    .into(),
                error: true
            })
        );
        assert_eq!(vm.next_tick(), None);
    }

    #[test]
    fn out_of_date_retries_until_a_connection_replaces_it() {
        let mut vm = Vm::new();
        vm.link(Link::OutOfDate);
        // Each retry against the old player reports it again; nothing changes.
        vm.take_dirty();
        vm.link(Link::OutOfDate);
        assert!(vm.take_dirty().is_empty());
        vm.link(Link::Connected);
        assert_eq!(vm.ui().connection, Connection::Connected);
        assert_eq!(vm.ui().banner, None);
        assert_eq!(vm.ui().toast.as_ref().unwrap().text, "Player reconnected");
        assert!(vm.take_dirty().contains(Dirty::CONNECTION));
    }

    #[test]
    fn out_of_date_after_a_connection_keeps_state_and_stays_put() {
        let mut vm = playing(225);
        vm.link(Link::OutOfDate);
        assert_eq!(vm.ui().connection, Connection::OutOfDate);
        assert!(vm.ui().banner.as_ref().is_some_and(|b| b.error));
        // No toast: the banner says it, and no reconnect follows.
        assert!(vm.ui().toast.is_none());
        assert!(vm.ui().now_playing.is_some());
        vm.take_dirty();
        vm.link(Link::OutOfDate);
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn demo_link_has_no_banner() {
        let mut vm = Vm::new();
        vm.link(Link::Demo);
        assert_eq!(vm.ui().connection, Connection::Demo);
        assert!(vm.ui().banner.is_none());
    }

    #[test]
    fn results_empty_state_texts() {
        let mut vm = connected();
        assert_eq!(
            vm.ui().results_empty(),
            Some(EmptyText {
                heading: "Search YouTube Music".into(),
                sub: "Type a song, artist or album, then press Return".into(),
                action: None,
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
                action: Some("Edit search"),
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
            vec![
                PaletteAction::ShowHome,
                PaletteAction::ShowSearch,
                PaletteAction::ShowLibrary,
                PaletteAction::ShowQueue
            ]
        );
        assert_eq!(
            PaletteAction::filter("search"),
            vec![PaletteAction::FocusSearch, PaletteAction::ShowSearch]
        );
        assert_eq!(
            PaletteAction::filter("play"),
            vec![PaletteAction::PlayPause, PaletteAction::PlaySelected]
        );
        assert!(PaletteAction::filter("xyz").is_empty());
    }

    #[test]
    fn palette_shortcuts_match_the_menus() {
        let keys: Vec<_> = PaletteAction::ALL.iter().map(|a| a.shortcut()).collect();
        assert_eq!(
            keys,
            [
                "Space", "⌘→", "⌘←", "↩", "⌘↩", "⌘F", "⌘1", "⌘2", "⌘3", "⌘4", "⌘R", "", "", "⌘↑",
                "⌘↓", ""
            ]
        );
    }

    #[test]
    fn total_time_agrees_with_the_listed_duration() {
        let mut vm = connected();
        vm.apply(Event::NowPlaying {
            track: track("a", 159),
            duration: Some(Duration::from_millis(158_600)),
        });
        assert_eq!(vm.ui().total_text(), "2:39");
        // Progress and seeking keep the engine's duration.
        vm.apply(Event::Position(Duration::from_millis(79_300)));
        assert!((vm.ui().progress() - 0.5).abs() < 1e-9);
        let mut t = track("b", 0);
        t.duration = None;
        vm.apply(Event::NowPlaying {
            track: t,
            duration: Some(Duration::from_secs(61)),
        });
        assert_eq!(vm.ui().total_text(), "1:01");
    }

    #[test]
    fn controls_follow_the_connection_queue_and_track() {
        let off = Controls {
            transport: false,
            seek: false,
            player: false,
        };
        let mut vm = Vm::new();
        assert_eq!(vm.ui().controls(), off);
        vm.link(Link::Connected);
        assert_eq!(
            vm.ui().controls(),
            Controls {
                transport: false,
                seek: false,
                player: true
            }
        );
        vm.apply(Event::QueueChanged {
            tracks: tracks(2),
            current: Some(0),
        });
        assert!(vm.ui().controls().transport && !vm.ui().controls().seek);
        vm.apply(Event::NowPlaying {
            track: track("0", 200),
            duration: Some(Duration::from_secs(200)),
        });
        let on = Controls {
            transport: true,
            seek: true,
            player: true,
        };
        assert_eq!(vm.ui().controls(), on);
        vm.link(Link::Lost(LinkError::NotResponding));
        assert_eq!(vm.ui().controls(), off);
        vm.link(Link::OutOfDate);
        assert_eq!(vm.ui().controls(), off);
        let (demo, _) = crate::demo::start();
        assert_eq!(demo.ui().controls(), on);
    }

    #[test]
    fn only_a_connected_player_can_be_stopped() {
        let mut vm = Vm::new();
        assert!(!vm.ui().can_stop_player());
        vm.link(Link::Connected);
        assert!(vm.ui().can_stop_player());
        vm.link(Link::Lost(LinkError::NotResponding));
        assert!(!vm.ui().can_stop_player());
        vm.link(Link::OutOfDate);
        assert!(!vm.ui().can_stop_player());
        let (demo, _) = crate::demo::start();
        assert!(!demo.ui().can_stop_player());
    }

    #[test]
    fn disabled_controls_send_nothing() {
        let mut vm = with_results(3);
        // Connected with an empty queue: no transport.
        assert!(vm.intent(Intent::Toggle).is_empty());
        assert!(vm.intent(Intent::Next).is_empty());
        assert!(vm.intent(Intent::Prev).is_empty());
        vm.link(Link::Lost(LinkError::NotResponding));
        for i in [
            Intent::PlayFrom {
                list: List::Results,
                from: 0,
            },
            Intent::Append {
                list: List::Results,
                index: 0,
            },
            Intent::PlayQueue(0),
            Intent::SetVolume(10),
            Intent::VolumeBy(-10),
            Intent::Undo,
        ] {
            assert!(vm.intent(i.clone()).is_empty(), "{i:?}");
        }
        // Searching stays open; the search is sent again on connect.
        assert_eq!(search_req(&mut vm, "b"), 2);
    }

    #[test]
    fn queue_badge_counts_tracks() {
        let mut vm = connected();
        assert_eq!(vm.ui().queue_badge(), None);
        vm.apply(Event::QueueChanged {
            tracks: tracks(5),
            current: None,
        });
        assert_eq!(vm.ui().queue_badge().as_deref(), Some("5"));
        assert_eq!(vm.take_dirty(), Dirty::QUEUE);
    }

    /// Playing the third of three queued tracks at 1:01, with results to replace them.
    fn replaced() -> (Vm, Vec<Track>) {
        let mut vm = with_results(4);
        let old = tracks(3);
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        vm.apply(Event::NowPlaying {
            track: old[2].clone(),
            duration: old[2].duration,
        });
        vm.apply(Event::Position(Duration::from_secs(61)));
        vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 1,
        });
        (vm, old)
    }

    #[test]
    fn replacing_a_queue_offers_undo() {
        let (vm, _) = replaced();
        let t = vm.ui().toast.clone().unwrap();
        assert_eq!(
            (t.text.as_str(), t.error, t.action),
            ("Queue replaced", false, Some("Undo"))
        );
        // Nothing to undo when the queue was empty.
        let mut vm = with_results(4);
        vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 1,
        });
        assert!(vm.ui().toast.is_none());
    }

    #[test]
    fn undo_restores_queue_track_and_position() {
        let (mut vm, old) = replaced();
        vm.take_dirty();
        assert_eq!(
            vm.intent(Intent::Undo),
            vec![
                Command::Enqueue {
                    tracks: old.clone(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(2)
            ]
        );
        assert!(vm.ui().toast.is_none());
        assert!(vm.intent(Intent::Undo).is_empty());
        // The session's answer: the queue, then the restored track once it starts.
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        assert!(vm.take_commands().is_empty());
        vm.apply(Event::NowPlaying {
            track: old[2].clone(),
            duration: old[2].duration,
        });
        assert_eq!(
            vm.take_commands(),
            vec![Command::Seek(SeekTarget::Absolute(Duration::from_secs(61)))]
        );
        assert_eq!(vm.ui().elapsed_text(), "1:01");
        // Held against the zero positions the daemon sends before it seeks.
        vm.apply(Event::Position(Duration::ZERO));
        assert_eq!(vm.ui().elapsed_text(), "1:01");
    }

    #[test]
    fn undo_keeps_a_paused_queue_paused() {
        let restored = |at: u64| {
            let mut vm = with_results(4);
            let old = tracks(3);
            vm.apply(Event::QueueChanged {
                tracks: old.clone(),
                current: Some(2),
            });
            vm.apply(Event::NowPlaying {
                track: old[2].clone(),
                duration: old[2].duration,
            });
            vm.apply(Event::State(PlayState::Paused));
            vm.apply(Event::Position(Duration::from_secs(at)));
            vm.intent(Intent::PlayFrom {
                list: List::Results,
                from: 1,
            });
            vm.apply(Event::State(PlayState::Playing));
            let now = vm.intent(Intent::Undo);
            assert_eq!(
                now,
                [
                    Command::Enqueue {
                        tracks: old.clone(),
                        mode: EnqueueMode::Replace
                    },
                    Command::PlayIndex(2),
                    Command::Pause
                ]
            );
            vm.apply(Event::QueueChanged {
                tracks: old.clone(),
                current: Some(2),
            });
            assert!(vm.take_commands().is_empty());
            vm.apply(Event::NowPlaying {
                track: old[2].clone(),
                duration: old[2].duration,
            });
            vm.take_commands()
        };
        assert_eq!(
            restored(61),
            [Command::Seek(SeekTarget::Absolute(Duration::from_secs(61)))]
        );
        // Paused at the very start: nothing to seek.
        assert!(restored(0).is_empty());
    }

    #[test]
    fn undo_seek_is_dropped_if_another_track_starts() {
        let (mut vm, old) = replaced();
        vm.intent(Intent::Undo);
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(0),
        });
        vm.apply(Event::NowPlaying {
            track: old[0].clone(),
            duration: old[0].duration,
        });
        assert!(vm.take_commands().is_empty());
    }

    #[test]
    fn undo_outlives_its_toast() {
        let (mut vm, old) = replaced();
        let id = vm.ui().toast.as_ref().unwrap().id;
        vm.dismiss_toast(id);
        assert_eq!(vm.undo_title(), Some("Undo Replace Queue"));
        assert_eq!(
            vm.intent(Intent::Undo),
            [
                Command::Enqueue {
                    tracks: old.clone(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(2)
            ]
        );
        assert_eq!(vm.undo_title(), None);
        assert!(vm.intent(Intent::Undo).is_empty());

        // Another toast takes the place of its toast, not of the offer.
        let (mut vm, _) = replaced();
        vm.apply(Event::TrackFailed {
            track: track("x", 1),
            error: "e".into(),
        });
        assert!(vm.ui().toast.as_ref().unwrap().error);
        assert_eq!(vm.intent(Intent::Undo).len(), 2);
        assert!(
            vm.ui().toast.as_ref().unwrap().error,
            "not the undo's toast"
        );
    }

    #[test]
    fn undo_offer_ends_when_the_queue_changes_again() {
        let (mut vm, old) = replaced();
        let new = tracks(4)[1..].to_vec();
        // News sent before the session had the replace, its answer, and the first track
        // starting keep it.
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        vm.apply(Event::QueueChanged {
            tracks: new.clone(),
            current: None,
        });
        vm.apply(Event::QueueChanged {
            tracks: new.clone(),
            current: Some(0),
        });
        // So does the next track starting on its own.
        vm.apply(Event::QueueChanged {
            tracks: new.clone(),
            current: Some(1),
        });
        assert_eq!(vm.undo_title(), Some("Undo Replace Queue"));
        // Once it has landed, the old queue is a change too.
        let mut back = replaced().0;
        back.apply(Event::QueueChanged {
            tracks: new.clone(),
            current: None,
        });
        back.apply(Event::QueueChanged {
            tracks: old,
            current: None,
        });
        assert_eq!(back.undo_title(), None);
        // An append, a remove or another client's change ends it, and its toast with it.
        vm.take_dirty();
        let mut more = new.clone();
        more.push(track("extra", 100));
        vm.apply(Event::QueueChanged {
            tracks: more,
            current: Some(1),
        });
        assert_eq!(vm.undo_title(), None);
        assert!(vm.ui().toast.is_none());
        assert!(vm.take_dirty().contains(Dirty::TOAST));
        assert!(vm.intent(Intent::Undo).is_empty());

        // After its toast is gone, too.
        let (mut vm, _) = cleared();
        let id = vm.ui().toast.as_ref().unwrap().id;
        vm.dismiss_toast(id);
        assert_eq!(vm.undo_title(), Some("Undo Clear Queue"));
        vm.apply(Event::QueueChanged {
            tracks: tracks(1),
            current: None,
        });
        assert_eq!(vm.undo_title(), None);
        assert!(vm.intent(Intent::Undo).is_empty());
    }

    #[test]
    fn undo_title_names_the_change() {
        assert_eq!(connected().undo_title(), None);
        assert_eq!(replaced().0.undo_title(), Some("Undo Replace Queue"));
        assert_eq!(cleared().0.undo_title(), Some("Undo Clear Queue"));
        // Replacing an empty queue offers nothing.
        let mut vm = with_results(4);
        vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 1,
        });
        assert_eq!(vm.undo_title(), None);
    }

    #[test]
    fn undo_without_a_loaded_track_restores_only_the_queue() {
        let mut vm = with_results(4);
        vm.apply(Event::QueueChanged {
            tracks: tracks(3),
            current: None,
        });
        vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 0,
        });
        assert_eq!(
            vm.intent(Intent::Undo),
            vec![Command::Enqueue {
                tracks: tracks(3),
                mode: EnqueueMode::Replace
            }]
        );
    }

    #[test]
    fn track_failed_toast_says_whether_playback_moved_on() {
        let mut vm = playing(225);
        vm.apply(Event::TrackFailed {
            track: track("a", 1),
            error: "video unavailable".into(),
        });
        vm.apply(Event::QueueChanged {
            tracks: tracks(3),
            current: Some(1),
        });
        assert_eq!(
            vm.ui().toast.as_ref().unwrap().text,
            "Couldn’t play “Title a”: video unavailable. Skipped to the next track."
        );

        let mut vm = playing(225);
        vm.apply(Event::TrackFailed {
            track: track("a", 1),
            error: "video unavailable".into(),
        });
        vm.apply(Event::State(PlayState::Stopped));
        assert_eq!(
            vm.ui().toast.as_ref().unwrap().text,
            "Couldn’t play “Title a”: video unavailable. Playback stopped."
        );
        // Only the event right after the failure tells its outcome.
        vm.apply(Event::QueueChanged {
            tracks: tracks(3),
            current: Some(1),
        });
        assert!(
            vm.ui()
                .toast
                .as_ref()
                .unwrap()
                .text
                .ends_with("Playback stopped.")
        );
    }

    #[test]
    fn provider_broken_keeps_its_toast_over_the_failure() {
        let mut vm = playing(225);
        vm.apply(Event::TrackFailed {
            track: track("a", 1),
            error: "e".into(),
        });
        vm.apply(Event::ProviderBroken {
            provider: ProviderId("ytmusic".into()),
            detail: "3 consecutive failures".into(),
        });
        vm.apply(Event::State(PlayState::Stopped));
        let t = vm.ui().toast.clone().unwrap();
        assert!(t.text.starts_with("YouTube Music stopped working"));
        assert!(!t.text.contains("Playback stopped"));
    }

    /// Connected with a queue of `n`, nothing loaded.
    fn queued(n: usize) -> Vm {
        let mut vm = connected();
        vm.apply(Event::QueueChanged {
            tracks: tracks(n),
            current: None,
        });
        vm.take_dirty();
        vm
    }

    fn edit(vm: &mut Vm, edit: QueueEdit, selected: Option<usize>) -> Vec<Command> {
        vm.intent(Intent::EditQueue { edit, selected })
    }

    #[test]
    fn queue_edits_send_zero_based_commands() {
        let mut vm = queued(5);
        assert_eq!(
            edit(&mut vm, QueueEdit::Remove(2), None),
            [Command::Remove(2)]
        );
        assert_eq!(
            edit(&mut vm, QueueEdit::MoveUp(2), None),
            [Command::Move { from: 2, to: 1 }]
        );
        assert_eq!(
            edit(&mut vm, QueueEdit::MoveDown(2), None),
            [Command::Move { from: 2, to: 3 }]
        );
        assert_eq!(edit(&mut vm, QueueEdit::Clear, None), [Command::Clear]);
    }

    #[test]
    fn queue_edits_past_the_ends_or_disconnected_send_nothing() {
        let mut vm = queued(5);
        for e in [
            QueueEdit::MoveUp(0),
            QueueEdit::MoveDown(4),
            QueueEdit::Remove(5),
            QueueEdit::MoveUp(5),
            QueueEdit::MoveDown(5),
        ] {
            assert_eq!(vm.ui().edit_command(e), None, "{e:?}");
            assert!(edit(&mut vm, e, Some(0)).is_empty(), "{e:?}");
        }
        assert_eq!(queued(0).ui().edit_command(QueueEdit::Clear), None);

        let mut vm = queued(5);
        vm.link(Link::Lost(LinkError::NotResponding));
        for e in [
            QueueEdit::Remove(0),
            QueueEdit::MoveDown(0),
            QueueEdit::Clear,
        ] {
            assert_eq!(vm.ui().edit_command(e), None, "{e:?}");
            assert!(edit(&mut vm, e, Some(0)).is_empty(), "{e:?}");
        }
    }

    /// Applies the session's answer to an edit: the queue with `n` tracks.
    fn answer(vm: &mut Vm, n: usize) -> Option<usize> {
        vm.apply(Event::QueueChanged {
            tracks: tracks(n),
            current: None,
        });
        vm.take_queue_selection()
    }

    #[test]
    fn selection_lands_on_the_next_row_after_a_remove() {
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::Remove(2), Some(2));
        assert_eq!(answer(&mut vm, 4), Some(2));
        // Removing the last row selects the new last row.
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::Remove(4), Some(4));
        assert_eq!(answer(&mut vm, 4), Some(3));
        let mut vm = queued(1);
        edit(&mut vm, QueueEdit::Remove(0), Some(0));
        assert_eq!(answer(&mut vm, 0), None);
    }

    #[test]
    fn selection_follows_its_track() {
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::MoveUp(2), Some(2));
        assert_eq!(answer(&mut vm, 5), Some(1));
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::MoveDown(2), Some(2));
        assert_eq!(answer(&mut vm, 5), Some(3));
        // An edit to another row, as from a context menu, keeps the selected track.
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::Remove(1), Some(3));
        assert_eq!(answer(&mut vm, 4), Some(2));
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::MoveDown(2), Some(3));
        assert_eq!(answer(&mut vm, 5), Some(2));
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::Remove(3), Some(1));
        assert_eq!(answer(&mut vm, 4), Some(1));
        let mut vm = queued(5);
        edit(&mut vm, QueueEdit::Clear, Some(1));
        assert_eq!(answer(&mut vm, 0), None);
    }

    #[test]
    fn selection_moves_only_once_and_only_after_an_edit() {
        let mut vm = queued(5);
        assert_eq!(answer(&mut vm, 5), None);
        edit(&mut vm, QueueEdit::Remove(2), Some(2));
        assert_eq!(vm.take_queue_selection(), None, "not before the answer");
        assert_eq!(answer(&mut vm, 4), Some(2));
        assert_eq!(vm.take_queue_selection(), None);
        // A later queue, say the next track starting, leaves the selection alone.
        assert_eq!(answer(&mut vm, 4), None);
        // Without a selection there is nothing to follow.
        edit(&mut vm, QueueEdit::Remove(0), None);
        assert_eq!(answer(&mut vm, 3), None);
    }

    #[test]
    fn removing_the_playing_row_offers_undo() {
        let (mut vm, old) = removed_playing();
        let t = vm.ui().toast.clone().unwrap();
        assert_eq!(
            (t.text.as_str(), t.error, t.action),
            ("Removed “Title 2”", false, Some("Undo"))
        );
        assert_eq!(vm.undo_title(), Some("Undo Remove from Queue"));
        assert_eq!(
            vm.intent(Intent::Undo),
            [
                Command::Enqueue {
                    tracks: old.clone(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(2)
            ]
        );
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        vm.apply(Event::NowPlaying {
            track: old[2].clone(),
            duration: old[2].duration,
        });
        assert_eq!(
            vm.take_commands(),
            vec![Command::Seek(SeekTarget::Absolute(Duration::from_secs(61)))]
        );
    }

    #[test]
    fn removing_another_row_offers_no_undo() {
        let mut vm = {
            let old = tracks(3);
            let mut vm = connected();
            vm.apply(Event::QueueChanged {
                tracks: old.clone(),
                current: Some(2),
            });
            vm.apply(Event::NowPlaying {
                track: old[2].clone(),
                duration: old[2].duration,
            });
            vm
        };
        assert_eq!(
            edit(&mut vm, QueueEdit::Remove(0), None),
            [Command::Remove(0)]
        );
        assert!(vm.ui().toast.is_none());
        assert_eq!(vm.undo_title(), None);
        assert!(vm.intent(Intent::Undo).is_empty());
    }

    #[test]
    fn remove_undo_survives_the_sessions_answer() {
        let (mut vm, old) = removed_playing();
        // The session starts the next track or stops; both leave the queue without row 2.
        vm.apply(Event::QueueChanged {
            tracks: old[..2].to_vec(),
            current: None,
        });
        assert_eq!(vm.undo_title(), Some("Undo Remove from Queue"));
        assert!(vm.ui().toast.is_some());
        // Any other change ends it.
        vm.apply(Event::QueueChanged {
            tracks: old[..1].to_vec(),
            current: None,
        });
        assert_eq!(vm.undo_title(), None);
    }

    #[test]
    fn undo_while_the_next_track_loads_starts_it_from_the_top() {
        let mut vm = connected();
        let old = tracks(3);
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(0),
        });
        vm.apply(Event::NowPlaying {
            track: old[0].clone(),
            duration: old[0].duration,
        });
        vm.apply(Event::Position(Duration::from_secs(200)));
        // The session moves `current` at load start; NowPlaying and position lag until Started.
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(1),
        });
        vm.apply(Event::State(PlayState::Buffering));
        assert_eq!(
            edit(&mut vm, QueueEdit::Remove(1), None),
            [Command::Remove(1)]
        );
        assert_eq!(
            vm.intent(Intent::Undo),
            [
                Command::Enqueue {
                    tracks: old.clone(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(1)
            ]
        );
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(1),
        });
        vm.apply(Event::NowPlaying {
            track: old[1].clone(),
            duration: old[1].duration,
        });
        assert!(vm.take_commands().is_empty());
    }

    /// Playing the third of three queued tracks at 1:01, then that row removed.
    fn removed_playing() -> (Vm, Vec<Track>) {
        let mut vm = connected();
        let old = tracks(3);
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        vm.apply(Event::NowPlaying {
            track: old[2].clone(),
            duration: old[2].duration,
        });
        vm.apply(Event::Position(Duration::from_secs(61)));
        assert_eq!(
            edit(&mut vm, QueueEdit::Remove(2), None),
            [Command::Remove(2)]
        );
        (vm, old)
    }

    /// Playing the third of three queued tracks at 1:01, then cleared.
    fn cleared() -> (Vm, Vec<Track>) {
        let mut vm = connected();
        let old = tracks(3);
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        vm.apply(Event::NowPlaying {
            track: old[2].clone(),
            duration: old[2].duration,
        });
        vm.apply(Event::Position(Duration::from_secs(61)));
        assert_eq!(edit(&mut vm, QueueEdit::Clear, None), [Command::Clear]);
        // The session's answer.
        vm.apply(Event::QueueChanged {
            tracks: Vec::new(),
            current: None,
        });
        vm.apply(Event::State(PlayState::Stopped));
        (vm, old)
    }

    #[test]
    fn clear_toast_says_whether_playback_stopped() {
        let text = |state: Option<PlayState>| {
            let mut vm = queued(3);
            if let Some(s) = state {
                vm.apply(Event::State(s));
            }
            edit(&mut vm, QueueEdit::Clear, None);
            vm.ui().toast.clone().unwrap().text
        };
        let stopped = "Queue cleared and playback stopped";
        assert_eq!(text(Some(PlayState::Playing)), stopped);
        assert_eq!(text(Some(PlayState::Buffering)), stopped);
        // Nothing was heard, so nothing stopped.
        assert_eq!(text(Some(PlayState::Paused)), "Queue cleared");
        assert_eq!(text(None), "Queue cleared");
    }

    #[test]
    fn clearing_the_queue_offers_undo() {
        let (mut vm, old) = cleared();
        let t = vm.ui().toast.clone().unwrap();
        assert_eq!(
            (t.text.as_str(), t.error, t.action),
            ("Queue cleared", false, Some("Undo"))
        );
        assert_eq!(
            vm.intent(Intent::Undo),
            [
                Command::Enqueue {
                    tracks: old.clone(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(2)
            ]
        );
        vm.apply(Event::QueueChanged {
            tracks: old.clone(),
            current: Some(2),
        });
        vm.apply(Event::NowPlaying {
            track: old[2].clone(),
            duration: old[2].duration,
        });
        assert_eq!(
            vm.take_commands(),
            [Command::Seek(SeekTarget::Absolute(Duration::from_secs(61)))]
        );
    }

    #[test]
    fn removing_a_row_offers_no_undo() {
        let mut vm = queued(3);
        edit(&mut vm, QueueEdit::Remove(1), Some(1));
        assert!(vm.ui().toast.is_none());
        assert!(vm.intent(Intent::Undo).is_empty());
    }

    #[test]
    fn dirty_accumulates_until_taken() {
        let mut vm = playing(225);
        vm.apply(Event::Volume(30));
        vm.apply(Event::Position(Duration::from_secs(5)));
        vm.apply(Event::State(PlayState::Paused));
        assert_eq!(
            vm.take_dirty(),
            Dirty::VOLUME | Dirty::ELAPSED | Dirty::SEEK | Dirty::STATE
        );
        assert!(vm.take_dirty().is_empty());
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

    fn provider() -> ProviderId {
        ProviderId("ytmusic".into())
    }

    fn collection(id: &str) -> Collection {
        Collection {
            id: requiem_core::CollectionId {
                provider: provider(),
                id: id.into(),
            },
            kind: requiem_core::CollectionKind::Playlist,
            title: format!("Playlist {id}"),
            subtitle: Some("Playlist".into()),
            artwork_url: None,
        }
    }

    fn shelf(title: &str, items: Vec<Item>) -> Shelf {
        Shelf {
            title: title.into(),
            items,
        }
    }

    fn track_items(n: usize) -> Vec<Item> {
        tracks(n).into_iter().map(Item::Track).collect()
    }

    fn account(state: AccountState) -> Event {
        Event::Account {
            provider: provider(),
            state,
        }
    }

    fn signed_in() -> Event {
        account(AccountState::SignedIn {
            name: Some("Ada".into()),
        })
    }

    /// The reqs of the `Browse` commands in `cmds`, with their pages.
    fn browses(cmds: &[Command]) -> Vec<(u64, Page)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Browse { req, page } => Some((*req, page.clone())),
                _ => None,
            })
            .collect()
    }

    /// A connected vm showing Home, with its fetch sent.
    fn home() -> (Vm, u64) {
        let mut vm = connected();
        let cmds = vm.intent(Intent::Visit(Place::Home));
        let fetches = browses(&cmds);
        let [(req, Page::Home)] = fetches.as_slice() else {
            panic!("expected one Home fetch, got {cmds:?}");
        };
        let req = *req;
        (vm, req)
    }

    fn results(req: u64, page: Page, shelves: Vec<Shelf>) -> Event {
        Event::BrowseResults { req, page, shelves }
    }

    #[test]
    fn browse_answers_land_per_page_and_older_fetches_of_a_page_are_dropped() {
        let (mut vm, home_req) = home();
        vm.apply(signed_in());
        // The sign-in fetched the Library while Home was still loading.
        let lib = browses(&vm.take_commands());
        let [(lib_req, Page::Library)] = lib.as_slice() else {
            panic!("expected the Library fetch, got {lib:?}");
        };
        assert!(lib_req > &home_req);

        // A late Home answer, after the Library request, still lands.
        vm.apply(results(
            home_req,
            Page::Home,
            vec![shelf("Quick picks", track_items(2))],
        ));
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().library.load, Load::Loading);
        assert!(vm.take_dirty().contains(Dirty::HOME));

        // A refresh supersedes the Home fetch in flight; the older answer is dropped.
        let first = browses(&vm.intent(Intent::Refresh))[0].0;
        let second = browses(&vm.intent(Intent::Refresh))[0].0;
        vm.apply(results(first, Page::Home, vec![shelf("Old", Vec::new())]));
        assert_eq!(vm.ui().home.load, Load::Loading);
        assert!(vm.ui().home.shelves.is_empty());
        vm.apply(results(second, Page::Home, vec![shelf("New", Vec::new())]));
        assert_eq!(vm.ui().home.shelves[0].title, "New");
        // Search shares the counter, so no req means two things.
        assert!(search_req(&mut vm, "q") > second);
    }

    #[test]
    fn a_failed_browse_shows_its_error_for_its_own_req_only() {
        let (mut vm, req) = home();
        vm.apply(Event::BrowseFailed {
            req: req + 50,
            error: "x".into(),
            sign_in_required: false,
        });
        assert_eq!(vm.ui().home.load, Load::Loading);
        vm.apply(Event::BrowseFailed {
            req,
            error: "network error".into(),
            sign_in_required: false,
        });
        assert_eq!(
            vm.ui().home.view(),
            PageView::Failed("network error".into())
        );
        // Try Again is a refresh.
        assert_eq!(browses(&vm.intent(Intent::Refresh)).len(), 1);
        assert_eq!(vm.ui().home.view(), PageView::Loading);
    }

    #[test]
    fn home_is_fetched_on_first_show_and_again_after_a_reconnect() {
        let mut vm = Vm::new();
        // Disconnected, the visit fetches nothing; the connection does.
        assert!(vm.intent(Intent::Visit(Place::Home)).is_empty());
        vm.link(Link::Connected);
        let first = browses(&vm.take_commands());
        assert!(matches!(first.as_slice(), [(_, Page::Home)]));
        // Visiting again while loaded or loading asks nothing.
        assert!(vm.intent(Intent::Visit(Place::Home)).is_empty());
        vm.apply(results(
            first[0].0,
            Page::Home,
            vec![shelf("A", Vec::new())],
        ));
        assert!(vm.intent(Intent::Visit(Place::Search)).is_empty());
        assert!(vm.intent(Intent::Visit(Place::Home)).is_empty());

        vm.link(Link::Lost(LinkError::NotResponding));
        vm.link(Link::Connected);
        let again = browses(&vm.take_commands());
        assert!(matches!(again.as_slice(), [(r, Page::Home)] if *r > first[0].0));
        assert!(vm.ui().home.shelves.is_empty());
        // An answer to the lost connection's fetch is dropped.
        vm.apply(results(
            first[0].0,
            Page::Home,
            vec![shelf("Stale", Vec::new())],
        ));
        assert!(vm.ui().home.shelves.is_empty());
    }

    #[test]
    fn the_account_comes_from_the_snapshot_and_from_events() {
        let mut vm = connected();
        assert_eq!(vm.ui().library_view(), LibraryView::Loading);
        vm.apply(account(AccountState::Unavailable("set up a client".into())));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::Unavailable("set up a client".into())
        );
        assert!(vm.take_dirty().contains(Dirty::ACCOUNT));
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(vm.ui().library_view(), LibraryView::SignedOut);
        let pending = AccountState::Pending {
            verification_url: "https://www.google.com/device".into(),
            user_code: "WXYZ-ABCD".into(),
            expires_at_unix: 1_000,
        };
        vm.apply(account(pending));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::Pending {
                verification_url: "https://www.google.com/device".into(),
                user_code: "WXYZ-ABCD".into(),
                expires_at_unix: 1_000,
            }
        );
        // A new connection forgets it until its snapshot tells it again.
        vm.link(Link::Lost(LinkError::NotResponding));
        vm.link(Link::Connected);
        assert_eq!(vm.ui().library_view(), LibraryView::Loading);
        vm.apply(signed_in());
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::SignedIn { name: Some(n), page: PageView::Loading } if n == "Ada"
        ));
    }

    #[test]
    fn sign_in_commands_name_the_accounts_provider() {
        let mut vm = connected();
        assert!(vm.intent(Intent::SignIn).is_empty());
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(
            vm.intent(Intent::SignIn),
            vec![Command::SignIn {
                provider: provider()
            }]
        );
        assert_eq!(
            vm.intent(Intent::SignOut),
            vec![Command::SignOut {
                provider: provider()
            }]
        );
    }

    #[test]
    fn signing_in_fetches_the_library_and_signing_out_forgets_it() {
        let mut vm = connected();
        vm.apply(account(AccountState::SignedOut));
        vm.apply(account(AccountState::Pending {
            verification_url: "https://www.google.com/device".into(),
            user_code: "WXYZ-ABCD".into(),
            expires_at_unix: 1_000,
        }));
        assert!(vm.take_commands().is_empty());
        vm.apply(signed_in());
        let cmds = browses(&vm.take_commands());
        let [(req, Page::Library)] = cmds.as_slice() else {
            panic!("expected the Library fetch, got {cmds:?}");
        };
        vm.apply(results(
            *req,
            Page::Library,
            vec![shelf("Playlists", vec![Item::Collection(collection("a"))])],
        ));
        assert_eq!(vm.ui().library_collections().len(), 1);
        // The same state announced again is not an entry.
        vm.apply(signed_in());
        assert!(vm.take_commands().is_empty());
        vm.apply(account(AccountState::SignedOut));
        assert!(vm.ui().library_collections().is_empty());
        assert_eq!(vm.ui().library.load, Load::Idle);
    }

    #[test]
    fn a_sign_in_required_failure_shows_the_signed_out_library() {
        let mut vm = connected();
        vm.apply(signed_in());
        let req = browses(&vm.take_commands())[0].0;
        vm.apply(Event::BrowseFailed {
            req,
            error: "sign in first".into(),
            sign_in_required: true,
        });
        assert_eq!(vm.ui().library_view(), LibraryView::SignedOut);
        assert!(vm.ui().can_run(PaletteAction::SignIn, Selection::default()));
        // Any other failure keeps the page, with its error.
        let retry = browses(&vm.intent(Intent::Visit(Place::Library)));
        assert!(retry.is_empty(), "a failed page waits for Try Again");
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        vm.apply(Event::BrowseFailed {
            req,
            error: "timeout".into(),
            sign_in_required: false,
        });
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::SignedIn { page: PageView::Failed(e), .. } if e == "timeout"
        ));
    }

    fn with_home_shelf(items: Vec<Item>) -> Vm {
        let (mut vm, req) = home();
        vm.apply(results(req, Page::Home, vec![shelf("Quick picks", items)]));
        vm.take_dirty();
        vm
    }

    #[test]
    fn clicking_a_shelf_track_plays_the_shelf_from_it_like_a_result() {
        let mut items = track_items(5);
        items.insert(2, Item::Collection(collection("c")));
        let mut vm = with_home_shelf(items);
        let want: Vec<Track> = tracks(5)[2..].to_vec();
        // Item 4 is the third track (items: t0 t1 c t2 t3 t4).
        let cmds = vm.intent(Intent::PlayFrom {
            list: List::Shelf(0),
            from: 4,
        });
        assert_eq!(
            cmds,
            vec![
                Command::Enqueue {
                    tracks: want[1..].to_vec(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(0)
            ]
        );
        // Replacing a queue offers Undo, as for a result.
        vm.apply(Event::QueueChanged {
            tracks: tracks(1),
            current: Some(0),
        });
        vm.intent(Intent::PlayFrom {
            list: List::Shelf(0),
            from: 0,
        });
        assert_eq!(vm.ui().toast.as_ref().unwrap().action, Some("Undo"));
        // A collection item is no track.
        assert!(
            vm.intent(Intent::Append {
                list: List::Shelf(0),
                index: 2
            })
            .is_empty()
        );
        assert_eq!(
            vm.intent(Intent::Append {
                list: List::Shelf(0),
                index: 3
            }),
            vec![Command::Enqueue {
                tracks: vec![tracks(5)[2].clone()],
                mode: EnqueueMode::Append
            }]
        );
        assert!(
            vm.intent(Intent::PlayFrom {
                list: List::Shelf(7),
                from: 0
            })
            .is_empty()
        );
    }

    #[test]
    fn a_collection_plays_whole_or_appends_every_track() {
        let mut vm = connected();
        let cmds = vm.intent(Intent::OpenCollection {
            collection: collection("a"),
            from: Place::Library,
        });
        let fetches = browses(&cmds);
        let [(req, Page::Collection(id))] = fetches.as_slice() else {
            panic!("expected the collection fetch, got {cmds:?}");
        };
        assert_eq!(id.id, "a");
        assert_eq!(vm.ui().place, Place::Collection);
        let req = *req;
        // Nothing to play before the tracks arrive.
        assert!(vm.intent(Intent::AppendAll(List::Collection)).is_empty());
        vm.apply(results(
            req,
            Page::Collection(id.clone()),
            vec![shelf("", track_items(3))],
        ));
        assert_eq!(vm.ui().collection.as_ref().unwrap().tracks.len(), 3);
        assert_eq!(
            vm.intent(Intent::PlayFrom {
                list: List::Collection,
                from: 0
            }),
            vec![
                Command::Enqueue {
                    tracks: tracks(3),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(0)
            ]
        );
        assert_eq!(
            vm.intent(Intent::AppendAll(List::Collection)),
            vec![Command::Enqueue {
                tracks: tracks(3),
                mode: EnqueueMode::Append
            }]
        );
        assert_eq!(
            vm.intent(Intent::PlayFrom {
                list: List::Collection,
                from: 2
            }),
            vec![
                Command::Enqueue {
                    tracks: tracks(3)[2..].to_vec(),
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(0)
            ]
        );
    }

    #[test]
    fn refresh_fetches_only_the_current_page() {
        let (mut vm, home_req) = home();
        vm.apply(signed_in());
        let lib_req = browses(&vm.take_commands())[0].0;
        vm.apply(results(home_req, Page::Home, vec![shelf("A", Vec::new())]));
        vm.apply(results(
            lib_req,
            Page::Library,
            vec![shelf("B", Vec::new())],
        ));

        let cmds = vm.intent(Intent::Refresh);
        assert!(matches!(browses(&cmds).as_slice(), [(_, Page::Home)]));
        assert_eq!(vm.ui().library.load, Load::Done);

        vm.intent(Intent::Visit(Place::Library));
        let cmds = vm.intent(Intent::Refresh);
        assert!(matches!(browses(&cmds).as_slice(), [(_, Page::Library)]));

        // Search and Queue have nothing to refetch.
        vm.intent(Intent::Visit(Place::Queue));
        assert!(vm.intent(Intent::Refresh).is_empty());
        vm.intent(Intent::Visit(Place::Search));
        assert!(vm.intent(Intent::Refresh).is_empty());
    }

    #[test]
    fn an_account_error_is_a_toast() {
        let mut vm = connected();
        vm.apply(Event::AccountError {
            provider: provider(),
            error: "the code expired".into(),
        });
        let t = vm.ui().toast.clone().unwrap();
        assert_eq!((t.text.as_str(), t.error), ("the code expired", true));
    }

    #[test]
    fn the_sign_in_text_names_the_whole_address() {
        let hint = sign_in_hint("https://www.google.com/device");
        assert_eq!(
            hint,
            "On any device, go to www.google.com/device and enter this code."
        );
        assert!(hint.contains("/device"));
        assert!(sign_in_hint("example.com/x/").contains("example.com/x and"));
    }

    #[test]
    fn the_code_expiry_counts_whole_minutes_up() {
        assert_eq!(expires_text(1_000 + 600, 1_000), "Code expires in 10 min");
        assert_eq!(expires_text(1_000 + 61, 1_000), "Code expires in 2 min");
        assert_eq!(expires_text(1_000 + 1, 1_000), "Code expires in 1 min");
        assert_eq!(expires_text(1_000, 1_000), "Code expired");
        assert_eq!(expires_text(900, 1_000), "Code expired");
    }

    #[test]
    fn sign_in_and_out_apply_to_their_account_state() {
        let mut vm = connected();
        let can = |vm: &Vm, a| vm.ui().can_run(a, Selection::default());
        assert!(!can(&vm, PaletteAction::SignIn) && !can(&vm, PaletteAction::SignOut));
        vm.apply(account(AccountState::SignedOut));
        assert!(can(&vm, PaletteAction::SignIn) && !can(&vm, PaletteAction::SignOut));
        vm.apply(signed_in());
        assert!(!can(&vm, PaletteAction::SignIn) && can(&vm, PaletteAction::SignOut));
        assert!(PaletteAction::SignIn.is_account() && !PaletteAction::Refresh.is_account());
    }
}
