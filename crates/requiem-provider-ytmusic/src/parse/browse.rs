//! Innertube browse pages: the home feed, the Library's playlists, playlists (`VL…`), albums
//! (`MPREb_…`) and artists (`UC…`). Only Home's continuations are followed.

use std::collections::HashMap;
use std::time::Duration;

use requiem_core::{
    Collection, CollectionId, CollectionKind, Item, ProviderError, ProviderId, Shelf, Track,
    TrackId,
};
use serde::Deserialize;
use serde_json::Value;

use super::{
    ListItem, NavEndpoint, PROVIDER, Run, Runs, SEPARATOR, Thumbnail, WatchEndpoint, collect_items,
    column_runs, is_live, largest, largest_thumbnail, parse_duration, parse_err, parse_subtitle,
};
use crate::log::debug;

/// Prefix of collection ids that are Innertube browse ids.
pub(crate) const BROWSE_PREFIX: &str = "browse:";

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct TwoRowItem {
    title: Option<Runs>,
    subtitle: Option<Runs>,
    navigation_endpoint: Option<NavEndpoint>,
    thumbnail_renderer: Option<Thumbnail>,
    badges: Vec<Value>,
    subtitle_badges: Vec<Value>,
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
    } else if browse_id.starts_with("UC") {
        CollectionKind::Artist
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

/// A first page carries `contents`; a page fetched by its continuation token,
/// `continuationContents`.
fn page_root(body: &[u8], what: &str) -> Result<Value, ProviderError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("{what} json: {e}")))?;
    if root.get("contents").is_none() && root.get("continuationContents").is_none() {
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
/// a fixed column holds the duration on collection pages. A live stream is no song.
fn list_item_track(item: &ListItem) -> Option<Track> {
    if is_live(&item.badges, &item.subtitle_badges) {
        return None;
    }
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
        artist_ids: sub.artist_ids,
        album: sub.album,
        duration: sub.duration.or_else(fixed),
        artwork_url: largest_thumbnail(item),
    })
}

fn two_row_nav(item: &TwoRowItem) -> Option<&NavEndpoint> {
    let first_run = item.title.as_ref().and_then(|t| t.runs.first());
    item.navigation_endpoint
        .as_ref()
        .or_else(|| first_run?.navigation_endpoint.as_ref())
}

/// Home is for music: user uploads and podcast episodes are left off it.
fn is_off_music(watch: Option<&WatchEndpoint>) -> bool {
    matches!(
        watch.and_then(WatchEndpoint::music_video_type),
        Some("MUSIC_VIDEO_TYPE_UGC" | "MUSIC_VIDEO_TYPE_PODCAST_EPISODE")
    )
}

/// A carousel card: a playlist, an album, or a song. Artists, live streams and other pages are
/// skipped.
fn two_row_item(item: &TwoRowItem) -> Option<Item> {
    if is_live(&item.badges, &item.subtitle_badges) {
        return None;
    }
    let title = joined(item.title.as_ref());
    let nav = two_row_nav(item)?;
    let artwork_url = largest(item.thumbnail_renderer.as_ref());
    let browse_id = nav
        .browse_endpoint
        .as_ref()
        .and_then(|b| b.browse_id.clone());
    // A mix opens as the playlist it plays.
    let mix = || {
        let id = nav
            .watch_playlist_endpoint
            .as_ref()?
            .playlist_id
            .as_deref()?;
        (!id.is_empty()).then(|| format!("VL{id}"))
    };
    if let Some(browse_id) = browse_id.or_else(mix) {
        if !(browse_id.starts_with("VL") || browse_id.starts_with("MPREb_")) {
            return None;
        }
        let subtitle = joined(item.subtitle.as_ref());
        return Some(Item::Collection(Collection {
            id: CollectionId {
                provider: ProviderId(PROVIDER.to_owned()),
                id: format!("{BROWSE_PREFIX}{browse_id}"),
            },
            kind: collection_kind(&browse_id),
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
        artist_ids: sub.artist_ids,
        album: sub.album,
        duration: sub.duration,
        artwork_url,
    }))
}

/// One page of the home feed.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct HomePage {
    pub shelves: Vec<Shelf>,
    /// The token that fetches the next page.
    pub next: Option<String>,
}

