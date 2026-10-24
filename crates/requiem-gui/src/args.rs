//! Command-line flags.

use std::path::PathBuf;

pub const USAGE: &str = "\
usage: requiem-gui [--demo] [--smoke] [--snapshot <png>] [--view <view>] [--size <w>x<h>]

  --demo             fill the window with fixed fake data; no player daemon
  --smoke            like --demo, then exit 0 after 2 s
  --snapshot <png>   render the window to a PNG and exit 0. With --demo, from fake data;
                     without it, for home or library, from the running player once the page
                     and its covers have loaded (20 s at most); exits 1 if the page fails
  --view <view>      the view shown at start: home (default), search, library, queue,
                     settings or palette; with --demo also home-scrolled, home-shelf-paged,
                     home-spotify, home-signed-out, collection, now-playing, now-playing-empty,
                     now-playing-last, library-signed-out, library-unavailable, signin,
                     settings-session, empty, disconnected, toast, out-of-date,
                     update-ready, update-failed, not-updatable, up-to-date, search-albums,
                     search-songs, search-artists, search-song-selected,
                     search-card-selected, artist or album
  --size <w>x<h>     the window's content size in points, at least 800x500";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Home,
    Search,
    Library,
    Queue,
    Settings,
    /// The search view with the command palette open.
    Palette,
    /// Demo only: Home scrolled down to its last shelf.
    HomeScrolled,
    /// Demo only: Home with its first shelf of cards paged forward once.
    HomeShelfPaged,
    /// Demo only: Home filtered to Spotify.
    HomeSpotify,
    /// Demo only: Home with both accounts signed out.
    HomeSignedOut,
    /// Demo only: a playlist's tracks, opened from the Library.
    Collection,
    /// Demo only: the playing track's page.
    NowPlaying,
    /// Demo only: the playing track's page with nothing playing.
    NowPlayingEmpty,
    /// Demo only: the playing track's page with the queue's last track playing.
    NowPlayingLast,
    /// Demo only: the Library with both accounts signed out.
    LibrarySignedOut,
    /// Demo only: the Library with YouTube Music unable to sign in.
    LibraryUnavailable,
    /// Demo only: the Library showing a Spotify sign-in code.
    SignIn,
    /// Demo only: Settings with YouTube Music's browser-session form open.
    SettingsSession,
    /// Demo only: the search view before any query.
    Empty,
    /// Demo only: the reconnecting banner.
    Disconnected,
    /// Demo only: an error toast.
    Toast,
    /// Demo only: the stale-daemon banner.
    OutOfDate,
    /// Demo only: the Restart to Update banner.
    UpdateReady,
    /// Demo only: the failed-update banner.
    UpdateFailed,
    /// Demo only: the banner of a copy that can't update itself.
    NotUpdatable,
    /// Demo only: the answer to Check for Updates… when there is nothing new.
    UpToDate,
    /// Demo only: the search view listing albums only.
    SearchAlbums,
    /// Demo only: the search view listing songs only.
    SearchSongs,
    /// Demo only: the search view listing artists only.
    SearchArtists,
    /// Demo only: the top artist's page, opened from the search results.
    Artist,
    /// Demo only: the playing Spotify track's artist's page, opened from Now Playing.
    ArtistSpotify,
    /// Demo only: an album's page, opened from the search results.
    Album,
    /// Demo only: the All view of the search results with a song selected by the keyboard.
    SearchSongSelected,
    /// Demo only: the All view of the search results with an album's card selected by the
    /// keyboard.
    SearchCardSelected,
}

