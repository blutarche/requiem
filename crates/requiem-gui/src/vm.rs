//! View model: folds daemon events and connection status into `UiState`, and maps user
//! intents to `Command`s. No AppKit types, so it is unit-tested on every OS.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::io::{self, Read};
use std::ops::{BitOr, BitOrAssign};
use std::time::{Duration, Instant};

use requiem_core::{
    AccountState, Collection, CollectionKind, Command, EnqueueMode, Event, Item, NO_OAUTH_CLIENT,
    Page, PlayState, ProviderId, Redacted, SeekTarget, Shelf, Track, TrackId,
};
use requiem_ipc::{UpdateState, UpdateStatus};

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
const OUT_OF_DATE: &str = "An older Requiem player is still running. Quit it from its \
                           menu-bar icon; this window reconnects on its own.";
const STUCK_HINT: &str = "If this persists, choose Quit and Stop Music from the Requiem \
                          menu-bar icon, then reopen Requiem.";
/// How long the answer to Check for Updates… stays when there is nothing new.
pub const UP_TO_DATE_FOR: Duration = Duration::from_secs(4);
/// The largest client file the setup sheet sends; providers take no more.
const MAX_CLIENT_FILE: u64 = 64 * 1024;
/// The largest browser session the session form sends; providers take no more.
const MAX_PASTE: usize = 64 * 1024;
/// A YouTube Music row's status while its pasted session is checked.
const CHECKING: &str = "Checking with YouTube Music\u{2026}";
/// Clearing this Mac's pasteboard leaves the copy on a computer the paste came from.
const CLEAR_CLIPBOARD: &str = "Clear the clipboard on the computer you copied from.";

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
    /// The Library's chip, filter text or sort, which only its own page shows.
    pub const LIBRARY_VIEW: Dirty = Dirty(1 << 14);
    pub const ALL: Dirty = Dirty((1 << 15) - 1);

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
    /// Starts a sign-in, or opens the setup sheet for a provider that needs setting up first.
    SignIn(ProviderId),
    SignOut(ProviderId),
    /// The setup sheet's chosen client file, or the client Settings' form built: its text, or
    /// why it couldn't be read or was refused.
    ImportClient(Result<Redacted, String>),
    /// The session form's paste, as `session_json` wrapped it, or why it was refused.
    ImportSession(Result<Redacted, String>),
    /// The setup sheet's Cancel, and each form's.
    CancelSetup,
    /// Opens `provider`'s client form, as the setup sheet does, whatever its account's state:
    /// Settings' Change Google Client… and Use Google Client…, and its form's Save and Choose
    /// Client File….
    ChangeClient(ProviderId),
    /// Opens `provider`'s browser-session form: Use Browser Session… in Settings and on the
    /// setup sheet.
    UseBrowserSession(ProviderId),
    /// Shows one service's items, or all of them, on Home, the Library and in search results.
    /// Local to the window: nothing is sent.
    SetFilter(ServiceFilter),
    /// Keeps one kind of collection on the Library page. Local to the window.
    SetLibraryChip(LibraryChip),
    /// Keeps the Library's collections whose title or subtitle has this text. Local to the
    /// window.
    SetLibraryText(String),
    /// Orders the Library page. Local to the window.
    SetLibrarySort(LibrarySort),
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
    /// The playing track's page; not in the sidebar.
    NowPlaying,
    Settings,
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

    fn name(self) -> &'static str {
        match self {
            Slot::Home => "Home",
            Slot::Library => "the Library",
            Slot::Collection => "this page",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Load {
    /// Not requested yet, or dropped for a reconnect.
    #[default]
    Idle,
    Loading,
    /// Asked again while the page shows its shelves, which stay until the answer replaces them.
    Refreshing,
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
    /// No provider is signed in; each listed provider can be.
    SignedOut(Vec<ProviderId>),
    Pending {
        provider: ProviderId,
        verification_url: String,
        user_code: String,
        expires_at_unix: u64,
    },
    /// The setup sheet: `provider` needs its client before it can sign in.
    Setup {
        provider: ProviderId,
        /// Why, as the provider puts it; `None` when it only says there is no client yet,
        /// which the sheet's steps already say.
        reason: Option<String>,
        /// Why the last client file was refused; shown instead of `reason`.
        error: Option<String>,
    },
    SignedIn {
        name: Option<String>,
        page: PageView,
    },
}

/// The kind of collection a Library chip keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryChip {
    #[default]
    All,
    Playlists,
    Mixes,
    Albums,
}

impl LibraryChip {
    pub const CHOICES: [LibraryChip; 4] = [
        LibraryChip::All,
        LibraryChip::Playlists,
        LibraryChip::Mixes,
        LibraryChip::Albums,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LibraryChip::All => "All",
            LibraryChip::Playlists => "Playlists",
            LibraryChip::Mixes => "Mixes",
            LibraryChip::Albums => "Albums",
        }
    }

    /// Whether `c` is one the chip keeps.
    fn keeps(self, c: &Collection) -> bool {
        match self {
            LibraryChip::All => true,
            LibraryChip::Albums => c.kind == CollectionKind::Album,
            LibraryChip::Mixes => c.kind == CollectionKind::Playlist && is_mix(c),
            LibraryChip::Playlists => c.kind == CollectionKind::Playlist && !is_mix(c),
        }
    }
}

/// The order the Library lists its collections in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibrarySort {
    /// The services' own order, taken in turn.
    #[default]
    Recent,
    Alphabetical,
}

impl LibrarySort {
    pub const CHOICES: [LibrarySort; 2] = [LibrarySort::Recent, LibrarySort::Alphabetical];

    pub fn label(self) -> &'static str {
        match self {
            LibrarySort::Recent => "Recent",
            LibrarySort::Alphabetical => "A\u{2013}Z",
        }
    }

    /// The value saved between launches.
    pub fn key(self) -> &'static str {
        match self {
            LibrarySort::Recent => "recent",
            LibrarySort::Alphabetical => "az",
        }
    }

    /// The sort a saved `key` names; any other value is Recent.
    pub fn from_key(key: &str) -> LibrarySort {
        Self::CHOICES
            .into_iter()
            .find(|s| s.key() == key)
            .unwrap_or_default()
    }
}

// The providers' collection ids, which are private to them. A mix or a radio is a playlist
// whose id, after the prefix, starts as Spotify's own playlists do (Daily Mixes, "This Is",
// Discover Weekly) or as YouTube Music's `RD` radios do.
const SPOTIFY_PLAYLIST: &str = "playlist:";
const SPOTIFY_MIXES: [&str; 2] = ["37i9dQZF1", "37i9dQZEVX"];
const YTMUSIC_PLAYLIST: &str = "browse:VL";
const YTMUSIC_MIX: &str = "RD";
const SPOTIFY_LIKED: &str = "collection";
const YTMUSIC_LIKED: &str = "browse:VLLM";

fn is_mix(c: &Collection) -> bool {
    let id = c.id.id.as_str();
    match c.id.provider.0.as_str() {
        "spotify" => id
            .strip_prefix(SPOTIFY_PLAYLIST)
            .is_some_and(|b| SPOTIFY_MIXES.iter().any(|p| b.starts_with(p))),
        "ytmusic" => id
            .strip_prefix(YTMUSIC_PLAYLIST)
            .is_some_and(|b| b.starts_with(YTMUSIC_MIX)),
        _ => false,
    }
}

/// Where a liked collection sits among the pinned: Spotify's, then YouTube Music's.
fn liked_rank(c: &Collection) -> Option<u8> {
    match (c.id.provider.0.as_str(), c.id.id.as_str()) {
        ("spotify", SPOTIFY_LIKED) => Some(0),
        ("ytmusic", YTMUSIC_LIKED) => Some(1),
        _ => None,
    }
}

/// What the Library page shows once signed in.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryGrid<'a> {
    /// The chips with something to keep, with whether each is the one in force.
    pub chips: Vec<(LibraryChip, bool)>,
    /// The collections to list.
    pub collections: Vec<&'a Collection>,
    /// Whether the Library has any collection at all, before the chip and the filter text.
    pub stocked: bool,
    /// Whether the Library mixes services, so its cards show their service's badge. The chip
    /// and the filter text do not change it.
    pub mixed: bool,
}

/// What the Home page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeView {
    /// Home needs a signed-in provider; each listed provider can be.
    SignedOut(Vec<ProviderId>),
    Page(PageView),
}

/// The setup sheet, while it is open; in Settings, the form chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub provider: ProviderId,
    /// Settings' open form; none while only the sheet is open.
    pub form: Option<Form>,
    pub error: Option<String>,
    /// A client was sent and not answered yet; a `SignedOut` answer starts the sign-in.
    pub importing: bool,
    /// The account's state when the last client was sent. After an error answers it, the
    /// `Account` that follows tells a refusal, unchanged, from an import that stood.
    sent_from: Option<AccountState>,
}

/// How Settings signs YouTube Music in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// The user's own Google client.
    Client,
    /// Request headers copied from a signed-in browser tab.
    Session,
}

/// An account's row in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub provider: ProviderId,
    pub state: AccountRowState,
    /// One line on the account, which VoiceOver reads after the service's name.
    pub status: String,
    /// The row offers its ways to sign in: Use Browser Session…, and Change Google Client…,
    /// or Use Google Client… while it needs setting up. The provider takes both, and neither
    /// form is open.
    pub methods: bool,
}

/// What an account's row in Settings shows and offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountRowState {
    /// Sign Out….
    SignedIn { name: Option<String> },
    /// Sign In….
    SignedOut,
    /// The code to enter, with Copy Code, Open in Browser and Cancel.
    Pending {
        verification_url: String,
        user_code: String,
        expires_at_unix: u64,
    },
    /// Nothing can sign in until a way is chosen: Use Browser Session…, the default, or Use
    /// Google Client….
    NeedsSetup,
    /// The client form.
    Setup(ClientForm),
    /// The browser-session form.
    Session(SessionForm),
    /// Why the provider can't sign in; nothing to do.
    Unavailable(String),
}

/// Settings' form for the user's own Google client: an ID, a secret, or the downloaded file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientForm {
    /// Why the form shows, as the provider puts it, or what setting up or changing the client
    /// takes.
    pub reason: String,
    /// Why the last client was refused; shown instead of `reason`.
    pub error: Option<String>,
}

/// Settings' form for a session pasted from a signed-in browser tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForm {
    /// Why the last paste was refused.
    pub error: Option<String>,
    /// A paste was sent and not answered yet.
    pub checking: bool,
}

/// What every Google OAuth client ID ends in.
const GOOGLE_CLIENT_ID_END: &str = ".apps.googleusercontent.com";
/// The client form's note while it changes a client.
const CHANGE_CLIENT: &str = "Enter the new client's ID and secret, or choose its file.";
/// The client form's note while there is no client yet.
const NEW_CLIENT: &str = "Enter your Google Cloud client's ID and secret, or choose its JSON file.";

/// Why a provider needs setting up, as the provider puts it, unless it only says there is no
/// client yet: the provider's text for that names a file path, and the GUI asks its own way.
fn shown_reason(reason: &str) -> Option<&str> {
    (!reason.starts_with(NO_OAUTH_CLIENT)).then_some(reason)
}

/// The client JSON Settings' Save sends for a typed client ID and secret, which the provider
/// reads as it reads a downloaded file's "installed" part; or why they were refused.
pub fn client_json(id: &str, secret: &str) -> Result<Redacted, String> {
    let (id, secret) = (id.trim(), secret.trim());
    if !client_filled(id, secret) {
        return Err("Enter the client ID and the client secret.".to_owned());
    }
    if !id.ends_with(GOOGLE_CLIENT_ID_END) {
        return Err(format!(
            "That isn't a Google client ID. It ends in {GOOGLE_CLIENT_ID_END}."
        ));
    }
    Ok(Redacted(
        serde_json::json!({ "client_id": id, "client_secret": secret }).to_string(),
    ))
}

/// The import Paste from Clipboard sends for the pasteboard's text, which the provider reads
/// as a browser session; or why it was refused. No message quotes the text.
pub fn session_json(paste: &str) -> Result<Redacted, String> {
    if paste.trim().is_empty() {
        return Err(
            "The clipboard has no text. Copy the request's headers in your browser, then paste."
                .to_owned(),
        );
    }
    if paste.len() > MAX_PASTE {
        return Err(format!(
            "That's larger than {} KiB. Copy the headers of one request named browse.",
            MAX_PASTE / 1024
        ));
    }
    Ok(Redacted(
        serde_json::json!({ "browser_session": paste }).to_string(),
    ))
}

/// Whether both of the client form's fields hold more than whitespace; Save waits for that.
pub fn client_filled(id: &str, secret: &str) -> bool {
    !id.trim().is_empty() && !secret.trim().is_empty()
}

/// Whether `provider` signs in with the user's own client: YouTube Music, with a Google one.
fn takes_client(provider: &ProviderId) -> bool {
    Service::of(provider) == Some(Service::YouTubeMusic)
}

/// The Sign Out sheet's informative text for `provider`. The account doesn't say how it
/// signed in, so a provider that takes a browser session always mentions it.
pub fn sign_out_info(provider: &ProviderId) -> String {
    let service = provider_name(&provider.0);
    let mut info = format!(
        "Your {service} playlists, albums and Home shelves leave Requiem until you sign in again."
    );
    if takes_client(provider) {
        info.push_str(
            " If you signed in with a browser session, Requiem can't sign that browser out; log \
             out there to end it.",
        );
    }
    info
}

