//! MPRIS2 on the session bus: `org.mpris.MediaPlayer2.requiem`.
//!
//! Interface methods run on zbus's executor thread and only queue an [`Action`]; they never
//! wait for the session. State is published from the event thread through [`Mpris::publish`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crossbeam_channel::Sender;
use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface};

use crate::model::{
    Action, Changed, NO_TRACK, Snapshot, from_micros, micros, playback_status, seek_target,
    volume_from_mpris, volume_to_mpris,
};

pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.requiem";
pub const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

pub type Shared = Arc<Mutex<Snapshot>>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, Snapshot> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Root {
    actions: Sender<Action>,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn quit(&self) {
        let _ = self.actions.send(Action::Quit);
    }

    fn raise(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "requiem"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    shared: Shared,
    actions: Sender<Action>,
}

impl Player {
    fn send(&self, action: Action) {
        let _ = self.actions.send(action);
    }

    /// Queues an absolute seek and moves the stored position to it, so a `Position` read
    /// right after `Seeked` is not stale. The next `Position` event corrects it if the
    /// session clamped the target.
    fn seek_to(&self, to: std::time::Duration) {
        lock(&self.shared).position = to;
        self.send(Action::Seek(to));
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        self.send(Action::Next);
    }

    fn previous(&self) {
        self.send(Action::Prev);
    }

    fn pause(&self) {
        self.send(Action::Pause);
    }

    fn play_pause(&self) {
        self.send(Action::Toggle);
    }

    /// The session has no command that stops while keeping the queue and the loaded track,
    /// which Play must be able to resume, so Stop pauses.
    fn stop(&self) {
        self.send(Action::Pause);
    }

    fn play(&self) {
        self.send(Action::Resume);
    }

    async fn seek(
        &self,
        offset: i64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let to = {
            let snap = lock(&self.shared);
            if snap.track.is_none() {
                return Ok(());
            }
            seek_target(snap.position, offset)
        };
        self.seek_to(to);
        Ok(Self::seeked(&emitter, micros(to)).await?)
    }

    async fn set_position(
        &self,
        track_id: ObjectPath<'_>,
        position: i64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let to = from_micros(position);
        {
            let snap = lock(&self.shared);
            let current = snap.track_path();
            let in_range = position >= 0 && snap.duration.is_none_or(|d| to <= d);
            if current.as_deref() != Some(track_id.as_str()) || !in_range {
                return Ok(());
            }
        }
        self.seek_to(to);
        Ok(Self::seeked(&emitter, position).await?)
    }

    fn open_uri(&self, _uri: &str) {}

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> &'static str {
        playback_status(lock(&self.shared).state)
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        metadata(&lock(&self.shared))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn volume(&self) -> f64 {
        volume_to_mpris(lock(&self.shared).volume)
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        self.send(Action::SetVolume(volume_from_mpris(volume)));
    }

    /// Never announced: clients read it on demand, and announcing it would wake them every tick.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        micros(lock(&self.shared).position)
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        lock(&self.shared).can_go_next()
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        lock(&self.shared).can_go_previous()
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

/// The `Metadata` map for the snapshot's track; empty when nothing is loaded.
pub fn metadata(snap: &Snapshot) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    let Some(track) = &snap.track else {
        return map;
    };
    let path = snap.track_path().unwrap_or_else(|| NO_TRACK.into());
    if let Ok(path) = ObjectPath::try_from(path) {
        put(&mut map, "mpris:trackid", path);
    }
    if let Some(d) = snap.duration {
        put(&mut map, "mpris:length", micros(d));
    }
    put(&mut map, "xesam:title", track.title.as_str());
    put(&mut map, "xesam:artist", track.artists.clone());
    if let Some(album) = &track.album {
        put(&mut map, "xesam:album", album.as_str());
    }
    if let Some(url) = &track.artwork_url {
        put(&mut map, "mpris:artUrl", url.as_str());
    }
    map
}

fn put<'a>(map: &mut HashMap<String, OwnedValue>, key: &str, value: impl Into<Value<'a>>) {
    // Only values holding file descriptors fail to convert; none of these do.
    if let Ok(v) = value.into().try_to_owned() {
        map.insert(key.into(), v);
    }
}

