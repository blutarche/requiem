//! `--demo`: fixed fake data and a stand-in for the daemon, so screenshots are deterministic.
//! All names are made up.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use requiem_core::{
    AccountState, Collection, CollectionId, CollectionKind, Command, EnqueueMode, Event,
    HOME_LIKED, HOME_MIXES, HOME_PLAYLISTS, Item, Page, PlayState, ProviderId, SearchResults,
    SeekTarget, Shelf, Track, TrackId,
};
use requiem_ipc::{UpdateState, UpdateStatus};

use crate::args::View;
use crate::vm::{
    BANNER_GRACE, Intent, Link, LinkError, Place, SearchChip, Service, ServiceFilter, Vm,
};

pub const QUERY: &str = "night drive";
/// The demo player's version, and the one its update offers.
const VERSION: &str = "0.2.0";
const NEXT_VERSION: &str = "0.2.1";

const RESULTS: [(&str, &[&str], Option<&str>, u64); 30] = [
    (
        "Night Drive Home",
        &["Hollow Coast"],
        Some("Low Tide Radio"),
        225,
    ),
    ("Paper Lanterns", &["Mira Vance"], Some("Glasshouse"), 242),
    (
        "Slow Satellites",
        &["The Lantern Choir"],
        Some("Orbit Hymns"),
        198,
    ),
    (
        "Neon Rain, Empty Streets",
        &["Velvet Static", "Noor Calloway"],
        Some("After Hours Transit"),
        267,
    ),
    (
        "Harbor Lights",
        &["Paper Harbors"],
        Some("Harbor Lights"),
        231,
    ),
    (
        "Fever in the Valley",
        &["Saint Ember"],
        Some("Wildfire Season"),
        178,
    ),
    (
        "Quiet Machines",
        &["Cobalt Garden"],
        Some("Signal & Noise"),
        312,
    ),
    (
        "Late Train to Marlow",
        &["Elias Wren"],
        Some("Small Hours"),
        213,
    ),
    (
        "Gold Static",
        &["Velvet Static"],
        Some("After Hours Transit"),
        187,
    ),
    (
        "Midnight Orchard",
        &["Juniper & Ash"],
        Some("Second Bloom"),
        255,
    ),
    (
        "The Long Way Round",
        &["Tamsin Hale"],
        Some("Northern Letters"),
        229,
    ),
    ("Undertow", &["Kestrel Bay"], Some("Undertow"), 280),
    (
        "Glass Coast",
        &["Hollow Coast"],
        Some("Low Tide Radio"),
        206,
    ),
    (
        "Starlight Motel",
        &["Northbound Echo"],
        Some("Route Nine"),
        238,
    ),
    ("Ribbons", &["Mira Vance"], Some("Glasshouse"), 167),
    (
        "Afterglow (Night Drive Edit)",
        &["Saint Ember", "Cobalt Garden"],
        None,
        201,
    ),
    ("Small Fires", &["Noor Calloway"], Some("Embers & Ink"), 249),
    (
        "Velvet Hours",
        &["The Lantern Choir"],
        Some("Orbit Hymns"),
        301,
    ),
    (
        "Driftwood Heart",
        &["Juniper & Ash"],
        Some("Second Bloom"),
        217,
    ),
    (
        "Signal Lost",
        &["Cobalt Garden"],
        Some("Signal & Noise"),
        262,
    ),
    (
        "Moonlit Interstate",
        &["Northbound Echo"],
        Some("Route Nine"),
        235,
    ),
    (
        "Silver Linings Club",
        &["Paper Harbors"],
        Some("Postcards"),
        194,
    ),
    (
        "Hometown Ghosts",
        &["Tamsin Hale"],
        Some("Northern Letters"),
        273,
    ),
    (
        "Ocean Avenue Blue",
        &["Kestrel Bay"],
        Some("Saltwater Diaries"),
        222,
    ),
    ("Still Awake", &["Elias Wren"], Some("Small Hours"), 175),
    (
        "Highway Psalm",
        &["The Lantern Choir", "Elias Wren"],
        Some("Orbit Hymns"),
        288,
    ),
    (
        "Paper Planes at Dawn",
        &["Mira Vance"],
        Some("Glasshouse"),
        209,
    ),
    (
        "Neon Cathedral",
        &["Velvet Static"],
        Some("After Hours Transit"),
        364,
    ),
    ("Window Seat", &["Noor Calloway"], Some("Embers & Ink"), 196),
    (
        "Last Exit Lullaby",
        &["Hollow Coast"],
        Some("Low Tide Radio"),
        291,
    ),
];

/// Indices into `RESULTS`; the current entry (index 2) is the 3:45 track.
const QUEUE: [usize; 8] = [4, 9, 0, 13, 6, 20, 10, 23];
const CURRENT: usize = 2;
const POSITION: Duration = Duration::from_secs(83);
const VOLUME: u8 = 70;
/// The result whose playback fails in the toast demo.
const FAILED: usize = 5;

/// Made-up artwork for result `i`; every fourth has none, to show the placeholder.
pub fn artwork_url(i: usize) -> Option<String> {
    (i % 4 != 3).then(|| format!("demo:{i:02}"))
}

const MIXES: [&str; 8] = [
    "Late Night Drive",
    "Rainy Day Focus",
    "Songs to Drive Through the Rain at Midnight With the Windows Down",
    "Sunday Slow Down",
    "Road Trip Echoes",
    "Deep Work",
    "Neon Evenings",
    "Quiet Hours",
];

/// The albums a search finds: title, artist, type and year.
const SEARCH_ALBUMS: [(&str, &str, &str, u16); 8] = [
    ("Low Tide Radio", "Hollow Coast", "Album", 2019),
    ("Night Drive", "Northbound Echo", "Single", 2024),
    ("After Hours Transit", "Velvet Static", "Album", 2021),
    ("Glasshouse", "Mira Vance", "Album", 2022),
    ("Orbit Hymns", "The Lantern Choir", "Album", 2018),
    ("Route Nine", "Northbound Echo", "EP", 2023),
    ("Signal & Noise", "Cobalt Garden", "Album", 2020),
    ("Small Hours", "Elias Wren", "Album", 2017),
];

