//! Command-line flags.

use std::path::PathBuf;

pub const USAGE: &str = "\
usage: requiem-gui [--demo] [--smoke] [--snapshot <png>] [--view <view>]

  --demo             fill the window with fixed fake data; no player daemon
  --smoke            like --demo, then exit 0 after 2 s
  --snapshot <png>   with --demo: render the window to a PNG and exit 0
  --view <view>      the view shown at start: search (default), queue or palette;
                     with --demo also empty, disconnected, toast or out-of-date";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Search,
    Queue,
    /// The search view with the command palette open.
    Palette,
    /// Demo only: the search view before any query.
    Empty,
    /// Demo only: the reconnecting banner.
    Disconnected,
    /// Demo only: an error toast.
    Toast,
    /// Demo only: the stale-daemon banner.
    OutOfDate,
}

impl View {
    const NAMES: [(&str, View); 7] = [
        ("search", View::Search),
        ("queue", View::Queue),
        ("palette", View::Palette),
        ("empty", View::Empty),
        ("disconnected", View::Disconnected),
        ("toast", View::Toast),
        ("out-of-date", View::OutOfDate),
    ];

    /// Whether the view is a staged demo state rather than a place in the app.
    pub fn is_demo_only(self) -> bool {
        !matches!(self, View::Search | View::Queue | View::Palette)
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
    if out.snapshot.is_some() && !out.demo {
        return Err(ArgsError::Bad("--snapshot needs --demo".into()));
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
    fn no_args_is_live_search() {
        let a = p(&[]).unwrap();
        assert_eq!(a, Args::default());
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
            p(&["--snapshot", "x.png"]),
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
    fn demo_states_need_demo_data() {
        for (name, view) in [
            ("empty", View::Empty),
            ("disconnected", View::Disconnected),
            ("toast", View::Toast),
            ("out-of-date", View::OutOfDate),
        ] {
            assert_eq!(p(&["--demo", "--view", name]).unwrap().view, view);
            assert_eq!(p(&["--smoke", "--view", name]).unwrap().view, view);
            assert!(matches!(p(&["--view", name]), Err(ArgsError::Bad(_))));
        }
    }
}