impl View {
    const NAMES: [(&str, View); 34] = [
        ("home", View::Home),
        ("search", View::Search),
        ("library", View::Library),
        ("queue", View::Queue),
        ("settings", View::Settings),
        ("palette", View::Palette),
        ("home-scrolled", View::HomeScrolled),
        ("home-shelf-paged", View::HomeShelfPaged),
        ("home-spotify", View::HomeSpotify),
        ("home-signed-out", View::HomeSignedOut),
        ("collection", View::Collection),
        ("now-playing", View::NowPlaying),
        ("now-playing-empty", View::NowPlayingEmpty),
        ("now-playing-last", View::NowPlayingLast),
        ("library-signed-out", View::LibrarySignedOut),
        ("library-unavailable", View::LibraryUnavailable),
        ("signin", View::SignIn),
        ("settings-session", View::SettingsSession),
        ("empty", View::Empty),
        ("disconnected", View::Disconnected),
        ("toast", View::Toast),
        ("out-of-date", View::OutOfDate),
        ("update-ready", View::UpdateReady),
        ("update-failed", View::UpdateFailed),
        ("not-updatable", View::NotUpdatable),
        ("up-to-date", View::UpToDate),
        ("search-albums", View::SearchAlbums),
        ("search-songs", View::SearchSongs),
        ("search-artists", View::SearchArtists),
        ("artist", View::Artist),
        ("artist-spotify", View::ArtistSpotify),
        ("album", View::Album),
        ("search-song-selected", View::SearchSongSelected),
        ("search-card-selected", View::SearchCardSelected),
    ];

    /// Whether the view is a staged demo state rather than a place in the app.
    pub fn is_demo_only(self) -> bool {
        !matches!(
            self,
            View::Home
                | View::Search
                | View::Library
                | View::Queue
                | View::Settings
                | View::Palette
        )
    }

    /// Whether a snapshot can wait on the running player's answers to show it.
    fn snapshots_live(self) -> bool {
        matches!(self, View::Home | View::Library)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    pub demo: bool,
    pub smoke: bool,
    pub snapshot: Option<PathBuf>,
    pub view: View,
    /// The window's content size, in points.
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    Help,
    Bad(String),
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, ArgsError> {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => out.demo = true,
            "--smoke" => out.smoke = true,
            "--snapshot" => {
                let path = it
                    .next()
                    .ok_or_else(|| ArgsError::Bad("--snapshot needs a path".into()))?;
                out.snapshot = Some(PathBuf::from(path));
            }
            "--view" => {
                let name = it
                    .next()
                    .ok_or_else(|| ArgsError::Bad("--view needs a value".into()))?;
                out.view = View::NAMES
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|&(_, v)| v)
                    .ok_or_else(|| {
                        let names: Vec<_> = View::NAMES.iter().map(|(n, _)| *n).collect();
                        ArgsError::Bad(format!(
                            "unknown view {name:?}; expected one of {}",
                            names.join(", ")
                        ))
                    })?;
            }
            "--size" => {
                let size = it
                    .next()
                    .ok_or_else(|| ArgsError::Bad("--size needs a value".into()))?;
                out.size = Some(parse_size(&size).ok_or_else(|| {
                    ArgsError::Bad(format!(
                        "bad size {size:?}; expected <w>x<h>, at least {MIN_W}x{MIN_H}"
                    ))
                })?);
            }
            "-h" | "--help" => return Err(ArgsError::Help),
            other => return Err(ArgsError::Bad(format!("unknown argument {other:?}"))),
        }
    }
    if out.snapshot.is_some() && !out.demo && !out.view.snapshots_live() {
        return Err(ArgsError::Bad(
            "--snapshot needs --demo, except for --view home or library".into(),
        ));
    }
    if out.view.is_demo_only() && !out.uses_demo_data() {
        return Err(ArgsError::Bad("this --view needs --demo".into()));
    }
    if out.snapshot.is_some() && out.smoke {
        return Err(ArgsError::Bad(
            "--snapshot and --smoke exclude each other".into(),
        ));
    }
    Ok(out)
}

/// The window's smallest content size, in points.
const MIN_W: u32 = 800;
const MIN_H: u32 = 500;

/// `<w>x<h>` in whole points, no smaller than the window allows.
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w >= MIN_W && h >= MIN_H).then_some((w, h))
}

