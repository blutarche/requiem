//! `--demo`: fixed fake data and a stand-in for the daemon, so screenshots are deterministic.
//! All names are made up.

use std::time::{Duration, Instant};

use requiem_core::{
    Command, EnqueueMode, Event, PlayState, ProviderId, SeekTarget, Track, TrackId,
};

use crate::args::View;
use crate::vm::{BANNER_GRACE, Intent, Link, Vm};

pub const QUERY: &str = "night drive";

const RESULTS: [(&str, &[&str], Option<&str>, u64); 30] = [
    (
        "Night Drive Home",
        &["Hollow Coast"],
        Some("Low Tide Radio"),
        225,
    ),
    ("Paper Lanterns", &["Mira Vance"], Some("Glasshouse"), 242),
    (
        "Slow Satellites",
        &["The Lantern Choir"],
        Some("Orbit Hymns"),
        198,
    ),
    (
        "Neon Rain, Empty Streets",
        &["Velvet Static", "Noor Calloway"],
        Some("After Hours Transit"),
        267,
    ),
    (
        "Harbor Lights",
        &["Paper Harbors"],
        Some("Harbor Lights"),
        231,
    ),
    (
        "Fever in the Valley",
        &["Saint Ember"],
        Some("Wildfire Season"),
        178,
    ),
    (
        "Quiet Machines",
        &["Cobalt Garden"],
        Some("Signal & Noise"),
        312,
    ),
    (
        "Late Train to Marlow",
        &["Elias Wren"],
        Some("Small Hours"),
        213,
    ),
    (
        "Gold Static",
        &["Velvet Static"],
        Some("After Hours Transit"),
        187,
    ),
    (
        "Midnight Orchard",
        &["Juniper & Ash"],
        Some("Second Bloom"),
        255,
    ),
    (
        "The Long Way Round",
        &["Tamsin Hale"],
        Some("Northern Letters"),
        229,
    ),
    ("Undertow", &["Kestrel Bay"], Some("Undertow"), 280),
    (
        "Glass Coast",
        &["Hollow Coast"],
        Some("Low Tide Radio"),
        206,
    ),
    (
        "Starlight Motel",
        &["Northbound Echo"],
        Some("Route Nine"),
        238,
    ),
    ("Ribbons", &["Mira Vance"], Some("Glasshouse"), 167),
    (
        "Afterglow (Night Drive Edit)",
        &["Saint Ember", "Cobalt Garden"],
        None,
        201,
    ),
    ("Small Fires", &["Noor Calloway"], Some("Embers & Ink"), 249),
    (
        "Velvet Hours",
        &["The Lantern Choir"],
        Some("Orbit Hymns"),
        301,
    ),
    (
        "Driftwood Heart",
        &["Juniper & Ash"],
        Some("Second Bloom"),
        217,
    ),
    (
        "Signal Lost",
        &["Cobalt Garden"],
        Some("Signal & Noise"),
        262,
    ),
    (
        "Moonlit Interstate",
        &["Northbound Echo"],
        Some("Route Nine"),
        235,
    ),
    (
        "Silver Linings Club",
        &["Paper Harbors"],
        Some("Postcards"),
        194,
    ),
    (
        "Hometown Ghosts",
        &["Tamsin Hale"],
        Some("Northern Letters"),
        273,
    ),
    (
        "Ocean Avenue Blue",
        &["Kestrel Bay"],
        Some("Saltwater Diaries"),
        222,
    ),
    ("Still Awake", &["Elias Wren"], Some("Small Hours"), 175),
    (
        "Highway Psalm",
        &["The Lantern Choir", "Elias Wren"],
        Some("Orbit Hymns"),
        288,
    ),
    (
        "Paper Planes at Dawn",
        &["Mira Vance"],
        Some("Glasshouse"),
        209,
    ),
    (
        "Neon Cathedral",
        &["Velvet Static"],
        Some("After Hours Transit"),
        364,
    ),
    ("Window Seat", &["Noor Calloway"], Some("Embers & Ink"), 196),
    (
        "Last Exit Lullaby",
        &["Hollow Coast"],
        Some("Low Tide Radio"),
        291,
    ),
];

