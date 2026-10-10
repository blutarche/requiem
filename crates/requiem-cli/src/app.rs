use std::io::{self, BufRead, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;
use requiem_core::{Command, EnqueueMode, Event, PlayState, SeekTarget, Session, Track};

const DEFAULT_AWAIT_SECS: f64 = 30.0;

/// How long a `--connect` run waits for the daemon's join snapshot; matches the client's
/// write timeout.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);

const HELP: &str = "\
commands:
  search <query>        search; prints numbered results
  play <n>              replace the queue with result n and play it
  add <n>               append result n to the queue
  queue                 show the queue (> marks the current track)
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
}

impl Hub {
    fn lock_ui(&self) -> MutexGuard<'_, Ui> {
        self.ui.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn say(&self, line: &str) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
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
                    self.say("no results");
                }
                for (i, t) in tracks.iter().enumerate() {
                    self.say(&format!("  {}. {}", i + 1, track_line(t)));
                }
                ui.results = Some(tracks);
                ui.seen.results = true;
            }
            Event::SearchFailed { error, .. } => self.say(&format!("search failed: {error}")),
            Event::QueueChanged { tracks, current } => {
                let cur = current.map_or("none".to_owned(), |i| (i + 1).to_string());
                self.say(&format!("queue: {} tracks, current {cur}", tracks.len()));
                ui.queue = tracks;
                ui.current = current;
            }
            Event::NowPlaying { track, duration } => {
                self.say(&format!("now playing: {}", track_label(&track)));
                ui.now = Some(track);
                ui.duration = duration;
                ui.position = Duration::ZERO;
                ui.seen.nowplaying = true;
            }
            Event::Position(at) => ui.position = at,
            Event::State(s) => {
                self.say(&format!("state: {}", state_name(s)));
                ui.state = s;
                match s {
                    PlayState::Playing => ui.seen.playing = true,
                    PlayState::Stopped => ui.seen.stopped = true,
                    _ => {}
                }
            }
            Event::Volume(v) => {
                self.say(&format!("volume: {v}"));
                ui.volume = v;
                ui.synced = true;
            }
            Event::TrackFailed { track, error } => {
                self.say(&format!("track failed: {}: {error}", track.title));
            }
            Event::ProviderBroken { detail, .. } => {
                self.say(&format!("provider broken: {detail}"));
            }
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
        self.hub
            .say(&format!("error: player disconnected: {reason}"));
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
        let say = |s: &str| self.hub.say(s);
        match cmd {
            "search" => {
                if arg.is_empty() {
                    say("error: usage: search <query>");
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
            }
            "play" | "add" => {
                let Some(track) = self.pick(arg) else {
                    return Flow::Continue;
                };
                if cmd == "play" {
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
            }
            "queue" => {
                let ui = self.hub.lock_ui();
                if ui.queue.is_empty() {
                    say("queue empty");
                }
                for (i, t) in ui.queue.iter().enumerate() {
                    let mark = if ui.current == Some(i) { '>' } else { ' ' };
                    say(&format!("{mark} {}. {}", i + 1, track_line(t)));
                }
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
            }
            "seek" => match parse_seek(arg) {
                Some(t) => self.player.send(Command::Seek(t)),
                None => say("error: usage: seek <secs|+secs|-secs>"),
            },
            "vol" => match arg.parse::<u8>() {
                Ok(v) if v <= 100 => self.player.send(Command::SetVolume(v)),
                _ => say("error: usage: vol <0-100>"),
            },
            "status" => say(&self.status()),
            "sleep" => match parse_secs(arg) {
                Some(d) => std::thread::sleep(d),
                None => say("error: usage: sleep <secs>"),
            },
            "await" => return self.await_event(arg),
            "mark" => {
                if arg.is_empty() {
                    say("error: usage: mark <name>");
                } else {
                    let ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    say(&format!("MARK {arg} {ms}"));
                }
            }
            "help" => say(HELP),
            "shutdown-daemon" => {
                if !self.player.remote() {
                    say("error: not connected to a daemon");
                } else {
                    self.player.send(Command::Shutdown);
                    return Flow::Quit;
                }
            }
            "quit" | "exit" => return Flow::Quit,
            other => {
                say(&format!("error: unknown command {other}"));
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
            self.hub.say("error: no search results yet");
            return None;
        };
        let track = arg
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| results.get(i));
        if track.is_none() {
            self.hub.say(&format!("error: no result {arg}"));
        }
        track.cloned()
    }

    fn clear_playback_seen(&self) {
        let mut ui = self.hub.lock_ui();
        ui.seen.playing = false;
        ui.seen.stopped = false;
        ui.seen.nowplaying = false;
    }

    fn status(&self) -> String {
        let ui = self.hub.lock_ui();
        let track = ui.now.as_ref().map_or("-".to_owned(), track_label);
        let dur = ui.duration.map_or("?:??".to_owned(), mss);
        format!(
            "status: {} | {track} | {} / {dur} | volume {}",
            state_name(ui.state),
            mss(ui.position),
            ui.volume
        )
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
                    .say("error: usage: await <results|playing|stopped|nowplaying> [timeout_secs]");
                return Flow::Continue;
            }
        };
        let timeout = match parts.next() {
            None => Duration::from_secs_f64(DEFAULT_AWAIT_SECS),
            Some(t) => match parse_secs(t) {
                Some(d) => d,
                None => {
                    self.hub.say("error: bad timeout");
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
                self.hub
                    .say(&format!("error: timed out waiting for {what}"));
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
    let app = App::new(player, out, script);
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
