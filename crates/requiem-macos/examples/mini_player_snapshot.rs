//! Renders the mini player to PNGs without showing it: playing with made-up artwork, and
//! empty, each in the dark and light appearances. Writes into the directory given as the
//! only argument.

#[cfg(target_os = "macos")]
fn main() {
    use std::path::PathBuf;
    use std::time::Duration;

    use objc2::MainThreadMarker;
    use requiem_core::PlayState;
    use requiem_macos::{PlayerView, artwork, mini_player_snapshot};

    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: mini_player_snapshot <dir>"),
    );
    let mtm = MainThreadMarker::new().expect("main thread");
    artwork::insert(mtm, "demo:mini", &artwork::demo_cover(0, 268.0));
    let view = PlayerView {
        title: "Requiem in D minor, K. 626: Lacrimosa".into(),
        artists: "Wolfgang Amadeus Mozart".into(),
        album: Some("Requiem".into()),
        duration: Some(Duration::from_secs(190)),
        position: Duration::from_secs(83),
        state: PlayState::Playing,
        artwork_url: Some("demo:mini".into()),
    };
    let mut no_art = view.clone();
    no_art.title = "Lux aeterna".into();
    no_art.state = PlayState::Paused;
    no_art.artwork_url = None;
    for (name, view) in [
        ("playing", Some(&view)),
        ("paused-no-art", Some(&no_art)),
        ("empty", None),
    ] {
        for (look, dark) in [("dark", true), ("light", false)] {
            let path = dir.join(format!("mini-{name}-{look}.png"));
            match mini_player_snapshot(view, 64, dark, &path) {
                Ok(()) => println!("{}", path.display()),
                Err(e) => {
                    eprintln!("{}: {e}", path.display());
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
