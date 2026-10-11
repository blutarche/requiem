//! Innertube browse pages: the anonymous home feed, playlists (`VL…`) and albums (`MPREb_…`).
//! Only the first page is read; continuations are not followed.

use requiem_core::{
    Collection, CollectionId, CollectionKind, Item, ProviderError, ProviderId, Shelf, Track,
    TrackId,
};
use serde::Deserialize;
use serde_json::Value;

use super::{
    ListItem, NavEndpoint, PROVIDER, Run, Runs, SEPARATOR, Thumbnail, collect_items, column_runs,
    largest, largest_thumbnail, parse_duration, parse_err, parse_subtitle,
};

/// Prefix of collection ids that are Innertube browse ids.
pub(crate) const BROWSE_PREFIX: &str = "browse:";

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct TwoRowItem {
    title: Option<Runs>,
    subtitle: Option<Runs>,
    navigation_endpoint: Option<NavEndpoint>,
    thumbnail_renderer: Option<Thumbnail>,
}

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

/// A carousel card: a playlist, an album, or a song. Artists and other pages are skipped.
fn two_row_item(item: &TwoRowItem) -> Option<Item> {
    let title = joined(item.title.as_ref());
    let first_run = item.title.as_ref().and_then(|t| t.runs.first());
    let nav = item
        .navigation_endpoint
        .as_ref()
        .or_else(|| first_run?.navigation_endpoint.as_ref())?;
    let artwork_url = largest(item.thumbnail_renderer.as_ref());
    if let Some(browse_id) = nav
        .browse_endpoint
        .as_ref()
        .and_then(|b| b.browse_id.as_deref())
    {
        if !(browse_id.starts_with("VL") || browse_id.starts_with("MPREb_")) {
            return None;
        }
        let subtitle = joined(item.subtitle.as_ref());
        return Some(Item::Collection(Collection {
            id: CollectionId {
                provider: ProviderId(PROVIDER.to_owned()),
                id: format!("{BROWSE_PREFIX}{browse_id}"),
            },
            kind: collection_kind(browse_id),
            title,
            subtitle: Some(subtitle.trim().to_owned()).filter(|s| !s.is_empty()),
            artwork_url,
        }));
    }
    let id = nav.watch_endpoint.as_ref()?.video_id.clone()?;
    let runs = item
        .subtitle
        .as_ref()
        .map_or(&[][..], |s| s.runs.as_slice());
    // "Song • Artist • Album": the leading type label is not an artist.
    let runs = match runs {
        [label, sep, rest @ ..]
            if matches!(label.text.as_deref(), Some("Song" | "Video"))
                && sep.text.as_deref() == Some(SEPARATOR) =>
        {
            rest
        }
        _ => runs,
    };
    let sub = parse_subtitle(runs);
    Some(Item::Track(Track {
        id: TrackId {
            provider: ProviderId(PROVIDER.to_owned()),
            id,
        },
        title,
        artists: sub.artists,
        album: sub.album,
        duration: sub.duration,
        artwork_url,
    }))
}

