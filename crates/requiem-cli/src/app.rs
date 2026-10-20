use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;
use requiem_core::{
    AccountState, Collection, CollectionKind, Command, EnqueueMode, Event, Item, Page, PlayState,
    ProviderId, SeekTarget, Session, Shelf, Track, TrackId,
};
use requiem_ipc::{UpdateRequest, UpdateState, UpdateStatus};
use serde_json::{Value, json};

const DEFAULT_AWAIT_SECS: f64 = 30.0;

/// How long a `--connect` run waits for the daemon's join snapshot; matches the client's
/// write timeout.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `signin` waits for the sign-in code.
const SIGN_IN_START: Duration = Duration::from_secs(60);
/// How long `signin` waits past the code's expiry for the provider to report it.
const SIGN_IN_GRACE: Duration = Duration::from_secs(30);
/// How long `signout` and `signin-client` wait for the provider to report it; Google's
/// revoke, the slowest part, gives up within 15 s.
const SIGN_OUT_WAIT: Duration = Duration::from_secs(30);
/// The largest file `signin-client` sends; providers take no more.
const MAX_CLIENT_FILE: u64 = 64 * 1024;

/// How long `update check` waits: a check, a 10 min download, then staging.
const UPDATE_CHECK_WAIT: Duration = Duration::from_secs(12 * 60);
/// How long `update install` waits for the daemon to stop or say no update is ready.
const UPDATE_INSTALL_WAIT: Duration = Duration::from_secs(10);

const HELP: &str = "\
commands:
  search <query>        search; prints numbered results
  home                  the home page of the provider that has one; prints numbered items under each shelf
  library               your playlists and liked songs from every signed-in provider (needs signin)
  open <n>              list the tracks of playlist or album n
  play <n>              replace the queue with track n of the last listing and play it
  add <n>               append track n of the last listing to the queue
  play-track <json>     like play, with the track object a --json result or item line prints
  add-track <json>      like add, with that track object
  queue                 show the queue (> marks the current track)
  jump <n>              play queue position n
  remove <n>            remove queue position n
  move <from> <to>      move queue position from so it ends up at to
  clear                 empty the queue and stop
  next | prev           skip
  pause | resume | toggle
  seek <secs>           absolute; +<secs> / -<secs> relative to now
  vol <0-100>           out-of-range values are rejected
  status                state, track, position / duration, volume
  sleep <secs>          wait (fractional ok)
  await <results|playing|stopped|nowplaying> [timeout_secs=30]
                        wait for that event since the last play/next/prev/search/browse
  signin [provider]     sign in (spotify or ytmusic; default: the first not signed in): prints
                        a code to enter on another device and waits until it is entered or
                        expires
  signout [provider]    sign out (spotify or ytmusic; default: the first signed in) and forget
                        the saved sign-in; waits until the provider reports it
  signin-client <provider> <path>
                        give the provider your own OAuth client, the JSON file its console
                        downloads (ytmusic: Google Cloud); waits until the provider reports it
  account               show the sign-in state
  mark <name>           print MARK <name> <unix_millis>
  update                with --connect: the self-update status
  update check          check for an update now and wait until it is ready or not found
  update install        restart the player on the ready update; music stops
  shutdown-daemon       with --connect: stop the daemon and exit
  help | quit           quit leaves a --connect daemon running";

/// How the REPL writes what it prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    /// One JSON object per line, each with a `"type"` field.
    Json,
}

/// What the REPL drives: an in-process `Session` or a daemon over IPC.
pub trait Player {
    fn send(&self, cmd: Command);

    /// Called once, by `App::new`.
    fn subscribe(&self) -> Receiver<Event>;

    /// True when commands go to a daemon that outlives this process.
    fn remote(&self) -> bool {
        false
    }

    /// The reason the player went away. Must be set before the `subscribe` channel closes.
    fn disconnected(&self) -> Option<String> {
        None
    }

    /// The daemon's latest self-update status and how many it has sent. `None` for an
    /// in-process session, which never updates.
    fn update_status(&self) -> Option<(UpdateStatus, usize)> {
        None
    }

    /// The status sent after the first `seen`, waiting for it until the connection ends or
    /// `timeout` passes.
    fn next_update(&self, _seen: usize, _timeout: Duration) -> Option<UpdateStatus> {
        None
    }

    fn request_update(&self, _req: UpdateRequest) {}
}

impl Player for Session {
    fn send(&self, cmd: Command) {
        Session::send(self, cmd);
    }

    fn subscribe(&self) -> Receiver<Event> {
        Session::subscribe(self)
    }
}

#[derive(Default)]
struct Seen {
    results: bool,
    playing: bool,
    stopped: bool,
    nowplaying: bool,
}

/// A `signin` waiting for its outcome.
struct SignInWait {
    provider: ProviderId,
    expires_at_unix: Option<u64>,
    /// The first state after `Pending`.
    ended: Option<AccountState>,
    /// An `AccountError` arrived.
    failed: bool,
}

/// A `signout` or `signin-client` waiting for the daemon's answer.
struct AccountWait {
    provider: ProviderId,
    /// The first state reported after the request.
    ended: Option<AccountState>,
    /// An `AccountError` arrived.
    failed: bool,
}

struct Ui {
    /// The latest search or browse; earlier answers are dropped.
    latest_req: u64,
    /// What `play <n>` and `open <n>` number: `None` until the latest search or browse has
    /// returned.
    listing: Option<Vec<Item>>,
    accounts: Vec<(ProviderId, AccountState)>,
    sign_in: Option<SignInWait>,
    sign_out: Option<AccountWait>,
    import_client: Option<AccountWait>,
    queue: Vec<Track>,
    current: Option<usize>,
    state: PlayState,
    now: Option<Track>,
    duration: Option<Duration>,
    position: Duration,
    volume: u8,
    seen: Seen,
    /// The first `Volume` has arrived, which is the last event of a daemon's join snapshot.
    synced: bool,
    /// The event channel has ended.
    closed: bool,
}