impl Browse {
    pub fn view(&self) -> PageView {
        match &self.load {
            Load::Idle | Load::Loading => PageView::Loading,
            Load::Failed { error, .. } => PageView::Failed(error.clone()),
            Load::Refreshing | Load::Done => PageView::Ready,
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

/// The text of the client file the setup sheet's file panel chose, opened or not; the error
/// is the sheet's.
pub fn read_client_file(file: io::Result<impl Read>) -> Result<Redacted, String> {
    let mut bytes = Vec::new();
    file.and_then(|f| f.take(MAX_CLIENT_FILE + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("Couldn’t read that file: {e}"))?;
    if bytes.len() as u64 > MAX_CLIENT_FILE {
        return Err(format!(
            "That file is larger than {} KiB; choose the client JSON you downloaded.",
            MAX_CLIENT_FILE / 1024
        ));
    }
    String::from_utf8(bytes)
        .map(Redacted)
        .map_err(|_| "That file isn’t text; choose the client JSON you downloaded.".to_owned())
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
    /// The daemon's account states, by provider.
    pub accounts: Vec<(ProviderId, AccountState)>,
    pub setup: Option<Setup>,
    /// The player's self-update status: `None` before it sends one, while disconnected, and
    /// in dev builds, which show nothing.
    pub update: Option<UpdateStatus>,
    /// The user's choice of service; `shown_filter` is what it comes to.
    pub filter: ServiceFilter,
    /// The Library's chip, filter text and sort; `library_grid` is what they come to.
    pub library_chip: LibraryChip,
    pub library_text: String,
    pub library_sort: LibrarySort,
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
            setup: None,
            update: None,
            filter: ServiceFilter::All,
            library_chip: LibraryChip::All,
            library_text: String::new(),
            library_sort: LibrarySort::Recent,
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

    /// The playing track's badge, which the player bar and Now Playing show while the queue
    /// mixes services.
    pub fn now_playing_badge(&self) -> Option<Service> {
        let track = self.now_playing.as_ref()?;
        let queue = self.queue.iter().map(|t| &t.id.provider);
        badge(&track.id.provider, mixed(queue))
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
            | PaletteAction::ShowQueue
            | PaletteAction::ShowNowPlaying
            | PaletteAction::ShowSettings => true,
            PaletteAction::ShowServices(_) => self.filter_shown(),
            PaletteAction::Refresh => can.player && self.refreshable(),
            PaletteAction::SignIn(service) => {
                can.player && self.sign_in_offers().contains(&service.provider())
            }
            PaletteAction::SignOut(service) => {
                can.player
                    && matches!(
                        self.account_state(&service.provider()),
                        Some(AccountState::SignedIn { .. })
                    )
            }
        }
    }

    pub fn account_state(&self, provider: &ProviderId) -> Option<&AccountState> {
        self.accounts
            .iter()
            .find(|(p, _)| p == provider)
            .map(|(_, s)| s)
    }

    fn signed_in(&self) -> impl Iterator<Item = &ProviderId> {
        self.accounts
            .iter()
            .filter(|(_, s)| matches!(s, AccountState::SignedIn { .. }))
            .map(|(p, _)| p)
    }

    /// Whether the sidebar offers the service filter: both services are signed in.
    pub fn filter_shown(&self) -> bool {
        Service::ALL
            .into_iter()
            .all(|s| self.signed_in().any(|p| *p == s.provider()))
    }

    /// The filter in force: the user's choice while the sidebar offers it, else All. Signing
    /// out of the chosen service, or of the other, ends the choice for as long as it lasts.
    pub fn shown_filter(&self) -> ServiceFilter {
        if self.filter_shown() {
            self.filter
        } else {
            ServiceFilter::All
        }
    }

    /// The one service the pages are filtered to, if any.
    pub fn shown_service(&self) -> Option<Service> {
        match self.shown_filter() {
            ServiceFilter::All => None,
            ServiceFilter::Only(s) => Some(s),
        }
    }

    /// The search results the filter keeps: the rows of the results table.
    pub fn shown_results(&self) -> Cow<'_, [Track]> {
        match self.shown_service() {
            None => Cow::Borrowed(&self.results),
            Some(s) => Cow::Owned(
                self.results
                    .iter()
                    .filter(|t| s.owns(&t.id.provider))
                    .cloned()
                    .collect(),
            ),
        }
    }

    /// Home's shelves as the filter leaves them: the cards of the shelves it empties are gone
    /// with them, and the rest keep their order. `List::Shelf` indexes this list.
    pub fn shown_home(&self) -> Cow<'_, [Shelf]> {
        match self.shown_service() {
            None => Cow::Borrowed(&self.home.shelves),
            Some(s) => Cow::Owned(
                self.home
                    .shelves
                    .iter()
                    .filter_map(|shelf| {
                        let items: Vec<Item> = shelf
                            .items
                            .iter()
                            .filter(|i| s.owns(item_provider(i)))
                            .cloned()
                            .collect();
                        (!items.is_empty()).then(|| Shelf {
                            title: shelf.title.clone(),
                            items,
                        })
                    })
                    .collect(),
            ),
        }
    }

    /// The providers Sign In applies to: the signed-out ones, the ones that need setting up
    /// first, and every one when the Library or Home failed for want of a sign-in.
    pub fn sign_in_offers(&self) -> Vec<ProviderId> {
        let rejected = [&self.library.load, &self.home.load].into_iter().any(|l| {
            matches!(
                l,
                Load::Failed {
                    sign_in_required: true,
                    ..
                }
            )
        });
        self.accounts
            .iter()
            .filter(|(p, s)| {
                matches!(s, AccountState::SignedOut)
                    || (rejected && matches!(s, AccountState::SignedIn { .. }))
                    || self.needs_setup(p).is_some()
            })
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Why `provider` can't sign in until the user gives it a client, if it is one that takes
    /// one: its `Unavailable` text.
    pub fn needs_setup(&self, provider: &ProviderId) -> Option<&str> {
        match self.account_state(provider)? {
            AccountState::Unavailable(reason) if takes_client(provider) => Some(reason),
            _ => None,
        }
    }

    /// The current place is a browse page that a fetch would fill.
    fn refreshable(&self) -> bool {
        match self.place {
            Place::Home => true,
            Place::Library => self.signed_in().next().is_some(),
            Place::Collection => self.collection.is_some(),
            Place::Search | Place::Queue | Place::NowPlaying | Place::Settings => false,
        }
    }

    /// The sign-in choices when Home failed for want of a sign-in; else the page.
    pub fn home_view(&self) -> HomeView {
        match &self.home.load {
            Load::Failed {
                sign_in_required: true,
                ..
            } => HomeView::SignedOut(self.sign_in_offers()),
            _ => HomeView::Page(self.home.view()),
        }
    }

    /// The setup sheet shows first, as the user's latest choice, then a pending sign-in, then
    /// the Library once any provider is signed in; with none, the sign-in choices. A provider
    /// with its sheet open needs setup, so it has no sign-in pending.
    pub fn library_view(&self) -> LibraryView {
        if self.accounts.is_empty() {
            return LibraryView::Loading;
        }
        if let Some(setup) = &self.setup
            && let Some(reason) = self.needs_setup(&setup.provider)
        {
            return LibraryView::Setup {
                provider: setup.provider.clone(),
                reason: shown_reason(reason).map(str::to_owned),
                error: setup.error.clone(),
            };
        }
        if let Some((
            provider,
            AccountState::Pending {
                verification_url,
                user_code,
                expires_at_unix,
            },
        )) = self
            .accounts
            .iter()
            .find(|(_, s)| matches!(s, AccountState::Pending { .. }))
        {
            return LibraryView::Pending {
                provider: provider.clone(),
                verification_url: verification_url.clone(),
                user_code: user_code.clone(),
                expires_at_unix: *expires_at_unix,
            };
        }
        let offers = self.sign_in_offers();
        if self.signed_in().next().is_some() {
            if matches!(
                self.library.load,
                Load::Failed {
                    sign_in_required: true,
                    ..
                }
            ) {
                return LibraryView::SignedOut(offers);
            }
            let names: Vec<&str> = self
                .accounts
                .iter()
                .filter_map(|(_, s)| match s {
                    AccountState::SignedIn { name } => name.as_deref(),
                    _ => None,
                })
                .collect();
            return LibraryView::SignedIn {
                name: (!names.is_empty()).then(|| names.join(", ")),
                page: self.library.view(),
            };
        }
        if !offers.is_empty() {
            return LibraryView::SignedOut(offers);
        }
        match self.accounts.iter().find_map(|(_, s)| match s {
            AccountState::Unavailable(text) => Some(text.clone()),
            _ => None,
        }) {
            Some(text) => LibraryView::Unavailable(text),
            None => LibraryView::Loading,
        }
    }

    /// Settings' account rows, one per provider, in the session's order. A provider that
    /// needs setting up offers its ways to sign in until one is chosen; the chosen form shows,
    /// as does one opened to change how a provider signs in.
    pub fn settings_view(&self) -> Vec<AccountRow> {
        self.accounts
            .iter()
            .map(|(provider, account)| {
                let setup = self
                    .setup
                    .as_ref()
                    .filter(|s| s.provider == *provider && takes_client(provider));
                let form = setup.and_then(|s| s.form);
                let error = setup.and_then(|s| s.error.clone());
                let checking = setup.is_some_and(|s| s.importing && s.form == Some(Form::Session));
                let reason = self.needs_setup(provider);
                let status = match account {
                    _ if checking => CHECKING.to_owned(),
                    AccountState::SignedIn { name: Some(n) } => format!("Signed in as {n}"),
                    AccountState::SignedIn { name: None } => "Signed in".to_owned(),
                    AccountState::SignedOut => "Not signed in".to_owned(),
                    AccountState::Pending { .. } => "Signing in".to_owned(),
                    _ if reason.is_some() => "Sign-in isn't set up".to_owned(),
                    AccountState::Unavailable(text) => text.clone(),
                };
                let state = match (form, reason, account) {
                    (Some(Form::Session), _, _) => {
                        AccountRowState::Session(SessionForm { error, checking })
                    }
                    (Some(Form::Client), reason, _) => AccountRowState::Setup(ClientForm {
                        reason: match reason {
                            Some(r) => shown_reason(r).unwrap_or(NEW_CLIENT),
                            None => CHANGE_CLIENT,
                        }
                        .to_owned(),
                        error,
                    }),
                    (None, Some(_), _) => AccountRowState::NeedsSetup,
                    (None, None, AccountState::SignedIn { name }) => {
                        AccountRowState::SignedIn { name: name.clone() }
                    }
                    (None, None, AccountState::SignedOut) => AccountRowState::SignedOut,
                    (
                        None,
                        None,
                        AccountState::Pending {
                            verification_url,
                            user_code,
                            expires_at_unix,
                        },
                    ) => AccountRowState::Pending {
                        verification_url: verification_url.clone(),
                        user_code: user_code.clone(),
                        expires_at_unix: *expires_at_unix,
                    },
                    (None, None, AccountState::Unavailable(text)) => {
                        AccountRowState::Unavailable(text.clone())
                    }
                };
                AccountRow {
                    provider: provider.clone(),
                    methods: takes_client(provider) && form.is_none(),
                    state,
                    status,
                }
            })
            .collect()
    }

    /// Whether Change Google Client… asks first: the account is signed in, and a client with
    /// another ID signs it out.
    pub fn change_client_asks(&self, provider: &ProviderId) -> bool {
        matches!(
            self.account_state(provider),
            Some(AccountState::SignedIn { .. })
        )
    }

    /// Where Sign In takes the window: the Library, which shows the code or the setup sheet;
    /// or nowhere from Settings, whose rows show both.
    pub fn sign_in_place(&self) -> Option<Place> {
        (self.place != Place::Settings).then_some(Place::Library)
    }

    /// The playlists and albums of the Library the filter keeps, in shelf order: the rows of
    /// its list.
    pub fn library_collections(&self) -> Vec<&Collection> {
        let service = self.shown_service();
        self.library
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .filter_map(|i| match i {
                Item::Collection(c) if service.is_none_or(|s| s.owns(&c.id.provider)) => Some(c),
                Item::Collection(_) | Item::Track(_) => None,
            })
            .collect()
    }

    /// What the Library page lists. The service switch and the chip pick the collections, the
    /// filter text keeps those whose title or subtitle has it, and the sort orders them, with
    /// the liked collections first, Spotify's before YouTube Music's. `collate` orders titles
    /// for A–Z. A chip with nothing to keep is left out, and with the chosen one gone
    /// the page falls back to All.
    pub fn library_grid(&self, collate: impl Fn(&str, &str) -> Ordering) -> LibraryGrid<'_> {
        let base = self.library_collections();
        let stocked = !base.is_empty();
        let mixed = mixed(base.iter().map(|c| &c.id.provider));
        let mut chips: Vec<(LibraryChip, bool)> = LibraryChip::CHOICES
            .into_iter()
            .filter(|chip| base.iter().any(|c| chip.keeps(c)))
            .map(|chip| (chip, false))
            .collect();
        let chosen = if chips.iter().any(|(c, _)| *c == self.library_chip) {
            self.library_chip
        } else {
            LibraryChip::All
        };
        for (chip, selected) in &mut chips {
            *selected = *chip == chosen;
        }
        let text = self.library_text.to_lowercase();
        let mut collections: Vec<&Collection> = base
            .into_iter()
            .filter(|c| chosen.keeps(c))
            .filter(|c| {
                text.is_empty()
                    || c.title.to_lowercase().contains(&text)
                    || c.subtitle
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&text))
            })
            .collect();
        if self.library_sort == LibrarySort::Alphabetical {
            collections.sort_by(|a, b| collate(&a.title, &b.title));
        }
        collections.sort_by_key(|c| liked_rank(c).unwrap_or(u8::MAX));
        LibraryGrid {
            chips,
            collections,
            stocked,
            mixed,
        }
    }

    /// The tracks of `list` from item `from` on.
    fn list_tracks(&self, list: List, from: usize) -> Vec<Track> {
        match list {
            List::Results => self
                .shown_results()
                .get(from..)
                .unwrap_or_default()
                .to_vec(),
            List::Collection => self
                .collection
                .as_ref()
                .and_then(|c| c.tracks.get(from..))
                .unwrap_or_default()
                .to_vec(),
            List::Shelf(i) => self
                .shown_home()
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
            List::Shelf(i) => match self.shown_home().get(i)?.items.get(index)? {
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

    /// Whether Check for Updates… and the automatic-check setting apply: the player can
    /// update itself.
    pub fn can_check_for_updates(&self) -> bool {
        self.update.as_ref().is_some_and(|u| {
            !matches!(
                u.state,
                UpdateState::NotUpdatable { .. } | UpdateState::Unsupported
            )
        })
    }

    /// Whether the player checks for updates on its own; `None` when it can't update.
    pub fn auto_update(&self) -> Option<bool> {
        let auto = self.update.as_ref()?.auto;
        self.can_check_for_updates().then_some(auto)
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
        if !self.shown_results().is_empty() {
            return None;
        }
        Some(if self.search == SearchStatus::Done {
            EmptyText {
                heading: "No results".to_owned(),
                sub: match self.shown_service() {
                    Some(s) => format!("Nothing from {} matched “{}”", s.name(), self.query),
                    None => format!("Nothing matched “{}”", self.query),
                },
                action: Some("Edit search"),
            }
        } else {
            EmptyText {
                heading: "Search".to_owned(),
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
    /// The label of the banner's button, if it has one: only Restart to Update.
    pub action: Option<&'static str>,
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
    /// The req each browse page waits on, by `Slot`, and whether the fetch was the player's
    /// idea rather than the user's, so its failure goes unsaid. A page drops answers to any
    /// other req, but only its own newer fetch supersedes it: a slow Home answer is not lost to
    /// a Library load.
    pending_browse: [Option<(u64, bool)>; 3],
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
    /// Check for Updates… was chosen and its check has not ended.
    checking_by_hand: bool,
    /// Until when the answer to Check for Updates… says there is nothing new.
    up_to_date_until: Option<Instant>,
    /// A pasted session was taken; the view clears the pasteboard it came from, once.
    session_saved: bool,
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

    /// Whether a pasted session was taken since the last call.
    pub fn take_session_saved(&mut self) -> bool {
        std::mem::take(&mut self.session_saved)
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
        let offline = self.offline_since.and_then(|since| {
            if self.ui.banner.is_none() {
                Some(since + BANNER_GRACE)
            } else if !self.stuck {
                Some(since + STUCK_AFTER)
            } else {
                None
            }
        });
        offline.into_iter().chain(self.up_to_date_until).min()
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
        // An import's error comes just before the `Account` that says whether it stood; any
        // other event first means the error came alone, as older players send a refusal.
        let answered = self
            .ui
            .setup
            .as_mut()
            .filter(|s| !s.importing)
            .and_then(|s| s.sent_from.take());
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
                if let Some((slot, _)) = self.take_browse(req) {
                    self.land(slot, shelves);
                }
            }
            Event::BrowseFailed {
                req,
                error,
                sign_in_required,
            } => {
                if let Some((slot, silent)) = self.take_browse(req) {
                    let refreshing = self
                        .browse_mut(slot)
                        .filter(|b| !sign_in_required && b.load == Load::Refreshing);
                    if let Some(browse) = refreshing {
                        // The stale shelves stay; only the refresh failed.
                        browse.load = Load::Done;
                        if !silent {
                            self.toast(format!("Couldn’t refresh {}: {error}", slot.name()), true);
                        }
                    } else {
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
            }
            Event::Account { provider, state } => {
                let answered = answered.filter(|_| {
                    self.ui
                        .setup
                        .as_ref()
                        .is_some_and(|s| s.provider == provider)
                });
                self.set_account(provider, state, answered);
            }
            Event::AccountError { provider, error } => match &mut self.ui.setup {
                Some(setup) if setup.provider == provider => {
                    setup.error = Some(error);
                    setup.importing = false;
                    self.dirty |= Dirty::LIBRARY;
                }
                _ => self.toast(error, true),
            },
            // The detail is for stderr, which the backend writes it to.
            Event::ProviderBroken { provider, .. } => {
                self.toast(
                    format!(
                        "{} stopped working; Requiem may need an update",
                        provider_name(&provider.0)
                    ),
                    true,
                );
            }
            // Taken as they come, never fetched again: a fetch would start the player fetching
            // anew, and a page that differs on every fetch would never settle.
            Event::PageChanged { page, shelves } => {
                let slot = match page {
                    Page::Home => Some(Slot::Home),
                    Page::Library => Some(Slot::Library),
                    Page::Collection(_) => None,
                };
                // A page not asked for yet gets the newest on its visit. One still loading
                // shows these until its own answer, which is as new or newer, lands.
                let load = slot.and_then(|slot| match self.browse_mut(slot)?.load {
                    Load::Done => Some(Load::Done),
                    Load::Refreshing => Some(Load::Refreshing),
                    Load::Loading if !shelves.is_empty() => Some(Load::Refreshing),
                    _ => None,
                });
                if let (Some(slot), Some(load)) = (slot, load)
                    && self
                        .browse_mut(slot)
                        .is_some_and(|b| b.load != load || b.shelves != shelves)
                {
                    self.set_browse(slot, Browse { load, shelves });
                }
            }
        }
    }

    /// The player's self-update status changed.
    pub fn update(&mut self, status: UpdateStatus) {
        self.update_at(status, Instant::now());
    }

    /// `update` with the clock passed in. "Up to date" shows only as the answer to Check
    /// for Updates…; a failed check shows nothing.
    pub fn update_at(&mut self, status: UpdateStatus, now: Instant) {
        match status.state {
            UpdateState::Checking => {}
            UpdateState::UpToDate if self.checking_by_hand => {
                self.checking_by_hand = false;
                self.up_to_date_until = Some(now + UP_TO_DATE_FOR);
            }
            _ => self.checking_by_hand = false,
        }
        self.ui.update = Some(status);
        self.refresh_banner(now);
    }

    /// Check for Updates…: whether to ask the player, which can't when it can't update.
    pub fn check_for_updates(&mut self) -> bool {
        let can = self.ui.can_check_for_updates();
        self.checking_by_hand |= can;
        can
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
        if !connected {
            // A new connection brings the player's current status.
            self.ui.update = None;
            self.checking_by_hand = false;
            self.up_to_date_until = None;
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
            Intent::SetFilter(filter) => {
                if self.ui.filter != filter {
                    self.ui.filter = filter;
                    self.dirty |= Dirty::RESULTS | Dirty::HOME | Dirty::LIBRARY | Dirty::ACCOUNT;
                }
                Vec::new()
            }
            Intent::SetLibraryChip(chip) => {
                if self.ui.library_chip != chip {
                    self.ui.library_chip = chip;
                    self.dirty |= Dirty::LIBRARY_VIEW;
                }
                Vec::new()
            }
            Intent::SetLibraryText(text) => {
                if self.ui.library_text != text {
                    self.ui.library_text = text;
                    self.dirty |= Dirty::LIBRARY_VIEW;
                }
                Vec::new()
            }
            Intent::SetLibrarySort(sort) => {
                if self.ui.library_sort != sort {
                    self.ui.library_sort = sort;
                    self.dirty |= Dirty::LIBRARY_VIEW;
                }
                Vec::new()
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
                    Place::Search | Place::Queue | Place::NowPlaying | Place::Settings => {
                        return Vec::new();
                    }
                };
                self.fetch(slot, false).into_iter().collect()
            }
            Intent::SignIn(provider) if can.player && self.knows(&provider) => {
                if self.ui.needs_setup(&provider).is_none() {
                    // The Library's sheet would cover the code this sign-in shows. Another
                    // provider's change of client, which only Settings shows, stays open.
                    let covers = self.ui.setup.as_ref().is_some_and(|s| {
                        s.provider == provider || self.ui.needs_setup(&s.provider).is_some()
                    });
                    if covers {
                        self.ui.setup = None;
                        self.dirty |= Dirty::LIBRARY;
                    }
                    return vec![Command::SignIn { provider }];
                }
                self.open_setup(provider, None);
                Vec::new()
            }
            Intent::ChangeClient(provider) if takes_client(&provider) && self.knows(&provider) => {
                self.open_setup(provider, Some(Form::Client));
                Vec::new()
            }
            Intent::UseBrowserSession(provider)
                if takes_client(&provider) && self.knows(&provider) =>
            {
                self.open_setup(provider, Some(Form::Session));
                Vec::new()
            }
            Intent::ImportClient(file) => self.import(Form::Client, file, can.player),
            Intent::ImportSession(paste) => self.import(Form::Session, paste, can.player),
            Intent::CancelSetup => {
                if self.ui.setup.take().is_some() {
                    self.dirty |= Dirty::LIBRARY;
                }
                Vec::new()
            }
            Intent::SignOut(provider) if can.player && self.knows(&provider) => {
                vec![Command::SignOut { provider }]
            }
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

    fn knows(&self, provider: &ProviderId) -> bool {
        self.ui.account_state(provider).is_some()
    }

    /// Opens the setup sheet for `provider`, and in Settings `form`, if any. One already open
    /// for it keeps its error, unless it changes form.
    fn open_setup(&mut self, provider: ProviderId, form: Option<Form>) {
        match &mut self.ui.setup {
            Some(setup) if setup.provider == provider => {
                if form.is_some() && setup.form != form {
                    setup.form = form;
                    setup.error = None;
                    self.dirty |= Dirty::LIBRARY;
                }
            }
            _ => {
                self.ui.setup = Some(Setup {
                    provider,
                    form,
                    error: None,
                    importing: false,
                    sent_from: None,
                });
                self.dirty |= Dirty::LIBRARY;
            }
        }
    }

    /// Sends the open setup's client or session as `form`, or shows why it can't.
    fn import(
        &mut self,
        form: Form,
        json: Result<Redacted, String>,
        connected: bool,
    ) -> Vec<Command> {
        let from = self
            .ui
            .setup
            .as_ref()
            .and_then(|s| self.ui.account_state(&s.provider))
            .cloned();
        let Some(setup) = &mut self.ui.setup else {
            return Vec::new();
        };
        setup.form = Some(form);
        self.dirty |= Dirty::LIBRARY;
        let json = match json {
            Ok(json) if connected => json,
            Ok(_) => {
                setup.error = Some("Not connected to the player. Try again in a moment.".into());
                return Vec::new();
            }
            Err(e) => {
                setup.error = Some(e);
                return Vec::new();
            }
        };
        setup.error = None;
        setup.importing = true;
        setup.sent_from = from;
        vec![Command::ImportSignInClient {
            provider: setup.provider.clone(),
            json,
        }]
    }

    /// The slot that waits on `req`, which then waits on none, and whether its fetch was silent.
    fn take_browse(&mut self, req: u64) -> Option<(Slot, bool)> {
        Slot::ALL.into_iter().find_map(|slot| {
            let waiting = &mut self.pending_browse[slot as usize];
            match *waiting {
                Some((r, silent)) if r == req => {
                    *waiting = None;
                    Some((slot, silent))
                }
                _ => None,
            }
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

    fn browse_mut(&mut self, slot: Slot) -> Option<&mut Browse> {
        match slot {
            Slot::Home => Some(&mut self.ui.home),
            Slot::Library => Some(&mut self.ui.library),
            Slot::Collection => self.ui.collection.as_mut().map(|p| &mut p.browse),
        }
    }

    /// Requests `slot`'s page. A page with shelves keeps them, refreshing, until the answer
    /// lands; one without shows a loading state. Nothing goes out while disconnected; the
    /// reconnect asks again. Library needs a signed-in account. A `silent` fetch that fails
    /// says nothing, unless it replaces one the user asked for.
    fn fetch(&mut self, slot: Slot, silent: bool) -> Option<Command> {
        if !self.ui.controls().player {
            return None;
        }
        let page = match slot {
            Slot::Home => Page::Home,
            Slot::Library => {
                self.ui.signed_in().next()?;
                Page::Library
            }
            Slot::Collection => {
                Page::Collection(self.ui.collection.as_ref()?.collection.id.clone())
            }
        };
        self.last_req += 1;
        let waiting = &mut self.pending_browse[slot as usize];
        let silent = silent && waiting.is_none_or(|(_, s)| s);
        *waiting = Some((self.last_req, silent));
        match self.browse_mut(slot) {
            // Nothing on screen changes, so nothing is redrawn.
            Some(browse) if !browse.shelves.is_empty() => browse.load = Load::Refreshing,
            _ => self.set_browse(
                slot,
                Browse {
                    load: Load::Loading,
                    shelves: Vec::new(),
                },
            ),
        }
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
            Place::Search | Place::Queue | Place::NowPlaying | Place::Settings => {
                return Vec::new();
            }
        };
        if *load != Load::Idle {
            return Vec::new();
        }
        self.fetch(slot, false).into_iter().collect()
    }

    /// A connection starts over: nothing asked before it is answered, and the snapshot that
    /// follows tells the accounts again.
    fn reset_browse(&mut self) {
        self.pending_browse = [None; 3];
        self.ui.home = Browse::default();
        self.ui.library = Browse::default();
        self.ui.accounts.clear();
        self.ui.setup = None;
        if let Some(page) = &mut self.ui.collection {
            page.browse = Browse::default();
            page.tracks.clear();
        }
        self.dirty |=
            Dirty::HOME | Dirty::LIBRARY | Dirty::COLLECTION | Dirty::ACCOUNT | Dirty::RESULTS;
    }

    /// `answered` is the state an import was sent from, when this follows its error.
    fn set_account(
        &mut self,
        provider: ProviderId,
        state: AccountState,
        answered: Option<AccountState>,
    ) {
        // Unchanged after its error, the import was refused, and the form keeps the error.
        let refused = answered.as_ref() == Some(&state);
        // The sheet closes once its provider is set up, and an import it sent goes on to the
        // sign-in it was for. One that stood despite its error says what it could not finish.
        if !refused
            && let Some(setup) = self.ui.setup.take_if(|s| {
                s.provider == provider && !matches!(state, AccountState::Unavailable(_))
            })
        {
            let stood = answered.is_some();
            let sent = setup.importing || stood;
            if sent && state == AccountState::SignedOut {
                self.outbox.push(Command::SignIn {
                    provider: provider.clone(),
                });
            }
            if stood && let Some(error) = setup.error {
                self.toast(error, true);
            } else if sent
                && setup.form == Some(Form::Session)
                && matches!(state, AccountState::SignedIn { .. })
            {
                self.session_saved = true;
                let service = provider_name(&provider.0);
                self.toast(format!("Signed in to {service}. {CLEAR_CLIPBOARD}"), false);
            }
        }
        // The states too, so a signed-in provider whose account changed counts as a change.
        let signed_in = |ui: &UiState| {
            ui.accounts
                .iter()
                .filter(|(_, s)| matches!(s, AccountState::SignedIn { .. }))
                .cloned()
                .collect::<Vec<_>>()
        };
        let was = signed_in(&self.ui);
        let was_filter = self.ui.shown_filter();
        let known = match self.ui.accounts.iter_mut().find(|(p, _)| *p == provider) {
            Some(entry) => {
                entry.1 = state;
                true
            }
            None => {
                self.ui.accounts.push((provider, state));
                false
            }
        };
        self.dirty |= Dirty::ACCOUNT;
        // The filter ends with the second service's sign-out, and its choice returns with the
        // sign-in.
        if self.ui.shown_filter() != was_filter {
            self.dirty |= Dirty::RESULTS | Dirty::HOME | Dirty::LIBRARY;
        }
        // Its sign-in choices follow the accounts.
        if matches!(self.ui.home_view(), HomeView::SignedOut(_)) {
            self.dirty |= Dirty::HOME;
        }
        let now = signed_in(&self.ui);
        if was == now {
            return;
        }
        // The snapshot after a connect only adds providers, and Home was asked for with it.
        let home = known && self.ui.home.load != Load::Idle;
        // Shelves of an account signed out, or another's since, are never shown again; a
        // sign-in that only adds one keeps them until the new answer.
        if was.iter().any(|a| !now.contains(a)) {
            for slot in [Slot::Home, Slot::Library] {
                self.pending_browse[slot as usize] = None;
                self.set_browse(slot, Browse::default());
            }
        }
        if home {
            let fetch = self.fetch(Slot::Home, true);
            self.outbox.extend(fetch);
        }
        if now.is_empty() {
            self.pending_browse[Slot::Library as usize] = None;
            self.set_browse(Slot::Library, Browse::default());
        } else {
            let fetch = self.fetch(Slot::Library, true);
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
                action: None,
            }),
            Some(w) if w < BANNER_GRACE => None,
            Some(_) if self.stuck => Some(Banner {
                text: match reason {
                    Some(r) => format!("{text} {} {STUCK_HINT}", r.text()),
                    None => format!("{text} {STUCK_HINT}"),
                },
                error: false,
                action: None,
            }),
            Some(_) => Some(Banner {
                text: text.to_owned(),
                error: false,
                action: None,
            }),
            None => None,
        };
        if self.up_to_date_until.is_some_and(|t| now >= t) {
            self.up_to_date_until = None;
        }
        let banner = banner.or_else(|| self.update_banner());
        if banner != self.ui.banner {
            self.ui.banner = banner;
            self.dirty |= Dirty::CONNECTION;
        }
    }

    /// The self-update banner, shown when the connection has none.
    fn update_banner(&self) -> Option<Banner> {
        let u = self.ui.update.as_ref()?;
        let (text, action) = match &u.state {
            UpdateState::Ready { version } => (
                format!("Requiem {version} is ready."),
                Some("Restart to Update"),
            ),
            UpdateState::Failed { reason, .. } => (
                format!(
                    "Requiem couldn’t update: {reason}. It keeps running {}.",
                    u.current
                ),
                None,
            ),
            UpdateState::NotUpdatable { .. } => (
                "Move Requiem to Applications to get updates.".to_owned(),
                None,
            ),
            _ if self.up_to_date_until.is_some() => {
                (format!("Requiem {} is up to date.", u.current), None)
            }
            _ => return None,
        };
        Some(Banner {
            text,
            error: false,
            action,
        })
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

/// Whether the queue's row `row` has played: it comes before the current one.
pub fn played(row: usize, current: Option<usize>) -> bool {
    current.is_some_and(|c| row < c)
}

/// How many upcoming tracks Now Playing lists.
pub const UP_NEXT: usize = 5;

/// The tracks after the current one that Now Playing lists; none without a current track.
pub fn up_next(queue: &[Track], current: Option<usize>) -> &[Track] {
    let Some(c) = current else { return &[] };
    let start = (c + 1).min(queue.len());
    &queue[start..(start + UP_NEXT).min(queue.len())]
}

pub fn provider_name(id: &str) -> &str {
    match id {
        "ytmusic" => "YouTube Music",
        "spotify" => "Spotify",
        other => other,
    }
}

/// Whether `providers` holds more than one provider. Only a list that mixes them marks its
/// items with their service's badge.
pub fn mixed<'a>(providers: impl IntoIterator<Item = &'a ProviderId>) -> bool {
    let mut providers = providers.into_iter();
    providers
        .next()
        .is_some_and(|first| providers.any(|p| p != first))
}

/// The badge of an item from `provider` in a list that is `mixed` or not; a provider with no
/// service has none.
pub fn badge(provider: &ProviderId, mixed: bool) -> Option<Service> {
    Service::of(provider).filter(|_| mixed)
}

pub fn item_provider(item: &Item) -> &ProviderId {
    match item {
        Item::Track(t) => &t.id.provider,
        Item::Collection(c) => &c.id.provider,
    }
}

/// A provider the menus and palette have sign-in items for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Spotify,
    YouTubeMusic,
}

impl Service {
    pub const ALL: [Service; 2] = [Service::Spotify, Service::YouTubeMusic];

    fn id(self) -> &'static str {
        match self {
            Service::Spotify => "spotify",
            Service::YouTubeMusic => "ytmusic",
        }
    }

    pub fn provider(self) -> ProviderId {
        ProviderId(self.id().to_owned())
    }

    pub fn name(self) -> &'static str {
        provider_name(self.id())
    }

    pub fn of(provider: &ProviderId) -> Option<Service> {
        Self::ALL.into_iter().find(|s| s.id() == provider.0)
    }

    /// Whether `provider` is this service.
    pub fn owns(self, provider: &ProviderId) -> bool {
        self.id() == provider.0
    }
}

/// Which service's items Home, the Library and search results show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceFilter {
    #[default]
    All,
    Only(Service),
}

impl ServiceFilter {
    /// The segments of the sidebar's control, in order.
    pub const CHOICES: [ServiceFilter; 3] = [
        ServiceFilter::All,
        ServiceFilter::Only(Service::Spotify),
        ServiceFilter::Only(Service::YouTubeMusic),
    ];

    pub fn label(self) -> &'static str {
        match self {
            ServiceFilter::All => "All",
            ServiceFilter::Only(s) => s.name(),
        }
    }

    /// The value saved between launches.
    pub fn key(self) -> &'static str {
        match self {
            ServiceFilter::All => "all",
            ServiceFilter::Only(s) => s.id(),
        }
    }

    /// The filter a saved `key` names; any other value is All.
    pub fn from_key(key: &str) -> ServiceFilter {
        Service::ALL
            .into_iter()
            .find(|s| s.id() == key)
            .map_or(ServiceFilter::All, ServiceFilter::Only)
    }
}

/// What the sign-in panel adds under the code for `provider`.
pub fn sign_in_notice(provider: &ProviderId) -> Option<&'static str> {
    (provider.0 == "spotify").then_some(
        "Spotify doesn't allow unofficial players, so your Spotify account could be suspended.",
    )
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
    ShowNowPlaying,
    ShowSettings,
    /// Filters Home, the Library and search results to one service, or to all.
    ShowServices(ServiceFilter),
    Refresh,
    SignIn(Service),
    SignOut(Service),
    VolumeUp,
    VolumeDown,
    ClearQueue,
}

impl PaletteAction {
    pub const ALL: [PaletteAction; 23] = [
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
        PaletteAction::ShowNowPlaying,
        PaletteAction::ShowSettings,
        PaletteAction::ShowServices(ServiceFilter::All),
        PaletteAction::ShowServices(ServiceFilter::Only(Service::Spotify)),
        PaletteAction::ShowServices(ServiceFilter::Only(Service::YouTubeMusic)),
        PaletteAction::Refresh,
        PaletteAction::SignIn(Service::Spotify),
        PaletteAction::SignOut(Service::Spotify),
        PaletteAction::SignIn(Service::YouTubeMusic),
        PaletteAction::SignOut(Service::YouTubeMusic),
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
            PaletteAction::ShowNowPlaying => "Show now playing",
            PaletteAction::ShowSettings => "Settings",
            PaletteAction::ShowServices(ServiceFilter::All) => "Show All Services",
            PaletteAction::ShowServices(ServiceFilter::Only(Service::Spotify)) => {
                "Show Spotify Only"
            }
            PaletteAction::ShowServices(ServiceFilter::Only(Service::YouTubeMusic)) => {
                "Show YouTube Music Only"
            }
            PaletteAction::Refresh => "Refresh",
            PaletteAction::SignIn(Service::Spotify) => "Sign in to Spotify\u{2026}",
            PaletteAction::SignOut(Service::Spotify) => "Sign out of Spotify",
            PaletteAction::SignIn(Service::YouTubeMusic) => "Sign in to YouTube Music\u{2026}",
            PaletteAction::SignOut(Service::YouTubeMusic) => "Sign out of YouTube Music",
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
            PaletteAction::ShowNowPlaying => "⌘L",
            PaletteAction::ShowSettings => "⌘,",
            PaletteAction::Refresh => "⌘R",
            PaletteAction::ShowServices(_) => "",
            PaletteAction::SignIn(_) | PaletteAction::SignOut(_) => "",
            PaletteAction::VolumeUp => "⌘↑",
            PaletteAction::VolumeDown => "⌘↓",
            PaletteAction::ClearQueue => "",
        }
    }

    /// Sign In and Sign Out are listed only while they apply.
    pub fn is_account(self) -> bool {
        matches!(self, PaletteAction::SignIn(_) | PaletteAction::SignOut(_))
    }

    /// The palette lists these only while they would act.
    pub fn listed_only_when_runnable(self) -> bool {
        self.is_account() || matches!(self, PaletteAction::ShowServices(_))
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
            PaletteAction::ShowNowPlaying,
            PaletteAction::ShowSettings,
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
        // sign-in actions and the service filter, and Refresh waits for a browse page.
        assert_eq!(runnable(&vm, none).len(), PaletteAction::ALL.len() - 10);
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
        assert_eq!(runnable(&vm, result).len(), PaletteAction::ALL.len() - 8);
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
            "YouTube Music stopped working; Requiem may need an update"
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
                error: false,
                action: None
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
                       this persists, choose Quit and Stop Music from the Requiem menu-bar \
                       icon, then reopen Requiem."
                    .into(),
                error: false,
                action: None
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
                error: false,
                action: None
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
                 Quit and Stop Music from the Requiem menu-bar icon, then reopen Requiem."
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
                text: "An older Requiem player is still running. Quit it from its menu-bar \
                       icon; this window reconnects on its own."
                    .into(),
                error: true,
                action: None
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
                heading: "Search".into(),
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
    fn up_next_lists_five_tracks_after_the_current_one() {
        let queue = tracks(8);
        let ids = |s: &[Track]| s.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(up_next(&queue, Some(1))), ids(&queue[2..7]));
        assert_eq!(ids(up_next(&queue, Some(5))), ids(&queue[6..8]));
        assert!(up_next(&queue, Some(7)).is_empty());
        assert!(up_next(&queue, None).is_empty());
        assert!(up_next(&[], Some(0)).is_empty());
    }

    #[test]
    fn only_rows_before_the_current_one_have_played() {
        assert!(played(1, Some(2)));
        assert!(!played(2, Some(2)));
        assert!(!played(3, Some(2)));
        assert!(!played(0, None));
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
                PaletteAction::ShowQueue,
                PaletteAction::ShowNowPlaying,
                PaletteAction::ShowServices(ServiceFilter::All),
                PaletteAction::ShowServices(ServiceFilter::Only(Service::Spotify)),
                PaletteAction::ShowServices(ServiceFilter::Only(Service::YouTubeMusic)),
            ]
        );
        assert_eq!(
            PaletteAction::filter("search"),
            vec![PaletteAction::FocusSearch, PaletteAction::ShowSearch]
        );
        assert_eq!(
            PaletteAction::filter("play"),
            vec![
                PaletteAction::PlayPause,
                PaletteAction::PlaySelected,
                PaletteAction::ShowNowPlaying
            ]
        );
        assert!(PaletteAction::filter("xyz").is_empty());
    }

    #[test]
    fn palette_shortcuts_match_the_menus() {
        let keys: Vec<_> = PaletteAction::ALL.iter().map(|a| a.shortcut()).collect();
        assert_eq!(
            keys,
            [
                "Space", "⌘→", "⌘←", "↩", "⌘↩", "⌘F", "⌘1", "⌘2", "⌘3", "⌘4", "⌘L", "⌘,", "", "",
                "", "⌘R", "", "", "", "", "⌘↑", "⌘↓", ""
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

    fn spotify() -> ProviderId {
        Service::Spotify.provider()
    }

    fn spotify_account(state: AccountState) -> Event {
        Event::Account {
            provider: spotify(),
            state,
        }
    }

    fn spotify_signed_in() -> Event {
        spotify_account(AccountState::SignedIn {
            name: Some("Sam".into()),
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
        assert_eq!(vm.ui().home.load, Load::Refreshing);
        assert_eq!(vm.ui().home.shelves[0].title, "Quick picks");
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
        vm.apply(spotify_account(AccountState::Unavailable(
            "set up a client".into(),
        )));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::Unavailable("set up a client".into())
        );
        assert!(vm.take_dirty().contains(Dirty::ACCOUNT));
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider()])
        );
        let pending = AccountState::Pending {
            verification_url: "https://www.google.com/device".into(),
            user_code: "WXYZ-ABCD".into(),
            expires_at_unix: 1_000,
        };
        vm.apply(account(pending));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::Pending {
                provider: provider(),
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
        assert!(vm.intent(Intent::SignIn(provider())).is_empty());
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_account(AccountState::SignedOut));
        assert_eq!(
            vm.intent(Intent::SignIn(provider())),
            vec![Command::SignIn {
                provider: provider()
            }]
        );
        assert_eq!(
            vm.intent(Intent::SignOut(spotify())),
            vec![Command::SignOut {
                provider: spotify()
            }]
        );
        // A provider the daemon never reported gets nothing.
        assert!(
            vm.intent(Intent::SignIn(ProviderId("nope".into())))
                .is_empty()
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
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider()])
        );
        assert!(vm.ui().can_run(
            PaletteAction::SignIn(Service::YouTubeMusic),
            Selection::default()
        ));
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

    fn pending() -> AccountState {
        AccountState::Pending {
            verification_url: "https://www.spotify.com/pair".into(),
            user_code: "ABCDEF".into(),
            expires_at_unix: 1_000,
        }
    }

    #[test]
    fn home_is_fetched_again_when_an_account_signs_in_or_out() {
        let (mut vm, req) = home();
        // The snapshot's accounts are the ones the first fetch was answered for.
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_signed_in());
        let cmds = browses(&vm.take_commands());
        assert!(matches!(cmds.as_slice(), [(_, Page::Library)]), "{cmds:?}");
        vm.apply(results(req, Page::Home, vec![shelf("A", Vec::new())]));

        vm.apply(spotify_account(pending()));
        let home_fetches = |vm: &mut Vm| {
            browses(&vm.take_commands())
                .into_iter()
                .filter(|(_, p)| *p == Page::Home)
                .count()
        };
        // Signed out, then pending: Home lost an account, whose shelves go.
        assert_eq!(home_fetches(&mut vm), 1);
        assert_eq!(vm.ui().home.load, Load::Loading);
        assert!(vm.ui().home.shelves.is_empty());
        vm.apply(signed_in());
        assert_eq!(home_fetches(&mut vm), 1);
        // The same state announced again changes nothing.
        vm.apply(signed_in());
        assert_eq!(home_fetches(&mut vm), 0);
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(home_fetches(&mut vm), 1);
    }

    /// A connected vm with Home answered and the Library fetched and answered, signed in as
    /// Ada.
    fn loaded() -> Vm {
        let (mut vm, home_req) = home();
        vm.apply(signed_in());
        let lib_req = browses(&vm.take_commands())[0].0;
        vm.apply(results(
            home_req,
            Page::Home,
            vec![shelf("Quick picks", track_items(2))],
        ));
        vm.apply(results(
            lib_req,
            Page::Library,
            vec![shelf("Playlists", track_items(1))],
        ));
        vm.take_dirty();
        vm
    }

    #[test]
    fn a_refresh_keeps_the_shelves_until_the_answer_replaces_them() {
        let mut vm = loaded();
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        assert_eq!(vm.ui().home.load, Load::Refreshing);
        assert_eq!(vm.ui().home.view(), PageView::Ready);
        assert_eq!(vm.ui().home.shelves[0].title, "Quick picks");
        assert!(!vm.take_dirty().contains(Dirty::HOME), "nothing to redraw");
        vm.apply(results(
            req,
            Page::Home,
            vec![shelf("Fresh", track_items(3))],
        ));
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().home.shelves[0].title, "Fresh");
        assert!(vm.take_dirty().contains(Dirty::HOME));
    }

    #[test]
    fn a_refresh_that_fails_keeps_the_shelves_and_says_so() {
        let mut vm = loaded();
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        vm.apply(Event::BrowseFailed {
            req,
            error: "network error".into(),
            sign_in_required: false,
        });
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().home.shelves[0].title, "Quick picks");
        let toast = vm.ui().toast.as_ref().expect("a toast");
        assert!(toast.error);
        assert_eq!(toast.text, "Couldn’t refresh Home: network error");
    }

    #[test]
    fn a_refresh_that_needs_a_sign_in_replaces_the_page() {
        let mut vm = loaded();
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        vm.apply(Event::BrowseFailed {
            req,
            error: "sign-in required".into(),
            sign_in_required: true,
        });
        assert!(vm.ui().home.shelves.is_empty());
        assert!(matches!(vm.ui().home_view(), HomeView::SignedOut(_)));
        assert!(vm.ui().toast.is_none());
    }

    #[test]
    fn a_library_refresh_keeps_the_shelves_too() {
        let mut vm = loaded();
        vm.intent(Intent::Visit(Place::Library));
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        assert_eq!(vm.ui().library.load, Load::Refreshing);
        assert_eq!(vm.ui().library.shelves[0].title, "Playlists");
        vm.apply(Event::BrowseFailed {
            req,
            error: "timeout".into(),
            sign_in_required: false,
        });
        assert_eq!(vm.ui().library.shelves[0].title, "Playlists");
        assert_eq!(
            vm.ui().toast.as_ref().map(|t| t.text.as_str()),
            Some("Couldn’t refresh the Library: timeout")
        );
    }

    #[test]
    fn an_open_collection_refreshes_in_place() {
        let mut vm = loaded();
        let cmds = browses(&vm.intent(Intent::OpenCollection {
            collection: collection("pl"),
            from: Place::Home,
        }));
        let [(req, page)] = cmds.as_slice() else {
            panic!("expected the collection fetch, got {cmds:?}");
        };
        vm.apply(results(
            *req,
            page.clone(),
            vec![shelf("Tracks", track_items(4))],
        ));
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        let shown = vm.ui().collection.as_ref().expect("the page");
        assert_eq!(shown.browse.load, Load::Refreshing);
        assert_eq!(shown.tracks.len(), 4);
        vm.apply(results(
            req,
            page.clone(),
            vec![shelf("Tracks", track_items(2))],
        ));
        assert_eq!(vm.ui().collection.as_ref().unwrap().tracks.len(), 2);
    }

    #[test]
    fn a_new_account_name_refetches_home_and_library_from_scratch() {
        let mut vm = loaded();
        vm.apply(account(AccountState::SignedIn {
            name: Some("Grace".into()),
        }));
        let cmds = browses(&vm.take_commands());
        assert!(
            matches!(
                cmds.as_slice(),
                [(_, Page::Home), (_, Page::Library)] | [(_, Page::Library), (_, Page::Home)]
            ),
            "{cmds:?}"
        );
        // Ada's shelves are never shown as Grace's.
        assert_eq!(vm.ui().home.load, Load::Loading);
        assert_eq!(vm.ui().library.load, Load::Loading);
        assert!(vm.ui().home.shelves.is_empty());
        assert!(vm.ui().library.shelves.is_empty());
    }

    #[test]
    fn a_second_sign_in_keeps_the_shelves_and_its_failed_refetch_says_nothing() {
        let mut vm = loaded();
        vm.apply(spotify_account(AccountState::SignedOut));
        vm.apply(spotify_signed_in());
        let cmds = browses(&vm.take_commands());
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        assert_eq!(vm.ui().home.load, Load::Refreshing);
        assert_eq!(vm.ui().library.load, Load::Refreshing);
        assert_eq!(vm.ui().home.shelves[0].title, "Quick picks");
        for (req, _) in cmds {
            vm.apply(Event::BrowseFailed {
                req,
                error: "timeout".into(),
                sign_in_required: false,
            });
        }
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().library.shelves[0].title, "Playlists");
        assert!(vm.ui().toast.is_none(), "{:?}", vm.ui().toast);
    }

    #[test]
    fn a_signed_out_service_takes_its_shelves_with_it() {
        let mut vm = loaded();
        vm.apply(spotify_signed_in());
        for (req, page) in browses(&vm.take_commands()) {
            vm.apply(results(req, page, vec![shelf("Both", track_items(2))]));
        }
        vm.apply(spotify_account(AccountState::SignedOut));
        let cmds = browses(&vm.take_commands());
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        assert_eq!(vm.ui().home.load, Load::Loading);
        assert_eq!(vm.ui().library.load, Load::Loading);
        assert!(vm.ui().home.shelves.is_empty());
        assert!(vm.ui().library.shelves.is_empty());
    }

    fn changed(page: Page, shelves: Vec<Shelf>) -> Event {
        Event::PageChanged { page, shelves }
    }

    #[test]
    fn a_changed_page_on_screen_takes_its_shelves_without_a_fetch() {
        let mut vm = loaded();
        vm.apply(changed(Page::Home, vec![shelf("Fresh", track_items(3))]));
        assert!(vm.take_commands().is_empty());
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().home.shelves, [shelf("Fresh", track_items(3))]);
        assert!(vm.take_dirty().contains(Dirty::HOME));

        vm.apply(changed(Page::Library, vec![shelf("New", track_items(1))]));
        assert!(vm.take_commands().is_empty());
        assert_eq!(vm.ui().library.load, Load::Done);
        assert_eq!(vm.ui().library.shelves, [shelf("New", track_items(1))]);

        // The same shelves again change nothing.
        vm.take_dirty();
        vm.apply(changed(Page::Library, vec![shelf("New", track_items(1))]));
        assert!(!vm.take_dirty().contains(Dirty::LIBRARY));
    }

    #[test]
    fn a_changed_page_not_yet_asked_for_is_left_for_its_visit() {
        // Home is loading; the Library, with nobody signed in, was never asked for.
        let (mut vm, _) = home();
        assert_eq!(vm.ui().library, Browse::default());
        vm.apply(changed(
            Page::Library,
            vec![shelf("Playlists", track_items(1))],
        ));
        assert_eq!(vm.ui().library, Browse::default());
        assert!(vm.take_commands().is_empty());
    }

    #[test]
    fn a_changed_page_still_loading_shows_the_change_until_its_answer() {
        let (mut vm, req) = home();
        vm.apply(changed(Page::Home, Vec::new()));
        assert_eq!(vm.ui().home.load, Load::Loading, "nothing to show yet");
        vm.apply(changed(Page::Home, vec![shelf("Early", track_items(1))]));
        assert!(vm.take_commands().is_empty());
        assert_eq!(vm.ui().home.view(), PageView::Ready);
        assert_eq!(vm.ui().home.shelves[0].title, "Early");
        vm.apply(results(
            req,
            Page::Home,
            vec![shelf("Later", track_items(2))],
        ));
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().home.shelves[0].title, "Later");
    }

    #[test]
    fn a_change_while_the_user_refreshes_still_reports_the_failure() {
        let mut vm = loaded();
        let req = browses(&vm.intent(Intent::Refresh))[0].0;
        vm.apply(changed(Page::Home, vec![shelf("Fresh", track_items(1))]));
        assert!(vm.take_commands().is_empty());
        assert_eq!(vm.ui().home.load, Load::Refreshing);
        vm.apply(Event::BrowseFailed {
            req,
            error: "network error".into(),
            sign_in_required: false,
        });
        assert_eq!(
            vm.ui().toast.as_ref().map(|t| t.text.as_str()),
            Some("Couldn’t refresh Home: network error")
        );
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().home.shelves[0].title, "Fresh");
    }

    #[test]
    fn pending_ticks_of_a_sign_in_fetch_nothing() {
        let mut vm = loaded();
        vm.apply(spotify_account(AccountState::SignedOut));
        vm.take_commands();
        vm.apply(spotify_account(pending()));
        for _ in 0..2 {
            vm.apply(spotify_account(AccountState::Pending {
                verification_url: "https://www.spotify.com/pair".into(),
                user_code: "GHIJKL".into(),
                expires_at_unix: 2_000,
            }));
        }
        assert!(browses(&vm.take_commands()).is_empty());
        assert_eq!(vm.ui().home.load, Load::Done);
        assert_eq!(vm.ui().library.load, Load::Done);
    }

    #[test]
    fn a_home_never_shown_waits_for_its_visit() {
        let mut vm = connected();
        vm.apply(account(AccountState::SignedOut));
        vm.apply(signed_in());
        let cmds = browses(&vm.take_commands());
        assert!(matches!(cmds.as_slice(), [(_, Page::Library)]), "{cmds:?}");
        assert_eq!(vm.ui().home.load, Load::Idle);
    }

    #[test]
    fn a_home_that_needs_a_sign_in_offers_the_librarys_choices() {
        let (mut vm, req) = home();
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_account(AccountState::SignedOut));
        vm.apply(Event::BrowseFailed {
            req,
            error: "sign-in required: sign in to see your music".into(),
            sign_in_required: true,
        });
        let both = vec![provider(), spotify()];
        assert_eq!(vm.ui().home_view(), HomeView::SignedOut(both.clone()));
        assert_eq!(vm.ui().library_view(), LibraryView::SignedOut(both));
        vm.take_dirty();

        // Its choices follow the accounts.
        vm.apply(spotify_account(pending()));
        assert!(vm.take_dirty().contains(Dirty::HOME));
        assert_eq!(vm.ui().home_view(), HomeView::SignedOut(vec![provider()]));

        // A signed-in account that Home still turns away can sign in again.
        vm.apply(signed_in());
        let req = browses(&vm.take_commands())
            .into_iter()
            .find(|(_, p)| *p == Page::Home)
            .expect("a Home fetch")
            .0;
        assert_eq!(vm.ui().home_view(), HomeView::Page(PageView::Loading));
        vm.apply(Event::BrowseFailed {
            req,
            error: "sign-in required: sign in to Google".into(),
            sign_in_required: true,
        });
        assert_eq!(vm.ui().home_view(), HomeView::SignedOut(vec![provider()]));
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

    /// Connected, with YouTube Music missing its client and its setup sheet open.
    fn setting_up() -> Vm {
        let mut vm = connected();
        vm.apply(account(AccountState::Unavailable("no client".into())));
        assert!(vm.intent(Intent::SignIn(provider())).is_empty());
        vm.take_dirty();
        vm
    }

    fn sheet(error: Option<&str>) -> LibraryView {
        LibraryView::Setup {
            provider: provider(),
            reason: Some("no client".into()),
            error: error.map(str::to_owned),
        }
    }

    const CLIENT: &str = r#"{"installed":{"client_id":"id","client_secret":"s"}}"#;

    fn import(vm: &mut Vm) -> Vec<Command> {
        vm.intent(Intent::ImportClient(Ok(CLIENT.into())))
    }

    #[test]
    fn no_client_yet_is_asked_for_in_the_guis_words() {
        let mut vm = connected();
        vm.apply(account(AccountState::Unavailable(format!(
            "{NO_OAUTH_CLIENT}: save the JSON of a \"TVs and Limited Input devices\" client to \
             /Users/x/Library/Application Support/requiem/google-client.json"
        ))));
        vm.intent(Intent::SignIn(provider()));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::Setup {
                provider: provider(),
                reason: None,
                error: None,
            }
        );
        vm.intent(Intent::ChangeClient(provider()));
        assert_eq!(
            first_row(&vm).state,
            AccountRowState::Setup(ClientForm {
                reason: NEW_CLIENT.into(),
                error: None,
            })
        );
        // Any other reason still shows, as does a refused client.
        vm.apply(account(AccountState::Unavailable(
            "no usable HOME: set REQUIEM_GOOGLE_CLIENT_FILE".into(),
        )));
        vm.apply(Event::AccountError {
            provider: provider(),
            error: "not a Google OAuth client".into(),
        });
        assert_eq!(
            first_row(&vm).state,
            AccountRowState::Setup(ClientForm {
                reason: "no usable HOME: set REQUIEM_GOOGLE_CLIENT_FILE".into(),
                error: Some("not a Google OAuth client".into()),
            })
        );
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::Setup {
                provider: provider(),
                reason: Some("no usable HOME: set REQUIEM_GOOGLE_CLIENT_FILE".into()),
                error: Some("not a Google OAuth client".into()),
            }
        );
    }

    #[test]
    fn a_provider_that_needs_setup_is_offered_and_opens_the_sheet() {
        let mut vm = connected();
        vm.apply(account(AccountState::Unavailable("no client".into())));
        vm.apply(spotify_account(AccountState::SignedOut));
        assert_eq!(vm.ui().sign_in_offers(), [provider(), spotify()]);
        assert_eq!(vm.ui().needs_setup(&provider()), Some("no client"));
        assert_eq!(vm.ui().needs_setup(&spotify()), None);
        assert!(vm.ui().can_run(
            PaletteAction::SignIn(Service::YouTubeMusic),
            Selection::default()
        ));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider(), spotify()])
        );
        // Only YouTube Music takes a client from the user.
        vm.apply(spotify_account(AccountState::Unavailable("nope".into())));
        assert_eq!(vm.ui().sign_in_offers(), [provider()]);
        assert_eq!(vm.ui().needs_setup(&spotify()), None);

        vm.take_dirty();
        assert!(vm.intent(Intent::SignIn(provider())).is_empty());
        assert_eq!(vm.ui().library_view(), sheet(None));
        assert!(vm.take_dirty().contains(Dirty::LIBRARY));
        assert!(vm.intent(Intent::CancelSetup).is_empty());
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider()])
        );
        assert_eq!(vm.ui().toast, None);
    }

    #[test]
    fn an_imported_client_starts_the_sign_in() {
        let mut vm = setting_up();
        assert_eq!(
            import(&mut vm),
            [Command::ImportSignInClient {
                provider: provider(),
                json: CLIENT.into(),
            }]
        );
        assert_eq!(vm.ui().library_view(), sheet(None));
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(
            vm.take_commands(),
            [Command::SignIn {
                provider: provider()
            }]
        );
        assert_eq!(vm.ui().setup, None);
    }

    const UNCONFIRMED: &str = "signed out on this computer, but Google did not confirm; remove requiem's access at https://myaccount.google.com/permissions";

    fn form_error(vm: &Vm) -> Option<String> {
        match first_row(vm).state {
            AccountRowState::Setup(f) => f.error,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_import_that_stood_still_signs_in_and_says_what_it_could_not_finish() {
        let mut vm = connected();
        vm.apply(signed_in());
        vm.take_commands();
        vm.intent(Intent::ChangeClient(provider()));
        assert_eq!(import(&mut vm).len(), 1);
        // The player's order: the error, then the new state.
        vm.apply(Event::AccountError {
            provider: provider(),
            error: UNCONFIRMED.into(),
        });
        // Alone, it reads as a refusal: an older player sends nothing more.
        assert_eq!(form_error(&vm).as_deref(), Some(UNCONFIRMED));
        assert!(!vm.ui().setup.as_ref().unwrap().importing);
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(
            vm.take_commands(),
            [Command::SignIn {
                provider: provider()
            }]
        );
        assert_eq!(vm.ui().setup, None);
        let toast = vm.ui().toast.as_ref().unwrap();
        assert!(toast.error && toast.text == UNCONFIRMED, "{toast:?}");
    }

    #[test]
    fn a_refused_import_keeps_its_form_through_the_unchanged_state() {
        let refused = "not a Google OAuth client";
        let refuse = |vm: &mut Vm| {
            assert_eq!(import(vm).len(), 1);
            vm.apply(Event::AccountError {
                provider: provider(),
                error: refused.into(),
            });
        };
        let mut vm = connected();
        vm.apply(signed_in());
        vm.take_commands();
        vm.intent(Intent::ChangeClient(provider()));
        refuse(&mut vm);
        vm.apply(signed_in());
        assert_eq!(form_error(&vm).as_deref(), Some(refused));
        assert!(vm.take_commands().is_empty());
        assert_eq!(vm.ui().toast, None);
        // So does one that needs setting up.
        let mut vm = setting_up();
        refuse(&mut vm);
        vm.apply(account(AccountState::Unavailable("no client".into())));
        assert_eq!(form_error(&vm).as_deref(), Some(refused));
        // From an older player, the error comes alone, and a later change answers nothing.
        let mut vm = connected();
        vm.apply(signed_in());
        vm.take_commands();
        vm.intent(Intent::ChangeClient(provider()));
        refuse(&mut vm);
        vm.apply(Event::Volume(50));
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(vm.ui().setup, None);
        assert!(
            !vm.take_commands()
                .iter()
                .any(|c| matches!(c, Command::SignIn { .. }))
        );
        assert_eq!(vm.ui().toast, None);
    }

    #[test]
    fn a_refused_client_shows_on_the_sheet() {
        let mut vm = setting_up();
        import(&mut vm);
        vm.apply(Event::AccountError {
            provider: provider(),
            error: "not a Google OAuth client".into(),
        });
        assert_eq!(vm.ui().toast, None);
        assert_eq!(
            vm.ui().library_view(),
            sheet(Some("not a Google OAuth client"))
        );
        assert!(vm.take_dirty().contains(Dirty::LIBRARY));
        // Another provider's error is still a toast.
        vm.apply(Event::AccountError {
            provider: spotify(),
            error: "the code expired".into(),
        });
        assert!(vm.ui().toast.is_some());
        // A file that couldn't be read sends nothing.
        let unread = Err("Couldn’t read that file".to_owned());
        assert!(vm.intent(Intent::ImportClient(unread)).is_empty());
        assert_eq!(
            vm.ui().library_view(),
            sheet(Some("Couldn’t read that file"))
        );
        // The next file clears it, and a still missing client keeps the sheet.
        assert_eq!(import(&mut vm).len(), 1);
        assert_eq!(vm.ui().library_view(), sheet(None));
        vm.apply(account(AccountState::Unavailable("no client".into())));
        assert_eq!(vm.ui().library_view(), sheet(None));
        assert!(vm.take_commands().is_empty());
    }

    #[test]
    fn the_sheet_closes_without_a_sign_in_it_did_not_ask_for() {
        // Set up elsewhere: the sheet closes, and the sign-in waits for the user.
        let mut vm = setting_up();
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(vm.ui().setup, None);
        assert!(vm.take_commands().is_empty());
        // Already signed in once the client is in: nothing to start.
        let mut vm = setting_up();
        import(&mut vm);
        vm.apply(signed_in());
        assert_eq!(vm.ui().setup, None);
        assert!(
            !vm.take_commands()
                .iter()
                .any(|c| matches!(c, Command::SignIn { .. }))
        );
        // Another provider's pending sign-in stays under the sheet, the user's latest choice.
        let mut vm = setting_up();
        vm.apply(spotify_account(AccountState::Pending {
            verification_url: "https://www.spotify.com/pair".into(),
            user_code: "ABC123".into(),
            expires_at_unix: 1_000,
        }));
        assert_eq!(vm.ui().library_view(), sheet(None));
        vm.intent(Intent::CancelSetup);
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::Pending { .. }
        ));
        // Signing in to it closes the sheet, which would cover its code.
        assert!(vm.intent(Intent::SignIn(provider())).is_empty());
        assert_eq!(vm.ui().library_view(), sheet(None));
        vm.take_dirty();
        assert_eq!(
            vm.intent(Intent::SignIn(spotify())),
            [Command::SignIn {
                provider: spotify()
            }]
        );
        assert_eq!(vm.ui().setup, None);
        assert!(vm.take_dirty().contains(Dirty::LIBRARY));
        // A new connection starts the Library over.
        vm.link(Link::Lost(LinkError::NotResponding));
        vm.link(Link::Connected);
        vm.apply(account(AccountState::Unavailable("no client".into())));
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider()])
        );
        assert!(import(&mut vm).is_empty());
    }

    #[test]
    fn a_client_chosen_while_disconnected_says_so() {
        let mut vm = setting_up();
        vm.link(Link::Lost(LinkError::NotResponding));
        vm.take_dirty();
        assert!(import(&mut vm).is_empty());
        let setup = vm.ui().setup.as_ref().unwrap();
        assert_eq!(
            setup.error.as_deref(),
            Some("Not connected to the player. Try again in a moment.")
        );
        assert!(!setup.importing);
        assert!(vm.take_dirty().contains(Dirty::LIBRARY));
    }

    /// The first account row in Settings.
    fn first_row(vm: &Vm) -> AccountRow {
        vm.ui().settings_view().remove(0)
    }

    #[test]
    fn settings_rows_follow_each_account_state() {
        let mut vm = connected();
        assert!(vm.ui().settings_view().is_empty());
        vm.apply(signed_in());
        let row = first_row(&vm);
        assert_eq!(row.provider, provider());
        assert_eq!(
            row.state,
            AccountRowState::SignedIn {
                name: Some("Ada".into())
            }
        );
        assert_eq!(row.status, "Signed in as Ada");
        assert!(row.methods);
        vm.apply(account(AccountState::SignedIn { name: None }));
        assert_eq!(first_row(&vm).status, "Signed in");

        vm.apply(account(AccountState::SignedOut));
        let row = first_row(&vm);
        assert_eq!(row.state, AccountRowState::SignedOut);
        assert_eq!(row.status, "Not signed in");

        vm.apply(account(pending()));
        let row = first_row(&vm);
        assert_eq!(
            row.state,
            AccountRowState::Pending {
                verification_url: "https://www.spotify.com/pair".into(),
                user_code: "ABCDEF".into(),
                expires_at_unix: 1_000,
            }
        );
        assert_eq!(row.status, "Signing in");

        // Nothing can sign in until a way is chosen.
        vm.apply(account(AccountState::Unavailable("no client".into())));
        let row = first_row(&vm);
        assert_eq!(row.state, AccountRowState::NeedsSetup);
        assert_eq!(row.status, "Sign-in isn't set up");
        assert!(row.methods);

        // A provider that takes no client from the user is only unavailable.
        let mut vm = connected();
        vm.apply(spotify_account(AccountState::Unavailable(
            "no helper".into(),
        )));
        let row = first_row(&vm);
        assert_eq!(row.state, AccountRowState::Unavailable("no helper".into()));
        assert_eq!(row.status, "no helper");
        assert!(!row.methods);
    }

    #[test]
    fn settings_shows_each_account_in_its_own_state() {
        let mut vm = connected();
        vm.apply(spotify_signed_in());
        vm.apply(account(pending()));
        let rows = vm.ui().settings_view();
        let states: Vec<_> = rows
            .iter()
            .map(|r| (r.provider.clone(), r.state.clone()))
            .collect();
        assert_eq!(
            states,
            [
                (
                    spotify(),
                    AccountRowState::SignedIn {
                        name: Some("Sam".into())
                    }
                ),
                (
                    provider(),
                    AccountRowState::Pending {
                        verification_url: "https://www.spotify.com/pair".into(),
                        user_code: "ABCDEF".into(),
                        expires_at_unix: 1_000,
                    }
                ),
            ]
        );
        assert!(!rows[0].methods && rows[1].methods);
    }

    #[test]
    fn the_client_form_shows_only_for_the_provider_that_needs_it() {
        let mut vm = setting_up();
        vm.apply(spotify_account(AccountState::SignedOut));
        // The sheet alone opens no form: Settings offers the ways to sign in.
        let rows = vm.ui().settings_view();
        assert_eq!(rows[0].state, AccountRowState::NeedsSetup);
        assert_eq!(rows[1].state, AccountRowState::SignedOut);
        // A file chosen on the sheet opens the client form, which shows its refusal too.
        import(&mut vm);
        vm.apply(Event::AccountError {
            provider: provider(),
            error: "not a Google OAuth client".into(),
        });
        assert_eq!(
            first_row(&vm).state,
            AccountRowState::Setup(ClientForm {
                reason: "no client".into(),
                error: Some("not a Google OAuth client".into()),
            })
        );
        assert!(!first_row(&vm).methods);
        // Cancel closes it, and the ways to sign in show again.
        vm.intent(Intent::CancelSetup);
        assert_eq!(first_row(&vm).state, AccountRowState::NeedsSetup);
        // A provider that takes no client has no form to open.
        assert!(vm.intent(Intent::ChangeClient(spotify())).is_empty());
        assert!(vm.intent(Intent::UseBrowserSession(spotify())).is_empty());
        assert_eq!(vm.ui().setup, None);
        assert_eq!(vm.ui().settings_view()[1].state, AccountRowState::SignedOut);
    }

    #[test]
    fn signing_out_of_youtube_music_says_the_browser_stays_signed_in() {
        assert_eq!(
            sign_out_info(&provider()),
            "Your YouTube Music playlists, albums and Home shelves leave Requiem until you sign \
             in again. If you signed in with a browser session, Requiem can't sign that browser \
             out; log out there to end it."
        );
        assert_eq!(
            sign_out_info(&spotify()),
            "Your Spotify playlists, albums and Home shelves leave Requiem until you sign in again."
        );
    }

    #[test]
    fn sign_out_is_offered_only_while_signed_in() {
        let mut vm = connected();
        for (state, signed_in) in [
            (
                AccountState::SignedIn {
                    name: Some("Sam".into()),
                },
                true,
            ),
            (AccountState::SignedOut, false),
            (pending(), false),
            (AccountState::Unavailable("nope".into()), false),
        ] {
            vm.apply(spotify_account(state));
            let offered = matches!(first_row(&vm).state, AccountRowState::SignedIn { .. });
            assert_eq!(offered, signed_in);
            let can = vm.ui().can_run(
                PaletteAction::SignOut(Service::Spotify),
                Selection::default(),
            );
            assert_eq!(can, signed_in);
        }
    }

    #[test]
    fn a_sign_in_from_settings_stays_in_settings() {
        let mut vm = connected();
        vm.apply(spotify_account(AccountState::SignedOut));
        vm.apply(account(AccountState::Unavailable("no client".into())));
        // Elsewhere, the Library shows the code or the setup sheet.
        vm.intent(Intent::Visit(Place::Home));
        assert_eq!(vm.ui().sign_in_place(), Some(Place::Library));

        vm.intent(Intent::Visit(Place::Settings));
        assert_eq!(vm.ui().sign_in_place(), None);
        assert_eq!(
            vm.intent(Intent::SignIn(spotify())),
            [Command::SignIn {
                provider: spotify()
            }]
        );
        // Its row shows the code, and the other's row the ways to sign in.
        vm.apply(spotify_account(pending()));
        assert!(vm.intent(Intent::SignIn(provider())).is_empty());
        let states: Vec<_> = vm
            .ui()
            .settings_view()
            .into_iter()
            .map(|r| r.state)
            .collect();
        assert!(
            matches!(
                states.as_slice(),
                [AccountRowState::Pending { .. }, AccountRowState::NeedsSetup]
            ),
            "{states:?}"
        );
    }

    #[test]
    fn a_sign_in_to_another_service_keeps_a_client_form_open() {
        let mut vm = connected();
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_account(AccountState::SignedOut));
        vm.intent(Intent::Visit(Place::Settings));
        vm.intent(Intent::ChangeClient(provider()));
        let form = first_row(&vm).state;
        assert!(matches!(form, AccountRowState::Setup(_)), "{form:?}");
        assert_eq!(
            vm.intent(Intent::SignIn(spotify())),
            [Command::SignIn {
                provider: spotify()
            }]
        );
        assert_eq!(first_row(&vm).state, form);
        // Signing in to its own account closes it.
        assert_eq!(
            vm.intent(Intent::SignIn(provider())),
            [Command::SignIn {
                provider: provider()
            }]
        );
        assert_eq!(vm.ui().setup, None);
        assert_eq!(first_row(&vm).state, AccountRowState::SignedOut);
    }

    const CLIENT_ID: &str = "123-abc.apps.googleusercontent.com";
    const SECRET: &str = "GOCSPX-not-a-real-secret";

    #[test]
    fn a_typed_client_reads_as_a_client_file_does() {
        let json = client_json(&format!("  {CLIENT_ID}\n"), &format!(" {SECRET} ")).unwrap();
        let root: serde_json::Value = serde_json::from_str(&json.0).unwrap();
        // As the provider reads it: an "installed" part if there is one, else the top level.
        let client = root.get("installed").unwrap_or(&root);
        let keys: Vec<&str> = client
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["client_id", "client_secret"]);
        assert_eq!(client["client_id"].as_str(), Some(CLIENT_ID));
        assert!(
            client["client_secret"].as_str() == Some(SECRET),
            "the secret didn't round-trip"
        );
        // Quotes and backslashes are escaped, not spliced in.
        let odd = r#"a"b\c"#;
        let json = client_json(CLIENT_ID, odd).unwrap();
        let root: serde_json::Value = serde_json::from_str(&json.0).unwrap();
        assert!(
            root["client_secret"].as_str() == Some(odd),
            "the secret didn't round-trip"
        );
    }

    #[test]
    fn a_typed_client_is_checked_before_it_is_sent() {
        // `err`, so a wrongly accepted client never prints its secret.
        let refused = |id: &str, secret: &str| client_json(id, secret).err();
        let empty = Some("Enter the client ID and the client secret.".to_owned());
        assert_eq!(refused("", SECRET), empty);
        assert_eq!(refused(CLIENT_ID, "  "), empty);
        assert_eq!(refused(" \t", ""), empty);
        let not_google = Some(
            "That isn't a Google client ID. It ends in .apps.googleusercontent.com.".to_owned(),
        );
        assert_eq!(refused("123-abc", SECRET), not_google);
        assert_eq!(
            refused("123-abc.apps.googleusercontent.com.example", SECRET),
            not_google
        );
        assert_eq!(refused(&format!(" {CLIENT_ID} "), SECRET), None);
        // Save waits for both fields.
        assert!(!client_filled("", SECRET));
        assert!(!client_filled(CLIENT_ID, " "));
        assert!(client_filled("x", "y"));
        // A refused client shows on the form and sends nothing.
        let mut vm = setting_up();
        let bad = client_json("123-abc", SECRET);
        assert!(vm.intent(Intent::ImportClient(bad)).is_empty());
        assert!(matches!(
            &first_row(&vm).state,
            AccountRowState::Setup(f) if f.error == not_google
        ));
    }

    #[test]
    fn the_secret_is_sent_but_neither_kept_nor_printed_by_the_vm() {
        let typed = Intent::ImportClient(client_json(CLIENT_ID, SECRET));
        assert!(
            !format!("{typed:?}").contains(SECRET),
            "the intent printed the secret"
        );
        let mut vm = setting_up();
        let cmds = vm.intent(Intent::ImportClient(client_json(CLIENT_ID, SECRET)));
        let [Command::ImportSignInClient { provider: p, json }] = cmds.as_slice() else {
            panic!("expected one client import");
        };
        assert_eq!(*p, provider());
        assert!(json.0.contains(SECRET), "the import lost the secret");
        // Debug output, which logs use, and the vm's state hold no copy.
        assert!(
            !format!("{cmds:?}").contains(SECRET),
            "the command printed the secret"
        );
        assert!(
            !format!("{vm:?}").contains(SECRET),
            "the vm kept the secret"
        );
        // The answer starts the sign-in, still without one.
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(
            vm.take_commands(),
            [Command::SignIn {
                provider: provider()
            }]
        );
        assert!(
            !format!("{vm:?}").contains(SECRET),
            "the vm kept the secret"
        );
    }

    #[test]
    fn changing_the_client_of_a_signed_in_account_asks_first() {
        let mut vm = connected();
        vm.apply(signed_in());
        vm.apply(spotify_signed_in());
        assert!(vm.ui().change_client_asks(&provider()));
        assert!(first_row(&vm).methods);

        // Asked and answered, the form opens over the account, and Cancel closes it.
        vm.take_dirty();
        assert!(vm.intent(Intent::ChangeClient(provider())).is_empty());
        assert!(vm.take_dirty().contains(Dirty::LIBRARY));
        let r = first_row(&vm);
        assert_eq!(
            r.state,
            AccountRowState::Setup(ClientForm {
                reason: CHANGE_CLIENT.into(),
                error: None,
            })
        );
        assert_eq!(r.status, "Signed in as Ada");
        assert!(!r.methods);
        // The Library keeps its list: its sheet is for a provider that needs setting up.
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::SignedIn { .. }
        ));
        vm.intent(Intent::CancelSetup);
        assert_eq!(
            first_row(&vm).state,
            AccountRowState::SignedIn {
                name: Some("Ada".into())
            }
        );

        // A client with another ID signs the account out, and its sign-in starts.
        vm.intent(Intent::ChangeClient(provider()));
        let sent = vm.intent(Intent::ImportClient(client_json(CLIENT_ID, SECRET)));
        assert_eq!(sent.len(), 1);
        vm.apply(account(AccountState::SignedOut));
        assert!(vm.take_commands().contains(&Command::SignIn {
            provider: provider()
        }));
        assert_eq!(vm.ui().setup, None);

        // Signed out or signing in, there is nothing to lose, so it doesn't ask.
        assert!(!vm.ui().change_client_asks(&provider()));
        vm.apply(account(pending()));
        assert!(!vm.ui().change_client_asks(&provider()));
        assert!(first_row(&vm).methods);
    }

    #[test]
    fn a_client_file_is_read_up_to_64_kib_of_text() {
        let read = |bytes: Vec<u8>| read_client_file(Ok(io::Cursor::new(bytes)));
        assert_eq!(read(CLIENT.into()), Ok(Redacted::from(CLIENT)));
        assert_eq!(
            read(vec![b' '; 64 * 1024]).map(|s| s.0.len()),
            Ok(64 * 1024)
        );
        let big = read(vec![b' '; 64 * 1024 + 1]).unwrap_err();
        assert!(big.contains("larger than 64 KiB"), "{big}");
        let binary = read(vec![0xff, 0xfe]).unwrap_err();
        assert!(binary.contains("isn’t text"), "{binary}");
        let missing = read_client_file(Err::<io::Cursor<Vec<u8>>, _>(io::Error::from(
            io::ErrorKind::NotFound,
        )))
        .unwrap_err();
        assert!(missing.starts_with("Couldn’t read that file"), "{missing}");
    }

    /// Made-up request headers, as a browser's developer tools copy them.
    const PASTE: &str = "cookie: SID=not-a-real-cookie; __Secure-3PAPISID=fake\n\"quoted\"\\";

    /// Settings, with YouTube Music missing its client and its session form open.
    fn pasting() -> Vm {
        let mut vm = connected();
        vm.apply(account(AccountState::Unavailable("no client".into())));
        vm.intent(Intent::Visit(Place::Settings));
        vm.take_dirty();
        assert!(vm.intent(Intent::UseBrowserSession(provider())).is_empty());
        vm
    }

    fn session_form(vm: &Vm) -> SessionForm {
        match first_row(vm).state {
            AccountRowState::Session(f) => f,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn use_browser_session_opens_its_form_in_place_of_the_choice() {
        let mut vm = pasting();
        assert!(vm.take_dirty().contains(Dirty::LIBRARY));
        let row = first_row(&vm);
        assert_eq!(
            row.state,
            AccountRowState::Session(SessionForm {
                error: None,
                checking: false,
            })
        );
        assert_eq!(row.status, "Sign-in isn't set up");
        assert!(!row.methods);
        // One form at a time: choosing the client form replaces it, with no error carried over.
        vm.intent(Intent::ImportSession(Err(
            "The clipboard has no text.".into()
        )));
        assert_eq!(
            session_form(&vm).error.as_deref(),
            Some("The clipboard has no text.")
        );
        vm.intent(Intent::ChangeClient(provider()));
        assert!(matches!(
            first_row(&vm).state,
            AccountRowState::Setup(ClientForm { error: None, .. })
        ));
        vm.intent(Intent::UseBrowserSession(provider()));
        assert_eq!(session_form(&vm).error, None);
        // Opened again, it keeps its error.
        vm.intent(Intent::ImportSession(Err(
            "The clipboard has no text.".into()
        )));
        vm.intent(Intent::UseBrowserSession(provider()));
        assert!(session_form(&vm).error.is_some());
        vm.intent(Intent::CancelSetup);
        assert_eq!(first_row(&vm).state, AccountRowState::NeedsSetup);
        // A signed-in account offers it too, beside its client.
        vm.apply(signed_in());
        assert!(first_row(&vm).methods);
        vm.intent(Intent::UseBrowserSession(provider()));
        assert!(matches!(first_row(&vm).state, AccountRowState::Session(_)));
        assert_eq!(first_row(&vm).status, "Signed in as Ada");
    }

    #[test]
    fn the_setup_sheet_opens_the_session_form_in_settings() {
        let mut vm = setting_up();
        assert_eq!(first_row(&vm).state, AccountRowState::NeedsSetup);
        vm.intent(Intent::UseBrowserSession(provider()));
        assert!(matches!(first_row(&vm).state, AccountRowState::Session(_)));
        // The Library still shows the sheet, for a provider that can't sign in yet.
        assert_eq!(vm.ui().library_view(), sheet(None));
    }

    #[test]
    fn a_pasted_session_is_checked_then_signs_in_and_clears_the_paste_once() {
        let mut vm = pasting();
        let cmds = vm.intent(Intent::ImportSession(session_json(PASTE)));
        let [Command::ImportSignInClient { provider: p, json }] = cmds.as_slice() else {
            panic!("expected one import");
        };
        assert_eq!(*p, provider());
        let sent: serde_json::Value = serde_json::from_str(&json.0).unwrap();
        assert!(
            sent == serde_json::json!({ "browser_session": PASTE }),
            "the import lost the paste"
        );
        let row = first_row(&vm);
        assert_eq!(row.status, "Checking with YouTube Music\u{2026}");
        assert!(session_form(&vm).checking);
        assert!(!vm.take_session_saved());
        // Taken: the account is signed in, with no device sign-in to start.
        vm.apply(signed_in());
        assert_eq!(vm.ui().setup, None);
        assert!(
            !vm.take_commands()
                .iter()
                .any(|c| matches!(c, Command::SignIn { .. }))
        );
        assert_eq!(first_row(&vm).status, "Signed in as Ada");
        let toast = vm.ui().toast.clone().unwrap();
        assert!(!toast.error);
        assert_eq!(
            toast.text,
            "Signed in to YouTube Music. Clear the clipboard on the computer you copied from."
        );
        assert!(vm.take_session_saved());
        assert!(!vm.take_session_saved());
        // Neither the commands' Debug, which logs use, nor the vm's state holds the paste.
        assert!(!format!("{cmds:?}").contains("not-a-real-cookie"));
        assert!(!format!("{vm:?}").contains("not-a-real-cookie"));
        // A client's import that stands clears nothing.
        let mut vm = setting_up();
        import(&mut vm);
        vm.apply(signed_in());
        assert!(!vm.take_session_saved());
    }

    #[test]
    fn a_refused_session_keeps_its_form_and_says_why() {
        let refused = "Google didn't accept that session; copy it again from a browser where you're signed in to YouTube Music";
        let mut vm = pasting();
        vm.intent(Intent::ImportSession(session_json(PASTE)));
        vm.apply(Event::AccountError {
            provider: provider(),
            error: refused.into(),
        });
        vm.apply(account(AccountState::Unavailable("no client".into())));
        assert_eq!(
            session_form(&vm),
            SessionForm {
                error: Some(refused.into()),
                checking: false,
            }
        );
        assert_eq!(first_row(&vm).status, "Sign-in isn't set up");
        assert_eq!(vm.ui().toast, None);
        assert!(!vm.take_session_saved());
        assert!(vm.take_commands().is_empty());
        // So is one sent while disconnected.
        let mut vm = pasting();
        vm.link(Link::Lost(LinkError::NotResponding));
        assert!(
            vm.intent(Intent::ImportSession(session_json(PASTE)))
                .is_empty()
        );
        assert_eq!(
            session_form(&vm).error.as_deref(),
            Some("Not connected to the player. Try again in a moment.")
        );
    }

    #[test]
    fn a_paste_is_checked_before_it_is_sent_and_never_quoted() {
        let empty = session_json(" \n\t").unwrap_err();
        assert!(empty.starts_with("The clipboard has no text."), "{empty}");
        let mut big = String::from("cookie: SID=not-a-real-cookie; ");
        big.push_str(&"x".repeat(64 * 1024));
        let too_big = session_json(&big).unwrap_err();
        assert!(too_big.contains("larger than 64 KiB"), "{too_big}");
        assert!(!too_big.contains("not-a-real-cookie"));
        let full = "x".repeat(64 * 1024);
        assert!(session_json(&full).is_ok());
        let ok = session_json(PASTE).unwrap();
        assert!(!format!("{ok:?}").contains("not-a-real-cookie"));
        // A refused paste shows on the form and sends nothing.
        let mut vm = pasting();
        assert!(
            vm.intent(Intent::ImportSession(session_json("")))
                .is_empty()
        );
        assert!(session_form(&vm).error.is_some());
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
    fn sign_in_and_out_apply_to_their_providers_state() {
        let mut vm = connected();
        let can = |vm: &Vm, a| vm.ui().can_run(a, Selection::default());
        let [sp_in, sp_out] = [
            PaletteAction::SignIn(Service::Spotify),
            PaletteAction::SignOut(Service::Spotify),
        ];
        let [yt_in, yt_out] = [
            PaletteAction::SignIn(Service::YouTubeMusic),
            PaletteAction::SignOut(Service::YouTubeMusic),
        ];
        assert!(
            ![sp_in, sp_out, yt_in, yt_out]
                .into_iter()
                .any(|a| can(&vm, a))
        );
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_signed_in());
        assert!(can(&vm, yt_in) && !can(&vm, yt_out));
        assert!(!can(&vm, sp_in) && can(&vm, sp_out));
        vm.apply(signed_in());
        vm.apply(spotify_account(AccountState::SignedOut));
        assert!(!can(&vm, yt_in) && can(&vm, yt_out));
        assert!(can(&vm, sp_in) && !can(&vm, sp_out));
        assert!(sp_in.is_account() && !PaletteAction::Refresh.is_account());
    }

    #[test]
    fn the_menu_and_palette_name_each_provider() {
        let labels: Vec<_> = PaletteAction::ALL
            .into_iter()
            .filter(|a| a.is_account())
            .map(PaletteAction::label)
            .collect();
        assert_eq!(
            labels,
            [
                "Sign in to Spotify\u{2026}",
                "Sign out of Spotify",
                "Sign in to YouTube Music\u{2026}",
                "Sign out of YouTube Music",
            ]
        );
        assert!(
            PaletteAction::ALL
                .iter()
                .all(|a| !a.label().contains("Google"))
        );
        assert_eq!(Service::of(&spotify()), Some(Service::Spotify));
        assert_eq!(Service::of(&ProviderId("demo".into())), None);
    }

    #[test]
    fn the_library_fetches_while_any_provider_is_signed_in() {
        let mut vm = connected();
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_account(AccountState::SignedOut));
        assert!(browses(&vm.take_commands()).is_empty());
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider(), spotify()])
        );
        vm.apply(spotify_signed_in());
        assert_eq!(browses(&vm.take_commands()).len(), 1);
        // The other signing in changes what the Library merges, so it asks again.
        vm.apply(signed_in());
        assert_eq!(browses(&vm.take_commands()).len(), 1);
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::SignedIn { name: Some(n), .. } if n == "Ada, Sam"
        ));
        // One signing out leaves the Library fetched for the rest.
        vm.apply(spotify_account(AccountState::SignedOut));
        assert_eq!(browses(&vm.take_commands()).len(), 1);
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::SignedIn { .. }
        ));
        // The same set signed in is not a reason to ask.
        vm.apply(signed_in());
        assert!(vm.take_commands().is_empty());
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(vm.ui().library.load, Load::Idle);
        assert_eq!(
            vm.ui().library_view(),
            LibraryView::SignedOut(vec![provider(), spotify()])
        );
    }

    #[test]
    fn the_sign_in_panel_names_the_provider_that_is_pending() {
        let mut vm = connected();
        vm.apply(account(AccountState::SignedOut));
        vm.apply(spotify_account(AccountState::Pending {
            verification_url: "https://www.spotify.com/pair".into(),
            user_code: "ABC123".into(),
            expires_at_unix: 1_000,
        }));
        assert!(matches!(
            vm.ui().library_view(),
            LibraryView::Pending { provider, .. } if provider == spotify()
        ));
        assert!(sign_in_notice(&spotify()).unwrap().contains("suspended"));
        assert_eq!(sign_in_notice(&provider()), None);
        // The cancel sends Sign Out for the pending provider.
        assert_eq!(
            vm.intent(Intent::SignOut(spotify())),
            vec![Command::SignOut {
                provider: spotify()
            }]
        );
    }

    #[test]
    fn provider_names_are_the_services_names() {
        assert_eq!(provider_name("spotify"), "Spotify");
        assert_eq!(provider_name("ytmusic"), "YouTube Music");
    }

    #[test]
    fn only_a_list_of_two_or_more_providers_is_mixed() {
        let (s, y) = (spotify(), provider());
        assert!(!mixed([]));
        assert!(!mixed([&s]));
        assert!(!mixed([&s, &s, &s]));
        assert!(mixed([&s, &s, &y]));
        assert!(mixed([&y, &s]));
    }

    #[test]
    fn badges_mark_the_known_services_of_a_mixed_list() {
        let other = ProviderId("other".into());
        assert_eq!(badge(&spotify(), true), Some(Service::Spotify));
        assert_eq!(badge(&provider(), true), Some(Service::YouTubeMusic));
        assert_eq!(badge(&other, true), None);
        assert_eq!(badge(&spotify(), false), None);
    }

    #[test]
    fn the_playing_track_has_a_badge_while_the_queue_mixes_services() {
        let from = |p: ProviderId, id: &str| {
            let mut t = track(id, 200);
            t.id.provider = p;
            t
        };
        let mut vm = connected();
        let (a, b) = (from(spotify(), "a"), from(provider(), "b"));
        vm.apply(Event::QueueChanged {
            tracks: vec![a.clone(), from(spotify(), "c")],
            current: Some(0),
        });
        vm.apply(Event::NowPlaying {
            track: a.clone(),
            duration: None,
        });
        vm.apply(Event::State(PlayState::Playing));
        assert_eq!(vm.ui().now_playing_badge(), None);
        vm.apply(Event::QueueChanged {
            tracks: vec![a, b],
            current: Some(0),
        });
        assert_eq!(vm.ui().now_playing_badge(), Some(Service::Spotify));
        vm.apply(Event::State(PlayState::Stopped));
        assert_eq!(vm.ui().now_playing_badge(), None);
    }

    fn status(state: UpdateState) -> UpdateStatus {
        UpdateStatus {
            current: "0.2.0".into(),
            auto: true,
            state,
        }
    }

    fn banner_text(vm: &Vm) -> Option<&str> {
        vm.ui().banner.as_ref().map(|b| b.text.as_str())
    }

    #[test]
    fn update_banners_follow_the_player() {
        let mut vm = connected();
        vm.update(status(UpdateState::Ready {
            version: "0.2.1".into(),
        }));
        assert_eq!(
            vm.ui().banner,
            Some(Banner {
                text: "Requiem 0.2.1 is ready.".into(),
                error: false,
                action: Some("Restart to Update")
            })
        );
        assert_eq!(vm.take_dirty(), Dirty::CONNECTION);
        vm.update(status(UpdateState::Failed {
            version: "0.2.1".into(),
            reason: "the new version did not start".into(),
        }));
        assert_eq!(
            vm.ui().banner,
            Some(Banner {
                text: "Requiem couldn’t update: the new version did not start. It keeps \
                       running 0.2.0."
                    .into(),
                error: false,
                action: None
            })
        );
        vm.update(status(UpdateState::NotUpdatable {
            reason: "it runs from a disk image".into(),
        }));
        assert_eq!(
            banner_text(&vm),
            Some("Move Requiem to Applications to get updates.")
        );
        for quiet in [
            UpdateState::Idle,
            UpdateState::Checking,
            UpdateState::UpToDate,
            UpdateState::Downloading {
                version: "0.2.1".into(),
            },
            UpdateState::Error {
                reason: "the update check failed: offline".into(),
            },
            UpdateState::Unsupported,
        ] {
            vm.update(status(quiet.clone()));
            assert_eq!(vm.ui().banner, None, "{quiet:?}");
        }
    }

    #[test]
    fn up_to_date_answers_only_check_for_updates_and_clears_itself() {
        let mut vm = connected();
        let t0 = Instant::now();
        vm.update_at(status(UpdateState::UpToDate), t0);
        assert_eq!(vm.ui().banner, None, "a scheduled check says nothing");
        assert!(vm.check_for_updates());
        vm.update_at(status(UpdateState::Checking), t0);
        assert_eq!(vm.ui().banner, None);
        vm.update_at(status(UpdateState::UpToDate), t0);
        assert_eq!(banner_text(&vm), Some("Requiem 0.2.0 is up to date."));
        assert_eq!(vm.next_tick(), Some(t0 + UP_TO_DATE_FOR));
        vm.take_dirty();
        vm.tick_at(t0 + UP_TO_DATE_FOR);
        assert_eq!(vm.ui().banner, None);
        assert_eq!(vm.take_dirty(), Dirty::CONNECTION);
        assert_eq!(vm.next_tick(), None);
        // Answered once: the next scheduled check is quiet again.
        vm.update_at(status(UpdateState::UpToDate), t0 + UP_TO_DATE_FOR);
        assert_eq!(vm.ui().banner, None);
    }

    #[test]
    fn a_failed_check_shows_nothing() {
        let mut vm = connected();
        vm.update(status(UpdateState::Idle));
        assert!(vm.check_for_updates());
        vm.update(status(UpdateState::Checking));
        vm.update(status(UpdateState::Idle));
        assert_eq!(vm.ui().banner, None);
        vm.update(status(UpdateState::UpToDate));
        assert_eq!(vm.ui().banner, None);
    }

    #[test]
    fn a_lost_player_takes_its_update_status_along() {
        let mut vm = connected();
        let t0 = Instant::now();
        vm.update_at(
            status(UpdateState::Ready {
                version: "0.2.1".into(),
            }),
            t0,
        );
        vm.link_at(Link::Lost(LinkError::NotResponding), t0);
        assert_eq!(vm.ui().update, None);
        assert_eq!(vm.ui().banner, None);
        assert!(!vm.ui().can_check_for_updates());
        vm.tick_at(t0 + BANNER_GRACE);
        assert_eq!(
            banner_text(&vm),
            Some("Player disconnected — reconnecting…")
        );
        vm.link_at(Link::Connected, t0 + BANNER_GRACE);
        assert_eq!(vm.ui().banner, None);
    }

    #[test]
    fn update_menus_need_a_player_that_can_update() {
        let mut vm = connected();
        assert!(!vm.ui().can_check_for_updates());
        assert!(!vm.check_for_updates());
        assert_eq!(vm.ui().auto_update(), None);
        for state in [
            UpdateState::NotUpdatable {
                reason: "it runs from a disk image".into(),
            },
            UpdateState::Unsupported,
        ] {
            vm.update(status(state));
            assert!(!vm.ui().can_check_for_updates());
            assert_eq!(vm.ui().auto_update(), None);
        }
        vm.update(UpdateStatus {
            auto: false,
            ..status(UpdateState::Idle)
        });
        assert!(vm.ui().can_check_for_updates());
        assert_eq!(vm.ui().auto_update(), Some(false));
        assert!(vm.check_for_updates());
    }

    /// A track of `provider`, titled by `id`.
    fn from(provider: &ProviderId, id: &str) -> Track {
        let mut t = track(id, 200);
        t.id.provider = provider.clone();
        t
    }

    fn collection_of(provider: &ProviderId, id: &str) -> Collection {
        let mut c = collection(id);
        c.id.provider = provider.clone();
        c
    }

    fn ids(tracks: &[Track]) -> Vec<&str> {
        tracks.iter().map(|t| t.id.id.as_str()).collect()
    }

    /// Search results that take the services in turn, Spotify first: 0 to 5.
    fn alternating() -> Vec<Track> {
        (0..6)
            .map(|i| from(&[spotify(), provider()][i % 2], &i.to_string()))
            .collect()
    }

    /// A connected vm with both services signed in.
    fn both_signed_in() -> Vm {
        let mut vm = connected();
        vm.apply(signed_in());
        vm.apply(spotify_signed_in());
        vm.take_dirty();
        vm
    }

    fn search_lands(vm: &mut Vm, tracks: Vec<Track>) {
        let req = search_req(vm, "q");
        vm.apply(Event::SearchResults { req, tracks });
    }

    const ONLY_SPOTIFY: ServiceFilter = ServiceFilter::Only(Service::Spotify);
    const ONLY_YT: ServiceFilter = ServiceFilter::Only(Service::YouTubeMusic);

    #[test]
    fn the_saved_filter_is_read_by_its_key_and_anything_else_is_all() {
        for filter in ServiceFilter::CHOICES {
            assert_eq!(ServiceFilter::from_key(filter.key()), filter);
        }
        assert_eq!(ONLY_SPOTIFY.key(), "spotify");
        assert_eq!(ONLY_YT.key(), "ytmusic");
        assert_eq!(ServiceFilter::All.key(), "all");
        for unknown in ["", "Spotify", "youtube", "all ", "tidal"] {
            assert_eq!(ServiceFilter::from_key(unknown), ServiceFilter::All);
        }
        assert_eq!(UiState::default().filter, ServiceFilter::All);
    }

    #[test]
    fn the_filter_is_offered_only_while_both_services_are_signed_in() {
        let mut vm = connected();
        let can = |vm: &Vm, a| vm.ui().can_run(a, Selection::default());
        let actions = ServiceFilter::CHOICES.map(PaletteAction::ShowServices);
        assert!(!vm.ui().filter_shown());
        assert!(!actions.iter().any(|&a| can(&vm, a)));
        vm.apply(signed_in());
        assert!(!vm.ui().filter_shown());
        vm.apply(spotify_account(AccountState::SignedOut));
        assert!(!vm.ui().filter_shown());
        assert!(!actions.iter().any(|&a| can(&vm, a)));
        vm.apply(spotify_signed_in());
        assert!(vm.ui().filter_shown());
        assert!(actions.iter().all(|&a| can(&vm, a)));
        assert!(actions.iter().all(|a| a.listed_only_when_runnable()));
        let labels: Vec<_> = actions.iter().map(|a| a.label()).collect();
        assert_eq!(
            labels,
            [
                "Show All Services",
                "Show Spotify Only",
                "Show YouTube Music Only"
            ]
        );
    }

    #[test]
    fn choosing_a_service_filters_search_results_and_sends_nothing() {
        let mut vm = both_signed_in();
        search_lands(&mut vm, alternating());
        vm.take_commands();
        vm.take_dirty();
        assert_eq!(
            ids(&vm.ui().shown_results()),
            ["0", "1", "2", "3", "4", "5"]
        );

        assert!(vm.intent(Intent::SetFilter(ONLY_SPOTIFY)).is_empty());
        assert!(vm.take_commands().is_empty());
        let dirty = vm.take_dirty();
        assert!(dirty.contains(Dirty::RESULTS | Dirty::HOME | Dirty::LIBRARY | Dirty::ACCOUNT));
        assert_eq!(ids(&vm.ui().shown_results()), ["0", "2", "4"]);
        // The daemon's answer is kept whole, so another choice needs no search.
        assert_eq!(vm.ui().results.len(), 6);

        vm.intent(Intent::SetFilter(ONLY_YT));
        assert_eq!(ids(&vm.ui().shown_results()), ["1", "3", "5"]);
        vm.intent(Intent::SetFilter(ServiceFilter::All));
        assert_eq!(vm.ui().shown_results().len(), 6);

        // Choosing what is chosen already changes nothing.
        vm.take_dirty();
        vm.intent(Intent::SetFilter(ServiceFilter::All));
        assert!(vm.take_dirty().is_empty());
    }

    #[test]
    fn a_service_with_no_results_says_so() {
        let mut vm = both_signed_in();
        search_lands(&mut vm, vec![from(&spotify(), "0")]);
        assert_eq!(vm.ui().results_empty(), None);
        vm.intent(Intent::SetFilter(ONLY_YT));
        assert!(vm.ui().shown_results().is_empty());
        let empty = vm.ui().results_empty().unwrap();
        assert_eq!(empty.heading, "No results");
        assert_eq!(empty.sub, "Nothing from YouTube Music matched “q”");
    }

    #[test]
    fn playing_from_a_filtered_list_queues_the_visible_tracks_from_the_clicked_one() {
        let mut vm = both_signed_in();
        search_lands(&mut vm, alternating());
        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        // The rows are Spotify's 0, 2 and 4; the second, 2, is clicked.
        let cmds = vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 1,
        });
        assert_eq!(
            cmds,
            vec![
                Command::Enqueue {
                    tracks: vec![from(&spotify(), "2"), from(&spotify(), "4")],
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(0)
            ]
        );
        assert_eq!(
            vm.intent(Intent::Append {
                list: List::Results,
                index: 2
            }),
            vec![Command::Enqueue {
                tracks: vec![from(&spotify(), "4")],
                mode: EnqueueMode::Append
            }]
        );
        let Command::Enqueue { tracks, .. } = &vm.intent(Intent::AppendAll(List::Results))[0]
        else {
            panic!("expected an enqueue");
        };
        assert_eq!(ids(tracks), ["0", "2", "4"]);
        // Past the visible rows is past the list, though the answer has more.
        let past = vm.intent(Intent::PlayFrom {
            list: List::Results,
            from: 3,
        });
        assert!(past.is_empty());
    }

    /// Home's shelves: Liked 0 to 3 alternate, one shelf is YouTube Music's alone, Mixes
    /// holds a playlist each, and one shelf is empty.
    fn mixed_home() -> Vec<Shelf> {
        let items = |tracks: &[(&ProviderId, &str)]| {
            tracks
                .iter()
                .map(|(p, id)| Item::Track(from(p, id)))
                .collect()
        };
        let (s, y) = (spotify(), provider());
        vec![
            shelf(
                "Liked",
                items(&[(&s, "0"), (&y, "1"), (&s, "2"), (&y, "3")]),
            ),
            shelf("Quick picks", items(&[(&y, "4")])),
            shelf(
                "Mixes",
                vec![
                    Item::Collection(collection_of(&s, "m0")),
                    Item::Collection(collection_of(&y, "m1")),
                ],
            ),
            shelf("Nothing", Vec::new()),
        ]
    }

    fn home_with(shelves: Vec<Shelf>) -> Vm {
        let (mut vm, req) = home();
        vm.apply(signed_in());
        vm.apply(spotify_signed_in());
        vm.apply(results(req, Page::Home, shelves));
        vm.take_commands();
        vm.take_dirty();
        vm
    }

    #[test]
    fn filtering_home_keeps_one_services_cards_and_hides_the_shelves_it_empties() {
        let mut vm = home_with(mixed_home());
        let titles = |vm: &Vm| -> Vec<String> {
            vm.ui()
                .shown_home()
                .iter()
                .map(|s| s.title.clone())
                .collect()
        };
        let card_ids = |vm: &Vm, shelf: usize| -> Vec<String> {
            vm.ui().shown_home()[shelf]
                .items
                .iter()
                .map(|i| match i {
                    Item::Track(t) => t.id.id.clone(),
                    Item::Collection(c) => c.id.id.clone(),
                })
                .collect()
        };
        assert_eq!(
            titles(&vm),
            ["Liked", "Quick picks", "Mixes", "Nothing"],
            "All shows the page as it came"
        );

        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        assert_eq!(titles(&vm), ["Liked", "Mixes"]);
        assert_eq!(card_ids(&vm, 0), ["0", "2"]);
        assert_eq!(card_ids(&vm, 1), ["m0"]);

        vm.intent(Intent::SetFilter(ONLY_YT));
        assert_eq!(titles(&vm), ["Liked", "Quick picks", "Mixes"]);
        assert_eq!(card_ids(&vm, 0), ["1", "3"]);
        assert_eq!(card_ids(&vm, 1), ["4"]);
        assert_eq!(card_ids(&vm, 2), ["m1"]);
        // The page's own shelves are untouched.
        assert_eq!(vm.ui().home.shelves.len(), 4);
    }

    #[test]
    fn a_changed_home_is_shown_through_the_filter() {
        let mut vm = home_with(vec![shelf("Quick picks", track_items(1))]);
        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        vm.apply(changed(Page::Home, mixed_home()));
        assert!(vm.take_commands().is_empty());
        let titles: Vec<String> = vm
            .ui()
            .shown_home()
            .iter()
            .map(|s| s.title.clone())
            .collect();
        assert_eq!(titles, ["Liked", "Mixes"]);
        assert_eq!(vm.ui().home.shelves, mixed_home());
    }

    #[test]
    fn a_card_plays_and_adds_from_the_shelves_the_filter_leaves() {
        let mut vm = home_with(mixed_home());
        vm.intent(Intent::SetFilter(ONLY_YT));
        // The shelves are Liked, Quick picks and Mixes, whose one playlist has no track.
        let cmds = vm.intent(Intent::PlayFrom {
            list: List::Shelf(0),
            from: 1,
        });
        assert_eq!(
            cmds,
            vec![
                Command::Enqueue {
                    tracks: vec![from(&provider(), "3")],
                    mode: EnqueueMode::Replace
                },
                Command::PlayIndex(0)
            ]
        );
        assert_eq!(
            vm.intent(Intent::Append {
                list: List::Shelf(1),
                index: 0
            }),
            vec![Command::Enqueue {
                tracks: vec![from(&provider(), "4")],
                mode: EnqueueMode::Append
            }]
        );
        assert!(
            vm.intent(Intent::PlayFrom {
                list: List::Shelf(2),
                from: 0
            })
            .is_empty()
        );

        // With Spotify's, Mixes is the second shelf, and there is no third.
        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        let cmds = vm.intent(Intent::PlayFrom {
            list: List::Shelf(0),
            from: 1,
        });
        assert_eq!(
            cmds[0],
            Command::Enqueue {
                tracks: vec![from(&spotify(), "2")],
                mode: EnqueueMode::Replace
            }
        );
        assert!(
            vm.intent(Intent::PlayFrom {
                list: List::Shelf(2),
                from: 0
            })
            .is_empty()
        );
    }

    #[test]
    fn filtering_the_library_keeps_one_services_playlists() {
        let mut vm = connected();
        vm.apply(signed_in());
        let lib = browses(&vm.take_commands());
        let [(req, Page::Library)] = lib.as_slice() else {
            panic!("expected the Library fetch, got {lib:?}");
        };
        vm.apply(spotify_signed_in());
        let (s, y) = (spotify(), provider());
        let items = [(&s, "a"), (&y, "b"), (&s, "c"), (&y, "d")]
            .into_iter()
            .map(|(p, id)| Item::Collection(collection_of(p, id)))
            .collect();
        // The second sign-in asked again; its answer is the one that lands.
        let again = browses(&vm.take_commands());
        let [(req2, Page::Library)] = again.as_slice() else {
            panic!("expected the Library fetch again, got {again:?}");
        };
        assert!(req2 > req);
        vm.apply(results(*req2, Page::Library, vec![shelf("Library", items)]));
        let rows = |vm: &Vm| -> Vec<String> {
            vm.ui()
                .library_collections()
                .iter()
                .map(|c| c.id.id.clone())
                .collect()
        };
        assert_eq!(rows(&vm), ["a", "b", "c", "d"]);
        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        assert_eq!(rows(&vm), ["a", "c"]);
        vm.intent(Intent::SetFilter(ONLY_YT));
        assert_eq!(rows(&vm), ["b", "d"]);
    }

    /// A Library of `(provider, id, kind, title, subtitle)` as the merged page lists it.
    type Entry<'a> = (
        &'a ProviderId,
        &'a str,
        CollectionKind,
        &'a str,
        Option<&'a str>,
    );

    fn library_of(entries: &[Entry]) -> Vm {
        let mut vm = both_signed_in();
        let items = entries
            .iter()
            .map(|&(p, id, kind, title, subtitle)| {
                let mut c = collection_of(p, id);
                c.kind = kind;
                c.title = title.into();
                c.subtitle = subtitle.map(str::to_owned);
                Item::Collection(c)
            })
            .collect();
        let req = browses(&vm.take_commands())
            .into_iter()
            .rfind(|(_, page)| *page == Page::Library)
            .expect("a Library fetch")
            .0;
        vm.apply(results(req, Page::Library, vec![shelf("Library", items)]));
        vm
    }

    const SP_LIKED: &str = "collection";
    const SP_DAILY_MIX: &str = "playlist:37i9dQZF1E39abc";
    const SP_DISCOVER: &str = "playlist:37i9dQZEVXcabc";
    const SP_OWN: &str = "playlist:1pKYYY0dkg23sQQXi0Q5zN";
    const YT_LIKED: &str = "browse:VLLM";
    const YT_MIX: &str = "browse:VLRDTMAKabc";
    const YT_OWN: &str = "browse:VLPLroad";
    const YT_ALBUM: &str = "browse:MPREb_abc";

    fn lowercase(a: &str, b: &str) -> Ordering {
        a.to_lowercase().cmp(&b.to_lowercase())
    }

    fn mixed_library() -> Vm {
        use CollectionKind::{Album, Playlist};
        let (s, y) = (spotify(), provider());
        // As the merge takes them in turn, Spotify first; the liked cards, the mixes and the
        // album are scattered.
        library_of(&[
            (&s, SP_OWN, Playlist, "road trip", Some("12 songs")),
            (&y, YT_MIX, Playlist, "My Supermix", Some("Mix")),
            (&s, SP_LIKED, Playlist, "Liked Songs", Some("247 songs")),
            (&y, YT_OWN, Playlist, "Night", None),
            (
                &s,
                SP_DAILY_MIX,
                Playlist,
                "Daily Mix 1",
                Some("Made for you"),
            ),
            (&y, YT_LIKED, Playlist, "Liked music", None),
            (&s, SP_DISCOVER, Playlist, "Discover Weekly", None),
            (&y, YT_ALBUM, Album, "Blue Hour", Some("Album")),
            (&s, "playlist:2b", Playlist, "Alpha", Some("1 song")),
        ])
    }

    fn titles(vm: &Vm) -> Vec<String> {
        vm.ui()
            .library_grid(lowercase)
            .collections
            .iter()
            .map(|c| c.title.clone())
            .collect()
    }

    fn chips(vm: &Vm) -> Vec<(LibraryChip, bool)> {
        vm.ui().library_grid(lowercase).chips
    }

    #[test]
    fn the_library_keeps_the_services_order_with_the_liked_collections_first() {
        let vm = mixed_library();
        assert_eq!(
            titles(&vm),
            [
                "Liked Songs",
                "Liked music",
                "road trip",
                "My Supermix",
                "Night",
                "Daily Mix 1",
                "Discover Weekly",
                "Blue Hour",
                "Alpha"
            ]
        );
    }

    #[test]
    fn a_collection_is_liked_by_its_id_not_its_title() {
        let (s, y) = (spotify(), provider());
        let vm = library_of(&[
            (&s, SP_OWN, CollectionKind::Playlist, "Liked Songs", None),
            (&y, YT_LIKED, CollectionKind::Playlist, "Favourites", None),
            (&s, SP_LIKED, CollectionKind::Playlist, "Saved", None),
        ]);
        assert_eq!(titles(&vm), ["Saved", "Favourites", "Liked Songs"]);
    }

    #[test]
    fn the_chips_keep_playlists_mixes_and_albums_apart() {
        let mut vm = mixed_library();
        let with = |vm: &mut Vm, chip| {
            vm.intent(Intent::SetLibraryChip(chip));
            titles(vm)
        };
        assert_eq!(
            with(&mut vm, LibraryChip::Playlists),
            ["Liked Songs", "Liked music", "road trip", "Night", "Alpha"]
        );
        assert_eq!(
            with(&mut vm, LibraryChip::Mixes),
            ["My Supermix", "Daily Mix 1", "Discover Weekly"]
        );
        assert_eq!(with(&mut vm, LibraryChip::Albums), ["Blue Hour"]);
        assert_eq!(with(&mut vm, LibraryChip::All).len(), 9);
        assert_eq!(
            chips(&vm).iter().map(|c| c.0).collect::<Vec<_>>(),
            LibraryChip::CHOICES
        );
    }

    #[test]
    fn a_chip_with_nothing_to_keep_is_hidden_and_the_page_falls_back_to_all() {
        let (s, y) = (spotify(), provider());
        let mut vm = library_of(&[
            (&s, SP_LIKED, CollectionKind::Playlist, "Liked Songs", None),
            (&y, YT_OWN, CollectionKind::Playlist, "Night", None),
        ]);
        assert_eq!(
            chips(&vm),
            [(LibraryChip::All, true), (LibraryChip::Playlists, false)]
        );
        vm.intent(Intent::SetLibraryChip(LibraryChip::Mixes));
        assert_eq!(titles(&vm), ["Liked Songs", "Night"]);
        assert_eq!(
            chips(&vm),
            [(LibraryChip::All, true), (LibraryChip::Playlists, false)]
        );
        vm.intent(Intent::SetLibraryChip(LibraryChip::Playlists));
        assert_eq!(chips(&vm)[1], (LibraryChip::Playlists, true));
    }

    #[test]
    fn the_filter_text_matches_titles_and_subtitles_whatever_the_case() {
        let mut vm = mixed_library();
        vm.intent(Intent::SetLibraryText("ROAD".into()));
        assert_eq!(titles(&vm), ["road trip"]);
        vm.intent(Intent::SetLibraryText("made for".into()));
        assert_eq!(titles(&vm), ["Daily Mix 1"]);
        vm.intent(Intent::SetLibraryText("zzz".into()));
        assert!(titles(&vm).is_empty());
        // The chips and the page's being stocked do not depend on the text.
        let grid = vm.ui().library_grid(lowercase);
        assert!(grid.stocked && grid.mixed);
        assert_eq!(grid.chips.len(), 4);
        vm.intent(Intent::SetLibraryText(String::new()));
        assert_eq!(titles(&vm).len(), 9);
    }

    #[test]
    fn a_to_z_orders_by_title_without_regard_to_case_and_keeps_the_liked_first() {
        let mut vm = mixed_library();
        vm.intent(Intent::SetLibrarySort(LibrarySort::Alphabetical));
        assert_eq!(
            titles(&vm),
            [
                "Liked Songs",
                "Liked music",
                "Alpha",
                "Blue Hour",
                "Daily Mix 1",
                "Discover Weekly",
                "My Supermix",
                "Night",
                "road trip"
            ]
        );
        vm.intent(Intent::SetLibrarySort(LibrarySort::Recent));
        assert_eq!(titles(&vm)[2], "road trip");
    }

    #[test]
    fn the_service_switch_narrows_the_chips_and_the_grid() {
        let mut vm = mixed_library();
        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        assert!(!vm.ui().library_grid(lowercase).mixed);
        assert_eq!(
            titles(&vm),
            [
                "Liked Songs",
                "road trip",
                "Daily Mix 1",
                "Discover Weekly",
                "Alpha"
            ]
        );
        // Spotify has no albums here.
        assert_eq!(
            chips(&vm).iter().map(|c| c.0).collect::<Vec<_>>(),
            [LibraryChip::All, LibraryChip::Playlists, LibraryChip::Mixes]
        );
        vm.intent(Intent::SetLibraryChip(LibraryChip::Mixes));
        assert_eq!(titles(&vm), ["Daily Mix 1", "Discover Weekly"]);
        vm.intent(Intent::SetFilter(ONLY_YT));
        assert_eq!(titles(&vm), ["My Supermix"]);
        vm.intent(Intent::SetLibraryChip(LibraryChip::Albums));
        assert_eq!(titles(&vm), ["Blue Hour"]);
    }

    #[test]
    fn the_library_view_changes_redraw_only_the_library_page() {
        let mut vm = mixed_library();
        vm.take_dirty();
        vm.intent(Intent::SetLibraryText("a".into()));
        assert_eq!(vm.take_dirty(), Dirty::LIBRARY_VIEW);
        vm.intent(Intent::SetLibraryText("a".into()));
        assert!(vm.take_dirty().is_empty());
        vm.intent(Intent::SetLibraryChip(LibraryChip::Mixes));
        vm.intent(Intent::SetLibrarySort(LibrarySort::Alphabetical));
        assert_eq!(vm.take_dirty(), Dirty::LIBRARY_VIEW);
    }

    #[test]
    fn the_saved_sort_is_read_by_its_key_and_anything_else_is_recent() {
        for sort in LibrarySort::CHOICES {
            assert_eq!(LibrarySort::from_key(sort.key()), sort);
        }
        assert_eq!(LibrarySort::from_key("name"), LibrarySort::Recent);
        assert_eq!(LibrarySort::Alphabetical.label(), "A–Z");
    }

    #[test]
    fn the_filter_ends_while_either_service_is_signed_out_and_returns_with_it() {
        let mut vm = both_signed_in();
        search_lands(&mut vm, alternating());
        vm.intent(Intent::SetFilter(ONLY_YT));
        assert_eq!(vm.ui().shown_filter(), ONLY_YT);
        assert_eq!(vm.ui().shown_service(), Some(Service::YouTubeMusic));
        vm.take_dirty();

        // The chosen service signs out: everything shows again, and the control goes.
        vm.apply(account(AccountState::SignedOut));
        assert_eq!(vm.ui().shown_filter(), ServiceFilter::All);
        assert_eq!(vm.ui().shown_results().len(), 6);
        assert!(!vm.ui().filter_shown());
        assert!(
            vm.take_dirty()
                .contains(Dirty::RESULTS | Dirty::HOME | Dirty::LIBRARY)
        );
        // The choice is kept for its service's return.
        assert_eq!(vm.ui().filter, ONLY_YT);
        vm.apply(signed_in());
        assert_eq!(ids(&vm.ui().shown_results()), ["1", "3", "5"]);

        // The other one's sign-out ends it too.
        vm.apply(spotify_account(AccountState::SignedOut));
        assert_eq!(vm.ui().shown_filter(), ServiceFilter::All);
        assert_eq!(vm.ui().shown_results().len(), 6);
    }

    #[test]
    fn a_saved_filter_waits_for_the_accounts_a_new_connection_brings() {
        let mut vm = connected();
        vm.intent(Intent::SetFilter(ONLY_SPOTIFY));
        search_lands(&mut vm, alternating());
        assert_eq!(vm.ui().shown_results().len(), 6);
        vm.apply(spotify_signed_in());
        assert_eq!(vm.ui().shown_results().len(), 6);
        vm.apply(signed_in());
        assert_eq!(ids(&vm.ui().shown_results()), ["0", "2", "4"]);
        // A reconnect forgets the accounts until the snapshot tells them again.
        vm.link(Link::Lost(LinkError::NotResponding));
        vm.take_dirty();
        vm.link(Link::Connected);
        assert_eq!(vm.ui().shown_results().len(), 6);
        assert!(vm.take_dirty().contains(Dirty::RESULTS));
        assert_eq!(vm.ui().filter, ONLY_SPOTIFY);
    }
}