/// Indices into `RESULTS`; the current entry (index 2) is the 3:45 track.
const QUEUE: [usize; 8] = [4, 9, 0, 13, 6, 20, 10, 23];
const CURRENT: usize = 2;
const POSITION: Duration = Duration::from_secs(83);
const VOLUME: u8 = 70;
/// The result whose playback fails in the toast demo.
const FAILED: usize = 5;

pub fn results() -> Vec<Track> {
    RESULTS
        .iter()
        .enumerate()
        .map(|(i, (title, artists, album, secs))| Track {
            id: TrackId {
                provider: ProviderId("demo".into()),
                id: format!("demo-{i:02}"),
            },
            title: (*title).to_owned(),
            artists: artists.iter().map(|a| (*a).to_owned()).collect(),
            album: album.map(str::to_owned),
            duration: Some(Duration::from_secs(*secs)),
            artwork_url: None,
        })
        .collect()
}

/// Stands in for the daemon: commands update the fake state where that is trivial.
#[derive(Debug)]
pub struct Daemon {
    results: Vec<Track>,
    queue: Vec<Track>,
    current: Option<usize>,
    state: PlayState,
    volume: u8,
    position: Duration,
}

impl Default for Daemon {
    fn default() -> Self {
        let results = results();
        let queue = QUEUE.iter().map(|&i| results[i].clone()).collect();
        Self {
            results,
            queue,
            current: Some(CURRENT),
            state: PlayState::Playing,
            volume: VOLUME,
            position: POSITION,
        }
    }
}

impl Daemon {
    /// The events a daemon sends a new client, in the protocol's snapshot order.
    pub fn snapshot(&self) -> Vec<Event> {
        let mut evs = vec![Event::QueueChanged {
            tracks: self.queue.clone(),
            current: self.current,
        }];
        let playing = self.playing();
        if let Some(t) = &playing {
            evs.push(Event::NowPlaying {
                track: t.clone(),
                duration: t.duration,
            });
        }
        evs.push(Event::State(self.state));
        evs.push(Event::Volume(self.volume));
        if playing.is_some() {
            evs.push(Event::Position(self.position));
        }
        evs
    }

    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        match cmd {
            // Every query matches everything.
            Command::Search { req, .. } => vec![Event::SearchResults {
                req,
                tracks: self.results.clone(),
            }],
            Command::Enqueue { tracks, mode } => {
                match mode {
                    EnqueueMode::Replace => {
                        self.queue = tracks;
                        self.current = None;
                    }
                    EnqueueMode::Append => self.queue.extend(tracks),
                }
                vec![self.queue_changed()]
            }
            Command::PlayIndex(i) => self.start(i),
            Command::Next => match self.current.map_or(0, |c| c + 1) {
                n if n < self.queue.len() => self.start(n),
                _ => self.stop(),
            },
            Command::Prev => match self.current {
                Some(c) if c > 0 => self.start(c - 1),
                _ => self.seek(Duration::ZERO),
            },
            Command::Pause => self.set_state(PlayState::Paused),
            Command::Resume => self.set_state(PlayState::Playing),
            Command::TogglePause => match self.state {
                PlayState::Playing | PlayState::Buffering => self.set_state(PlayState::Paused),
                PlayState::Paused => self.set_state(PlayState::Playing),
                PlayState::Stopped => Vec::new(),
            },
            Command::Seek(SeekTarget::Absolute(to)) => self.seek(to),
            Command::Seek(SeekTarget::Relative(s)) => {
                let step = Duration::from_secs(s.unsigned_abs());
                let to = if s >= 0 {
                    self.position.saturating_add(step)
                } else {
                    self.position.saturating_sub(step)
                };
                self.seek(to)
            }
            Command::SetVolume(v) => {
                self.volume = v.min(100);
                vec![Event::Volume(self.volume)]
            }
            Command::Shutdown => Vec::new(),
        }
    }

    fn playing(&self) -> Option<Track> {
        if self.state == PlayState::Stopped {
            return None;
        }
        self.current.and_then(|c| self.queue.get(c).cloned())
    }

    pub fn queue_changed(&self) -> Event {
        Event::QueueChanged {
            tracks: self.queue.clone(),
            current: self.current,
        }
    }

    fn start(&mut self, i: usize) -> Vec<Event> {
        let Some(track) = self.queue.get(i).cloned() else {
            return Vec::new();
        };
        self.current = Some(i);
        self.state = PlayState::Playing;
        self.position = Duration::ZERO;
        vec![
            self.queue_changed(),
            Event::NowPlaying {
                duration: track.duration,
                track,
            },
            Event::State(PlayState::Playing),
            Event::Position(Duration::ZERO),
        ]
    }

    fn stop(&mut self) -> Vec<Event> {
        self.state = PlayState::Stopped;
        vec![Event::State(PlayState::Stopped)]
    }

    fn set_state(&mut self, state: PlayState) -> Vec<Event> {
        if self.playing().is_none() {
            return Vec::new();
        }
        self.state = state;
        vec![Event::State(state)]
    }

    fn seek(&mut self, to: Duration) -> Vec<Event> {
        let Some(track) = self.playing() else {
            return Vec::new();
        };
        self.position = track.duration.map_or(to, |d| to.min(d));
        vec![Event::Position(self.position)]
    }
}

