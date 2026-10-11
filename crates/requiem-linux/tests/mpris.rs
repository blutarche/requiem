//! MPRIS against a private session bus: run with `dbus-run-session -- cargo test`.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use requiem_core::{Event, PlayState, ProviderId, Track, TrackId};
use requiem_linux::model::Action;
use zbus::blocking::proxy::Builder as ProxyBuilder;
use zbus::blocking::{Connection, Proxy, fdo::PropertiesProxy};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedValue};

const BUS: &str = "org.mpris.MediaPlayer2.requiem";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const WAIT: Duration = Duration::from_secs(5);

fn track() -> Track {
    Track {
        id: TrackId {
            provider: ProviderId("ytmusic".into()),
            id: "ab-c".into(),
        },
        title: "Song".into(),
        artists: vec!["Ann".into(), "Bo".into()],
        album: Some("Album".into()),
        duration: Some(Duration::from_secs(200)),
        artwork_url: Some("https://example.test/a.jpg".into()),
    }
}

/// Uncached, so reads see the service's current value rather than the last signal.
fn proxy(conn: &Connection, dest: &str, path: &'static str, iface: &'static str) -> Proxy<'static> {
    ProxyBuilder::new(conn)
        .destination(dest.to_owned())
        .unwrap()
        .path(path)
        .unwrap()
        .interface(iface)
        .unwrap()
        .cache_properties(CacheProperties::No)
        .build()
        .unwrap()
}

fn next(rx: &Receiver<Action>) -> Action {
    rx.recv_timeout(WAIT).expect("an action")
}

