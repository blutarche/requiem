//! Shows the status item with a fake track: playing, then paused after 5 s, then exits
//! after 20 s (or `REQUIEM_DEMO_SECS`). Prints every `UiAction` the menu or media keys send.

use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use requiem_core::PlayState;
use requiem_macos::{PlayerView, run};

const PAUSE_AFTER: Duration = Duration::from_secs(5);

fn main() {
    let secs = std::env::var("REQUIEM_DEMO_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let total = Duration::from_secs(secs);

    let (views_tx, views) = unbounded();
    let (actions, actions_rx) = unbounded();
    let (stop_tx, stop) = unbounded();

    thread::spawn(move || {
        for action in actions_rx {
            println!("action: {action:?}");
        }
    });

    thread::spawn(move || {
        let start = Instant::now();
        let mut view = PlayerView {
            title: "Requiem in D minor, K. 626: Lacrimosa".into(),
            artists: "Wolfgang Amadeus Mozart".into(),
            album: Some("Requiem".into()),
            duration: Some(Duration::from_secs(190)),
            position: Duration::ZERO,
            state: PlayState::Playing,
        };
        let _ = views_tx.send(Some(view.clone()));
        if total > PAUSE_AFTER {
            thread::sleep(PAUSE_AFTER);
            view.position = PAUSE_AFTER;
            view.state = PlayState::Paused;
            let _ = views_tx.send(Some(view));
        }
        thread::sleep(total.saturating_sub(start.elapsed()));
        let _ = stop_tx.send(());
    });

    run(views, actions, stop);
    println!("run returned");
}