/// A view model and daemon stand-in in the demo's starting state: the snapshot applied and
/// `QUERY` searched.
pub fn start() -> (Vm, Daemon) {
    start_in(View::Search)
}

/// `start`, staged for a demo-only `view`: `Empty` skips the search; `Disconnected`,
/// `OutOfDate` and `Toast` add their banner or toast. Other views get the plain start.
pub fn start_in(view: View) -> (Vm, Daemon) {
    let t0 = Instant::now();
    let mut vm = Vm::new_at(t0);
    let mut daemon = Daemon::default();
    vm.link(Link::Demo);
    for ev in daemon.snapshot() {
        vm.apply(ev);
    }
    if view != View::Empty {
        for cmd in vm.intent(Intent::Search(QUERY.into())) {
            for ev in daemon.apply(cmd) {
                vm.apply(ev);
            }
        }
    }
    match view {
        View::Disconnected => {
            // Past the grace, as a real outage would be by the time its banner shows.
            vm.link_at(Link::Lost("demo".into()), t0);
            vm.tick_at(t0 + BANNER_GRACE);
        }
        View::OutOfDate => vm.link(Link::OutOfDate),
        View::Toast => {
            vm.apply(Event::TrackFailed {
                track: daemon.results[FAILED].clone(),
                error: "video unavailable".into(),
            });
            vm.apply(daemon.queue_changed());
        }
        View::Search | View::Queue | View::Palette | View::Empty => {}
    }
    (vm, daemon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{Connection, EmptyAction, SearchStatus};

    fn run(vm: &mut Vm, daemon: &mut Daemon, intent: Intent) {
        for cmd in vm.intent(intent) {
            for ev in daemon.apply(cmd) {
                vm.apply(ev);
            }
        }
    }

    #[test]
    fn starts_with_the_specified_fake_state() {
        let (vm, _) = start();
        let ui = vm.ui();
        assert_eq!(ui.connection, Connection::Demo);
        assert_eq!(ui.results.len(), 30);
        assert_eq!(ui.search, SearchStatus::Done);
        assert_eq!(ui.query, QUERY);
        assert_eq!(ui.queue.len(), 8);
        assert_eq!(ui.current, Some(2));
        assert_eq!(ui.state, PlayState::Playing);
        assert_eq!(ui.elapsed_text(), "1:23");
        assert_eq!(ui.total_text(), "3:45");
        assert_eq!(ui.volume, 70);
        assert_eq!(ui.now_playing.as_ref(), Some(&ui.queue[2]));
    }

    #[test]
    fn empty_has_no_query_or_results() {
        let (vm, _) = start_in(View::Empty);
        let ui = vm.ui();
        assert_eq!(ui.query, "");
        assert!(ui.results.is_empty());
        assert_eq!(ui.search, SearchStatus::Idle);
        let empty = ui.results_empty().unwrap();
        assert_eq!(empty.heading, "Search YouTube Music");
        assert_eq!(empty.action, EmptyAction::StartSearch);
        assert_eq!(ui.state, PlayState::Playing);
    }

    #[test]
    fn staged_banners_and_toast() {
        let (vm, _) = start_in(View::Disconnected);
        let b = vm.ui().banner.clone().unwrap();
        assert!(b.text.starts_with("Player disconnected") && !b.error);
        assert!(vm.ui().toast.is_none());

        let (vm, _) = start_in(View::OutOfDate);
        let b = vm.ui().banner.clone().unwrap();
        assert!(b.text.starts_with("An older requiem player") && b.error);

        let (vm, _) = start_in(View::Toast);
        let t = vm.ui().toast.clone().unwrap();
        assert!(t.error);
        assert_eq!(
            t.text,
            "Couldn't play “Fever in the Valley”: video unavailable. Skipped to the next track."
        );
        assert!(vm.ui().banner.is_none());
        assert_eq!(vm.ui().results.len(), 30);
    }

    #[test]
    fn track_ids_are_unique() {
        let r = results();
        let mut ids: Vec<_> = r.iter().map(|t| t.id.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 30);
    }

    #[test]
    fn toggle_pauses_and_resumes() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::Toggle);
        assert_eq!(vm.ui().state, PlayState::Paused);
        run(&mut vm, &mut d, Intent::Toggle);
        assert_eq!(vm.ui().state, PlayState::Playing);
    }

    #[test]
    fn play_result_replaces_queue_and_plays() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::PlayResult(25));
        let ui = vm.ui();
        assert_eq!(ui.queue.len(), 5);
        assert_eq!(ui.current, Some(0));
        assert_eq!(ui.now_playing.as_ref().unwrap().title, "Highway Psalm");
        assert_eq!(ui.elapsed_text(), "0:00");
    }

    #[test]
    fn append_and_queue_click() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::AppendResult(1));
        assert_eq!(vm.ui().queue.len(), 9);
        assert_eq!(vm.ui().queue[8].title, "Paper Lanterns");
        run(&mut vm, &mut d, Intent::PlayQueue(8));
        assert_eq!(vm.ui().current, Some(8));
        assert_eq!(
            vm.ui().now_playing.as_ref().unwrap().title,
            "Paper Lanterns"
        );
    }

    #[test]
    fn next_prev_and_stop_at_end() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::Next);
        assert_eq!(vm.ui().current, Some(3));
        run(&mut vm, &mut d, Intent::Prev);
        assert_eq!(vm.ui().current, Some(2));
        run(&mut vm, &mut d, Intent::PlayQueue(7));
        run(&mut vm, &mut d, Intent::Next);
        assert_eq!(vm.ui().state, PlayState::Stopped);
        assert!(vm.ui().now_playing.is_none());
    }

    #[test]
    fn seek_and_volume() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::SeekBy(5));
        assert_eq!(vm.ui().elapsed_text(), "1:28");
        run(&mut vm, &mut d, Intent::SeekTo(Duration::from_secs(10)));
        assert_eq!(vm.ui().elapsed_text(), "0:10");
        run(&mut vm, &mut d, Intent::VolumeBy(-10));
        assert_eq!(vm.ui().volume, 60);
        run(&mut vm, &mut d, Intent::SetVolume(5));
        assert_eq!(vm.ui().volume, 5);
    }
}