struct Hub {
    ui: Mutex<Ui>,
    changed: Condvar,
    out: Mutex<Box<dyn Write + Send>>,
    format: Format,
    /// Print the daemon's events; off for `requiem update`, which prints its answer alone.
    events: bool,
}

impl Hub {
    fn lock_ui(&self) -> MutexGuard<'_, Ui> {
        self.ui.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_line(&self, line: &str) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    /// Prints `text` in text mode and `json` in JSON mode.
    fn emit(&self, text: &str, json: Value) {
        match self.format {
            Format::Text => self.write_line(&printable(text)),
            Format::Json => self.write_line(&json.to_string()),
        }
    }

    fn error(&self, message: &str) {
        self.emit(
            &format!("error: {message}"),
            json!({ "type": "error", "message": message }),
        );
    }

    /// JSON mode only: confirms that a command was accepted. Text mode prints nothing.
    fn ack(&self, command: &str) {
        if self.format == Format::Json {
            self.write_line(&json!({ "type": "ack", "command": command }).to_string());
        }
    }

    /// Lines are written before the lock is released so that an `await` woken by this
    /// event cannot print ahead of them.
    fn on_event(&self, ev: Event) {
        let mut ui = self.lock_ui();
        if !self.events {
            if let Event::Volume(_) = ev {
                ui.synced = true;
                drop(ui);
                self.changed.notify_all();
            }
            return;
        }
        match ev {
            Event::SearchResults { req, tracks } => {
                if req != ui.latest_req {
                    return;
                }
                if tracks.is_empty() {
                    self.emit("no results", json!({ "type": "no_results" }));
                }
                for (i, t) in tracks.iter().enumerate() {
                    self.emit(
                        &format!(
                            "  {}. {} · {}",
                            i + 1,
                            track_line(t),
                            provider_name(&t.id.provider)
                        ),
                        track_json("result", t, [("index", json!(i + 1))]),
                    );
                }
                ui.listing = Some(tracks.into_iter().map(Item::Track).collect());
                ui.seen.results = true;
            }
            Event::SearchFailed { error, .. } => self.emit(
                &format!("search failed: {error}"),
                json!({ "type": "search_failed", "error": error }),
            ),
            Event::BrowseResults { req, shelves, .. } => {
                if req != ui.latest_req {
                    return;
                }
                ui.listing = Some(self.print_shelves(shelves));
                ui.seen.results = true;
            }
            Event::BrowseFailed {
                req,
                error,
                sign_in_required,
            } => {
                if req != ui.latest_req {
                    return;
                }
                let hint = if sign_in_required {
                    " (try signin)"
                } else {
                    ""
                };
                self.emit(
                    &format!("browse failed: {error}{hint}"),
                    json!({
                        "type": "browse_failed",
                        "error": error,
                        "sign_in_required": sign_in_required,
                    }),
                );
            }
            Event::Account { provider, state } => self.on_account(&mut ui, provider, state),
            Event::AccountError { provider, error } => {
                if let Some(w) = ui.sign_in.as_mut().filter(|w| w.provider == provider) {
                    w.failed = true;
                }
                let ui = &mut *ui;
                for w in [&mut ui.sign_out, &mut ui.import_client] {
                    if let Some(w) = w.as_mut().filter(|w| w.provider == provider) {
                        w.failed = true;
                    }
                }
                self.emit(
                    &format!("account error: {error}"),
                    json!({ "type": "account_error", "provider": provider.0, "error": error }),
                );
            }
            Event::QueueChanged { tracks, current } => {
                let cur = current.map_or("none".to_owned(), |i| (i + 1).to_string());
                self.emit(
                    &format!("queue: {} tracks, current {cur}", tracks.len()),
                    json!({
                        "type": "queue_changed",
                        "count": tracks.len(),
                        "current": current.map(|i| i + 1),
                    }),
                );
                ui.queue = tracks;
                ui.current = current;
            }
            Event::NowPlaying { track, duration } => {
                self.emit(
                    &format!("now playing: {}", track_label(&track)),
                    track_json(
                        "now_playing",
                        &track,
                        [("duration_ms", json!(duration.map(|d| d.as_millis() as u64)))],
                    ),
                );
                ui.now = Some(track);
                ui.duration = duration;
                ui.position = Duration::ZERO;
                ui.seen.nowplaying = true;
            }
            Event::Position(at) => ui.position = at,
            Event::State(s) => {
                self.emit(
                    &format!("state: {}", state_name(s)),
                    json!({ "type": "state", "state": state_name(s) }),
                );
                ui.state = s;
                match s {
                    PlayState::Playing => ui.seen.playing = true,
                    PlayState::Stopped => ui.seen.stopped = true,
                    _ => {}
                }
            }
            Event::Volume(v) => {
                self.emit(
                    &format!("volume: {v}"),
                    json!({ "type": "volume", "volume": v }),
                );
                ui.volume = v;
                ui.synced = true;
            }
            Event::TrackFailed { track, error } => {
                self.emit(
                    &format!("track failed: {}: {error}", track.title),
                    json!({ "type": "track_failed", "title": track.title, "error": error }),
                );
            }
            Event::ProviderBroken { detail, .. } => {
                self.emit(
                    &format!("provider broken: {detail}"),
                    json!({ "type": "provider_broken", "detail": detail }),
                );
            }
        }
        drop(ui);
        self.changed.notify_all();
    }