/// A page of the home feed, the first or a continuation: its carousels, empty ones left out,
/// and the next page's token. Songs here may carry no duration.
pub(crate) fn parse_home(body: &[u8]) -> Result<HomePage, ProviderError> {
    let root = page_root(body, "home")?;
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
                        let card = TwoRowItem::deserialize(v).ok()?;
                        let watch = two_row_nav(&card).and_then(|n| n.watch_endpoint.as_ref());
                        if is_off_music(watch) {
                            return None;
                        }
                        two_row_item(&card)
                    } else {
                        let v = entry.get("musicResponsiveListItemRenderer")?;
                        let row = ListItem::deserialize(v).ok()?;
                        let watch = column_runs(&row, 0)
                            .iter()
                            .find_map(|r| r.navigation_endpoint.as_ref()?.watch_endpoint.as_ref());
                        if is_off_music(watch) {
                            return None;
                        }
                        list_item_track(&row).map(Item::Track)
                    }
                })
                .collect();
            (!items.is_empty()).then(|| Shelf {
                title: joined(title.as_ref()),
                items,
            })
        })
        .collect();
    let mut lists = Vec::new();
    find_all(
        &root,
        &["sectionListRenderer", "sectionListContinuation"],
        &mut lists,
    );
    let next = lists.first().and_then(|l| {
        l.get("continuations")?
            .as_array()?
            .iter()
            .find_map(|c| c.pointer("/nextContinuationData/continuation")?.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    });
    Ok(HomePage { shelves, next })
}

/// The podcast queue every account's Library lists among its playlists.
const EPISODES_FOR_LATER: &str = "browse:VLSE";

/// The playlists on the Library's Playlists tab. Its first tile makes a new playlist and opens
/// nothing, so it is left out with the podcast queue and any other card that is not a
/// collection.
pub(crate) fn parse_library_playlists(body: &[u8]) -> Result<Vec<Collection>, ProviderError> {
    let root = root(body, "library")?;
    let mut grids = Vec::new();
    find_all(&root, &["gridRenderer"], &mut grids);
    let collections = grids
        .iter()
        .filter_map(|g| g.get("items")?.as_array())
        .flatten()
        .filter_map(|entry| {
            let v = entry.get("musicTwoRowItemRenderer")?;
            match two_row_item(&TwoRowItem::deserialize(v).ok()?)? {
                Item::Collection(c) if c.id.id != EPISODES_FOR_LATER => Some(c),
                _ => None,
            }
        })
        .collect();
    Ok(collections)
}

/// The albums, singles and EPs, and playlists an artist's page lists, each in page order.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ArtistReleases {
    pub albums: Vec<Collection>,
    pub singles: Vec<Collection>,
    pub playlists: Vec<Collection>,
}

/// A release card's subtitle as a search shows it ("Album • Artist • 2013"). The artist's page
/// leaves out the artist and, on its album shelf, the kind: "2013", or "Single • 2005".
fn release_subtitle(subtitle: Option<&str>, artist: &str) -> String {
    let mut groups: Vec<&str> = subtitle
        .into_iter()
        .flat_map(|s| s.split(SEPARATOR))
        .filter(|g| !g.is_empty())
        .collect();
    let labelled = groups
        .first()
        .is_some_and(|g| !g.starts_with(|c: char| c.is_ascii_digit()));
    let label = if labelled { groups.remove(0) } else { "Album" };
    let mut parts = vec![label];
    if !artist.is_empty() {
        parts.push(artist);
    }
    parts.extend(groups);
    parts.join(SEPARATOR)
}

/// An artist's albums, singles and EPs, and the YouTube Music playlists that feature them.
/// Shelves are told apart by structure, not by their titles, which are localized: the
/// discography shelves (Albums, then Singles & EPs) link their title to a
/// `MUSIC_PAGE_TYPE_ARTIST_DISCOGRAPHY` page, and the playlists shelf ("Featured on") is the
/// one whose title links nowhere. "Playlists by <artist>" links to the artist and is left out.
/// Release subtitles name `artist`, unless it is empty.
pub(crate) fn parse_artist_releases(
    body: &[u8],
    artist: &str,
) -> Result<ArtistReleases, ProviderError> {
    Ok(artist_releases(&root(body, "artist")?, artist))
}

