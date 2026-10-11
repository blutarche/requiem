//! The tray against a mock StatusNotifierWatcher on a private bus: run with
//! `dbus-run-session -- cargo test`.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use requiem_core::{Event, PlayState, ProviderId, Track, TrackId};
use requiem_linux::model::Action;
use zbus::blocking::proxy::Builder as ProxyBuilder;
use zbus::blocking::{Connection, Proxy, connection::Builder};
use zbus::interface;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

const WAIT: Duration = Duration::from_secs(5);

struct Watcher(Sender<String>);

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        let _ = self.0.send(service.into());
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        Vec::new()
    }
}

/// (icon name, icon pixmaps, title, description)
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

type Props = HashMap<String, OwnedValue>;

struct Node {
    id: i32,
    props: Props,
    children: Vec<Node>,
}

fn node(v: &Value<'_>) -> Node {
    let Value::Structure(s) = v else {
        panic!("layout node is a struct: {v:?}")
    };
    let f = s.fields();
    let Value::I32(id) = &f[0] else { panic!("id") };
    let Value::Dict(props) = &f[1] else {
        panic!("props")
    };
    let Value::Array(kids) = &f[2] else {
        panic!("children")
    };
    Node {
        id: *id,
        props: props.try_clone().unwrap().try_into().unwrap(),
        children: kids
            .iter()
            .map(|k| match k {
                Value::Value(inner) => node(inner),
                other => node(other),
            })
            .collect(),
    }
}

fn str_prop(n: &Node, key: &str) -> Option<String> {
    n.props
        .get(key)
        .and_then(|v| String::try_from(v.clone()).ok())
}

fn bool_prop(n: &Node, key: &str, default: bool) -> bool {
    n.props
        .get(key)
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(default)
}

fn layout(conn: &Connection, service: &str) -> Node {
    let menu = proxy(conn, service, "/MenuBar", "com.canonical.dbusmenu");
    let reply = menu
        .call_method("GetLayout", &(0i32, -1i32, Vec::<String>::new()))
        .unwrap();
    let body = reply.body();
    let (_rev, (id, props, kids)): (u32, (i32, Props, Vec<OwnedValue>)) =
        body.deserialize().unwrap();
    Node {
        id,
        props,
        children: kids.iter().map(|k| node(k)).collect(),
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

fn track() -> Track {
    Track {
        id: TrackId {
            provider: ProviderId("ytmusic".into()),
            id: "abc".into(),
        },
        title: "Song".into(),
        artists: vec!["Ann".into()],
        album: None,
        duration: None,
        artwork_url: None,
    }
}

fn next(rx: &Receiver<Action>) -> Action {
    rx.recv_timeout(WAIT).expect("an action")
}

#[test]
fn tray_registers_and_serves_icon_tooltip_and_menu() {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        eprintln!("skipped: no session bus; run under dbus-run-session");
        return;
    }
    let (reg_tx, reg_rx) = unbounded();
    let _watcher = Builder::session()
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at("/StatusNotifierWatcher", Watcher(reg_tx))
        .unwrap()
        .build()
        .unwrap();

    let (tx, rx) = unbounded();
    let desktop = requiem_linux::start(tx).expect("registered on the private bus");
    let service = reg_rx.recv_timeout(WAIT).expect("item registered");
    assert!(
        service.starts_with("org.kde.StatusNotifierItem-"),
        "{service}"
    );

    let conn = Connection::session().unwrap();
    let item = proxy(
        &conn,
        &service,
        "/StatusNotifierItem",
        "org.kde.StatusNotifierItem",
    );
    assert_eq!(item.get_property::<String>("Title").unwrap(), "requiem");
    assert_eq!(item.get_property::<String>("Id").unwrap(), "requiem");
    assert_eq!(
        item.get_property::<String>("IconName").unwrap(),
        "media-playback-stop"
    );
    assert_eq!(item.get_property::<String>("Status").unwrap(), "Passive");
    assert_eq!(
        item.get_property::<ObjectPath<'_>>("Menu")
            .unwrap()
            .as_str(),
        "/MenuBar"
    );

    let root = layout(&conn, &service);
    let labels: Vec<_> = root
        .children
        .iter()
        .map(|c| (str_prop(c, "label"), str_prop(c, "type")))
        .collect();
    let item_label = |l: &str| (Some(l.to_string()), None);
    assert_eq!(
        labels,
        [
            item_label("Nothing playing"),
            item_label("Play"),
            item_label("Next"),
            item_label("Previous"),
            (None, Some("separator".into())),
            item_label("Quit"),
        ]
    );
    assert!(!bool_prop(&root.children[0], "enabled", true));
    assert!(bool_prop(&root.children[1], "enabled", true));

    desktop.apply(&Event::NowPlaying {
        track: track(),
        duration: None,
    });
    desktop.apply(&Event::State(PlayState::Playing));
    // The tray applies updates on its own thread; wait for the icon to follow.
    let mut icon = String::new();
    for _ in 0..50 {
        icon = item.get_property("IconName").unwrap();
        if icon == "media-playback-start" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(icon, "media-playback-start");
    assert_eq!(item.get_property::<String>("Status").unwrap(), "Active");
    let tip: ToolTip = item.get_property("ToolTip").unwrap();
    assert_eq!(tip.2, "Song — Ann");
    let root = layout(&conn, &service);
    assert_eq!(str_prop(&root.children[0], "label").unwrap(), "Song — Ann");
    assert_eq!(str_prop(&root.children[1], "label").unwrap(), "Pause");

    desktop.apply(&Event::State(PlayState::Paused));
    for _ in 0..50 {
        icon = item.get_property("IconName").unwrap();
        if icon == "media-playback-pause" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(icon, "media-playback-pause");
    assert_eq!(item.get_property::<String>("Status").unwrap(), "Active");

    item.call_method("Activate", &(0i32, 0i32)).unwrap();
    assert_eq!(next(&rx), Action::Toggle);

    let menu = Proxy::new(
        &conn,
        service.as_str(),
        "/MenuBar",
        "com.canonical.dbusmenu",
    )
    .unwrap();
    for (child, want) in [
        (1, Action::Toggle),
        (2, Action::Next),
        (3, Action::Prev),
        (5, Action::Quit),
    ] {
        let id = layout(&conn, &service).children[child].id;
        menu.call_method("Event", &(id, "clicked", Value::from(0i32), 0u32))
            .unwrap();
        assert_eq!(next(&rx), want);
    }

    drop(desktop);
    assert!(
        rx.recv_timeout(Duration::from_secs(5))
            .is_err_and(|e| e.is_disconnected()),
        "dropping the desktop releases every action sender"
    );
}