/// A claimed bus name with the root and player objects served on it.
pub struct Mpris {
    conn: Connection,
}

impl Mpris {
    pub fn start(shared: Shared, actions: Sender<Action>) -> zbus::Result<Self> {
        let conn = Builder::session()?
            .name(BUS_NAME)?
            .serve_at(
                PATH,
                Root {
                    actions: actions.clone(),
                },
            )?
            .serve_at(PATH, Player { shared, actions })?
            .build()?;
        Ok(Self { conn })
    }

    /// Announces the properties `changed` flags, with values taken from `snap`.
    pub fn publish(&self, snap: &Snapshot, changed: Changed) {
        let mut props: HashMap<&str, Value<'_>> = HashMap::new();
        if changed.status {
            props.insert("PlaybackStatus", playback_status(snap.state).into());
        }
        if changed.metadata {
            props.insert("Metadata", metadata(snap).into());
        }
        if changed.volume {
            props.insert("Volume", volume_to_mpris(snap.volume).into());
        }
        if changed.can_go {
            props.insert("CanGoNext", snap.can_go_next().into());
            props.insert("CanGoPrevious", snap.can_go_previous().into());
        }
        if props.is_empty() {
            return;
        }
        let invalidated: Vec<&str> = Vec::new();
        let _ = self.conn.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(PLAYER_IFACE, props, invalidated),
        );
    }
}

impl Drop for Mpris {
    /// Drops the interfaces, and with them their action senders, so the daemon's action
    /// thread can end.
    fn drop(&mut self) {
        let server = self.conn.object_server();
        let _ = server.remove::<Root, _>(PATH);
        let _ = server.remove::<Player, _>(PATH);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use requiem_core::{Event, ProviderId, Track, TrackId};

    use super::*;

    fn snap_with(track: Track, duration: Option<Duration>) -> Snapshot {
        let mut s = Snapshot::default();
        s.apply(&Event::NowPlaying { track, duration });
        s
    }

    fn track() -> Track {
        Track {
            id: TrackId {
                provider: ProviderId("ytmusic".into()),
                id: "ab-c".into(),
            },
            title: "Song".into(),
            artists: vec!["Ann".into(), "Bo".into()],
            album: Some("Album".into()),
            duration: None,
            artwork_url: Some("https://example.test/a.jpg".into()),
        }
    }

    #[test]
    fn metadata_has_every_field_with_its_type() {
        let m = metadata(&snap_with(track(), Some(Duration::from_millis(2500))));
        let path = ObjectPath::try_from(m["mpris:trackid"].clone()).unwrap();
        assert_eq!(path.as_str(), "/org/requiem/track/ytmusic/ab_2dc");
        assert_eq!(i64::try_from(m["mpris:length"].clone()).unwrap(), 2_500_000);
        assert_eq!(String::try_from(m["xesam:title"].clone()).unwrap(), "Song");
        assert_eq!(
            Vec::<String>::try_from(m["xesam:artist"].clone()).unwrap(),
            ["Ann", "Bo"]
        );
        assert_eq!(String::try_from(m["xesam:album"].clone()).unwrap(), "Album");
        assert_eq!(
            String::try_from(m["mpris:artUrl"].clone()).unwrap(),
            "https://example.test/a.jpg"
        );
        assert_eq!(m.len(), 6);
    }

    #[test]
    fn metadata_omits_what_is_unknown() {
        let mut t = track();
        t.album = None;
        t.artwork_url = None;
        let m = metadata(&snap_with(t, None));
        assert!(!m.contains_key("mpris:length"));
        assert!(!m.contains_key("xesam:album"));
        assert!(!m.contains_key("mpris:artUrl"));
        assert!(m.contains_key("mpris:trackid"));
    }

    #[test]
    fn metadata_is_empty_without_a_track() {
        assert!(metadata(&Snapshot::default()).is_empty());
    }
}