/// The playlists a search finds: title and song count.
const SEARCH_PLAYLISTS: [(&str, usize); 6] = [
    ("Night Drive Essentials", 48),
    ("Midnight Highway", 72),
    ("Synthwave Night Drive", 35),
    (
        "Driving Through the City Lights After Everyone Else Has Gone Home",
        120,
    ),
    ("Late Night Radio", 61),
    ("Slow Roads", 27),
];

const LIBRARY_ADJECTIVES: [&str; 5] = ["Golden", "Quiet", "Midnight", "Early", "Slow"];
const LIBRARY_NOUNS: [&str; 4] = ["Mornings", "Roads", "Rooms", "Waves"];

/// The first demo cover URL index of each group; every URL is `demo:NN`.
const HOME_TRACK_ART: usize = 30;
const HOME_PLAYLIST_ART: usize = 42;
const HOME_MIX_ART: usize = 50;
const LIBRARY_ART: usize = 60;
const LIBRARY_LEN: usize = 20;
const SEARCH_ALBUM_ART: usize = LIBRARY_ART + LIBRARY_LEN;
const SEARCH_PLAYLIST_ART: usize = SEARCH_ALBUM_ART + SEARCH_ALBUMS.len();
const COLLECTION_LEN: usize = 15;

fn demo_url(i: usize) -> String {
    format!("demo:{i:02}")
}

/// Every demo cover URL with the seed it is drawn from, for the app to cache.
pub fn covers() -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = (0..RESULTS.len())
        .filter_map(|i| artwork_url(i).map(|u| (u, i)))
        .collect();
    let end = SEARCH_PLAYLIST_ART + SEARCH_PLAYLISTS.len();
    out.extend((HOME_TRACK_ART..end).map(|i| (demo_url(i), i)));
    out
}

/// Spotify has 6 of the results and 8 of the Library's entries, which the merged lists take
/// in turn with YouTube Music's, Spotify first: the even ones of the first this many.
const MIXED_RESULTS: usize = 12;
const MIXED_LIBRARY: usize = 16;

fn spotify() -> ProviderId {
    ProviderId("spotify".into())
}

fn ytmusic() -> ProviderId {
    ProviderId("ytmusic".into())
}

/// The provider of entry `i` of a merged list whose first `mixed` entries alternate.
fn taken_in_turn(i: usize, mixed: usize) -> ProviderId {
    if i < mixed && i.is_multiple_of(2) {
        spotify()
    } else {
        ytmusic()
    }
}

/// Home's shelves alternate throughout, as both services have them.
fn collection_id(kind: &str, i: usize) -> CollectionId {
    CollectionId {
        provider: taken_in_turn(i, usize::MAX),
        id: format!("demo-{kind}-{i:02}"),
    }
}

fn home_tracks() -> Vec<Track> {
    let mut tracks = results();
    tracks.drain(..12);
    tracks.truncate(12);
    for (k, t) in tracks.iter_mut().enumerate() {
        t.id.provider = taken_in_turn(k, usize::MAX);
        t.artwork_url = (k % 5 != 4).then(|| demo_url(HOME_TRACK_ART + k));
    }
    tracks
}

/// Home as both accounts merge into it: Liked songs, Mixes and Your playlists, which both
/// services have, with their items alternating, Spotify's first. Then the shelves only one
/// service has, taken in turn by service. The Liked cards have no cover, as neither service
/// gives them one.
pub fn home() -> Vec<Shelf> {
    let playlists = ["Liked Songs", "Liked videos"]
        .into_iter()
        .map(|t| (t.to_owned(), None, None))
        .chain((2..8).map(|i| {
            (
                format!(
                    "{} {}",
                    LIBRARY_ADJECTIVES[i % LIBRARY_ADJECTIVES.len()],
                    LIBRARY_NOUNS[i % LIBRARY_NOUNS.len()]
                ),
                Some(format!("Playlist · {} songs", 9 + 4 * i)),
                Some(demo_url(HOME_PLAYLIST_ART + i)),
            )
        }))
        .enumerate()
        .map(|(i, (title, subtitle, artwork_url))| {
            Item::Collection(Collection {
                id: collection_id("playlist", i),
                kind: CollectionKind::Playlist,
                title,
                subtitle,
                artwork_url,
            })
        })
        .collect();
    let mixes = MIXES
        .iter()
        .enumerate()
        .map(|(i, title)| {
            Item::Collection(Collection {
                id: collection_id("mix", i),
                kind: CollectionKind::Playlist,
                title: (*title).to_owned(),
                subtitle: Some(format!("Playlist · {} songs", 12 + 3 * i)),
                artwork_url: (i != 5).then(|| demo_url(HOME_MIX_ART + i)),
            })
        })
        .collect();
    let one_service = |title: &str, service: ProviderId| Shelf {
        title: title.into(),
        items: results()
            .into_iter()
            .filter(|t| t.id.provider == service)
            .skip(2)
            .take(8)
            .map(Item::Track)
            .collect(),
    };
    vec![
        Shelf {
            title: HOME_LIKED.into(),
            items: home_tracks().into_iter().map(Item::Track).collect(),
        },
        Shelf {
            title: HOME_MIXES.into(),
            items: mixes,
        },
        Shelf {
            title: HOME_PLAYLISTS.into(),
            items: playlists,
        },
        one_service("Jump back in", spotify()),
        one_service("Quick picks", ytmusic()),
    ]
}

/// One entry of a service's Library: its id, kind, title and subtitle.
type LibraryEntry = (
    &'static str,
    CollectionKind,
    &'static str,
    Option<&'static str>,
);

const PLAYLIST: CollectionKind = CollectionKind::Playlist;
const ALBUM: CollectionKind = CollectionKind::Album;

