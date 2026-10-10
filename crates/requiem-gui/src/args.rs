//! Command-line flags.

use std::path::PathBuf;

pub const USAGE: &str = "\
usage: requiem-gui [--demo] [--smoke] [--snapshot <png>] [--view search|queue|palette]

  --demo             fill the window with fixed fake data; no player daemon
  --smoke            like --demo, then exit 0 after 2 s
  --snapshot <png>   with --demo: render the window to a PNG and exit 0
  --view <view>      the view shown at start: search (default), queue or palette";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Search,
    Queue,
    /// The search view with the command palette open.
    Palette,
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
                out.view = match it.next().as_deref() {
                    Some("search") => View::Search,
                    Some("queue") => View::Queue,
                    Some("palette") => View::Palette,
                    Some(other) => {
                        return Err(ArgsError::Bad(format!(
                            "unknown view {other:?}; expected search, queue or palette"
                        )));
                    }
                    None => return Err(ArgsError::Bad("--view needs a value".into())),
                };
            }
            "-h" | "--help" => return Err(ArgsError::Help),
            other => return Err(ArgsError::Bad(format!("unknown argument {other:?}"))),
        }
    }
    if out.snapshot.is_some() && !out.demo {
        return Err(ArgsError::Bad("--snapshot needs --demo".into()));
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
    }
}
