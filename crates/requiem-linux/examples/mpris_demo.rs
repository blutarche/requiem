//! Serves a fake paused track over MPRIS and prints the actions it receives, for trying
//! `playerctl -p requiem status|metadata|play-pause|position 30|...` by hand.
//! Needs a session bus. Runs for 60 s, or until Quit.

#[cfg(target_os = "linux")]
fn main() {
    use std::time::Duration;

    use crossbeam_channel::unbounded;
    use requiem_core::{Event, PlayState, ProviderId, Track, TrackId};

    let (tx, rx) = unbounded();
    let Some(desktop) = requiem_linux::start(tx) else {
        eprintln!("mpris_demo: no session bus");
        std::process::exit(1);
    };
    let track = Track {
        id: TrackId {
            provider: ProviderId("ytmusic".into()),
            id: "dQw4w9WgXcQ".into(),
        },
        title: "Demo Song".into(),
        artists: vec!["Demo Artist".into()],
        album: Some("Demo Album".into()),
        duration: Some(Duration::from_secs(213)),
        artwork_url: Some("https://example.test/art.jpg".into()),
    };
    desktop.apply(&Event::QueueChanged {
        tracks: vec![track.clone(), track.clone()],
        current: Some(0),
    });
    desktop.apply(&Event::NowPlaying {
        track,
        duration: Some(Duration::from_secs(213)),
    });
    desktop.apply(&Event::State(PlayState::Paused));
    desktop.apply(&Event::Volume(80));
    desktop.apply(&Event::Position(Duration::from_secs(12)));
    println!("ready");

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while let Ok(action) = rx.recv_deadline(deadline) {
        println!("action: {action:?}");
        if action == requiem_linux::model::Action::Quit {
            break;
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("mpris_demo: Linux only");
}