    /// Prints each shelf's title and its items, numbered across shelves; returns the items.
    fn print_shelves(&self, shelves: Vec<Shelf>) -> Vec<Item> {
        if shelves.iter().all(|s| s.items.is_empty()) {
            self.emit("no results", json!({ "type": "no_results" }));
        }
        let mut listing = Vec::new();
        for shelf in shelves {
            if shelf.items.is_empty() {
                continue;
            }
            self.emit(
                &format!("{}:", shelf.title),
                json!({ "type": "shelf", "title": shelf.title }),
            );
            for item in shelf.items {
                let n = listing.len() + 1;
                self.emit(&format!("  {n}. {}", item_line(&item)), item_json(&item, n));
                listing.push(item);
            }
        }
        listing
    }

    /// The first state a provider reports is its starting state and is not printed; later
    /// ones are printed when they change it, and while a `signin` waits on that provider.
    fn on_account(&self, ui: &mut Ui, provider: ProviderId, state: AccountState) {
        let waiting = ui.sign_in.as_mut().filter(|w| w.provider == provider);
        let repeat = waiting.is_some()
            && matches!(
                state,
                AccountState::Pending { .. } | AccountState::SignedIn { .. }
            );
        if let Some(w) = waiting {
            match &state {
                AccountState::Pending {
                    expires_at_unix, ..
                } => w.expires_at_unix = Some(*expires_at_unix),
                other => w.ended = Some(other.clone()),
            }
        }
        for w in [&mut ui.sign_out, &mut ui.import_client] {
            if let Some(w) = w.as_mut().filter(|w| w.provider == provider) {
                w.ended.get_or_insert_with(|| state.clone());
            }
        }
        match ui.accounts.iter_mut().find(|(p, _)| *p == provider) {
            Some((_, known)) => {
                if *known != state || repeat {
                    self.emit(&account_text(&state), account_json(&provider, &state));
                }
                *known = state;
            }
            None => {
                if repeat {
                    self.emit(&account_text(&state), account_json(&provider, &state));
                }
                ui.accounts.push((provider, state));
            }
        }
    }
}

fn mss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn state_name(s: PlayState) -> &'static str {
    match s {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Buffering => "buffering",
        PlayState::Stopped => "stopped",
    }
}

/// `kind` is the object's `"type"`; `extra` adds or overrides fields. `duration_ms` defaults to
/// the track's own length.
fn track_json<const N: usize>(kind: &str, t: &Track, extra: [(&str, Value); N]) -> Value {
    let mut o = json!({
        "type": kind,
        "id": { "provider": t.id.provider.0, "id": t.id.id },
        "title": t.title,
        "artists": t.artists,
        "album": t.album,
        "duration_ms": t.duration.map(|d| d.as_millis() as u64),
        "artwork_url": t.artwork_url,
    });
    for (k, v) in extra {
        o[k] = v;
    }
    o
}

/// The inverse of `track_json`: a track from a `--json` result line. `type` and `index` are ignored.
fn parse_track(text: &str) -> Result<Track, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    let str_at = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("missing string field {key}"))
    };
    let opt_str = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let id = v.get("id").ok_or("missing field id")?;
    let artists = v
        .get("artists")
        .and_then(Value::as_array)
        .ok_or("missing array field artists")?
        .iter()
        .map(|a| {
            a.as_str()
                .map(str::to_owned)
                .ok_or("artists must be strings")
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Track {
        id: TrackId {
            provider: ProviderId(str_at(id, "provider")?),
            id: str_at(id, "id")?,
        },
        title: str_at(&v, "title")?,
        artists,
        album: opt_str("album"),
        duration: v
            .get("duration_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis),
        artwork_url: opt_str("artwork_url"),
    })
}

fn kind_name(k: CollectionKind) -> &'static str {
    match k {
        CollectionKind::Playlist => "playlist",
        CollectionKind::Album => "album",
    }
}

fn collection_line(c: &Collection) -> String {
    let mut s = format!("[{}] {}", kind_name(c.kind), c.title);
    if let Some(sub) = &c.subtitle {
        s.push_str(&format!(" — {sub}"));
    }
    s
}

fn item_line(item: &Item) -> String {
    match item {
        Item::Track(t) => track_line(t),
        Item::Collection(c) => collection_line(c),
    }
}

/// An `item` line. Tracks carry every `track_json` field, so `play-track` takes them as is.
fn item_json(item: &Item, index: usize) -> Value {
    match item {
        Item::Track(t) => track_json(
            "item",
            t,
            [
                ("index", json!(index)),
                ("kind", json!("track")),
                ("subtitle", json!(t.artists.join(", "))),
            ],
        ),
        Item::Collection(c) => json!({
            "type": "item",
            "index": index,
            "kind": kind_name(c.kind),
            "id": { "provider": c.id.provider.0, "id": c.id.id },
            "title": c.title,
            "subtitle": c.subtitle,
            "artwork_url": c.artwork_url,
        }),
    }
}

/// Replaces control characters, which a terminal would act on, with U+FFFD. Titles, channel
/// names and API errors come from remote services. JSON mode escapes them already.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

fn account_text(state: &AccountState) -> String {
    match state {
        AccountState::Unavailable(hint) => format!("sign-in unavailable: {hint}"),
        AccountState::SignedOut => "signed out".to_owned(),
        AccountState::Pending {
            verification_url,
            user_code,
            ..
        } => format!("Go to {verification_url} and enter {user_code}"),
        AccountState::SignedIn { name: Some(n) } => format!("signed in as {n}"),
        AccountState::SignedIn { name: None } => "signed in".to_owned(),
    }
}