fn artist_releases(root: &Value, artist: &str) -> ArtistReleases {
    let mut carousels = Vec::new();
    find_all(root, &["musicCarouselShelfRenderer"], &mut carousels);
    let mut found = ArtistReleases::default();
    let mut discographies = 0;
    for carousel in carousels {
        let title_run = "/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0";
        let link = carousel.pointer(&format!("{title_run}/navigationEndpoint"));
        let page_type = link
            .and_then(|l| {
                l.pointer(
                    "/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
                )
            })
            .and_then(Value::as_str);
        let (kind, into) = match (link, page_type) {
            (Some(_), Some("MUSIC_PAGE_TYPE_ARTIST_DISCOGRAPHY")) => {
                discographies += 1;
                let into = if discographies == 1 {
                    &mut found.albums
                } else {
                    &mut found.singles
                };
                (CollectionKind::Album, into)
            }
            (None, _) => (CollectionKind::Playlist, &mut found.playlists),
            _ => continue,
        };
        let cards = carousel
            .get("contents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| TwoRowItem::deserialize(entry.get("musicTwoRowItemRenderer")?).ok())
            .filter_map(|card| match two_row_item(&card)? {
                Item::Collection(c) if c.kind == kind => Some(c),
                _ => None,
            });
        for mut c in cards {
            if kind == CollectionKind::Album {
                c.subtitle = Some(release_subtitle(c.subtitle.as_deref(), artist));
            } else if let Some(rest) = c
                .subtitle
                .as_deref()
                .and_then(|s| s.strip_prefix("Playlist \u{2022} "))
            {
                // Search's playlist hits leave out the kind; drop it so both read alike.
                c.subtitle = Some(rest.to_owned());
            }
            into.push(c);
        }
    }
    found
}

/// The shelves of an artist's page, in this order, each left out when empty.
const ARTIST_TOP_SONGS: &str = "Top songs";
const ARTIST_ALBUMS: &str = "Albums";
const ARTIST_SINGLES: &str = "Singles & EPs";
const ARTIST_FEATURED: &str = "Featured on";

/// An artist's page: its top songs, the page's first song list, then its albums, its singles
/// and EPs, and the playlists it is featured on. The cards leave out the artist, whose page
/// it is.
pub(crate) fn parse_artist_page(body: &[u8]) -> Result<Vec<Shelf>, ProviderError> {
    let root = root(body, "artist")?;
    let mut lists = Vec::new();
    find_all(&root, &["musicShelfRenderer"], &mut lists);
    let mut rows = Vec::new();
    if let Some(contents) = lists.first().and_then(|l| l.get("contents")) {
        collect_items(contents, &mut rows);
    }
    let top: Vec<Item> = rows
        .iter()
        .filter_map(list_item_track)
        .map(Item::Track)
        .collect();
    let releases = artist_releases(&root, "");
    let collections = |cs: Vec<Collection>| cs.into_iter().map(Item::Collection).collect();
    let shelves = [
        (ARTIST_TOP_SONGS, top),
        (ARTIST_ALBUMS, collections(releases.albums)),
        (ARTIST_SINGLES, collections(releases.singles)),
        (ARTIST_FEATURED, collections(releases.playlists)),
    ];
    Ok(shelves
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(title, items)| Shelf {
            title: title.to_owned(),
            items,
        })
        .collect())
}

/// The browse id of the playlist an artist's top songs link to ("Show all"): the page's first
/// song list's `bottomEndpoint`, when it opens a playlist.
fn top_songs_playlist(body: &[u8]) -> Option<String> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let mut lists = Vec::new();
    find_all(&root, &["musicShelfRenderer"], &mut lists);
    let target = lists.first()?.pointer("/bottomEndpoint/browseEndpoint")?;
    let page_type = target.pointer(
        "/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
    );
    if page_type.and_then(Value::as_str) != Some("MUSIC_PAGE_TYPE_PLAYLIST") {
        return None;
    }
    let id = target.get("browseId")?.as_str()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// The artist page's top songs rows leave out their durations, which the playlist they link to
/// has. Fills each missing duration from the playlist row with the same track id; the rows, their
/// order and their count stay the page's. A playlist that can't be had leaves them as they were.
fn fill_top_song_durations(
    body: &[u8],
    shelves: &mut [Shelf],
    fetch_playlist: impl FnOnce(&str) -> Result<Vec<u8>, ProviderError>,
) {
    let Some(top) = shelves.first_mut().filter(|s| s.title == ARTIST_TOP_SONGS) else {
        return;
    };
    let missing = |item: &Item| matches!(item, Item::Track(t) if t.duration.is_none());
    if !top.items.iter().any(missing) {
        return;
    }
    let Some(playlist) = top_songs_playlist(body) else {
        return;
    };
    let durations: HashMap<String, Duration> = match fetch_playlist(&playlist)
        .and_then(|b| parse_collection(&b, CollectionKind::Playlist))
    {
        Ok(shelf) => shelf
            .items
            .into_iter()
            .filter_map(|item| match item {
                Item::Track(t) => Some((t.id.id, t.duration?)),
                Item::Collection(_) => None,
            })
            .collect(),
        Err(e) => {
            debug!("artist's top songs without durations: {e}");
            return;
        }
    };
    for item in &mut top.items {
        if let Item::Track(t) = item
            && t.duration.is_none()
        {
            t.duration = durations.get(&t.id.id).copied();
        }
    }
}