#[test]
fn mpris_object_serves_state_and_routes_actions() {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        eprintln!("skipped: no session bus; run under dbus-run-session");
        return;
    }
    let (tx, rx) = unbounded();
    let desktop = requiem_linux::start(tx).expect("registered on the private bus");

    let conn = Connection::session().unwrap();
    let root = proxy(&conn, BUS, PATH, "org.mpris.MediaPlayer2");
    let player = proxy(&conn, BUS, PATH, PLAYER);

    assert_eq!(root.get_property::<String>("Identity").unwrap(), "requiem");
    assert!(root.get_property::<bool>("CanQuit").unwrap());
    assert!(!root.get_property::<bool>("CanRaise").unwrap());
    assert!(!root.get_property::<bool>("HasTrackList").unwrap());

    assert_eq!(
        player.get_property::<String>("PlaybackStatus").unwrap(),
        "Stopped"
    );
    assert!(
        player
            .get_property::<HashMap<String, OwnedValue>>("Metadata")
            .unwrap()
            .is_empty()
    );
    for rate in ["Rate", "MinimumRate", "MaximumRate"] {
        assert_eq!(player.get_property::<f64>(rate).unwrap(), 1.0);
    }
    for can in ["CanPlay", "CanPause", "CanSeek", "CanControl"] {
        assert!(player.get_property::<bool>(can).unwrap(), "{can}");
    }
    assert!(!player.get_property::<bool>("CanGoNext").unwrap());

    let props = PropertiesProxy::builder(&conn)
        .destination(BUS)
        .unwrap()
        .path(PATH)
        .unwrap()
        .build()
        .unwrap();
    let mut changes = props.receive_properties_changed().unwrap();

    desktop.apply(&Event::QueueChanged {
        tracks: vec![track(), track()],
        current: Some(0),
    });
    desktop.apply(&Event::NowPlaying {
        track: track(),
        duration: Some(Duration::from_secs(200)),
    });
    desktop.apply(&Event::State(PlayState::Playing));
    desktop.apply(&Event::Volume(30));
    desktop.apply(&Event::Position(Duration::from_secs(10)));
    desktop.apply(&Event::State(PlayState::Paused));
    desktop.apply(&Event::State(PlayState::Playing));

    assert_eq!(
        player.get_property::<String>("PlaybackStatus").unwrap(),
        "Playing"
    );
    assert!(player.get_property::<bool>("CanGoNext").unwrap());
    assert!(!player.get_property::<bool>("CanGoPrevious").unwrap());
    assert_eq!(player.get_property::<f64>("Volume").unwrap(), 0.3);
    assert_eq!(player.get_property::<i64>("Position").unwrap(), 10_000_000);

    let meta: HashMap<String, OwnedValue> = player.get_property("Metadata").unwrap();
    let id = ObjectPath::try_from(meta["mpris:trackid"].clone()).unwrap();
    assert_eq!(id.as_str(), "/org/requiem/track/ytmusic/ab_2dc");
    assert_eq!(
        i64::try_from(meta["mpris:length"].clone()).unwrap(),
        200_000_000
    );
    assert_eq!(
        String::try_from(meta["xesam:title"].clone()).unwrap(),
        "Song"
    );
    assert_eq!(
        Vec::<String>::try_from(meta["xesam:artist"].clone()).unwrap(),
        ["Ann", "Bo"]
    );
    assert_eq!(
        String::try_from(meta["xesam:album"].clone()).unwrap(),
        "Album"
    );
    assert_eq!(
        String::try_from(meta["mpris:artUrl"].clone()).unwrap(),
        "https://example.test/a.jpg"
    );

    // Everything that changed was announced, and Position never was.
    let mut announced: Vec<String> = Vec::new();
    while announced.iter().filter(|k| *k == "PlaybackStatus").count() < 3 {
        let sig = changes.next().expect("PropertiesChanged");
        let args = sig.args().unwrap();
        if args.interface_name().as_str() != PLAYER {
            continue;
        }
        announced.extend(args.changed_properties().keys().map(|k| k.to_string()));
    }
    for key in ["PlaybackStatus", "Metadata", "Volume", "CanGoNext"] {
        assert!(announced.iter().any(|k| k == key), "{key} in {announced:?}");
    }
    assert!(!announced.iter().any(|k| k == "Position"), "{announced:?}");

    player.call_method("PlayPause", &()).unwrap();
    assert_eq!(next(&rx), Action::Toggle);
    player.call_method("Play", &()).unwrap();
    assert_eq!(next(&rx), Action::Resume);
    player.call_method("Pause", &()).unwrap();
    assert_eq!(next(&rx), Action::Pause);
    player.call_method("Stop", &()).unwrap();
    assert_eq!(next(&rx), Action::Pause);
    player.call_method("Next", &()).unwrap();
    assert_eq!(next(&rx), Action::Next);
    player.call_method("Previous", &()).unwrap();
    assert_eq!(next(&rx), Action::Prev);

    let mut seeked = player.receive_signal("Seeked").unwrap();
    player.call_method("Seek", &(5_000_000i64,)).unwrap();
    assert_eq!(next(&rx), Action::Seek(Duration::from_secs(15)));
    let pos: i64 = seeked.next().unwrap().body().deserialize().unwrap();
    assert_eq!(pos, 15_000_000);
    assert_eq!(player.get_property::<i64>("Position").unwrap(), 15_000_000);
    player.call_method("Seek", &(-60_000_000i64,)).unwrap();
    assert_eq!(next(&rx), Action::Seek(Duration::ZERO));

    let other = ObjectPath::try_from("/org/requiem/track/ytmusic/other").unwrap();
    player
        .call_method("SetPosition", &(&other, 1_000_000i64))
        .unwrap();
    player.call_method("SetPosition", &(&id, -1i64)).unwrap();
    player
        .call_method("SetPosition", &(&id, 900_000_000i64))
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "mismatched or out-of-range seeks are ignored"
    );
    player
        .call_method("SetPosition", &(&id, 42_000_000i64))
        .unwrap();
    assert_eq!(next(&rx), Action::Seek(Duration::from_secs(42)));

    player.set_property("Volume", 0.5f64).unwrap();
    assert_eq!(next(&rx), Action::SetVolume(50));
    player.set_property("Volume", 7.0f64).unwrap();
    assert_eq!(next(&rx), Action::SetVolume(100));

    root.call_method("Quit", &()).unwrap();
    assert_eq!(next(&rx), Action::Quit);

    drop(desktop);
    assert!(
        rx.recv_timeout(Duration::from_secs(5))
            .is_err_and(|e| e.is_disconnected()),
        "dropping the desktop releases every action sender"
    );
}
