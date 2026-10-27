//! Player state and the pure mappings between it and what MPRIS and the tray show.
//! Nothing here touches D-Bus, so it builds and tests on every OS.

use std::time::Duration;

use requiem_core::{Command, Event, PlayState, SeekTarget, Track, TrackId};

/// What the tray or MPRIS asks of the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Resume,
    Pause,
    Toggle,
    Next,
    Prev,
    Seek(Duration),
    SetVolume(u8),
    /// Raise the window, launching it when none is open.
    ShowWindow,
    Quit,
}

impl Action {
    /// The session command to send; `None` for `ShowWindow` and `Quit`, which the daemon
    /// handles itself.
    pub fn command(self) -> Option<Command> {
        Some(match self {
            Action::Resume => Command::Resume,
            Action::Pause => Command::Pause,
            Action::Toggle => Command::TogglePause,
            Action::Next => Command::Next,
            Action::Prev => Command::Prev,
            Action::Seek(to) => Command::Seek(SeekTarget::Absolute(to)),
            Action::SetVolume(v) => Command::SetVolume(v),
            Action::ShowWindow | Action::Quit => return None,
        })
    }
}

/// Which published properties an event changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changed {
    pub status: bool,
    pub metadata: bool,
    pub volume: bool,
    pub can_go: bool,
}

impl Changed {
    pub fn any(self) -> bool {
        self.status || self.metadata || self.volume || self.can_go
    }
}

/// The player as the latest events describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub track: Option<Track>,
    pub duration: Option<Duration>,
    pub state: PlayState,
    pub position: Duration,
    pub volume: u8,
    pub queue_len: usize,
    pub current: Option<usize>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            track: None,
            duration: None,
            state: PlayState::Stopped,
            position: Duration::ZERO,
            volume: 0,
            queue_len: 0,
            current: None,
        }
    }
}

impl Snapshot {
    pub fn apply(&mut self, ev: &Event) -> Changed {
        let mut changed = Changed::default();
        match ev {
            Event::NowPlaying { track, duration } => {
                self.track = Some(track.clone());
                self.duration = duration.or(track.duration);
                self.position = Duration::ZERO;
                changed.metadata = true;
            }
            Event::Position(at) => self.position = *at,
            Event::State(state) => {
                changed.status = self.state != *state;
                self.state = *state;
                // The session drops its track on stop and replays `NowPlaying` only for a
                // loaded one.
                if *state == PlayState::Stopped {
                    changed.metadata = self.track.take().is_some();
                    self.duration = None;
                    self.position = Duration::ZERO;
                }
            }
            Event::Volume(v) => {
                changed.volume = self.volume != *v;
                self.volume = *v;
            }
            Event::QueueChanged { tracks, current } => {
                let before = (self.can_go_next(), self.can_go_previous());
                self.queue_len = tracks.len();
                self.current = *current;
                changed.can_go = before != (self.can_go_next(), self.can_go_previous());
            }
            _ => {}
        }
        changed
    }

    /// Whether `Next` would move to another track. At the last track the session stops
    /// instead, which MPRIS does not count.
    pub fn can_go_next(&self) -> bool {
        self.current.is_some_and(|c| c + 1 < self.queue_len)
    }

    /// MPRIS defines CanGoPrevious as "the current track would change"; the session's `Prev`
    /// only restarts the first track.
    pub fn can_go_previous(&self) -> bool {
        self.current.is_some_and(|c| c > 0)
    }

    pub fn track_path(&self) -> Option<String> {
        self.track.as_ref().map(|t| track_path(&t.id))
    }

    /// "Title — Artist, Artist" as the tray shows it; `None` while nothing is loaded.
    pub fn headline(&self) -> Option<String> {
        let t = self.track.as_ref()?;
        Some(if t.artists.is_empty() {
            t.title.clone()
        } else {
            format!("{} — {}", t.title, t.artists.join(", "))
        })
    }
}

pub const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

/// A valid D-Bus object path for a track. Object path elements allow only `[A-Za-z0-9_]`
/// (YouTube ids contain `-`), so every other byte becomes `_` plus two hex digits. `_`
/// itself is escaped too, so distinct ids never share a path.
pub fn track_path(id: &TrackId) -> String {
    format!(
        "/org/requiem/track/{}/{}",
        escape_element(&id.provider.0),
        escape_element(&id.id)
    )
}