fn account_json(provider: &ProviderId, state: &AccountState) -> Value {
    let mut o = json!({ "type": "account", "provider": provider.0 });
    match state {
        AccountState::Unavailable(hint) => {
            o["state"] = json!("unavailable");
            o["detail"] = json!(hint);
        }
        AccountState::SignedOut => o["state"] = json!("signed_out"),
        AccountState::Pending {
            verification_url,
            user_code,
            expires_at_unix,
        } => {
            o["state"] = json!("pending");
            o["verification_url"] = json!(verification_url);
            o["user_code"] = json!(user_code);
            o["expires_at_unix"] = json!(expires_at_unix);
        }
        AccountState::SignedIn { name } => {
            o["state"] = json!("signed_in");
            o["name"] = json!(name);
        }
    }
    o
}

fn update_state_name(state: &UpdateState) -> &'static str {
    match state {
        UpdateState::Idle => "idle",
        UpdateState::Checking => "checking",
        UpdateState::UpToDate => "up_to_date",
        UpdateState::Downloading { .. } => "downloading",
        UpdateState::Ready { .. } => "ready",
        UpdateState::Failed { .. } => "failed",
        UpdateState::Error { .. } => "error",
        UpdateState::NotUpdatable { .. } => "not_updatable",
        UpdateState::Unsupported => "unsupported",
    }
}