/// Spotify's Library in its own order, with ids as the provider makes them: the liked
/// collection, then mixes, an album and playlists of the user's.
const SPOTIFY_LIBRARY: [LibraryEntry; 8] = [
    (
        "collection",
        PLAYLIST,
        "Liked Songs",
        Some("Auto playlist · 312 songs"),
    ),
    (
        "playlist:37i9dQZF1E39demoMix01",
        PLAYLIST,
        "Daily Mix 1",
        Some("Playlist · 50 songs"),
    ),
    (
        "playlist:37i9dQZEVXcdemoWeekly",
        PLAYLIST,
        "Discover Weekly",
        Some("Playlist · 30 songs"),
    ),
    (
        "playlist:2demoGoldenMornings01",
        PLAYLIST,
        "Golden Mornings",
        Some("Playlist · 22 songs"),
    ),
    (
        "album:4demoAlbumFieldNotes001",
        ALBUM,
        "Field Notes",
        Some("Album · Hollow Coast"),
    ),
    (
        "playlist:2demoMidnightRooms02",
        PLAYLIST,
        "Midnight Rooms",
        Some("Playlist · 57 songs"),
    ),
    (
        "playlist:2demoEarlyWaves00003",
        PLAYLIST,
        "Early Waves",
        Some("Playlist · 78 songs"),
    ),
    (
        "playlist:2demoSlowRoads000004",
        PLAYLIST,
        "Slow Roads",
        Some("Playlist · 8 songs"),
    ),
];

/// YouTube Music's Library in its own order: the liked collection, then mixes and radios,
/// albums and playlists of the user's.
const YTMUSIC_LIBRARY: [LibraryEntry; 12] = [
    (
        "browse:VLLM",
        PLAYLIST,
        "Liked music",
        Some("Auto playlist · 188 songs"),
    ),
    (
        "browse:VLRDTMAKdemoSupermix01",
        PLAYLIST,
        "My Supermix",
        Some("Playlist · 50 videos"),
    ),
    (
        "browse:VLPLdemoQuietRoads0001",
        PLAYLIST,
        "Quiet Roads",
        Some("Playlist · 15 videos"),
    ),
    (
        "browse:VLRDCLAKdemoChillMix02",
        PLAYLIST,
        "Chill Mix",
        Some("Playlist · 25 videos"),
    ),
    (
        "browse:MPREb_demoAlbumBlueHour",
        ALBUM,
        "Blue Hour",
        Some("Album · Hollow Coast"),
    ),
    (
        "browse:VLPLdemoGoldenRoads002",
        PLAYLIST,
        "Golden Roads",
        Some("Playlist · 43 videos"),
    ),
    (
        "browse:MPREb_demoAlbumLanterns",
        ALBUM,
        "Paper Lanterns",
        Some("Album · The Verdant Hours"),
    ),
    (
        "browse:VLPLdemoMidnightWaves3",
        PLAYLIST,
        "Midnight Waves",
        Some("Playlist · 57 videos"),
    ),
    (
        "browse:VLPLdemoEarlyMornings4",
        PLAYLIST,
        "Early Mornings",
        Some("Playlist · 64 videos"),
    ),
    (
        "browse:VLPLdemoLongTitle00005",
        PLAYLIST,
        "Songs to Drive Through the Rain at Midnight With the Windows Down",
        Some("Playlist · 120 videos"),
    ),
    (
        "browse:VLPLdemoSlowMornings06",
        PLAYLIST,
        "Slow Mornings",
        Some("Playlist · 36 videos"),
    ),
    (
        "browse:VLPLdemoGoldenRooms007",
        PLAYLIST,
        "Golden Rooms",
        Some("Playlist · 50 videos"),
    ),
];

/// The Library as both accounts merge into it: their entries taken in turn, Spotify first,
/// for as long as both have any, then the rest of YouTube Music's.
pub fn library() -> Vec<Shelf> {
    let mut spotify_entries = SPOTIFY_LIBRARY.iter();
    let mut ytmusic_entries = YTMUSIC_LIBRARY.iter();
    let items = (0..LIBRARY_LEN)
        .map(|i| {
            let (provider, &(id, kind, title, subtitle)) =
                if taken_in_turn(i, MIXED_LIBRARY) == spotify() {
                    (spotify(), spotify_entries.next().expect("a Spotify entry"))
                } else {
                    (
                        ytmusic(),
                        ytmusic_entries.next().expect("a YouTube Music entry"),
                    )
                };
            Item::Collection(Collection {
                id: CollectionId {
                    provider,
                    id: id.into(),
                },
                kind,
                title: title.into(),
                subtitle: subtitle.map(str::to_owned),
                artwork_url: (i % 7 != 4).then(|| demo_url(LIBRARY_ART + i)),
            })
        })
        .collect();
    vec![Shelf {
        title: "Library".into(),
        items,
    }]
}

/// The collection the `collection` view opens: the Library's first YouTube Music playlist
/// that is not the liked one.
pub fn collection() -> Collection {
    match library().remove(0).items.remove(5) {
        Item::Collection(c) => c,
        Item::Track(_) => unreachable!("the Library lists playlists"),
    }
}

