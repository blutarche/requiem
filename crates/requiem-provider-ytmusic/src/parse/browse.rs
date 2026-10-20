//! Innertube browse pages: playlists (`VL…`) and albums (`MPREb_…`).
//! Only the first page is read; continuations are not followed.

use requiem_core::{CollectionKind, Item, ProviderError, ProviderId, Shelf, Track, TrackId};
use serde::Deserialize;
use serde_json::Value;

use super::{
    ListItem, PROVIDER, Run, Runs, SEPARATOR, Thumbnail, collect_items, column_runs, largest,
    largest_thumbnail, parse_duration, parse_err, parse_subtitle,
};

/// Prefix of collection ids that are Innertube browse ids.
pub(crate) const BROWSE_PREFIX: &str = "browse:";

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Header {
    title: Option<Runs>,
    strapline_text_one: Option<Runs>,
    thumbnail: Option<Thumbnail>,
}

pub(crate) fn collection_kind(browse_id: &str) -> CollectionKind {
    if browse_id.starts_with("MPREb_") {
        CollectionKind::Album
    } else {
        CollectionKind::Playlist
    }
}

fn joined(runs: Option<&Runs>) -> String {
    runs.map(|r| r.runs.iter().filter_map(|r| r.text.as_deref()).collect())
        .unwrap_or_default()
}

fn root(body: &[u8], what: &str) -> Result<Value, ProviderError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("{what} json: {e}")))?;
    if root.get("contents").is_none() {
        return Err(parse_err(format!("{what} response has no contents")));
    }
    Ok(root)
}

/// Every value under one of `keys`, in document order, without looking inside them.
fn find_all<'a>(v: &'a Value, keys: &[&str], out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if keys.contains(&k.as_str()) {
                    out.push(child);
                } else {
                    find_all(child, keys, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| find_all(c, keys, out)),
        _ => {}
    }
}

fn video_id(item: &ListItem) -> Option<String> {
    let from_data = item
        .playlist_item_data
        .as_ref()
        .and_then(|p| p.video_id.clone());
    let from_title = || {
        column_runs(item, 0).iter().find_map(|r| {
            r.navigation_endpoint
                .as_ref()?
                .watch_endpoint
                .as_ref()?
                .video_id
                .clone()
        })
    };
    from_data.or_else(from_title).filter(|v| !v.is_empty())
}

/// A song row. Columns after the title hold the artists, then the album or a play count;
/// a fixed column holds the duration on collection pages.
fn list_item_track(item: &ListItem) -> Option<Track> {
    let id = video_id(item)?;
    let title: String = column_runs(item, 0)
        .iter()
        .filter_map(|r| r.text.as_deref())
        .collect();
    let separator = Run {
        text: Some(SEPARATOR.to_owned()),
        navigation_endpoint: None,
    };
    let mut rest: Vec<&Run> = Vec::new();
    for i in 1..item.flex_columns.len() {
        if i > 1 {
            rest.push(&separator);
        }
        rest.extend(column_runs(item, i));
    }
    let sub = parse_subtitle(rest);
    let fixed = || {
        item.fixed_columns
            .iter()
            .filter_map(|c| c.renderer.as_ref()?.text.as_ref())
            .flat_map(|t| &t.runs)
            .find_map(|r| parse_duration(r.text.as_deref()?))
    };
    Some(Track {
        id: TrackId {
            provider: ProviderId(PROVIDER.to_owned()),
            id,
        },
        title,
        artists: sub.artists,
        album: sub.album,
        duration: sub.duration.or_else(fixed),
        artwork_url: largest_thumbnail(item),
    })
}

