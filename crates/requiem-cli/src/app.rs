use std::io::{self, BufRead, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use requiem_core::{Command, EnqueueMode, Event, PlayState, SeekTarget, Session, Track};

const DEFAULT_AWAIT_SECS: f64 = 30.0;

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
  help | quit";

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
    /// Script mode only: abort with this exit code.
    Exit(u8),
}

pub struct App<'a> {
    session: &'a Session,
    hub: Arc<Hub>,
    script: bool,
}

impl<'a> App<'a> {
    /// Subscribes and starts the printer thread, which ends when the session shuts down.
    pub fn new(session: &'a Session, out: impl Write + Send + 'static, script: bool) -> Self {
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
            }),
            changed: Condvar::new(),
            out: Mutex::new(Box::new(out)),
        });
        let rx = session.subscribe();
        let h = hub.clone();
        std::thread::Builder::new()
            .name("requiem-printer".into())
            .spawn(move || {
                for ev in rx {
                    h.on_event(ev);
                }
            })
            .expect("spawn printer thread");
        Self {
            session,
            hub,
            script,
        }
    }

    pub fn exec(&self, line: &str) -> Flow {
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
                self.session.send(Command::Search {
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
                    self.session.send(Command::Enqueue {
                        tracks: vec![track],
                        mode: EnqueueMode::Replace,
                    });
                    self.session.send(Command::PlayIndex(0));
                } else {
                    self.session.send(Command::Enqueue {
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
                self.session.send(match cmd {
                    "next" => Command::Next,
                    "prev" => Command::Prev,
                    "pause" => Command::Pause,
                    "resume" => Command::Resume,
                    _ => Command::TogglePause,
                });
            }
            "seek" => match parse_seek(arg) {
                Some(t) => self.session.send(Command::Seek(t)),
                None => say("error: usage: seek <secs|+secs|-secs>"),
            },
            "vol" => match arg.parse::<u8>() {
                Ok(v) if v <= 100 => self.session.send(Command::SetVolume(v)),
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

/// Returns the process exit code: 0 on quit or end of input, 2 on a script-mode abort.
/// The caller shuts the session down afterwards.
pub fn run(
    session: &Session,
    input: impl BufRead,
    out: impl Write + Send + 'static,
    script: bool,
) -> io::Result<u8> {
    let app = App::new(session, out, script);
    for line in input.lines() {
        match app.exec(&line?) {
            Flow::Continue => {}
            Flow::Quit => return Ok(0),
            Flow::Exit(code) => return Ok(code),
        }
    }
    Ok(0)
}