/// A collection's tracks, all from its own `provider`.
fn collection_tracks(provider: &ProviderId) -> Vec<Track> {
    let mut tracks = results();
    tracks.truncate(COLLECTION_LEN);
    for t in &mut tracks {
        t.id.provider = provider.clone();
    }
    tracks
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The state a pending Spotify sign-in shows: 15 minutes, with half a minute of slack for the
/// render.
fn pending() -> AccountState {
    AccountState::Pending {
        verification_url: "https://www.spotify.com/pair".into(),
        user_code: "WXYZ-ABCD".into(),
        expires_at_unix: unix_now() + 14 * 60 + 30,
    }
}

fn unavailable() -> AccountState {
    AccountState::Unavailable("no usable HOME, so there is nowhere to keep a session".into())
}

pub fn results() -> Vec<Track> {
    RESULTS
        .iter()
        .enumerate()
        .map(|(i, (title, artists, album, secs))| Track {
            id: TrackId {
                provider: taken_in_turn(i, MIXED_RESULTS),
                id: format!("demo-{i:02}"),
            },
            title: (*title).to_owned(),
            artists: artists.iter().map(|a| (*a).to_owned()).collect(),
            album: album.map(str::to_owned),
            duration: Some(Duration::from_secs(*secs)),
            artwork_url: artwork_url(i),
        })
        .collect()
}

/// What every demo search finds. Only YouTube Music finds albums and playlists, as in the
/// player; one of each has no cover, to show the placeholder.
pub fn search_results() -> SearchResults {
    let collection =
        |kind, id: String, title: &str, subtitle: String, art: Option<usize>| Collection {
            id: CollectionId {
                provider: ytmusic(),
                id,
            },
            kind,
            title: title.to_owned(),
            subtitle: Some(subtitle),
            artwork_url: art.map(demo_url),
        };
    let albums = SEARCH_ALBUMS
        .iter()
        .enumerate()
        .map(|(i, (title, artist, kind, year))| {
            collection(
                CollectionKind::Album,
                format!("demo-search-album-{i:02}"),
                title,
                format!("{kind} \u{2022} {artist} \u{2022} {year}"),
                (i != 5).then_some(SEARCH_ALBUM_ART + i),
            )
        })
        .collect();
    let playlists = SEARCH_PLAYLISTS
        .iter()
        .enumerate()
        .map(|(i, (title, songs))| {
            collection(
                CollectionKind::Playlist,
                format!("demo-search-playlist-{i:02}"),
                title,
                format!("YouTube Music \u{2022} {songs} songs"),
                (i != 3).then_some(SEARCH_PLAYLIST_ART + i),
            )
        })
        .collect();
    SearchResults {
        tracks: results(),
        artists: Vec::new(),
        albums,
        playlists,
    }
}

/// Stands in for the daemon: commands update the fake state where that is trivial.
#[derive(Debug)]
pub struct Daemon {
    results: Vec<Track>,
    queue: Vec<Track>,
    current: Option<usize>,
    state: PlayState,
    volume: u8,
    position: Duration,
    accounts: Vec<(ProviderId, AccountState)>,
}

impl Default for Daemon {
    fn default() -> Self {
        let results = results();
        let queue = QUEUE.iter().map(|&i| results[i].clone()).collect();
        Self {
            results,
            queue,
            current: Some(CURRENT),
            state: PlayState::Playing,
            volume: VOLUME,
            position: POSITION,
            accounts: vec![
                (
                    spotify(),
                    AccountState::SignedIn {
                        name: Some("Demo Listener".into()),
                    },
                ),
                (ytmusic(), AccountState::SignedIn { name: None }),
            ],
        }
    }
}

impl Daemon {
    /// The events a daemon sends a new client, in the protocol's snapshot order.
    pub fn snapshot(&self) -> Vec<Event> {
        let mut evs: Vec<Event> = self
            .accounts
            .iter()
            .map(|(provider, state)| Event::Account {
                provider: provider.clone(),
                state: state.clone(),
            })
            .collect();
        evs.push(Event::QueueChanged {
            tracks: self.queue.clone(),
            current: self.current,
        });
        let playing = self.playing();
        if let Some(t) = &playing {
            evs.push(Event::NowPlaying {
                track: t.clone(),
                duration: t.duration,
            });
        }
        evs.push(Event::State(self.state));
        evs.push(Event::Volume(self.volume));
        if playing.is_some() {
            evs.push(Event::Position(self.position));
        }
        evs
    }

    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        match cmd {
            // Every query matches everything.
            Command::Search { req, .. } => vec![Event::SearchResults {
                req,
                results: SearchResults {
                    tracks: self.results.clone(),
                    ..search_results()
                },
            }],
            Command::Enqueue { tracks, mode } => {
                match mode {
                    EnqueueMode::Replace => {
                        self.queue = tracks;
                        self.current = None;
                    }
                    EnqueueMode::Append => self.queue.extend(tracks),
                }
                vec![self.queue_changed()]
            }
            Command::PlayIndex(i) => self.start(i),
            Command::Next => match self.current.map_or(0, |c| c + 1) {
                n if n < self.queue.len() => self.start(n),
                _ => self.stop(),
            },
            Command::Prev => match self.current {
                Some(c) if c > 0 => self.start(c - 1),
                _ => self.seek(Duration::ZERO),
            },
            Command::Pause => self.set_state(PlayState::Paused),
            Command::Resume => self.resume(),
            Command::TogglePause => match self.state {
                PlayState::Playing | PlayState::Buffering => self.set_state(PlayState::Paused),
                PlayState::Paused | PlayState::Stopped => self.resume(),
            },
            Command::Seek(SeekTarget::Absolute(to)) => self.seek(to),
            Command::Seek(SeekTarget::Relative(s)) => {
                let step = Duration::from_secs(s.unsigned_abs());
                let to = if s >= 0 {
                    self.position.saturating_add(step)
                } else {
                    self.position.saturating_sub(step)
                };
                self.seek(to)
            }
            Command::SetVolume(v) => {
                self.volume = v.min(100);
                vec![Event::Volume(self.volume)]
            }
            Command::Remove(i) => self.remove(i),
            Command::Move { from, to } => self.move_track(from, to),
            Command::Clear => {
                self.queue.clear();
                self.current = None;
                let mut evs = self.stop();
                evs.push(self.queue_changed());
                evs
            }
            Command::Browse {
                req,
                page: Page::Home,
            } if !self
                .accounts
                .iter()
                .any(|(_, s)| matches!(s, AccountState::SignedIn { .. })) =>
            {
                vec![Event::BrowseFailed {
                    req,
                    error: "sign-in required: sign in to see your music".into(),
                    sign_in_required: true,
                }]
            }
            Command::Browse { req, page } => {
                let shelves = match &page {
                    Page::Home => home(),
                    Page::Library => library(),
                    Page::Collection(id) => vec![Shelf {
                        title: String::new(),
                        items: collection_tracks(&id.provider)
                            .into_iter()
                            .map(Item::Track)
                            .collect(),
                    }],
                };
                vec![Event::BrowseResults { req, page, shelves }]
            }
            Command::Shutdown
            | Command::Cue(_)
            | Command::SignIn { .. }
            | Command::SignOut { .. }
            | Command::ImportSignInClient { .. } => Vec::new(),
        }
    }

    /// As the session: removing the playing track plays the next one, if any.
    fn remove(&mut self, i: usize) -> Vec<Event> {
        if i >= self.queue.len() {
            return Vec::new();
        }
        self.queue.remove(i);
        match self.current {
            Some(c) if c == i => {
                let more = i < self.queue.len();
                if more && matches!(self.state, PlayState::Playing | PlayState::Buffering) {
                    return self.start(i);
                }
                self.current = if more { i.checked_sub(1) } else { None };
                let mut evs = if self.state == PlayState::Stopped {
                    Vec::new()
                } else {
                    self.stop()
                };
                evs.push(self.queue_changed());
                evs
            }
            Some(c) if i < c => {
                self.current = Some(c - 1);
                vec![self.queue_changed()]
            }
            _ => vec![self.queue_changed()],
        }
    }

    fn move_track(&mut self, from: usize, to: usize) -> Vec<Event> {
        let len = self.queue.len();
        if from >= len || to >= len || from == to {
            return Vec::new();
        }
        let track = self.queue.remove(from);
        self.queue.insert(to, track);
        self.current = self.current.map(|c| {
            if c == from {
                to
            } else if from < c && c <= to {
                c - 1
            } else if to <= c && c < from {
                c + 1
            } else {
                c
            }
        });
        vec![self.queue_changed()]
    }

    fn playing(&self) -> Option<Track> {
        if self.state == PlayState::Stopped {
            return None;
        }
        self.current.and_then(|c| self.queue.get(c).cloned())
    }

    pub fn queue_changed(&self) -> Event {
        Event::QueueChanged {
            tracks: self.queue.clone(),
            current: self.current,
        }
    }

    fn start(&mut self, i: usize) -> Vec<Event> {
        let Some(track) = self.queue.get(i).cloned() else {
            return Vec::new();
        };
        self.current = Some(i);
        self.state = PlayState::Playing;
        self.position = Duration::ZERO;
        vec![
            self.queue_changed(),
            Event::NowPlaying {
                duration: track.duration,
                track,
            },
            Event::State(PlayState::Playing),
            Event::Position(Duration::ZERO),
        ]
    }

    /// As the session: a stopped queue starts after the last track played, or from the top
    /// once it is past the end.
    fn resume(&mut self) -> Vec<Event> {
        match self.state {
            PlayState::Paused => self.set_state(PlayState::Playing),
            PlayState::Stopped if !self.queue.is_empty() => {
                let next = self.current.map_or(0, |c| c + 1);
                self.start(if next < self.queue.len() { next } else { 0 })
            }
            _ => Vec::new(),
        }
    }

    fn stop(&mut self) -> Vec<Event> {
        self.state = PlayState::Stopped;
        vec![Event::State(PlayState::Stopped)]
    }

    fn set_state(&mut self, state: PlayState) -> Vec<Event> {
        if self.playing().is_none() {
            return Vec::new();
        }
        self.state = state;
        vec![Event::State(state)]
    }

    fn seek(&mut self, to: Duration) -> Vec<Event> {
        let Some(track) = self.playing() else {
            return Vec::new();
        };
        self.position = track.duration.map_or(to, |d| to.min(d));
        vec![Event::Position(self.position)]
    }
}

