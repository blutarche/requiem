use std::time::{Duration, Instant};

use requiem_core::{Event, PlayState, Track};

use crate::PlayerView;

/// A position further than this from where the last view extrapolated to is a seek.
const SEEK_TOLERANCE: Duration = Duration::from_secs(2);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MENU_TITLE_CHARS: usize = 40;

/// Folds core `Event`s into the current [`PlayerView`].
///
/// [`apply`](Self::apply) reports a view only when something the UI shows changed: the
/// track, the state, the duration, or a position more than 2 s from where the last
/// reported view extrapolates to (a seek, or drift worth correcting). Steady `Position`
/// ticks are not changes because macOS extrapolates elapsed time from the playback rate.
#[derive(Debug)]
pub struct ViewTracker {
    track: Option<Track>,
    duration: Option<Duration>,
    state: PlayState,
    /// Latest known position and when it was known.
    reported: (Duration, Instant),
    /// Last view returned and when; positions are checked against its extrapolation.
    shown: Option<(PlayerView, Instant)>,
}

impl Default for ViewTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewTracker {
    pub fn new() -> Self {
        Self {
            track: None,
            duration: None,
            state: PlayState::Stopped,
            reported: (Duration::ZERO, Instant::now()),
            shown: None,
        }
    }

    /// Returns `Some(view)` when the shown view changed; `Some(None)` means nothing to show.
    pub fn apply(&mut self, ev: &Event) -> Option<Option<PlayerView>> {
        self.apply_at(ev, Instant::now())
    }

    fn apply_at(&mut self, ev: &Event, now: Instant) -> Option<Option<PlayerView>> {
        match ev {
            Event::NowPlaying { track, duration } => {
                self.track = Some(track.clone());
                self.duration = *duration;
                self.reported = (Duration::ZERO, now);
            }
            Event::Position(at) => self.reported = (*at, now),
            Event::State(state) => {
                // Re-anchor under the old state so time played up to now is kept.
                self.reported = (self.position_at(now), now);
                self.state = *state;
                // Session drops its track on stop and only replays `NowPlaying` for a loaded one.
                if *state == PlayState::Stopped {
                    self.track = None;
                    self.duration = None;
                }
            }
            _ => return None,
        }
        let view = self.view(now);
        if !changed(self.shown.as_ref(), view.as_ref(), now, SEEK_TOLERANCE) {
            return None;
        }
        self.shown = view.clone().map(|v| (v, now));
        Some(view)
    }

    fn position_at(&self, now: Instant) -> Duration {
        let (at, since) = self.reported;
        let pos = if self.state == PlayState::Playing {
            at + now.saturating_duration_since(since)
        } else {
            at
        };
        self.duration.map_or(pos, |d| pos.min(d))
    }

    fn view(&self, now: Instant) -> Option<PlayerView> {
        if self.state == PlayState::Stopped {
            return None;
        }
        let track = self.track.as_ref()?;
        Some(PlayerView {
            title: track.title.clone(),
            artists: track.artists.join(", "),
            album: track.album.clone(),
            duration: self.duration,
            position: self.position_at(now),
            state: self.state,
        })
    }
}

/// Whether `next` shows differently from `shown`, a view applied at the paired instant
/// whose position has advanced since at the rate its state implies.
pub(crate) fn changed(
    shown: Option<&(PlayerView, Instant)>,
    next: Option<&PlayerView>,
    now: Instant,
    tolerance: Duration,
) -> bool {
    let (shown, at, next) = match (shown, next) {
        (Some((shown, at)), Some(next)) => (shown, *at, next),
        (None, None) => return false,
        _ => return true,
    };
    let expected = if shown.state == PlayState::Playing {
        shown.position + now.saturating_duration_since(at)
    } else {
        shown.position
    };
    shown.title != next.title
        || shown.artists != next.artists
        || shown.album != next.album
        || shown.duration != next.duration
        || shown.state != next.state
        || expected.abs_diff(next.position) > tolerance
}