/// The page `browse_id` opens: an artist's shelves, or a playlist or album as one shelf.
/// `fetch_playlist` browses the playlist an artist's top songs link to, for their durations.
pub(crate) fn parse_page(
    body: &[u8],
    browse_id: &str,
    fetch_playlist: impl FnOnce(&str) -> Result<Vec<u8>, ProviderError>,
) -> Result<Vec<Shelf>, ProviderError> {
    match collection_kind(browse_id) {
        CollectionKind::Artist => {
            let mut shelves = parse_artist_page(body)?;
            fill_top_song_durations(body, &mut shelves, fetch_playlist);
            Ok(shelves)
        }
        kind => Ok(vec![parse_collection(body, kind)?]),
    }
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
    let header_artists = header
        .strapline_text_one
        .as_ref()
        .map(|s| parse_subtitle(&s.runs));
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
                if t.artists.is_empty()
                    && let Some(sub) = &header_artists
                {
                    t.artists.clone_from(&sub.artists);
                    t.artist_ids.clone_from(&sub.artist_ids);
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
            CollectionKind::Artist => "Artist".to_owned(),
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
    // Live anonymous capture of Daft Punk's artist page, 2026-10-23: its Top songs, Albums,
    // Singles & EPs, Videos, Featured on, Playlists by and Fans might also like shelves,
    // trimmed.
    const ARTIST: &str = include_str!("../../tests/fixtures/browse_artist.json");
    // The playlist that artist's Top songs link to, 2026-10-23: the five songs the page lists.
    const TOP_SONGS: &str = include_str!("../../tests/fixtures/browse_artist_top_songs.json");

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

    fn artist(browse_id: &str) -> CollectionId {
        CollectionId {
            provider: ProviderId("ytmusic".into()),
            id: format!("browse:{browse_id}"),
        }
    }

    #[test]
    fn home_fixture_shelves() {
        let page = parse_home(HOME.as_bytes()).unwrap();
        assert!(
            page.next
                .as_deref()
                .unwrap()
                .starts_with("4qmFsgKhAhIMRkVtdXNpY19ob21l")
        );
        let shelves = page.shelves;
        let titles: Vec<&str> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Quick picks", "Trending community playlists"]);

        let picks = &shelves[0].items;
        assert_eq!(picks.len(), 3);
        let t = track(&picks[0]);
        assert_eq!(t.id.id, "FV_SqGu4IzE");
        assert_eq!(t.id.provider, ProviderId("ytmusic".into()));
        assert_eq!(t.title, "Amor Factura");
        assert_eq!(t.artists, ["Ricardo Arjona", "Grupo Firme"]);
        assert_eq!(
            t.artist_ids,
            [
                Some(artist("UCGqiM9WAE7sXx2mhoJ_GzOQ")),
                Some(artist("UCJKREB2c3dO8dgdo8FtdO_A"))
            ]
        );
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
        assert_eq!(lists.len(), 4);
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
    fn home_leaves_out_uploads_and_keeps_official_videos() {
        let page = parse_home(HOME.as_bytes()).unwrap();
        let ids: Vec<&str> = page
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .filter_map(|i| match i {
                Item::Track(t) => Some(t.id.id.as_str()),
                Item::Collection(_) => None,
            })
            .collect();
        assert!(!ids.contains(&"ugcRow00001"));
        assert!(!ids.contains(&"ugcCard0001"));
        let video = track(&page.shelves[1].items[3]);
        assert_eq!(video.id.id, "omvCard0001");
        assert_eq!(video.title, "Live Session Clip");
        assert_eq!(video.artists, ["Daft Punk"]);
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
        let HomePage { shelves, next } = parse_home(body.as_bytes()).unwrap();
        assert_eq!(next, None);
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

    // Cut down from a real Home: "Listen together" is three live streams, and the Quick picks
    // rows name their artists in runs that link nowhere.
    const LISTEN_TOGETHER: &str = include_str!("../../tests/fixtures/home_listen_together.json");
    const UNLINKED_ARTISTS: &str = include_str!("../../tests/fixtures/home_unlinked_artists.json");

    #[test]
    fn live_streams_are_not_songs() {
        let raw: Value = serde_json::from_str(LISTEN_TOGETHER).unwrap();
        let mut cards = Vec::new();
        find_all(&raw, &["musicTwoRowItemRenderer"], &mut cards);
        assert_eq!(cards.len(), 3);
        let page = parse_home(LISTEN_TOGETHER.as_bytes()).unwrap();
        assert_eq!(page.shelves, []);

        let body = r#"{"contents":{"sectionListRenderer":{"contents":[
            {"musicCarouselShelfRenderer":{"header":{"musicCarouselShelfBasicHeaderRenderer":
                {"title":{"runs":[{"text":"Rows"}]}}},
             "contents":[
               {"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"live"},
                  "badges":[{"liveBadgeRenderer":{}}],
                  "flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":
                     {"text":{"runs":[{"text":"Live row"}]}}}]}},
               {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Live card"}]},
                  "badges":[{"liveBadgeRenderer":{}}],
                  "navigationEndpoint":{"watchEndpoint":{"videoId":"live2"}}}},
               {"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"song"},
                  "badges":[{"musicInlineBadgeRenderer":{}}],
                  "flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":
                     {"text":{"runs":[{"text":"Song row"}]}}}]}}]}}]}}}"#;
        let page = parse_home(body.as_bytes()).unwrap();
        assert_eq!(ids(&page.shelves[0]), ["song"]);
    }

    #[test]
    fn unlinked_artists_come_from_the_first_group() {
        let page = parse_home(UNLINKED_ARTISTS.as_bytes()).unwrap();
        assert_eq!(page.shelves.len(), 1);
        assert_eq!(page.shelves[0].title, "Quick picks");
        let items = &page.shelves[0].items;
        assert_eq!(items.len(), 3);

        let linked = track(&items[0]);
        assert_eq!(linked.title, "Rip & Tear");
        assert_eq!(linked.artists, ["Mick Gordon"]);
        assert_eq!(
            linked.album.as_deref(),
            Some("DOOM (Original Game Soundtrack)")
        );

        let two = track(&items[1]);
        assert_eq!(two.id.id, "mmHFyfyngMk");
        assert_eq!(two.artists, ["Papa Roach", "Hanumankind"]);
        assert_eq!(
            two.album.as_deref(),
            Some("See U in Hell (from the Netflix Series \"Devil May Cry\")")
        );

        let one = track(&items[2]);
        assert_eq!(one.id.id, "51PbvYmPoDg");
        assert_eq!(one.artists, ["Papa Roach"]);
        assert_eq!(
            one.album.as_deref(),
            Some("Devil May Cry: Season 2 (Soundtrack from the Netflix Series)")
        );
    }

    // Synthetic, hand-built to the paths ytmusicapi reads: a signed-in Home's first page and a
    // continuation of it, and the Library's Playlists tab. No value in them is real.
    const SIGNED_IN_HOME: &str = include_str!("../../tests/fixtures/signed_in_home.json");
    const HOME_CONTINUATION: &str =
        include_str!("../../tests/fixtures/signed_in_home_continuation.json");
    const LIBRARY: &str = include_str!("../../tests/fixtures/signed_in_library_playlists.json");

    fn ids(shelf: &Shelf) -> Vec<&str> {
        shelf
            .items
            .iter()
            .map(|i| match i {
                Item::Track(t) => t.id.id.as_str(),
                Item::Collection(c) => c.id.id.as_str(),
            })
            .collect()
    }

    #[test]
    fn signed_in_home_has_songs_without_durations_mixes_and_a_next_page() {
        let page = parse_home(SIGNED_IN_HOME.as_bytes()).unwrap();
        assert_eq!(page.next.as_deref(), Some("FAKE-CONTINUATION-1"));
        let titles: Vec<&str> = page.shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Quick picks", "Mixed for you", "Listen again"]);

        assert_eq!(ids(&page.shelves[0]), ["fakeVid0001", "fakeVid0002"]);
        let one = track(&page.shelves[0].items[0]);
        assert_eq!(one.title, "Fake Song One");
        assert_eq!(one.artists, ["Fake Artist"]);
        assert_eq!(one.album.as_deref(), Some("Fake Album"));
        assert_eq!(one.duration, None);
        assert_eq!(
            one.artwork_url.as_deref(),
            Some("https://lh3.googleusercontent.com/fake-one=w120-h120")
        );
        let two = track(&page.shelves[0].items[1]);
        assert_eq!(two.artists, ["Another Fake Artist"]);
        assert_eq!(two.duration, Some(Duration::from_secs(3 * 60 + 25)));

        assert_eq!(
            ids(&page.shelves[1]),
            ["browse:VLRDTMAKfakeSupermix01", "browse:VLRDCLAKfakeMix02"]
        );
        let mix = collection(&page.shelves[1].items[0]);
        assert_eq!(mix.kind, CollectionKind::Playlist);
        assert_eq!(mix.title, "My Supermix");
        assert!(mix.artwork_url.as_deref().unwrap().ends_with("w544-h544"));

        // The artist card and the podcast episode are skipped.
        assert_eq!(
            ids(&page.shelves[2]),
            ["fakeVid0003", "browse:MPREb_fakeAlbum01"]
        );
        let card = track(&page.shelves[2].items[0]);
        assert_eq!(card.artists, ["Fake Artist"]);
        assert_eq!(card.artist_ids, [Some(artist("UCfakeArtist0001"))]);
        // Its artist is named but not linked.
        assert_eq!(two.artist_ids, [None]);
    }

    #[test]
    fn a_home_continuation_parses_as_a_page() {
        let page = parse_home(HOME_CONTINUATION.as_bytes()).unwrap();
        assert_eq!(page.next.as_deref(), Some("FAKE-CONTINUATION-2"));
        assert_eq!(page.shelves.len(), 4);
        assert_eq!(page.shelves[0].title, "Your favorites");
        assert_eq!(ids(&page.shelves[1]), ["fakeVid0004"]);
        let last = br#"{"continuationContents":{"sectionListContinuation":{"contents":[]}}}"#;
        assert_eq!(parse_home(last).unwrap(), HomePage::default());
        assert!(matches!(parse_home(b"{}"), Err(ProviderError::Parse(_))));
    }

    #[test]
    fn library_playlists_skip_the_new_playlist_tile_and_unknown_cards() {
        let lists = parse_library_playlists(LIBRARY.as_bytes()).unwrap();
        let ids: Vec<&str> = lists.iter().map(|c| c.id.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "browse:VLLM",
                "browse:VLPLfakeRoadTrip01",
                "browse:VLPLfakeNight02"
            ]
        );
        let road = &lists[1];
        assert_eq!(road.title, "Road trip");
        assert_eq!(
            road.subtitle.as_deref(),
            Some("Playlist • Requiem Tester • 12 tracks")
        );
        assert!(road.artwork_url.is_some());
        assert!(matches!(
            parse_library_playlists(b"{}"),
            Err(ProviderError::Parse(_))
        ));
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
        assert_eq!(t.artist_ids, [Some(artist("UCHiQKgD-JprgAKU75eukOQg"))]);
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
        // The rows link no artist; the header's link is the album's artist.
        assert_eq!(t.artist_ids, [Some(artist(DAFT_PUNK))]);
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
    fn artist_page_releases_are_found_by_shelf_structure() {
        let found = parse_artist_releases(ARTIST.as_bytes(), "Daft Punk").unwrap();
        let titles = |cs: &[Collection]| cs.iter().map(|c| c.title.clone()).collect::<Vec<_>>();
        assert_eq!((found.albums.len(), found.singles.len()), (10, 10));
        assert_eq!(
            titles(&found.albums)[..5],
            [
                "Random Access Memories (Drumless Edition)",
                "Random Access Memories",
                "TRON: Legacy",
                "Human After All",
                "Discovery",
            ]
        );
        assert_eq!(found.albums[5].title, "Homework (25th Anniversary Edition)");
        assert_eq!(found.singles[0].title, "GLBTM (Studio Outtakes)");
        assert!(
            found
                .albums
                .iter()
                .chain(&found.singles)
                .all(|a| a.kind == CollectionKind::Album
                    && a.id.id.starts_with("browse:MPREb_")
                    && a.artwork_url.is_some())
        );
        assert_eq!(
            found.albums[1].subtitle.as_deref(),
            Some("Album \u{2022} Daft Punk \u{2022} 2013")
        );
        assert_eq!(
            found.singles[3].subtitle.as_deref(),
            Some("Single \u{2022} Daft Punk \u{2022} 2005")
        );
        assert_eq!(
            titles(&found.playlists),
            [
                "Presenting Daft Punk",
                "Best Of French Touch",
                "Going For Gold",
                "Summer Dance Classics",
            ]
        );
        assert_eq!(
            found.playlists[0].id.id,
            "browse:VLRDCLAK5uy_n20FRYQXNt1p1wS55Nj2r14IouO5weaYU"
        );
        assert_eq!(
            found.playlists[0].subtitle.as_deref(),
            Some("YouTube Music")
        );
        assert!(
            found
                .playlists
                .iter()
                .all(|p| p.kind == CollectionKind::Playlist)
        );
    }

    #[test]
    fn artist_page_shelves_come_in_page_order_after_its_top_songs() {
        let shelves = parse_artist_page(ARTIST.as_bytes()).unwrap();
        let titles: Vec<&str> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Top songs", "Albums", "Singles & EPs", "Featured on"]
        );
        let counts: Vec<usize> = shelves.iter().map(|s| s.items.len()).collect();
        assert_eq!(counts, [5, 10, 10, 4]);
        let top: Vec<&Track> = shelves[0].items.iter().map(track).collect();
        assert_eq!(top[0].id.id, "khnokW3Mw24");
        assert_eq!(top[0].title, "Instant Crush (feat. Julian Casablancas)");
        assert_eq!(top[0].artists, ["Daft Punk", "Julian Casablancas"]);
        assert_eq!(
            top[0].artist_ids,
            [
                Some(artist(DAFT_PUNK)),
                Some(artist("UCWhpbdGLnR8XtXLr0yl2nYA"))
            ]
        );
        assert_eq!(top[0].album.as_deref(), Some("Random Access Memories"));
        assert_eq!(top[4].title, "Veridis Quo");
        let collections = |i: usize| -> Vec<&Collection> {
            shelves[i]
                .items
                .iter()
                .map(|item| match item {
                    Item::Collection(c) => c,
                    Item::Track(t) => panic!("{} is no collection", t.title),
                })
                .collect()
        };
        assert_eq!(collections(1)[1].title, "Random Access Memories");
        // The page is the artist's own, so its cards leave the artist out.
        assert_eq!(
            collections(1)[1].subtitle.as_deref(),
            Some("Album \u{2022} 2013")
        );
        assert_eq!(collections(2)[0].title, "GLBTM (Studio Outtakes)");
        assert_eq!(
            collections(2)[3].subtitle.as_deref(),
            Some("Single \u{2022} 2005")
        );
        assert_eq!(collections(3)[0].title, "Presenting Daft Punk");
        assert!(
            collections(3)
                .iter()
                .all(|c| c.kind == CollectionKind::Playlist)
        );
    }

    #[test]
    fn an_artist_page_leaves_out_its_empty_shelves() {
        let v: Value = serde_json::from_str(ARTIST).unwrap();
        let path = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";
        let sections = v.pointer(path).unwrap().as_array().unwrap();
        // Featured on alone, without top songs or releases.
        let only_featured = serde_json::json!({"contents": [sections[4]]});
        let shelves = parse_artist_page(only_featured.to_string().as_bytes()).unwrap();
        let titles: Vec<&str> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Featured on"]);
        assert!(parse_artist_page(br#"{"contents":{}}"#).unwrap().is_empty());
        assert!(matches!(
            parse_artist_page(b"{}"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn a_browse_id_picks_its_page_parser() {
        let artist = parse_page(ARTIST.as_bytes(), DAFT_PUNK, |_| {
            Err(ProviderError::Parse("no playlist".into()))
        })
        .unwrap();
        assert_eq!(artist, parse_artist_page(ARTIST.as_bytes()).unwrap());
        let album = parse_page(ALBUM.as_bytes(), "MPREb_x", |_| unreachable!()).unwrap();
        assert_eq!(
            album,
            [parse_collection(ALBUM.as_bytes(), CollectionKind::Album).unwrap()]
        );
    }

    const DAFT_PUNK: &str = "UCRr1xG_2WIDs18a6cIiCxeA";
    const TOP_SONGS_ID: &str = "VLOLAK5uy_lNVBcjNtiCwq-n95uoxJ-Gd4vsfMkyZNs";

    fn top_songs(shelves: &[Shelf]) -> Vec<&Track> {
        assert_eq!(shelves[0].title, "Top songs");
        shelves[0].items.iter().map(track).collect()
    }

    #[test]
    fn an_artists_top_songs_link_to_their_playlist() {
        assert_eq!(
            top_songs_playlist(ARTIST.as_bytes()).as_deref(),
            Some(TOP_SONGS_ID)
        );
        let mut v: Value = serde_json::from_str(ARTIST).unwrap();
        let path = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicShelfRenderer/bottomEndpoint/browseEndpoint";
        v.pointer_mut(path).unwrap()["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]
            ["pageType"] = Value::from("MUSIC_PAGE_TYPE_ALBUM");
        assert_eq!(top_songs_playlist(v.to_string().as_bytes()), None);
        v.pointer_mut(path).unwrap().take();
        assert_eq!(top_songs_playlist(v.to_string().as_bytes()), None);
        assert_eq!(top_songs_playlist(br#"{"contents":{}}"#), None);
        assert_eq!(top_songs_playlist(b"nope"), None);
    }

    #[test]
    fn top_songs_take_their_durations_from_the_playlist_by_track_id() {
        let mut asked = Vec::new();
        let shelves = parse_page(ARTIST.as_bytes(), DAFT_PUNK, |id| {
            asked.push(id.to_owned());
            Ok(TOP_SONGS.as_bytes().to_vec())
        })
        .unwrap();
        assert_eq!(asked, [TOP_SONGS_ID]);
        let bare = parse_artist_page(ARTIST.as_bytes()).unwrap();
        assert!(top_songs(&bare).iter().all(|t| t.duration.is_none()));

        let secs: Vec<Option<u64>> = top_songs(&shelves)
            .iter()
            .map(|t| t.duration.map(|d| d.as_secs()))
            .collect();
        assert_eq!(
            secs,
            [
                Some(5 * 60 + 38),
                Some(4 * 60 + 9),
                Some(5 * 60 + 21),
                Some(7 * 60 + 10),
                Some(5 * 60 + 46)
            ]
        );
        // The page's own rows, order and count stay.
        let mut filled = shelves.clone();
        for item in &mut filled[0].items {
            if let Item::Track(t) = item {
                t.duration = None;
            }
        }
        assert_eq!(filled, bare);
    }

    #[test]
    fn a_top_song_the_playlist_lacks_stays_without_a_duration() {
        let mut v: Value = serde_json::from_str(TOP_SONGS).unwrap();
        let path = "/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents/0/musicPlaylistShelfRenderer/contents";
        v.pointer_mut(path)
            .unwrap()
            .as_array_mut()
            .unwrap()
            .remove(1);
        let shelves = parse_page(ARTIST.as_bytes(), DAFT_PUNK, |_| {
            Ok(v.to_string().into_bytes())
        })
        .unwrap();
        let top = top_songs(&shelves);
        assert_eq!(top.len(), 5);
        assert_eq!(top[0].duration, Some(Duration::from_secs(5 * 60 + 38)));
        assert_eq!(top[1].duration, None);
        assert_eq!(top[2].duration, Some(Duration::from_secs(5 * 60 + 21)));
    }

    #[test]
    fn a_playlist_that_fails_leaves_the_artist_page_as_it_was() {
        let bare = parse_artist_page(ARTIST.as_bytes()).unwrap();
        let failed = parse_page(ARTIST.as_bytes(), DAFT_PUNK, |_| {
            Err(ProviderError::Network("offline".into()))
        })
        .unwrap();
        assert_eq!(failed, bare);
        let garbled = parse_page(ARTIST.as_bytes(), DAFT_PUNK, |_| Ok(b"nope".to_vec())).unwrap();
        assert_eq!(garbled, bare);
        assert_eq!(top_songs(&failed).len(), 5);
    }

    #[test]
    fn release_subtitles_take_the_kind_and_artist_a_search_shows() {
        let sub = |s: Option<&str>| release_subtitle(s, "Daft Punk");
        assert_eq!(sub(Some("2013")), "Album \u{2022} Daft Punk \u{2022} 2013");
        assert_eq!(
            sub(Some("EP \u{2022} 2005")),
            "EP \u{2022} Daft Punk \u{2022} 2005"
        );
        assert_eq!(sub(None), "Album \u{2022} Daft Punk");
        assert_eq!(release_subtitle(Some("2013"), ""), "Album \u{2022} 2013");
    }

    #[test]
    fn artist_page_without_releases_has_none() {
        let found = parse_artist_releases(br#"{"contents":{}}"#, "A").unwrap();
        assert_eq!(found, ArtistReleases::default());
        assert!(matches!(
            parse_artist_releases(b"{}", "A"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn collection_kinds_and_bad_bodies() {
        assert_eq!(collection_kind("MPREb_x"), CollectionKind::Album);
        assert_eq!(collection_kind("VLPLx"), CollectionKind::Playlist);
        assert_eq!(collection_kind("UCx"), CollectionKind::Artist);
        assert!(matches!(parse_home(b"{}"), Err(ProviderError::Parse(_))));
        assert!(matches!(
            parse_collection(b"nope", CollectionKind::Album),
            Err(ProviderError::Parse(_))
        ));
    }
}