/// The window's banner lines where it has one.
fn update_text(status: &UpdateStatus) -> String {
    let current = &status.current;
    match &status.state {
        UpdateState::Idle => format!("Requiem {current}"),
        UpdateState::Checking => format!("Requiem {current}, checking for updates"),
        UpdateState::UpToDate => format!("Requiem {current} is up to date."),
        UpdateState::Downloading { version } => {
            format!("Requiem {current}, downloading {version}")
        }
        UpdateState::Ready { version } => format!(
            "Requiem {version} is ready. requiem update install restarts to it; music stops."
        ),
        UpdateState::Failed { reason, .. } => {
            format!("Requiem couldn't update: {reason}. It keeps running {current}.")
        }
        UpdateState::Error { reason } => format!("Requiem {current}: {reason}"),
        UpdateState::NotUpdatable { reason } => format!(
            "Requiem {current} can't update itself: {reason}. Move Requiem to Applications to get updates."
        ),
        UpdateState::Unsupported => "Self-update is macOS only".to_owned(),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The file's text, up to `MAX_CLIENT_FILE` bytes. Errors never quote it.
fn read_client_file(path: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(MAX_CLIENT_FILE + 1).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_CLIENT_FILE {
        return Err(format!("over {} KiB", MAX_CLIENT_FILE / 1024));
    }
    String::from_utf8(bytes).map_err(|_| "not UTF-8 text".to_owned())
}

fn track_label(t: &Track) -> String {
    format!("{} — {}", t.artists.join(", "), t.title)
}

fn provider_name(id: &ProviderId) -> &str {
    match id.0.as_str() {
        "ytmusic" => "YouTube Music",
        "spotify" => "Spotify",
        other => other,
    }
}

fn track_line(t: &Track) -> String {
    let mut s = track_label(t);
    if let Some(a) = &t.album {
        s.push_str(&format!(" [{a}]"));
    }
    if let Some(d) = t.duration {
        s.push_str(&format!(" ({})", mss(d)));
    }
    s
}

pub fn parse_seek(arg: &str) -> Option<SeekTarget> {
    if let Some(n) = arg.strip_prefix('+') {
        return n.parse::<i64>().ok().map(SeekTarget::Relative);
    }
    if arg.starts_with('-') {
        return arg.parse::<i64>().ok().map(SeekTarget::Relative);
    }
    arg.parse::<u64>()
        .ok()
        .map(|s| SeekTarget::Absolute(Duration::from_secs(s)))
}

fn parse_secs(arg: &str) -> Option<Duration> {
    arg.parse::<f64>()
        .ok()
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
    /// Abort with this exit code: a script-mode failure, or a lost daemon connection.
    Exit(u8),
}

pub struct App<'a> {
    player: &'a dyn Player,
    hub: Arc<Hub>,
    script: bool,
}

impl<'a> App<'a> {
    /// Subscribes and starts the printer thread, which ends when the session shuts down.
    pub fn new(player: &'a dyn Player, out: impl Write + Send + 'static, script: bool) -> Self {
        Self::with_format(player, out, script, Format::Text)
    }

    pub fn with_format(
        player: &'a dyn Player,
        out: impl Write + Send + 'static,
        script: bool,
        format: Format,
    ) -> Self {
        Self::build(player, out, script, format, true)
    }

    fn build(
        player: &'a dyn Player,
        out: impl Write + Send + 'static,
        script: bool,
        format: Format,
        events: bool,
    ) -> Self {
        let hub = Arc::new(Hub {
            ui: Mutex::new(Ui {
                latest_req: 0,
                listing: None,
                accounts: Vec::new(),
                sign_in: None,
                sign_out: None,
                import_client: None,
                queue: Vec::new(),
                current: None,
                state: PlayState::Stopped,
                now: None,
                duration: None,
                position: Duration::ZERO,
                volume: 100,
                seen: Seen::default(),
                synced: false,
                closed: false,
            }),
            changed: Condvar::new(),
            out: Mutex::new(Box::new(out)),
            format,
            events,
        });
        let rx = player.subscribe();
        let h = hub.clone();
        std::thread::Builder::new()
            .name("requiem-printer".into())
            .spawn(move || {
                for ev in rx {
                    h.on_event(ev);
                }
                h.lock_ui().closed = true;
                h.changed.notify_all();
            })
            .expect("spawn printer thread");
        Self {
            player,
            hub,
            script,
        }
    }

    /// Blocks until the daemon's join snapshot has been applied, the channel has closed, or
    /// `SYNC_TIMEOUT` passes, so the first command sees the daemon's state.
    fn sync(&self) {
        let deadline = Instant::now() + SYNC_TIMEOUT;
        let mut ui = self.hub.lock_ui();
        while !(ui.synced || ui.closed) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            ui = self
                .hub
                .changed
                .wait_timeout(ui, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// `Exit` once the daemon connection is lost; `None` while it is up.
    fn lost(&self, closed: bool) -> Option<Flow> {
        if !closed {
            return None;
        }
        let reason = self.player.disconnected()?;
        self.hub.error(&format!("player disconnected: {reason}"));
        Some(Flow::Exit(if self.script { 2 } else { 1 }))
    }

    pub fn exec(&self, line: &str) -> Flow {
        let closed = self.hub.lock_ui().closed;
        if let Some(flow) = self.lost(closed) {
            return flow;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Flow::Continue;
        }
        let (cmd, arg) = match line.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (line, ""),
        };
        match cmd {
            "search" => {
                if arg.is_empty() {
                    self.hub.error("usage: search <query>");
                    return Flow::Continue;
                }
                let req = self.next_listing();
                self.player.send(Command::Search {
                    req,
                    query: arg.to_owned(),
                });
                self.hub.ack(cmd);
            }
            "home" => self.browse(cmd, Page::Home),
            "library" => self.browse(cmd, Page::Library),
            "open" => match self.pick(arg) {
                Some(Item::Collection(c)) => self.browse(cmd, Page::Collection(c.id)),
                Some(Item::Track(_)) => self
                    .hub
                    .error(&format!("result {arg} is a track; use play {arg}")),
                None => {}
            },
            "signin" => return self.sign_in(cmd, arg),
            "signout" => return self.sign_out(cmd, arg),
            "signin-client" => return self.import_client(cmd, arg),
            "account" => {
                let ui = self.accounts_known(None);
                if ui.accounts.is_empty() {
                    self.hub
                        .emit("no accounts", json!({ "type": "no_accounts" }));
                }
                for (p, state) in &ui.accounts {
                    self.hub.emit(
                        &format!("{}: {}", p.0, account_text(state)),
                        account_json(p, state),
                    );
                }
            }
            "play" | "add" | "play-track" | "add-track" => {
                let track = if cmd.ends_with("-track") {
                    match parse_track(arg) {
                        Ok(t) => t,
                        Err(e) => {
                            self.hub.error(&format!("{cmd}: {e}"));
                            return Flow::Continue;
                        }
                    }
                } else {
                    match self.pick(arg) {
                        Some(Item::Track(t)) => t,
                        Some(Item::Collection(c)) => {
                            self.hub.error(&format!(
                                "result {arg} is a {}; use open {arg}",
                                kind_name(c.kind)
                            ));
                            return Flow::Continue;
                        }
                        None => return Flow::Continue,
                    }
                };
                if cmd.starts_with("play") {
                    self.clear_playback_seen();
                    self.player.send(Command::Enqueue {
                        tracks: vec![track],
                        mode: EnqueueMode::Replace,
                    });
                    self.player.send(Command::PlayIndex(0));
                } else {
                    self.player.send(Command::Enqueue {
                        tracks: vec![track],
                        mode: EnqueueMode::Append,
                    });
                }
                self.hub.ack(cmd);
            }
            "queue" => {
                let ui = self.hub.lock_ui();
                if ui.queue.is_empty() {
                    self.hub
                        .emit("queue empty", json!({ "type": "queue_empty" }));
                }
                for (i, t) in ui.queue.iter().enumerate() {
                    let current = ui.current == Some(i);
                    let mark = if current { '>' } else { ' ' };
                    self.hub.emit(
                        &format!("{mark} {}. {}", i + 1, track_line(t)),
                        track_json(
                            "queue_item",
                            t,
                            [("index", json!(i + 1)), ("current", json!(current))],
                        ),
                    );
                }
            }
            "jump" => match self.queue_pos(arg) {
                Some(i) => {
                    self.clear_playback_seen();
                    self.player.send(Command::PlayIndex(i));
                    self.hub.ack(cmd);
                }
                None => self.say_no_position(arg),
            },
            "remove" => match self.queue_pos(arg) {
                Some(i) => {
                    self.player.send(Command::Remove(i));
                    self.hub.ack(cmd);
                }
                None => self.say_no_position(arg),
            },
            "move" => {
                let mut it = arg.split_whitespace();
                let (Some(from), Some(to), None) = (it.next(), it.next(), it.next()) else {
                    self.hub.error("usage: move <from> <to>");
                    return Flow::Continue;
                };
                match (self.queue_pos(from), self.queue_pos(to)) {
                    (Some(from), Some(to)) => {
                        self.player.send(Command::Move { from, to });
                        self.hub.ack(cmd);
                    }
                    (None, _) => self.say_no_position(from),
                    (_, None) => self.say_no_position(to),
                }
            }
            "clear" => {
                self.clear_playback_seen();
                self.player.send(Command::Clear);
                self.hub.ack(cmd);
            }
            "next" | "prev" | "pause" | "resume" | "toggle" => {
                self.clear_playback_seen();
                self.player.send(match cmd {
                    "next" => Command::Next,
                    "prev" => Command::Prev,
                    "pause" => Command::Pause,
                    "resume" => Command::Resume,
                    _ => Command::TogglePause,
                });
                self.hub.ack(cmd);
            }
            "seek" => match parse_seek(arg) {
                Some(t) => {
                    self.player.send(Command::Seek(t));
                    self.hub.ack(cmd);
                }
                None => self.hub.error("usage: seek <secs|+secs|-secs>"),
            },
            "vol" => match arg.parse::<u8>() {
                Ok(v) if v <= 100 => {
                    self.player.send(Command::SetVolume(v));
                    self.hub.ack(cmd);
                }
                _ => self.hub.error("usage: vol <0-100>"),
            },
            "status" => self.status(),
            "sleep" => match parse_secs(arg) {
                Some(d) => std::thread::sleep(d),
                None => self.hub.error("usage: sleep <secs>"),
            },
            "await" => return self.await_event(arg),
            "mark" => {
                if arg.is_empty() {
                    self.hub.error("usage: mark <name>");
                } else {
                    let ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    self.hub.emit(
                        &format!("MARK {arg} {ms}"),
                        json!({ "type": "mark", "name": arg, "unix_millis": ms as u64 }),
                    );
                }
            }
            "help" => self.hub.emit(HELP, json!({ "type": "help", "text": HELP })),
            "update" => return self.update(arg),
            "shutdown-daemon" => {
                if !self.player.remote() {
                    self.hub.error("not connected to a daemon");
                } else {
                    self.player.send(Command::Shutdown);
                    self.hub.ack(cmd);
                    return Flow::Quit;
                }
            }
            "quit" | "exit" => return Flow::Quit,
            other => {
                self.hub.error(&format!("unknown command {other}"));
                if self.script {
                    return Flow::Exit(2);
                }
            }
        }
        Flow::Continue
    }

    /// Item `arg` of the last listing; prints the error and returns `None` otherwise.
    fn pick(&self, arg: &str) -> Option<Item> {
        let ui = self.hub.lock_ui();
        let Some(listing) = &ui.listing else {
            self.hub.error("no search results yet");
            return None;
        };
        let item = arg
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| listing.get(i));
        if item.is_none() {
            self.hub.error(&format!("no result {arg}"));
        }
        item.cloned()
    }

    /// Starts a new listing: answers to earlier searches and browses are dropped from now on.
    fn next_listing(&self) -> u64 {
        let mut ui = self.hub.lock_ui();
        ui.latest_req += 1;
        ui.listing = None;
        ui.seen.results = false;
        ui.latest_req
    }

    fn browse(&self, cmd: &str, page: Page) {
        let req = self.next_listing();
        self.player.send(Command::Browse { req, page });
        self.hub.ack(cmd);
    }

    /// The UI once it knows of an account, or of `provider`'s if given, or after
    /// `SYNC_TIMEOUT`. The session announces accounts on subscribe; this covers a printer thread
    /// that is behind, whose late announcement a sign-in would take for its outcome.
    fn accounts_known(&self, provider: Option<&ProviderId>) -> MutexGuard<'_, Ui> {
        let deadline = Instant::now() + SYNC_TIMEOUT;
        let mut ui = self.hub.lock_ui();
        let unknown = |ui: &Ui| match provider {
            Some(p) => !ui.accounts.iter().any(|(q, _)| q == p),
            None => ui.accounts.is_empty(),
        };
        while unknown(&ui) && !ui.closed {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            ui = self
                .hub
                .changed
                .wait_timeout(ui, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        ui
    }

    /// The provider named by `arg`. Else, for sign-out (`signed_in`), the first signed in; for
    /// sign-in, the first that isn't. Failing that, the first with an account.
    fn account_provider(&self, arg: &str, signed_in: bool) -> Option<ProviderId> {
        if !arg.is_empty() {
            let provider = ProviderId(arg.to_owned());
            drop(self.accounts_known(Some(&provider)));
            return Some(provider);
        }
        let ui = self.accounts_known(None);
        let pick = ui
            .accounts
            .iter()
            .find(|(_, s)| matches!(s, AccountState::SignedIn { .. }) == signed_in)
            .or_else(|| ui.accounts.first())
            .map(|(p, _)| p.clone());
        drop(ui);
        if pick.is_none() {
            self.hub.error("no provider has accounts");
        }
        pick
    }

    /// Sends `SignIn` and waits until the sign-in ends: up to `SIGN_IN_START` for the code,
    /// then until the code expires. A sign-in that goes back to signed out with no error was
    /// ended by a sign-out, which the session reports without one.
    fn sign_in(&self, cmd: &str, arg: &str) -> Flow {
        let Some(provider) = self.account_provider(arg, false) else {
            return if self.script {
                Flow::Exit(2)
            } else {
                Flow::Continue
            };
        };
        self.hub.lock_ui().sign_in = Some(SignInWait {
            provider: provider.clone(),
            expires_at_unix: None,
            ended: None,
            failed: false,
        });
        self.player.send(Command::SignIn { provider });
        self.hub.ack(cmd);
        let started = Instant::now();
        let mut ui = self.hub.lock_ui();
        let outcome = loop {
            let w = ui.sign_in.as_ref().expect("set above");
            if let Some(state) = &w.ended {
                break Some(matches!(state, AccountState::SignedIn { .. }));
            }
            if ui.closed {
                break None;
            }
            let deadline = match w.expires_at_unix {
                Some(at) => {
                    Instant::now()
                        + Duration::from_secs(at.saturating_sub(unix_now()))
                        + SIGN_IN_GRACE
                }
                None => started + SIGN_IN_START,
            };
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break Some(false);
            };
            ui = self
                .hub
                .changed
                .wait_timeout(ui, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        let w = ui.sign_in.take().expect("set above");
        drop(ui);
        let cancelled = w.expires_at_unix.is_some()
            && !w.failed
            && matches!(w.ended, Some(AccountState::SignedOut));
        match outcome {
            Some(true) => Flow::Continue,
            None => self.lost(true).unwrap_or(Flow::Continue),
            Some(false) => {
                if w.ended.is_none() {
                    self.hub.error("timed out waiting for sign-in");
                } else if cancelled {
                    self.hub.error("sign-in was cancelled");
                }
                if self.script {
                    Flow::Exit(2)
                } else {
                    Flow::Continue
                }
            }
        }
    }

    /// Sends `SignOut` and waits up to `SIGN_OUT_WAIT` for the daemon's answer. It succeeds
    /// when the provider is left neither signed in nor signing in, so that a `signin` after it
    /// starts over, and no error came with it.
    fn sign_out(&self, cmd: &str, arg: &str) -> Flow {
        let Some(provider) = self.account_provider(arg, true) else {
            return Flow::Continue;
        };
        if !self.has_account(&provider) {
            return self.failed();
        }
        self.hub.lock_ui().sign_out = Some(AccountWait {
            provider: provider.clone(),
            ended: None,
            failed: false,
        });
        self.player.send(Command::SignOut { provider });
        let Some(w) = self.wait_answer(|ui| &mut ui.sign_out) else {
            return self.lost(true).unwrap_or(Flow::Continue);
        };
        let left = w.ended.as_ref().is_some_and(|state| {
            !matches!(
                state,
                AccountState::SignedIn { .. } | AccountState::Pending { .. }
            )
        });
        if left && !w.failed {
            self.hub.ack(cmd);
            return Flow::Continue;
        }
        if !w.failed && w.ended.is_none() {
            self.hub.error("timed out waiting for sign-out");
        }
        self.failed()
    }

    /// Reads the client file at the path in `arg`, sends `ImportSignInClient` and waits up to
    /// `SIGN_OUT_WAIT` for the provider's account, the sign of success.
    fn import_client(&self, cmd: &str, arg: &str) -> Flow {
        let Some((name, path)) = arg
            .split_once(char::is_whitespace)
            .map(|(p, f)| (p, f.trim()))
        else {
            self.hub.error("usage: signin-client <provider> <path>");
            return self.failed();
        };
        let json = match read_client_file(path) {
            Ok(json) => json,
            Err(e) => {
                self.hub.error(&format!("signin-client: {path}: {e}"));
                return self.failed();
            }
        };
        let provider = ProviderId(name.to_owned());
        if !self.has_account(&provider) {
            return self.failed();
        }
        self.hub.lock_ui().import_client = Some(AccountWait {
            provider: provider.clone(),
            ended: None,
            failed: false,
        });
        self.player.send(Command::ImportSignInClient {
            provider,
            json: json.into(),
        });
        let Some(w) = self.wait_answer(|ui| &mut ui.import_client) else {
            return self.lost(true).unwrap_or(Flow::Continue);
        };
        if w.failed {
            return self.failed();
        }
        if w.ended.is_none() {
            self.hub.error("timed out waiting for signin-client");
            return self.failed();
        }
        self.hub.ack(cmd);
        Flow::Continue
    }

    /// Whether `provider` has an account, once the UI knows; says so when it has none. A
    /// closed connection passes, for the caller's wait to report.
    fn has_account(&self, provider: &ProviderId) -> bool {
        let ui = self.accounts_known(Some(provider));
        let known = ui.closed || ui.accounts.iter().any(|(p, _)| p == provider);
        drop(ui);
        if !known {
            self.hub.error(&format!("no provider {}", provider.0));
        }
        known
    }

    /// Waits up to `SIGN_OUT_WAIT` for the answer the `slot` of the UI waits on, and takes
    /// it. `None` when the connection ended first.
    fn wait_answer(&self, slot: fn(&mut Ui) -> &mut Option<AccountWait>) -> Option<AccountWait> {
        let deadline = Instant::now() + SIGN_OUT_WAIT;
        let mut ui = self.hub.lock_ui();
        loop {
            let w = slot(&mut ui).as_ref().expect("set by the caller");
            if w.failed || w.ended.is_some() {
                break;
            }
            if ui.closed {
                slot(&mut ui).take();
                return None;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            ui = self
                .hub
                .changed
                .wait_timeout(ui, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        slot(&mut ui).take()
    }

    fn failed(&self) -> Flow {
        if self.script {
            Flow::Exit(2)
        } else {
            Flow::Continue
        }
    }

    /// The 0-based queue index for a 1-based `queue` position, if it exists.
    fn queue_pos(&self, arg: &str) -> Option<usize> {
        let len = self.hub.lock_ui().queue.len();
        arg.parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < len)
    }

    fn say_no_position(&self, arg: &str) {
        self.hub.error(&format!("no queue position {arg}"));
    }

    fn clear_playback_seen(&self) {
        let mut ui = self.hub.lock_ui();
        ui.seen.playing = false;
        ui.seen.stopped = false;
        ui.seen.nowplaying = false;
    }

    fn status(&self) {
        let ui = self.hub.lock_ui();
        let track = ui.now.as_ref().map_or("-".to_owned(), track_label);
        let dur = ui.duration.map_or("?:??".to_owned(), mss);
        self.hub.emit(
            &format!(
                "status: {} | {track} | {} / {dur} | volume {}",
                state_name(ui.state),
                mss(ui.position),
                ui.volume
            ),
            json!({
                "type": "status",
                "state": state_name(ui.state),
                "track": ui.now.as_ref().map(|t| track_json("track", t, [])),
                "position_ms": ui.position.as_millis() as u64,
                "duration_ms": ui.duration.map(|d| d.as_millis() as u64),
                "volume": ui.volume,
            }),
        );
    }

    fn update(&self, arg: &str) -> Flow {
        let Some((status, seen)) = self.player.update_status() else {
            self.hub.error("not connected to a daemon");
            return Flow::Continue;
        };
        match arg {
            "" => self.print_update(&status),
            "check" => return self.update_check(seen),
            "install" => return self.update_install(&status, seen),
            _ => {
                self.hub.error("usage: update [check|install]");
                if self.script {
                    return Flow::Exit(2);
                }
            }
        }
        Flow::Continue
    }

    fn print_update(&self, status: &UpdateStatus) {
        let mut json = json!({
            "type": "update",
            "current": status.current,
            "auto": status.auto,
            "state": update_state_name(&status.state),
        });
        match &status.state {
            UpdateState::Downloading { version } | UpdateState::Ready { version } => {
                json["version"] = json!(version);
            }
            UpdateState::Failed { version, reason } => {
                json["version"] = json!(version);
                json["reason"] = json!(reason);
            }
            UpdateState::Error { reason } | UpdateState::NotUpdatable { reason } => {
                json["reason"] = json!(reason);
            }
            _ => {}
        }
        self.hub.emit(&update_text(status), json);
        if self.hub.format == Format::Text && !matches!(status.state, UpdateState::Unsupported) {
            self.hub.write_line(if status.auto {
                "automatic checks: on"
            } else {
                "automatic checks: off"
            });
        }
    }

    /// A failed update command: the script stops with 1, the REPL goes on.
    fn update_failed(&self, message: &str) -> Flow {
        self.hub.error(message);
        if self.script {
            Flow::Exit(1)
        } else {
            Flow::Continue
        }
    }

    /// The daemon answers a check it starts with `Checking`, then maybe `Downloading`, then a
    /// settled state; one it ignores, with its status unchanged.
    fn update_check(&self, mut seen: usize) -> Flow {
        self.player.request_update(UpdateRequest::Check);
        self.hub.ack("update check");
        let deadline = Instant::now() + UPDATE_CHECK_WAIT;
        let mut working: Option<UpdateState> = None;
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return self.update_failed("timed out waiting for the update check");
            };
            let Some(status) = self.player.next_update(seen, left) else {
                if let Some(flow) = self.lost(self.player.disconnected().is_some()) {
                    return flow;
                }
                continue;
            };
            seen += 1;
            match &status.state {
                UpdateState::Checking => working = Some(status.state.clone()),
                UpdateState::Downloading { version } => {
                    if working != Some(status.state.clone()) {
                        self.hub.emit(
                            &format!("downloading Requiem {version}"),
                            json!({ "type": "update_downloading", "version": version }),
                        );
                    }
                    working = Some(status.state.clone());
                }
                UpdateState::Failed { .. } | UpdateState::Error { .. } => {
                    self.print_update(&status);
                    return if self.script {
                        Flow::Exit(1)
                    } else {
                        Flow::Continue
                    };
                }
                _ => {
                    self.print_update(&status);
                    return Flow::Continue;
                }
            }
        }
    }

    /// Accepted, the daemon stops serving to restart on the new version; refused, it
    /// repeats its status.
    fn update_install(&self, status: &UpdateStatus, seen: usize) -> Flow {
        self.player.request_update(UpdateRequest::Install);
        self.hub.ack("update install");
        if let Some(status) = self.player.next_update(seen, UPDATE_INSTALL_WAIT) {
            self.print_update(&status);
            return self.update_failed("no update is ready to install");
        }
        if self.player.disconnected().is_none() {
            return self.update_failed("timed out waiting for the player to restart");
        }
        let version = match &status.state {
            UpdateState::Ready { version } => version.as_str(),
            _ => "the new version",
        };
        self.hub.emit(
            &format!("Requiem is restarting on {version}"),
            json!({ "type": "update_restarting", "version": version }),
        );
        Flow::Quit
    }

    fn await_event(&self, arg: &str) -> Flow {
        let mut parts = arg.split_whitespace();
        let what = parts.next().unwrap_or("");
        let pick: fn(&Seen) -> bool = match what {
            "results" => |s| s.results,
            "playing" => |s| s.playing,
            "stopped" => |s| s.stopped,
            "nowplaying" => |s| s.nowplaying,
            _ => {
                self.hub
                    .error("usage: await <results|playing|stopped|nowplaying> [timeout_secs]");
                return Flow::Continue;
            }
        };
        let timeout = match parts.next() {
            None => Duration::from_secs_f64(DEFAULT_AWAIT_SECS),
            Some(t) => match parse_secs(t) {
                Some(d) => d,
                None => {
                    self.hub.error("bad timeout");
                    return Flow::Continue;
                }
            },
        };
        let deadline = Instant::now() + timeout;
        let mut ui = self.hub.lock_ui();
        while !pick(&ui.seen) {
            if ui.closed {
                drop(ui);
                return self.lost(true).unwrap_or(Flow::Continue);
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                drop(ui);
                self.hub.error(&format!("timed out waiting for {what}"));
                return if self.script {
                    Flow::Exit(2)
                } else {
                    Flow::Continue
                };
            };
            ui = self
                .hub
                .changed
                .wait_timeout(ui, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Flow::Continue
    }
}

/// Returns the process exit code: 0 on quit or end of input, 2 on a script-mode abort, 1 on
/// a lost daemon connection in interactive mode. The caller shuts the session down afterwards.
pub fn run(
    player: &dyn Player,
    input: impl BufRead,
    out: impl Write + Send + 'static,
    script: bool,
) -> io::Result<u8> {
    run_with_format(player, input, out, script, Format::Text)
}

/// `requiem update [check|install]`: runs `line` as a script would, printing its answer
/// without the daemon's events. Returns the exit code.
pub fn run_update(
    player: &dyn Player,
    line: &str,
    out: impl Write + Send + 'static,
    format: Format,
) -> u8 {
    let app = App::build(player, out, true, format, false);
    if player.remote() {
        app.sync();
    }
    match app.exec(line) {
        Flow::Continue | Flow::Quit => 0,
        Flow::Exit(code) => code,
    }
}

/// `run` with a choice of output format.
pub fn run_with_format(
    player: &dyn Player,
    input: impl BufRead,
    out: impl Write + Send + 'static,
    script: bool,
    format: Format,
) -> io::Result<u8> {
    let app = App::with_format(player, out, script, format);
    if player.remote() {
        app.sync();
    }
    for line in input.lines() {
        match app.exec(&line?) {
            Flow::Continue => {}
            Flow::Quit => return Ok(0),
            Flow::Exit(code) => return Ok(code),
        }
    }
    Ok(0)
}