/// "Title — Artists", cut to 40 chars with an ellipsis.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn menu_title(view: &PlayerView) -> String {
    let full = if view.artists.is_empty() {
        view.title.clone()
    } else {
        format!("{} — {}", view.title, view.artists)
    };
    if full.chars().count() <= MENU_TITLE_CHARS {
        return full;
    }
    let cut: String = full.chars().take(MENU_TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use requiem_core::{ProviderId, TrackId};

    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn track(title: &str) -> Track {
        Track {
            id: TrackId {
                provider: ProviderId("test".into()),
                id: title.into(),
            },
            title: title.into(),
            artists: vec!["Ann".into(), "Bo".into()],
            album: Some("Album".into()),
            duration: Some(secs(200)),
            artwork_url: None,
        }
    }

    fn now_playing(title: &str, duration: Option<Duration>) -> Event {
        Event::NowPlaying {
            track: track(title),
            duration,
        }
    }

    struct Harness {
        tracker: ViewTracker,
        now: Instant,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                tracker: ViewTracker::new(),
                now: Instant::now(),
            }
        }

        /// A harness already playing "A" (200 s) at position 0.
        fn playing() -> Self {
            let mut h = Self::new();
            h.apply(now_playing("A", Some(secs(200))));
            h.apply(Event::State(PlayState::Playing))
                .expect("playing is a change");
            h
        }

        fn advance(&mut self, by: Duration) {
            self.now += by;
        }

        fn apply(&mut self, ev: Event) -> Option<Option<PlayerView>> {
            self.tracker.apply_at(&ev, self.now)
        }

        /// Advances by `by` and reports the position the engine would have reached.
        fn tick(&mut self, by: Duration, at: Duration) -> Option<Option<PlayerView>> {
            self.advance(by);
            self.apply(Event::Position(at))
        }

        fn shown(update: Option<Option<PlayerView>>) -> PlayerView {
            update.expect("a change").expect("something to show")
        }
    }

    #[test]
    fn ignores_events_the_ui_does_not_show() {
        let mut h = Harness::playing();
        let events = [
            Event::SearchResults {
                req: 1,
                tracks: vec![track("B")],
            },
            Event::SearchFailed {
                req: 2,
                error: "boom".into(),
            },
            Event::QueueChanged {
                tracks: vec![track("A"), track("B")],
                current: Some(1),
            },
            Event::Volume(40),
            Event::TrackFailed {
                track: track("B"),
                error: "gone".into(),
            },
            Event::ProviderBroken {
                provider: ProviderId("test".into()),
                detail: "broken".into(),
            },
        ];
        for ev in events {
            h.advance(secs(5));
            assert_eq!(h.apply(ev.clone()), None, "{ev:?}");
        }
    }

    #[test]
    fn nothing_to_show_before_a_track() {
        let mut h = Harness::new();
        assert_eq!(h.apply(Event::State(PlayState::Playing)), None);
        assert_eq!(h.apply(Event::Position(secs(30))), None);
        assert_eq!(h.apply(Event::State(PlayState::Paused)), None);
    }

    #[test]
    fn track_shows_once_playing() {
        let mut h = Harness::new();
        assert_eq!(h.apply(now_playing("A", Some(secs(180)))), None);
        let view = Harness::shown(h.apply(Event::State(PlayState::Playing)));
        assert_eq!(
            view,
            PlayerView {
                title: "A".into(),
                artists: "Ann, Bo".into(),
                album: Some("Album".into()),
                duration: Some(secs(180)),
                position: Duration::ZERO,
                state: PlayState::Playing,
            }
        );
    }

    #[test]
    fn repeated_state_is_not_a_change() {
        let mut h = Harness::playing();
        h.advance(ms(500));
        assert_eq!(h.apply(Event::State(PlayState::Playing)), None);
    }

    #[test]
    fn steady_ticks_are_not_changes() {
        let mut h = Harness::playing();
        for i in 1..=200 {
            assert_eq!(h.tick(ms(250), ms(250 * i)), None, "tick {i}");
        }
    }

    #[test]
    fn small_drift_is_not_a_change() {
        let mut h = Harness::playing();
        assert_eq!(h.tick(secs(1), ms(1500)), None);
        assert_eq!(h.tick(secs(1), ms(500)), None);
    }

    #[test]
    fn forward_seek_is_a_change() {
        let mut h = Harness::playing();
        for i in 1..=40 {
            h.tick(ms(250), ms(250 * i));
        }
        let view = Harness::shown(h.tick(ms(250), secs(60)));
        assert_eq!(view.position, secs(60));
        assert_eq!(view.state, PlayState::Playing);
        assert_eq!(h.tick(ms(250), ms(60_250)), None);
    }

    #[test]
    fn backward_seek_is_a_change() {
        let mut h = Harness::playing();
        for i in 1..=120 {
            h.tick(ms(250), ms(250 * i));
        }
        let view = Harness::shown(h.tick(ms(250), Duration::ZERO));
        assert_eq!(view.position, Duration::ZERO);
    }

    #[test]
    fn seek_threshold_is_more_than_two_seconds() {
        let mut h = Harness::playing();
        assert_eq!(h.apply(Event::Position(secs(2))), None);
        let view = Harness::shown(h.apply(Event::Position(secs(2) + ms(1))));
        assert_eq!(view.position, secs(2) + ms(1));
    }

    #[test]
    fn accumulated_drift_resyncs() {
        let mut h = Harness::playing();
        // The engine falls 50 ms behind the wall clock per tick.
        for i in 1..=40 {
            assert_eq!(h.tick(ms(250), ms(200 * i)), None, "tick {i}");
        }
        let view = Harness::shown(h.tick(ms(250), ms(200 * 41)));
        assert_eq!(view.position, ms(8200));
        assert_eq!(h.tick(ms(250), ms(8450)), None);
    }

    #[test]
    fn pause_carries_the_extrapolated_position() {
        let mut h = Harness::playing();
        for i in 1..=11 {
            h.tick(ms(250), ms(250 * i));
        }
        h.advance(ms(250));
        let view = Harness::shown(h.apply(Event::State(PlayState::Paused)));
        assert_eq!(view.state, PlayState::Paused);
        assert_eq!(view.position, secs(3));
    }

    #[test]
    fn paused_position_does_not_extrapolate() {
        let mut h = Harness::playing();
        h.tick(secs(3), secs(3));
        h.apply(Event::State(PlayState::Paused)).expect("pause");
        assert_eq!(h.tick(secs(10), secs(3)), None);
        let view = Harness::shown(h.tick(ms(100), secs(6)));
        assert_eq!(view.position, secs(6));
        assert_eq!(view.state, PlayState::Paused);
    }

    #[test]
    fn resume_extrapolates_again() {
        let mut h = Harness::playing();
        h.tick(secs(3), secs(3));
        h.apply(Event::State(PlayState::Paused)).expect("pause");
        h.advance(secs(10));
        let view = Harness::shown(h.apply(Event::State(PlayState::Playing)));
        assert_eq!(view.position, secs(3));
        assert_eq!(h.tick(secs(1), secs(4)), None);
        assert_eq!(h.tick(secs(5), secs(9)), None);
    }

    #[test]
    fn buffering_holds_the_position() {
        let mut h = Harness::playing();
        h.tick(secs(5), secs(5));
        let view = Harness::shown(h.apply(Event::State(PlayState::Buffering)));
        assert_eq!(view.state, PlayState::Buffering);
        assert_eq!(view.position, secs(5));
        assert_eq!(h.tick(secs(5), secs(5)), None);
        let view = Harness::shown(h.apply(Event::State(PlayState::Playing)));
        assert_eq!(view.position, secs(5));
        assert_eq!(h.tick(secs(1), secs(6)), None);
    }

    #[test]
    fn track_change_resets_the_position() {
        let mut h = Harness::playing();
        h.tick(secs(30), secs(30));
        let view = Harness::shown(h.apply(now_playing("B", Some(secs(100)))));
        assert_eq!(view.title, "B");
        assert_eq!(view.position, Duration::ZERO);
        assert_eq!(view.duration, Some(secs(100)));
        assert_eq!(h.apply(Event::State(PlayState::Playing)), None);
        assert_eq!(h.tick(secs(1), secs(1)), None);
    }

    #[test]
    fn same_track_restarting_is_a_change() {
        let mut h = Harness::playing();
        h.tick(secs(30), secs(30));
        let view = Harness::shown(h.apply(now_playing("A", Some(secs(200)))));
        assert_eq!(view.title, "A");
        assert_eq!(view.position, Duration::ZERO);
    }

    #[test]
    fn duration_change_is_a_change() {
        let mut h = Harness::playing();
        h.advance(ms(500));
        let view = Harness::shown(h.apply(now_playing("A", Some(secs(180)))));
        assert_eq!(view.duration, Some(secs(180)));
        h.advance(ms(500));
        let view = Harness::shown(h.apply(now_playing("A", None)));
        assert_eq!(view.duration, None);
    }

    #[test]
    fn stopping_clears_the_view() {
        let mut h = Harness::playing();
        h.tick(secs(5), secs(5));
        assert_eq!(h.apply(Event::State(PlayState::Stopped)), Some(None));
        assert_eq!(h.apply(Event::State(PlayState::Stopped)), None);
        assert_eq!(h.apply(Event::State(PlayState::Playing)), None);
        let view = Harness::shown(h.apply(now_playing("B", Some(secs(100)))));
        assert_eq!(view.title, "B");
        assert_eq!(view.position, Duration::ZERO);
    }

    #[test]
    fn snapshot_of_a_paused_track() {
        let mut h = Harness::new();
        let queue = Event::QueueChanged {
            tracks: vec![track("A")],
            current: Some(0),
        };
        assert_eq!(h.apply(queue), None);
        assert_eq!(h.apply(now_playing("A", Some(secs(200)))), None);
        let view = Harness::shown(h.apply(Event::State(PlayState::Paused)));
        assert_eq!(view.state, PlayState::Paused);
        assert_eq!(view.position, Duration::ZERO);
        assert_eq!(h.apply(Event::Volume(80)), None);
        let view = Harness::shown(h.apply(Event::Position(secs(90))));
        assert_eq!(view.position, secs(90));
    }

    #[test]
    fn position_is_capped_at_the_duration() {
        let mut h = Harness::new();
        h.apply(now_playing("A", Some(secs(10))));
        h.apply(Event::State(PlayState::Playing));
        h.advance(secs(15));
        let view = Harness::shown(h.apply(Event::State(PlayState::Paused)));
        assert_eq!(view.position, secs(10));
    }

    #[test]
    fn apply_uses_the_real_clock() {
        let mut tracker = ViewTracker::default();
        assert_eq!(tracker.apply(&now_playing("A", None)), None);
        let view = tracker
            .apply(&Event::State(PlayState::Playing))
            .flatten()
            .expect("a view");
        assert_eq!(view.title, "A");
        assert_eq!(tracker.apply(&Event::Position(ms(10))), None);
    }

    fn view(title: &str, artists: &str) -> PlayerView {
        PlayerView {
            title: title.into(),
            artists: artists.into(),
            album: None,
            duration: None,
            position: Duration::ZERO,
            state: PlayState::Playing,
        }
    }

    #[test]
    fn menu_title_joins_title_and_artists() {
        assert_eq!(menu_title(&view("Song", "Ann, Bo")), "Song — Ann, Bo");
        assert_eq!(menu_title(&view("Song", "")), "Song");
    }

    #[test]
    fn menu_title_keeps_forty_chars() {
        let title = "x".repeat(40);
        assert_eq!(menu_title(&view(&title, "")), title);
    }

    #[test]
    fn menu_title_truncates_with_an_ellipsis() {
        let shown = menu_title(&view(&"x".repeat(41), ""));
        assert_eq!(shown, format!("{}…", "x".repeat(39)));
        assert_eq!(shown.chars().count(), 40);

        let shown = menu_title(&view("Requiem in D minor, K. 626: Lacrimosa", "Mozart"));
        assert_eq!(shown, "Requiem in D minor, K. 626: Lacrimosa —…");
    }

    #[test]
    fn menu_title_counts_chars_not_bytes() {
        let title = "音".repeat(45);
        let shown = menu_title(&view(&title, ""));
        assert_eq!(shown, format!("{}…", "音".repeat(39)));
    }
}