fn escape_element(s: &str) -> String {
    if s.is_empty() {
        // Path elements cannot be empty. A lone `_` is unreachable by escaping.
        return "_".into();
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

/// MPRIS has no Buffering: a track that is loading is on its way to playing.
pub fn playback_status(state: PlayState) -> &'static str {
    match state {
        PlayState::Playing | PlayState::Buffering => "Playing",
        PlayState::Paused => "Paused",
        PlayState::Stopped => "Stopped",
    }
}

pub fn icon_name(state: PlayState) -> &'static str {
    match state {
        PlayState::Playing | PlayState::Buffering => "media-playback-start",
        PlayState::Paused => "media-playback-pause",
        PlayState::Stopped => "media-playback-stop",
    }
}

pub fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// Negative values clamp to zero.
pub fn from_micros(us: i64) -> Duration {
    Duration::from_micros(us.max(0).unsigned_abs())
}

/// u8 0-100 to MPRIS 0.0-1.0.
pub fn volume_to_mpris(v: u8) -> f64 {
    f64::from(v.min(100)) / 100.0
}

/// MPRIS 0.0-1.0 to u8 0-100, clamped. NaN reads as 0.
pub fn volume_from_mpris(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// The absolute target for MPRIS `Seek`: the last known position plus the offset, not below 0.
pub fn seek_target(position: Duration, offset_us: i64) -> Duration {
    from_micros(micros(position).saturating_add(offset_us))
}

#[cfg(test)]
mod tests {
    use requiem_core::ProviderId;

    use super::*;

    fn id(provider: &str, id: &str) -> TrackId {
        TrackId {
            provider: ProviderId(provider.into()),
            id: id.into(),
        }
    }

    fn track(n: &str) -> Track {
        Track {
            id: id("ytmusic", n),
            title: format!("T{n}"),
            artists: vec!["Ann".into(), "Bo".into()],
            artist_ids: Vec::new(),
            album: None,
            duration: Some(Duration::from_secs(90)),
            artwork_url: None,
        }
    }

    fn queue(len: usize, current: Option<usize>) -> Event {
        Event::QueueChanged {
            tracks: (0..len).map(|i| track(&i.to_string())).collect(),
            current,
        }
    }

    fn valid_object_path(p: &str) -> bool {
        p.starts_with('/')
            && p.len() > 1
            && p[1..]
                .split('/')
                .all(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    }

    #[test]
    fn track_paths_are_valid_object_paths() {
        for raw in ["dQw4w9WgXcQ", "a-b_c", "", "../x", "é/ü", "a b\0c", "_"] {
            let p = track_path(&id("ytmusic", raw));
            assert!(valid_object_path(&p), "{raw:?} -> {p}");
        }
        assert_eq!(
            track_path(&id("ytmusic", "ab-c")),
            "/org/requiem/track/ytmusic/ab_2dc"
        );
    }

    #[test]
    fn track_paths_do_not_collide() {
        let a = track_path(&id("p", "a-b"));
        let b = track_path(&id("p", "a_b"));
        let c = track_path(&id("p", "a_2db"));
        assert!(a != b && a != c && b != c);
        assert_ne!(track_path(&id("p", "")), track_path(&id("p", "_")));
    }

    #[test]
    fn micros_round_trip_and_clamp() {
        assert_eq!(micros(Duration::from_millis(1500)), 1_500_000);
        assert_eq!(from_micros(1_500_000), Duration::from_millis(1500));
        assert_eq!(from_micros(-5), Duration::ZERO);
        assert_eq!(micros(Duration::MAX), i64::MAX);
    }

    #[test]
    fn volume_maps_both_ways() {
        assert_eq!(volume_to_mpris(0), 0.0);
        assert_eq!(volume_to_mpris(50), 0.5);
        assert_eq!(volume_to_mpris(100), 1.0);
        assert_eq!(volume_from_mpris(0.0), 0);
        assert_eq!(volume_from_mpris(0.5), 50);
        assert_eq!(volume_from_mpris(1.0), 100);
        assert_eq!(volume_from_mpris(0.555), 56);
        assert_eq!(volume_from_mpris(-1.0), 0);
        assert_eq!(volume_from_mpris(7.0), 100);
        assert_eq!(volume_from_mpris(f64::NAN), 0);
        for v in 0..=100u8 {
            assert_eq!(volume_from_mpris(volume_to_mpris(v)), v);
        }
    }

    #[test]
    fn states_map_to_status_and_icon() {
        assert_eq!(playback_status(PlayState::Playing), "Playing");
        assert_eq!(playback_status(PlayState::Buffering), "Playing");
        assert_eq!(playback_status(PlayState::Paused), "Paused");
        assert_eq!(playback_status(PlayState::Stopped), "Stopped");
        assert_eq!(icon_name(PlayState::Playing), "media-playback-start");
        assert_eq!(icon_name(PlayState::Paused), "media-playback-pause");
        assert_eq!(icon_name(PlayState::Stopped), "media-playback-stop");
    }

    #[test]
    fn seek_is_last_position_plus_offset_not_below_zero() {
        let at = Duration::from_secs(10);
        assert_eq!(seek_target(at, 5_000_000), Duration::from_secs(15));
        assert_eq!(seek_target(at, -4_000_000), Duration::from_secs(6));
        assert_eq!(seek_target(at, -60_000_000), Duration::ZERO);
        assert_eq!(seek_target(at, i64::MIN), Duration::ZERO);
    }

    #[test]
    fn can_go_edges() {
        let mut s = Snapshot::default();
        assert!(!s.can_go_next() && !s.can_go_previous());

        s.apply(&queue(0, None));
        assert!(!s.can_go_next() && !s.can_go_previous());
        s.apply(&queue(3, None));
        assert!(!s.can_go_next() && !s.can_go_previous());

        s.apply(&queue(3, Some(0)));
        assert!(s.can_go_next() && !s.can_go_previous());
        s.apply(&queue(3, Some(1)));
        assert!(s.can_go_next() && s.can_go_previous());
        s.apply(&queue(3, Some(2)));
        assert!(!s.can_go_next() && s.can_go_previous());
        s.apply(&queue(1, Some(0)));
        assert!(!s.can_go_next() && !s.can_go_previous());
    }

    #[test]
    fn queue_events_report_can_go_only_when_it_flips() {
        let mut s = Snapshot::default();
        assert!(s.apply(&queue(3, Some(0))).can_go);
        assert!(!s.apply(&queue(4, Some(0))).can_go);
        assert!(s.apply(&queue(4, Some(1))).can_go);
    }

    #[test]
    fn events_fold_into_the_snapshot() {
        let mut s = Snapshot::default();
        let c = s.apply(&Event::NowPlaying {
            track: track("a"),
            duration: None,
        });
        assert!(c.metadata && !c.status);
        assert_eq!(s.duration, Some(Duration::from_secs(90)));
        assert_eq!(s.headline().as_deref(), Some("Ta — Ann, Bo"));

        let c = s.apply(&Event::State(PlayState::Playing));
        assert!(c.status && !c.metadata);
        assert!(!s.apply(&Event::State(PlayState::Playing)).any());

        assert!(!s.apply(&Event::Position(Duration::from_secs(7))).any());
        assert_eq!(s.position, Duration::from_secs(7));

        assert!(s.apply(&Event::Volume(40)).volume);
        assert!(!s.apply(&Event::Volume(40)).any());

        let c = s.apply(&Event::State(PlayState::Stopped));
        assert!(c.status && c.metadata);
        assert!(s.track.is_none() && s.headline().is_none() && s.track_path().is_none());
        assert_eq!(s.position, Duration::ZERO);
        // Already empty: stopping again changes no metadata.
        assert!(!s.apply(&Event::State(PlayState::Stopped)).metadata);
    }

    #[test]
    fn headline_without_artists_is_the_title() {
        let mut s = Snapshot::default();
        let mut t = track("a");
        t.artists.clear();
        s.apply(&Event::NowPlaying {
            track: t,
            duration: None,
        });
        assert_eq!(s.headline().as_deref(), Some("Ta"));
    }

    #[test]
    fn actions_become_commands() {
        assert_eq!(Action::Toggle.command(), Some(Command::TogglePause));
        assert_eq!(
            Action::Seek(Duration::from_secs(3)).command(),
            Some(Command::Seek(SeekTarget::Absolute(Duration::from_secs(3))))
        );
        assert_eq!(Action::SetVolume(9).command(), Some(Command::SetVolume(9)));
        assert_eq!(Action::ShowWindow.command(), None);
        assert_eq!(Action::Quit.command(), None);
    }
}
