//! What the windows show, worked out without a toolkit: the pages, table rows, card text and
//! shelf shapes both front-ends share.

use requiem_core::{CollectionKind, Item, Shelf, Track};

use crate::vm::{self, Place, Service};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppView {
    Home,
    Search,
    Library,
    Queue,
    /// A playlist or album opened from Home or the Library; not in the sidebar.
    Collection,
    /// The playing track's page; not in the sidebar.
    NowPlaying,
    /// The accounts; at the foot of the sidebar.
    Settings,
}

impl AppView {
    pub fn place(self) -> Place {
        match self {
            AppView::Home => Place::Home,
            AppView::Search => Place::Search,
            AppView::Library => Place::Library,
            AppView::Queue => Place::Queue,
            AppView::Collection => Place::Collection,
            AppView::NowPlaying => Place::NowPlaying,
            AppView::Settings => Place::Settings,
        }
    }

    pub fn of(place: Place) -> AppView {
        match place {
            Place::Home => AppView::Home,
            Place::Search => AppView::Search,
            Place::Library => AppView::Library,
            Place::Queue => AppView::Queue,
            Place::Collection => AppView::Collection,
            Place::NowPlaying => AppView::NowPlaying,
            Place::Settings => AppView::Settings,
        }
    }
}

/// Where Back leaves Now Playing or Settings, `here`, for: the view it was opened `from`; Home
/// instead of a collection that is no longer open, or whose Back, from the `origin` of its
/// pages, would come back `here`.
pub fn back_to(from: AppView, here: AppView, origin: Option<Place>) -> AppView {
    match from {
        AppView::Collection if origin.is_none_or(|o| o == here.place()) => AppView::Home,
        view => view,
    }
}

/// Whether a snapshot of a connected window has what it waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotState {
    Waiting,
    /// The page is in; its covers may still be arriving.
    Loaded {
        covers: bool,
    },
    Failed(String),
}

/// One row's text, formatted once per list change, and its artwork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowText {
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration: String,
    pub artwork: Option<String>,
    /// The service badge on the artwork, in a list that mixes services.
    pub badge: Option<Service>,
}

pub fn rows(tracks: &[Track]) -> Vec<RowText> {
    let mixed = vm::mixed(tracks.iter().map(|t| &t.id.provider));
    tracks
        .iter()
        .map(|t| RowText {
            title: t.title.clone(),
            artists: vm::artists(t),
            album: t.album.clone().unwrap_or_default(),
            duration: t.duration.map(vm::fmt_time).unwrap_or_default(),
            artwork: t.artwork_url.clone(),
            badge: vm::badge(&t.id.provider, mixed),
        })
        .collect()
}

/// Whether the tracks of a collection of `kind` list their album: an album's own tracks all
/// share one.
pub fn lists_album(kind: CollectionKind) -> bool {
    kind != CollectionKind::Album
}

/// A row's number in an album's list.
pub fn track_number(row: usize) -> String {
    (row + 1).to_string()
}

/// The queue's count line: nothing while it is empty.
pub fn queue_count_text(songs: usize) -> String {
    match songs {
        0 => String::new(),
        1 => "1 song".to_owned(),
        n => format!("{n} songs"),
    }
}

/// The collections of `shelves`, a shelf each, leaving out the tracks and the shelves they
/// leave empty.
pub fn collection_shelves(shelves: &[Shelf]) -> Vec<Shelf> {
    shelves
        .iter()
        .filter_map(|s| {
            let items: Vec<Item> = s
                .items
                .iter()
                .filter(|i| matches!(i, Item::Collection(_)))
                .cloned()
                .collect();
            (!items.is_empty()).then(|| Shelf {
                title: s.title.clone(),
                items,
            })
        })
        .collect()
}

/// A card's title, subtitle and artwork. A track's subtitle is its artists.
pub fn card_parts(item: &Item) -> (&str, String, Option<&str>) {
    match item {
        Item::Track(t) => (&t.title, vm::artists(t), t.artwork_url.as_deref()),
        Item::Collection(c) => (
            &c.title,
            c.subtitle.clone().unwrap_or_default(),
            c.artwork_url.as_deref(),
        ),
    }
}

/// Whether `item` is an artist, whose photo is round.
pub fn is_artist(item: &Item) -> bool {
    matches!(item, Item::Collection(c) if c.kind == CollectionKind::Artist)
}

/// The subtitle a card shows: under a section of artists, which already says what they are,
/// without the leading "Artist".
pub fn shown_subtitle(subtitle: &str, artists: bool) -> &str {
    if !artists {
        return subtitle;
    }
    match subtitle.strip_prefix("Artist") {
        Some("") => "",
        Some(rest) => rest.strip_prefix(" \u{2022} ").unwrap_or(subtitle),
        None => subtitle,
    }
}

/// Whether a shelf is a grid of tracks, YouTube Music's compact "Quick picks" layout.
pub fn is_grid(shelf: &Shelf) -> bool {
    !shelf.items.is_empty() && shelf.items.iter().all(|i| matches!(i, Item::Track(_)))
}

pub const ADD_W: f64 = 44.0;
pub const DURATION_W: f64 = 80.0;
/// Narrower tables drop the album column.
pub const ALBUM_MIN_W: f64 = 640.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Widths {
    pub title: f64,
    pub artists: f64,
    pub album: Option<f64>,
}

/// Text-column widths for a table `width` wide, with or without the "+" column and the Album
/// column. They fill what the fixed columns leave, rounding into the last, so the columns end
/// flush with the table.
pub fn column_widths(width: f64, add: bool, album: bool) -> Widths {
    let add_w = if add { ADD_W } else { 0.0 };
    let flex = (width - add_w - DURATION_W).max(0.0);
    if !album || width < ALBUM_MIN_W {
        let title = (flex * 46.0 / 76.0).floor();
        Widths {
            title,
            artists: flex - title,
            album: None,
        }
    } else {
        let title = (flex * 0.46).floor();
        let artists = (flex * 0.30).floor();
        Widths {
            title,
            artists,
            album: Some(flex - title - artists),
        }
    }
}

/// Seconds since the epoch, for sign-in codes' expiry.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_from_settings_skips_a_collection_that_would_come_back() {
        let here = AppView::Settings;
        assert_eq!(back_to(AppView::Queue, here, None), AppView::Queue);
        assert_eq!(back_to(AppView::Collection, here, None), AppView::Home);
        assert_eq!(
            back_to(AppView::Collection, here, Some(Place::Settings)),
            AppView::Home
        );
        assert_eq!(
            back_to(AppView::Collection, here, Some(Place::Library)),
            AppView::Collection
        );
    }

    #[test]
    fn subtitles_under_artists_leave_the_kind_out() {
        let sub = "Artist \u{2022} 68.1M monthly audience";
        assert_eq!(shown_subtitle(sub, true), "68.1M monthly audience");
        assert_eq!(shown_subtitle(sub, false), sub);
        assert_eq!(shown_subtitle("Artist", true), "");
    }

    #[test]
    fn the_queue_counts_its_songs() {
        assert_eq!(queue_count_text(0), "");
        assert_eq!(queue_count_text(1), "1 song");
        assert_eq!(queue_count_text(8), "8 songs");
    }
}