/// A view model and daemon stand-in in the demo's starting state: the snapshot applied and
/// `QUERY` searched.
pub fn start() -> (Vm, Daemon) {
    start_in(View::Search)
}

/// Sends what the view model has to say, and applies the answers, until it is quiet.
fn settle(vm: &mut Vm, daemon: &mut Daemon) {
    loop {
        let cmds = vm.take_commands();
        if cmds.is_empty() {
            return;
        }
        for cmd in cmds {
            for ev in daemon.apply(cmd) {
                vm.apply(ev);
            }
        }
    }
}

fn run_intent(vm: &mut Vm, daemon: &mut Daemon, intent: Intent) {
    for cmd in vm.intent(intent) {
        for ev in daemon.apply(cmd) {
            vm.apply(ev);
        }
    }
    settle(vm, daemon);
}

/// `start`, staged for a demo-only `view`: `Empty` skips the search; `Disconnected`,
/// `OutOfDate`, `Toast` and the update views add their banner or toast; the Library views and
/// `HomeSignedOut` set the account; `HomeSpotify` filters Home to Spotify; `SettingsSession`
/// opens YouTube Music's session form in Settings; `NowPlayingEmpty` stops playback;
/// `NowPlayingLast` plays the queue's last track; `SearchAlbums` and `SearchSongs` pick that
/// chip of the results. Other views get the plain start.
pub fn start_in(view: View) -> (Vm, Daemon) {
    let t0 = Instant::now();
    let mut vm = Vm::new_at(t0);
    let mut daemon = Daemon::default();
    match view {
        View::LibrarySignedOut | View::HomeSignedOut => {
            daemon.accounts = vec![
                (spotify(), AccountState::SignedOut),
                (ytmusic(), AccountState::SignedOut),
            ];
        }
        View::LibraryUnavailable => {
            daemon.accounts = vec![(ytmusic(), unavailable())];
        }
        View::SettingsSession => {
            daemon.accounts = vec![
                (spotify(), AccountState::SignedOut),
                (ytmusic(), AccountState::SignedOut),
            ];
        }
        View::SignIn => {
            daemon.accounts = vec![(spotify(), pending()), (ytmusic(), AccountState::SignedOut)];
        }
        _ => {}
    }
    vm.link(Link::Demo);
    for ev in daemon.snapshot() {
        vm.apply(ev);
    }
    settle(&mut vm, &mut daemon);
    if view != View::Empty {
        run_intent(&mut vm, &mut daemon, Intent::Search(QUERY.into()));
    }
    match view {
        View::Disconnected => {
            // Past the grace, as a real outage would be by the time its banner shows.
            vm.link_at(Link::Lost(LinkError::NotResponding), t0);
            vm.tick_at(t0 + BANNER_GRACE);
        }
        View::OutOfDate => vm.link(Link::OutOfDate),
        View::UpdateReady => vm.update(update(UpdateState::Ready {
            version: NEXT_VERSION.into(),
        })),
        View::UpdateFailed => vm.update(update(UpdateState::Failed {
            version: NEXT_VERSION.into(),
            reason: "the new version did not start".into(),
        })),
        View::NotUpdatable => vm.update(update(UpdateState::NotUpdatable {
            reason: "Requiem's volume is read-only".into(),
        })),
        View::UpToDate => {
            vm.update(update(UpdateState::Idle));
            vm.check_for_updates();
            vm.update(update(UpdateState::UpToDate));
        }
        View::Toast => {
            vm.apply(Event::TrackFailed {
                track: daemon.results[FAILED].clone(),
                error: "video unavailable".into(),
            });
            vm.apply(daemon.queue_changed());
        }
        View::Home | View::HomeScrolled | View::HomeShelfPaged | View::HomeSignedOut => {
            run_intent(&mut vm, &mut daemon, Intent::Visit(Place::Home))
        }
        View::HomeSpotify => {
            run_intent(&mut vm, &mut daemon, Intent::Visit(Place::Home));
            vm.intent(Intent::SetFilter(ServiceFilter::Only(Service::Spotify)));
        }
        View::Library | View::LibrarySignedOut | View::LibraryUnavailable | View::SignIn => {
            run_intent(&mut vm, &mut daemon, Intent::Visit(Place::Library));
        }
        View::SettingsSession => {
            run_intent(&mut vm, &mut daemon, Intent::Visit(Place::Settings));
            run_intent(&mut vm, &mut daemon, Intent::SignIn(ytmusic()));
        }
        View::Collection => {
            run_intent(&mut vm, &mut daemon, Intent::Visit(Place::Library));
            let open = Intent::OpenCollection {
                collection: collection(),
                from: Place::Library,
            };
            run_intent(&mut vm, &mut daemon, open);
        }
        View::NowPlayingEmpty => {
            // Skipping past the last track stops playback.
            let last = daemon.queue.len() - 1;
            run_intent(&mut vm, &mut daemon, Intent::PlayQueue(last));
            run_intent(&mut vm, &mut daemon, Intent::Next);
        }
        View::NowPlayingLast => {
            let last = daemon.queue.len() - 1;
            run_intent(&mut vm, &mut daemon, Intent::PlayQueue(last));
        }
        View::SearchAlbums => {
            vm.intent(Intent::SetSearchChip(SearchChip::Albums));
        }
        View::SearchSongs => {
            vm.intent(Intent::SetSearchChip(SearchChip::Songs));
        }
        View::Search
        | View::Queue
        | View::Settings
        | View::Palette
        | View::Empty
        | View::NowPlaying => {}
    }
    (vm, daemon)
}