impl Args {
    /// `--smoke` starts like `--demo`.
    pub fn uses_demo_data(&self) -> bool {
        self.demo || self.smoke
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, ArgsError> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn no_args_is_live_home() {
        let a = p(&[]).unwrap();
        assert_eq!(a, Args::default());
        assert_eq!(a.view, View::Home);
        assert!(!a.uses_demo_data());
    }

    #[test]
    fn demo_snapshot_and_view() {
        let a = p(&["--demo", "--snapshot", "/tmp/x.png", "--view", "queue"]).unwrap();
        assert!(a.demo);
        assert_eq!(a.snapshot, Some(PathBuf::from("/tmp/x.png")));
        assert_eq!(a.view, View::Queue);
        assert_eq!(
            p(&["--view", "palette", "--demo"]).unwrap().view,
            View::Palette
        );
        assert_eq!(p(&["--view", "settings"]).unwrap().view, View::Settings);
    }

    #[test]
    fn smoke_uses_demo_data() {
        let a = p(&["--smoke"]).unwrap();
        assert!(a.smoke && a.uses_demo_data());
    }

    #[test]
    fn a_size_is_width_by_height_no_smaller_than_the_window_allows() {
        let a = p(&["--demo", "--size", "2200x1440"]).unwrap();
        assert_eq!(a.size, Some((2200, 1440)));
        assert_eq!(p(&["--demo"]).unwrap().size, None);
        for bad in ["2200", "x1440", "2200x", "799x600", "1000x499", "wide"] {
            assert!(
                matches!(p(&["--size", bad]), Err(ArgsError::Bad(_))),
                "{bad}"
            );
        }
        assert!(matches!(p(&["--size"]), Err(ArgsError::Bad(_))));
    }

    #[test]
    fn errors() {
        assert_eq!(p(&["--help"]), Err(ArgsError::Help));
        assert!(matches!(p(&["--snapshot"]), Err(ArgsError::Bad(_))));
        assert!(matches!(
            p(&["--snapshot", "x.png", "--view", "queue"]),
            Err(ArgsError::Bad(_))
        ));
        assert!(matches!(
            p(&["--demo", "--smoke", "--snapshot", "x.png"]),
            Err(ArgsError::Bad(_))
        ));
        assert!(matches!(p(&["--view", "nope"]), Err(ArgsError::Bad(_))));
        assert!(matches!(p(&["--view"]), Err(ArgsError::Bad(_))));
        assert!(matches!(p(&["--in-process"]), Err(ArgsError::Bad(_))));
        assert!(matches!(p(&["--view", "toast"]), Err(ArgsError::Bad(_))));
    }

    #[test]
    fn home_and_library_snapshot_the_running_player() {
        for (name, view) in [("home", View::Home), ("library", View::Library)] {
            let a = p(&["--snapshot", "x.png", "--view", name]).unwrap();
            assert_eq!((a.view, a.demo), (view, false));
        }
        assert_eq!(
            p(&["--snapshot", "x.png"]).unwrap().view,
            View::Home,
            "home is the default view"
        );
    }

    #[test]
    fn demo_states_need_demo_data() {
        for (name, view) in [
            ("home-scrolled", View::HomeScrolled),
            ("home-shelf-paged", View::HomeShelfPaged),
            ("home-spotify", View::HomeSpotify),
            ("home-signed-out", View::HomeSignedOut),
            ("collection", View::Collection),
            ("now-playing", View::NowPlaying),
            ("now-playing-empty", View::NowPlayingEmpty),
            ("now-playing-last", View::NowPlayingLast),
            ("library-signed-out", View::LibrarySignedOut),
            ("library-unavailable", View::LibraryUnavailable),
            ("signin", View::SignIn),
            ("settings-session", View::SettingsSession),
            ("empty", View::Empty),
            ("disconnected", View::Disconnected),
            ("toast", View::Toast),
            ("out-of-date", View::OutOfDate),
            ("update-ready", View::UpdateReady),
            ("update-failed", View::UpdateFailed),
            ("not-updatable", View::NotUpdatable),
            ("up-to-date", View::UpToDate),
            ("search-albums", View::SearchAlbums),
            ("search-songs", View::SearchSongs),
            ("search-artists", View::SearchArtists),
            ("artist", View::Artist),
            ("artist-spotify", View::ArtistSpotify),
            ("album", View::Album),
            ("search-song-selected", View::SearchSongSelected),
            ("search-card-selected", View::SearchCardSelected),
        ] {
            assert_eq!(p(&["--demo", "--view", name]).unwrap().view, view);
            assert_eq!(p(&["--smoke", "--view", name]).unwrap().view, view);
            assert!(matches!(p(&["--view", name]), Err(ArgsError::Bad(_))));
        }
    }
}
