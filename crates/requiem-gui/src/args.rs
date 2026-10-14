//! Command-line flags.

use std::path::PathBuf;

pub const USAGE: &str = "\
usage: requiem-gui [--demo] [--smoke] [--snapshot <png>] [--view <view>]

  --demo             fill the window with fixed fake data; no player daemon
  --smoke            like --demo, then exit 0 after 2 s
  --snapshot <png>   render the window to a PNG and exit 0. With --demo, from fake data;
                     without it, for home or library, from the running player once the page
                     and its covers have loaded (20 s at most); exits 1 if the page fails
  --view <view>      the view shown at start: home (default), search, library, queue or
                     palette; with --demo also home-scrolled, home-shelf-paged, collection,
                     now-playing, now-playing-empty, now-playing-last, library-signed-out,
                     library-unavailable, signin, empty, disconnected, toast, out-of-date,
                     update-ready, update-failed, not-updatable or up-to-date";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Home,
    Search,
    Library,
    Queue,
    /// The search view with the command palette open.
    Palette,
    /// Demo only: Home scrolled down to its last shelf.
    HomeScrolled,
    /// Demo only: Home with its first shelf of cards paged forward once.
    HomeShelfPaged,
    /// Demo only: a playlist's tracks, opened from the Library.
    Collection,
    /// Demo only: the playing track's page.
    NowPlaying,
    /// Demo only: the playing track's page with nothing playing.
    NowPlayingEmpty,
    /// Demo only: the playing track's page with the queue's last track playing.
    NowPlayingLast,
    /// Demo only: the Library with the account signed out.
    LibrarySignedOut,
    /// Demo only: the Library with no Google client set up.
    LibraryUnavailable,
    /// Demo only: the Library showing a sign-in code.
    SignIn,
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
}

impl View {
    const NAMES: [(&str, View); 22] = [
        ("home", View::Home),
        ("search", View::Search),
        ("library", View::Library),
        ("queue", View::Queue),
        ("palette", View::Palette),
        ("home-scrolled", View::HomeScrolled),
        ("home-shelf-paged", View::HomeShelfPaged),
        ("collection", View::Collection),
        ("now-playing", View::NowPlaying),
        ("now-playing-empty", View::NowPlayingEmpty),
        ("now-playing-last", View::NowPlayingLast),
        ("library-signed-out", View::LibrarySignedOut),
        ("library-unavailable", View::LibraryUnavailable),
        ("signin", View::SignIn),
        ("empty", View::Empty),
        ("disconnected", View::Disconnected),
        ("toast", View::Toast),
        ("out-of-date", View::OutOfDate),
        ("update-ready", View::UpdateReady),
        ("update-failed", View::UpdateFailed),
        ("not-updatable", View::NotUpdatable),
        ("up-to-date", View::UpToDate),
    ];

    /// Whether the view is a staged demo state rather than a place in the app.
    pub fn is_demo_only(self) -> bool {
        !matches!(
            self,
            View::Home | View::Search | View::Library | View::Queue | View::Palette
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
    }

    #[test]
    fn smoke_uses_demo_data() {
        let a = p(&["--smoke"]).unwrap();
        assert!(a.smoke && a.uses_demo_data());
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
            ("collection", View::Collection),
            ("now-playing", View::NowPlaying),
            ("now-playing-empty", View::NowPlayingEmpty),
            ("now-playing-last", View::NowPlayingLast),
            ("library-signed-out", View::LibrarySignedOut),
            ("library-unavailable", View::LibraryUnavailable),
            ("signin", View::SignIn),
            ("empty", View::Empty),
            ("disconnected", View::Disconnected),
            ("toast", View::Toast),
            ("out-of-date", View::OutOfDate),
            ("update-ready", View::UpdateReady),
            ("update-failed", View::UpdateFailed),
            ("not-updatable", View::NotUpdatable),
            ("up-to-date", View::UpToDate),
        ] {
            assert_eq!(p(&["--demo", "--view", name]).unwrap().view, view);
            assert_eq!(p(&["--smoke", "--view", name]).unwrap().view, view);
            assert!(matches!(p(&["--view", name]), Err(ArgsError::Bad(_))));
        }
    }
}