fn update(state: UpdateState) -> UpdateStatus {
    UpdateStatus {
        current: VERSION.into(),
        auto: true,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{self, Connection, HomeView, List, PageView, QueueEdit, SearchStatus};

    fn run(vm: &mut Vm, daemon: &mut Daemon, intent: Intent) {
        for cmd in vm.intent(intent) {
            for ev in daemon.apply(cmd) {
                vm.apply(ev);
            }
        }
    }

    #[test]
    fn starts_with_the_specified_fake_state() {
        let (vm, _) = start();
        let ui = vm.ui();
        assert_eq!(ui.connection, Connection::Demo);
        assert_eq!(ui.results.len(), 30);
        assert_eq!(ui.search, SearchStatus::Done);
        assert_eq!(ui.query, QUERY);
        assert_eq!(ui.queue.len(), 8);
        assert_eq!(ui.current, Some(2));
        assert_eq!(ui.state, PlayState::Playing);
        assert_eq!(ui.elapsed_text(), "1:23");
        assert_eq!(ui.total_text(), "3:45");
        assert_eq!(ui.volume, 70);
        assert_eq!(ui.now_playing.as_ref(), Some(&ui.queue[2]));
    }

    #[test]
    fn home_shows_the_accounts_music_or_asks_for_a_sign_in() {
        let (vm, _) = start_in(View::Home);
        let titles: Vec<&str> = vm.ui().home.shelves.iter().map(|s| &*s.title).collect();
        assert_eq!(
            titles,
            [
                HOME_LIKED,
                HOME_MIXES,
                HOME_PLAYLISTS,
                "Jump back in",
                "Quick picks"
            ]
        );
        for shelf in &vm.ui().home.shelves[..3] {
            let providers: Vec<_> = shelf.items.iter().map(vm::item_provider).collect();
            assert_eq!(providers[..2], [&spotify(), &ytmusic()], "{}", shelf.title);
        }
        assert_eq!(vm.ui().home_view(), HomeView::Page(PageView::Ready));

        let (vm, _) = start_in(View::HomeSignedOut);
        assert_eq!(vm.ui().place, Place::Home);
        assert_eq!(
            vm.ui().home_view(),
            HomeView::SignedOut(vec![spotify(), ytmusic()])
        );
    }

    #[test]
    fn the_demo_filters_home_to_one_service_for_its_spotify_view() {
        let (vm, _) = start_in(View::Home);
        assert!(vm.ui().filter_shown());
        assert_eq!(vm.ui().shown_home().len(), 5);

        let (vm, _) = start_in(View::HomeSpotify);
        let shelves = vm.ui().shown_home();
        let titles: Vec<&str> = shelves.iter().map(|s| &*s.title).collect();
        // Quick picks is YouTube Music's alone.
        assert_eq!(
            titles,
            [HOME_LIKED, HOME_MIXES, HOME_PLAYLISTS, "Jump back in"]
        );
        for shelf in shelves.iter() {
            assert!(
                shelf
                    .items
                    .iter()
                    .all(|i| *vm::item_provider(i) == spotify()),
                "{}",
                shelf.title
            );
        }
    }

    #[test]
    fn results_and_the_library_take_the_services_in_turn() {
        let providers: Vec<_> = results().into_iter().map(|t| t.id.provider).collect();
        assert_eq!(providers[..3], [spotify(), ytmusic(), spotify()]);
        assert_eq!(providers.iter().filter(|p| **p == spotify()).count(), 6);
        assert!(providers[MIXED_RESULTS..].iter().all(|p| *p == ytmusic()));
        let library = library().remove(0).items;
        let spotify_entries = library
            .iter()
            .filter(|i| *vm::item_provider(i) == spotify())
            .count();
        assert_eq!(spotify_entries, SPOTIFY_LIBRARY.len());
        assert_eq!(*vm::item_provider(&library[1]), ytmusic());
        assert_eq!(*vm::item_provider(&library[5]), ytmusic());
        // A collection's tracks are all its own provider's.
        let c = collection();
        assert!(
            collection_tracks(&c.id.provider)
                .iter()
                .all(|t| t.id.provider == c.id.provider)
        );
    }

    #[test]
    fn a_search_finds_youtube_music_albums_and_playlists_with_cached_covers() {
        let mut daemon = Daemon::default();
        let Some(Event::SearchResults { results, .. }) = daemon
            .apply(Command::Search {
                req: 1,
                query: QUERY.into(),
            })
            .pop()
        else {
            panic!("a search is answered with results");
        };
        assert_eq!(results.tracks, daemon.results);
        assert_eq!(results.albums.len(), SEARCH_ALBUMS.len());
        assert_eq!(results.playlists.len(), SEARCH_PLAYLISTS.len());
        assert!(
            results
                .albums
                .iter()
                .all(|a| a.kind == CollectionKind::Album)
        );
        assert!(
            results
                .playlists
                .iter()
                .all(|p| p.kind == CollectionKind::Playlist)
        );
        let collections: Vec<_> = results.albums.iter().chain(&results.playlists).collect();
        assert!(collections.iter().all(|c| c.id.provider == ytmusic()));
        let cached: Vec<String> = covers().into_iter().map(|(url, _)| url).collect();
        let arts: Vec<_> = collections
            .iter()
            .filter_map(|c| c.artwork_url.as_ref())
            .collect();
        assert_eq!(arts.len(), collections.len() - 2);
        assert!(arts.iter().all(|u| cached.contains(u)));
    }

    #[test]
    fn empty_has_no_query_or_results() {
        let (vm, _) = start_in(View::Empty);
        let ui = vm.ui();
        assert_eq!(ui.query, "");
        assert!(ui.results.is_empty());
        assert_eq!(ui.search, SearchStatus::Idle);
        let empty = ui.results_empty().unwrap();
        assert_eq!(empty.heading, "Search");
        assert_eq!(empty.action, None);
        assert_eq!(ui.state, PlayState::Playing);
    }

    #[test]
    fn staged_banners_and_toast() {
        let (vm, _) = start_in(View::Disconnected);
        let b = vm.ui().banner.clone().unwrap();
        assert!(b.text.starts_with("Player disconnected") && !b.error);
        assert!(vm.ui().toast.is_none());

        let (vm, _) = start_in(View::OutOfDate);
        let b = vm.ui().banner.clone().unwrap();
        assert!(b.text.starts_with("An older Requiem player") && b.error);

        let (vm, _) = start_in(View::Toast);
        let t = vm.ui().toast.clone().unwrap();
        assert!(t.error);
        assert_eq!(
            t.text,
            "Couldn’t play “Fever in the Valley”: video unavailable. Skipped to the next track."
        );
        assert!(vm.ui().banner.is_none());
        assert_eq!(vm.ui().results.len(), 30);
    }

    #[test]
    fn staged_update_banners() {
        let banner = |view| start_in(view).0.ui().banner.clone().unwrap();
        let ready = banner(View::UpdateReady);
        assert_eq!(ready.text, "Requiem 0.2.1 is ready.");
        assert_eq!(ready.action, Some("Restart to Update"));
        assert_eq!(
            banner(View::UpdateFailed).text,
            "Requiem couldn’t update: the new version did not start. It keeps running 0.2.0."
        );
        assert_eq!(
            banner(View::NotUpdatable).text,
            "Move Requiem to Applications to get updates."
        );
        assert_eq!(banner(View::UpToDate).text, "Requiem 0.2.0 is up to date.");
    }

    #[test]
    fn track_ids_are_unique() {
        let r = results();
        let mut ids: Vec<_> = r.iter().map(|t| t.id.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 30);
    }

    #[test]
    fn toggle_pauses_and_resumes() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::Toggle);
        assert_eq!(vm.ui().state, PlayState::Paused);
        run(&mut vm, &mut d, Intent::Toggle);
        assert_eq!(vm.ui().state, PlayState::Playing);
    }

    #[test]
    fn play_result_replaces_queue_and_plays() {
        let (mut vm, mut d) = start();
        run(
            &mut vm,
            &mut d,
            Intent::PlayFrom {
                list: List::Results,
                from: 25,
            },
        );
        let ui = vm.ui();
        assert_eq!(ui.queue.len(), 5);
        assert_eq!(ui.current, Some(0));
        assert_eq!(ui.now_playing.as_ref().unwrap().title, "Highway Psalm");
        assert_eq!(ui.elapsed_text(), "0:00");
    }

    #[test]
    fn append_and_queue_click() {
        let (mut vm, mut d) = start();
        run(
            &mut vm,
            &mut d,
            Intent::Append {
                list: List::Results,
                index: 1,
            },
        );
        assert_eq!(vm.ui().queue.len(), 9);
        assert_eq!(vm.ui().queue[8].title, "Paper Lanterns");
        run(&mut vm, &mut d, Intent::PlayQueue(8));
        assert_eq!(vm.ui().current, Some(8));
        assert_eq!(
            vm.ui().now_playing.as_ref().unwrap().title,
            "Paper Lanterns"
        );
    }

    #[test]
    fn next_prev_and_stop_at_end() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::Next);
        assert_eq!(vm.ui().current, Some(3));
        run(&mut vm, &mut d, Intent::Prev);
        assert_eq!(vm.ui().current, Some(2));
        run(&mut vm, &mut d, Intent::PlayQueue(7));
        run(&mut vm, &mut d, Intent::Next);
        assert_eq!(vm.ui().state, PlayState::Stopped);
        assert!(vm.ui().now_playing.is_none());
    }

    #[test]
    fn play_while_stopped_starts_from_the_top_once_past_the_end() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::PlayQueue(7));
        run(&mut vm, &mut d, Intent::Next);
        assert_eq!(vm.ui().state, PlayState::Stopped);
        run(&mut vm, &mut d, Intent::Toggle);
        assert_eq!(vm.ui().state, PlayState::Playing);
        assert_eq!(vm.ui().current, Some(0));
        assert_eq!(vm.ui().now_playing.as_ref(), Some(&vm.ui().queue[0]));

        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::PlayQueue(7));
        run(&mut vm, &mut d, Intent::Next);
        for ev in d.apply(Command::Resume) {
            vm.apply(ev);
        }
        assert_eq!(
            (vm.ui().state, vm.ui().current),
            (PlayState::Playing, Some(0))
        );
    }

    #[test]
    fn play_while_stopped_starts_after_the_last_track_played() {
        let (mut vm, mut d) = start();
        let after = vm.ui().queue[3].clone();
        run(&mut vm, &mut d, Intent::Toggle);
        // Removing the paused current track stops, with the cursor just before its place.
        edit(&mut vm, &mut d, QueueEdit::Remove(2), None);
        assert_eq!(vm.ui().state, PlayState::Stopped);
        assert_eq!(vm.ui().current, Some(1));
        run(&mut vm, &mut d, Intent::Toggle);
        assert_eq!(
            (vm.ui().state, vm.ui().current),
            (PlayState::Playing, Some(2))
        );
        assert_eq!(vm.ui().now_playing.as_ref(), Some(&after));

        // Resume does the same; paused, it only resumes.
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::Toggle);
        for ev in d.apply(Command::Resume) {
            vm.apply(ev);
        }
        assert_eq!(
            (vm.ui().state, vm.ui().current),
            (PlayState::Playing, Some(2))
        );
        assert_eq!(vm.ui().elapsed_text(), "1:23");
    }

    #[test]
    fn play_with_an_empty_queue_does_nothing() {
        let mut d = Daemon::default();
        d.apply(Command::Clear);
        assert!(d.apply(Command::Resume).is_empty());
        assert!(d.apply(Command::TogglePause).is_empty());
    }

    #[test]
    fn seek_and_volume() {
        let (mut vm, mut d) = start();
        run(&mut vm, &mut d, Intent::SeekBy(5));
        assert_eq!(vm.ui().elapsed_text(), "1:28");
        run(&mut vm, &mut d, Intent::SeekTo(Duration::from_secs(10)));
        assert_eq!(vm.ui().elapsed_text(), "0:10");
        run(&mut vm, &mut d, Intent::VolumeBy(-10));
        assert_eq!(vm.ui().volume, 60);
        run(&mut vm, &mut d, Intent::SetVolume(5));
        assert_eq!(vm.ui().volume, 5);
    }

    fn edit(vm: &mut Vm, d: &mut Daemon, edit: QueueEdit, selected: Option<usize>) {
        run(vm, d, Intent::EditQueue { edit, selected });
    }

    #[test]
    fn queue_edits_keep_the_current_track() {
        let (mut vm, mut d) = start();
        let titles =
            |vm: &Vm| -> Vec<String> { vm.ui().queue.iter().map(|t| t.title.clone()).collect() };
        let before = titles(&vm);
        edit(&mut vm, &mut d, QueueEdit::Remove(0), Some(0));
        assert_eq!(titles(&vm), before[1..]);
        assert_eq!(vm.ui().current, Some(1));
        assert_eq!(vm.take_queue_selection(), Some(0));
        edit(&mut vm, &mut d, QueueEdit::MoveDown(1), Some(1));
        assert_eq!(vm.ui().current, Some(2));
        assert_eq!(vm.take_queue_selection(), Some(2));
        assert_eq!(vm.ui().now_playing.as_ref(), Some(&vm.ui().queue[2]));
        // Removing the playing track plays the next.
        edit(&mut vm, &mut d, QueueEdit::Remove(2), Some(2));
        assert_eq!(vm.ui().current, Some(2));
        assert_eq!(vm.ui().now_playing.as_ref(), Some(&vm.ui().queue[2]));
        assert_eq!(vm.ui().state, PlayState::Playing);
    }

    #[test]
    fn clear_and_undo() {
        let (mut vm, mut d) = start();
        let queue = vm.ui().queue.clone();
        edit(&mut vm, &mut d, QueueEdit::Clear, None);
        assert!(vm.ui().queue.is_empty());
        assert_eq!(vm.ui().state, PlayState::Stopped);
        run(&mut vm, &mut d, Intent::Undo);
        for cmd in vm.take_commands() {
            for ev in d.apply(cmd) {
                vm.apply(ev);
            }
        }
        assert_eq!(vm.ui().queue, queue);
        assert_eq!(vm.ui().current, Some(2));
        assert_eq!(vm.ui().elapsed_text(), "1:23");
    }

    /// Runs `intent`, then what the view model raises in answer, as the app does.
    fn settle(vm: &mut Vm, d: &mut Daemon, intent: Intent) {
        run(vm, d, intent);
        loop {
            let cmds = vm.take_commands();
            if cmds.is_empty() {
                break;
            }
            for cmd in cmds {
                for ev in d.apply(cmd) {
                    vm.apply(ev);
                }
            }
        }
    }

    #[test]
    fn undo_leaves_a_paused_queue_paused() {
        for change in [
            Intent::PlayFrom {
                list: List::Results,
                from: 25,
            },
            Intent::EditQueue {
                edit: QueueEdit::Clear,
                selected: None,
            },
        ] {
            let (mut vm, mut d) = start();
            let queue = vm.ui().queue.clone();
            settle(&mut vm, &mut d, Intent::Toggle);
            assert_eq!(vm.ui().state, PlayState::Paused);
            settle(&mut vm, &mut d, change);
            settle(&mut vm, &mut d, Intent::Undo);
            let ui = vm.ui();
            assert_eq!(ui.queue, queue);
            assert_eq!(ui.current, Some(2));
            assert_eq!(ui.now_playing.as_ref(), Some(&queue[2]));
            assert_eq!(ui.state, PlayState::Paused);
            assert_eq!(ui.elapsed_text(), "1:23");
        }
    }
}