/// The home feed's carousels, empty ones left out. Songs here carry no duration.
pub(crate) fn parse_home(body: &[u8]) -> Result<Vec<Shelf>, ProviderError> {
    let root = root(body, "home")?;
    let mut carousels = Vec::new();
    find_all(&root, &["musicCarouselShelfRenderer"], &mut carousels);
    let shelves = carousels
        .into_iter()
        .filter_map(|c| {
            let title = c
                .pointer("/header/musicCarouselShelfBasicHeaderRenderer/title")
                .and_then(|t| Runs::deserialize(t).ok());
            let items: Vec<Item> = c
                .get("contents")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|entry| {
                    if let Some(v) = entry.get("musicTwoRowItemRenderer") {
                        two_row_item(&TwoRowItem::deserialize(v).ok()?)
                    } else {
                        let v = entry.get("musicResponsiveListItemRenderer")?;
                        list_item_track(&ListItem::deserialize(v).ok()?).map(Item::Track)
                    }
                })
                .collect();
            (!items.is_empty()).then(|| Shelf {
                title: joined(title.as_ref()),
                items,
            })
        })
        .collect();
    Ok(shelves)
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

    const HOME: &str = include_str!("../../tests/fixtures/browse_home.json");
    const PLAYLIST: &str = include_str!("../../tests/fixtures/browse_playlist.json");
    const ALBUM: &str = include_str!("../../tests/fixtures/browse_album.json");

    fn track(item: &Item) -> &Track {
        match item {
            Item::Track(t) => t,
            Item::Collection(c) => panic!("not a track: {c:?}"),
        }
    }

    fn collection(item: &Item) -> &Collection {
        match item {
            Item::Collection(c) => c,
            Item::Track(t) => panic!("not a collection: {t:?}"),
        }
    }

    #[test]
    fn home_fixture_shelves() {
        let shelves = parse_home(HOME.as_bytes()).unwrap();
        let titles: Vec<&str> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Quick picks", "Trending community playlists"]);

        let picks = &shelves[0].items;
        assert_eq!(picks.len(), 3);
        let t = track(&picks[0]);
        assert_eq!(t.id.id, "FV_SqGu4IzE");
        assert_eq!(t.id.provider, ProviderId("ytmusic".into()));
        assert_eq!(t.title, "Amor Factura");
        assert_eq!(t.artists, ["Ricardo Arjona", "Grupo Firme"]);
        assert_eq!(t.album.as_deref(), Some("Amor Factura"));
        assert_eq!(t.duration, None);
        assert!(
            t.artwork_url
                .as_deref()
                .unwrap()
                .contains("yt3.googleusercontent.com")
        );
        let t = track(&picks[1]);
        assert_eq!(t.title, "Patient Zero");
        assert_eq!(t.artists, ["Taylor Swift"]);
        assert_eq!(
            t.album.as_deref(),
            Some("The Life of a Showgirl: The Encore")
        );

        let lists = &shelves[1].items;
        assert_eq!(lists.len(), 3);
        let c = collection(&lists[0]);
        assert_eq!(c.id.id, "browse:VLPLW0lk8zFei2v91CDXtie506Q669dkKQQu");
        assert_eq!(c.kind, CollectionKind::Playlist);
        assert_eq!(c.title, "เพลงไว้ฟังขับรถ");
        assert_eq!(c.subtitle.as_deref(), Some("Metha Kunsamut • 1.5M views"));
        let art = c.artwork_url.as_deref().unwrap();
        let raw: Value = serde_json::from_str(HOME).unwrap();
        let mut thumbs = Vec::new();
        find_all(&raw, &["musicTwoRowItemRenderer"], &mut thumbs);
        let biggest = thumbs[0]
            .pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .max_by_key(|t| t["width"].as_u64())
            .unwrap()["url"]
            .as_str()
            .unwrap();
        assert_eq!(art, biggest);
    }

    #[test]
    fn home_skips_empty_shelves_and_unknown_cards() {
        let body = r#"{"contents":{"sectionListRenderer":{"contents":[
            {"musicCarouselShelfRenderer":{"header":{"musicCarouselShelfBasicHeaderRenderer":
                {"title":{"runs":[{"text":"Artists"}]}}},
             "contents":[{"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Someone"}]},
                "navigationEndpoint":{"browseEndpoint":{"browseId":"UCabc"}}}}]}},
            {"musicCarouselShelfRenderer":{"header":{"musicCarouselShelfBasicHeaderRenderer":
                {"title":{"runs":[{"text":"Mixed"}]}}},
             "contents":[
               {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Homework"}]},
                  "subtitle":{"runs":[{"text":"Album"},{"text":" • "},{"text":"Daft Punk"}]},
                  "navigationEndpoint":{"browseEndpoint":{"browseId":"MPREb_abc"}}}},
               {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"One More Time"}]},
                  "subtitle":{"runs":[{"text":"Song"},{"text":" • "},{"text":"Daft Punk"}]},
                  "navigationEndpoint":{"watchEndpoint":{"videoId":"FGBhQbmPwH8"}}}},
               {"musicResponsiveListItemRenderer":{"flexColumns":[]}}]}}]}}}"#;
        let shelves = parse_home(body.as_bytes()).unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Mixed");
        assert_eq!(shelves[0].items.len(), 2);
        let album = collection(&shelves[0].items[0]);
        assert_eq!(album.id.id, "browse:MPREb_abc");
        assert_eq!(album.kind, CollectionKind::Album);
        let song = track(&shelves[0].items[1]);
        assert_eq!(song.id.id, "FGBhQbmPwH8");
        assert_eq!(song.artists, ["Daft Punk"]);
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
        assert!(matches!(parse_home(b"{}"), Err(ProviderError::Parse(_))));
        assert!(matches!(
            parse_collection(b"nope", CollectionKind::Album),
            Err(ProviderError::Parse(_))
        ));
    }
}