/// A playlist or album page as one shelf named after it. Album rows leave out the artist
/// and artwork, which come from the page header.
pub(crate) fn parse_collection(body: &[u8], kind: CollectionKind) -> Result<Shelf, ProviderError> {
    let root = root(body, "collection")?;
    let mut headers = Vec::new();
    find_all(&root, &["musicResponsiveHeaderRenderer"], &mut headers);
    let header = headers
        .first()
        .and_then(|h| Header::deserialize(*h).ok())
        .unwrap_or_default();
    let name = joined(header.title.as_ref());
    let header_artists: Vec<String> = header
        .strapline_text_one
        .as_ref()
        .map(|s| parse_subtitle(&s.runs).artists)
        .unwrap_or_default();
    let header_art = largest(header.thumbnail.as_ref());

    let mut shelves = Vec::new();
    find_all(
        &root,
        &["musicShelfRenderer", "musicPlaylistShelfRenderer"],
        &mut shelves,
    );
    let mut rows = Vec::new();
    for shelf in shelves {
        if let Some(contents) = shelf.get("contents") {
            collect_items(contents, &mut rows);
        }
    }
    let items = rows
        .iter()
        .filter_map(list_item_track)
        .map(|mut t| {
            if kind == CollectionKind::Album {
                if t.artists.is_empty() {
                    t.artists.clone_from(&header_artists);
                }
                if t.album.is_none() && !name.is_empty() {
                    t.album = Some(name.clone());
                }
            }
            if t.artwork_url.is_none() {
                t.artwork_url.clone_from(&header_art);
            }
            Item::Track(t)
        })
        .collect();
    let title = if name.is_empty() {
        match kind {
            CollectionKind::Album => "Album".to_owned(),
            CollectionKind::Playlist => "Playlist".to_owned(),
        }
    } else {
        name
    };
    Ok(Shelf { title, items })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const PLAYLIST: &str = include_str!("../../tests/fixtures/browse_playlist.json");
    const ALBUM: &str = include_str!("../../tests/fixtures/browse_album.json");

    fn track(item: &Item) -> &Track {
        match item {
            Item::Track(t) => t,
            Item::Collection(c) => panic!("not a track: {c:?}"),
        }
    }

    #[test]
    fn playlist_fixture_tracks() {
        let shelf = parse_collection(PLAYLIST.as_bytes(), CollectionKind::Playlist).unwrap();
        assert_eq!(shelf.title, "เพลงไว้ฟังขับรถ");
        assert_eq!(shelf.items.len(), 3);
        let t = track(&shelf.items[0]);
        assert_eq!(t.id.id, "kkppQnFl2-8");
        assert_eq!(t.title, "ฤดูฝน");
        assert_eq!(t.artists, ["Paradox"]);
        assert_eq!(t.album.as_deref(), Some("BEFORE SUNRISE AFTER SUNSET"));
        assert_eq!(t.duration, Some(Duration::from_secs(4 * 60 + 48)));
        assert!(t.artwork_url.is_some());
        let ids: Vec<&str> = shelf
            .items
            .iter()
            .map(|i| track(i).id.id.as_str())
            .collect();
        assert_eq!(ids, ["kkppQnFl2-8", "bP7rQabXjuw", "R5RUFOkecV0"]);
    }

    #[test]
    fn album_rows_take_artist_album_and_art_from_the_header() {
        let shelf = parse_collection(ALBUM.as_bytes(), CollectionKind::Album).unwrap();
        assert_eq!(shelf.title, "Homework");
        let ids: Vec<&str> = shelf
            .items
            .iter()
            .map(|i| track(i).id.id.as_str())
            .collect();
        // The "more from" carousels below the track list are not part of the album.
        assert_eq!(ids, ["yuoghR-5-Xs", "OzZAHvq_UqI", "uURB-vo9rZ4"]);
        let t = track(&shelf.items[0]);
        assert_eq!(t.title, "Daftendirekt");
        assert_eq!(t.artists, ["Daft Punk"]);
        assert_eq!(t.album.as_deref(), Some("Homework"));
        assert_eq!(t.duration, Some(Duration::from_secs(2 * 60 + 45)));
        assert!(
            t.artwork_url
                .as_deref()
                .unwrap()
                .contains("yt3.googleusercontent.com")
        );
    }

    #[test]
    fn collection_kinds_and_bad_bodies() {
        assert_eq!(collection_kind("MPREb_x"), CollectionKind::Album);
        assert_eq!(collection_kind("VLPLx"), CollectionKind::Playlist);
        assert!(matches!(
            parse_collection(b"nope", CollectionKind::Album),
            Err(ProviderError::Parse(_))
        ));
    }
}
