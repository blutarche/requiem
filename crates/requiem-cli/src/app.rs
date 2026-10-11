use std::io::{self, BufRead, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;
use requiem_core::{
    Command, EnqueueMode, Event, PlayState, ProviderId, SeekTarget, Session, Track, TrackId,
};
use serde_json::{Value, json};

const DEFAULT_AWAIT_SECS: f64 = 30.0;

/// How long a `--connect` run waits for the daemon's join snapshot; matches the client's
/// write timeout.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);

const HELP: &str = "\
commands:
  search <query>        search; prints numbered results
  play <n>              replace the queue with result n and play it
  add <n>               append result n to the queue
  play-track <json>     like play, with the track object a --json result line prints
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
                        wait for that event since the last play/next/prev/search
  mark <name>           print MARK <name> <unix_millis>
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

struct Ui {
    latest_req: u64,
    /// `None` until the latest search has returned.
    results: Option<Vec<Track>>,
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
            Format::Text => self.write_line(text),
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
                        &format!("  {}. {}", i + 1, track_line(t)),
                        track_json("result", t, [("index", json!(i + 1))]),
                    );
                }
                ui.results = Some(tracks);
                ui.seen.results = true;
            }
            Event::SearchFailed { error, .. } => self.emit(
                &format!("search failed: {error}"),
                json!({ "type": "search_failed", "error": error }),
            ),
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
            Event::BrowseResults { .. }
            | Event::BrowseFailed { .. }
            | Event::Account { .. }
            | Event::SignInFailed { .. } => {}
        }
        drop(ui);
        self.changed.notify_all();
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

fn track_label(t: &Track) -> String {
    format!("{} — {}", t.artists.join(", "), t.title)
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
        let hub = Arc::new(Hub {
            ui: Mutex::new(Ui {
                latest_req: 0,
                results: None,
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
                let req = {
                    let mut ui = self.hub.lock_ui();
                    ui.latest_req += 1;
                    ui.results = None;
                    ui.seen.results = false;
                    ui.latest_req
                };
                self.player.send(Command::Search {
                    req,
                    query: arg.to_owned(),
                });
                self.hub.ack(cmd);
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
                    let Some(t) = self.pick(arg) else {
                        return Flow::Continue;
                    };
                    t
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

    /// The track for `play <n>` / `add <n>`; prints the error and returns `None` otherwise.
    fn pick(&self, arg: &str) -> Option<Track> {
        let ui = self.hub.lock_ui();
        let Some(results) = &ui.results else {
            self.hub.error("no search results yet");
            return None;
        };
        let track = arg
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| results.get(i));
        if track.is_none() {
            self.hub.error(&format!("no result {arg}"));
        }
        track.cloned()
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
