//! Response parsers. Every field is optional so a missing one never fails deserialization;
//! a structurally unusable response is `ProviderError::Parse`.

use std::collections::BTreeSet;
use std::time::Duration;

use requiem_core::{
    Collection, CollectionId, CollectionKind, FormatHint, ProviderError, ProviderId, Track, TrackId,
};
use serde::Deserialize;
use serde_json::Value;

mod browse;
mod radio;

pub(crate) use browse::{
    ArtistReleases, BROWSE_PREFIX, collection_kind, parse_artist_releases, parse_collection,
    parse_home, parse_library_playlists, parse_page,
};
pub(crate) use radio::parse_radio;

pub(crate) const PROVIDER: &str = "ytmusic";

fn parse_err(msg: impl Into<String>) -> ProviderError {
    ProviderError::Parse(msg.into())
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ResponseContext {
    visitor_data: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct VisitorEnvelope {
    response_context: Option<ResponseContext>,
}

pub(crate) fn visitor_data(body: &[u8]) -> Option<String> {
    let env: VisitorEnvelope = serde_json::from_slice(body).ok()?;
    env.response_context?.visitor_data.filter(|v| !v.is_empty())
}

/// Whether YouTube took the request as signed in: the `logged_in` tracking param, which a
/// response may leave out.
pub(crate) fn logged_in(root: &Value) -> Option<bool> {
    root.pointer("/responseContext/serviceTrackingParams")?
        .as_array()?
        .iter()
        .filter_map(|s| s.get("params")?.as_array())
        .flatten()
        .find(|p| p.get("key").and_then(Value::as_str) == Some("logged_in"))
        .and_then(|p| p.get("value")?.as_str())
        .map(|v| v == "1")
}

// ---------------------------------------------------------------- search

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ListItem {
    playlist_item_data: Option<PlaylistItemData>,
    navigation_endpoint: Option<NavEndpoint>,
    flex_columns: Vec<FlexColumn>,
    fixed_columns: Vec<FixedColumn>,
    thumbnail: Option<Thumbnail>,
    badges: Vec<Value>,
    subtitle_badges: Vec<Value>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PlaylistItemData {
    video_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FlexColumn {
    #[serde(rename = "musicResponsiveListItemFlexColumnRenderer")]
    renderer: Option<FlexRenderer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FixedColumn {
    #[serde(rename = "musicResponsiveListItemFixedColumnRenderer")]
    renderer: Option<FlexRenderer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FlexRenderer {
    text: Option<Runs>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Runs {
    runs: Vec<Run>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Run {
    text: Option<String>,
    navigation_endpoint: Option<NavEndpoint>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct NavEndpoint {
    browse_endpoint: Option<BrowseEndpoint>,
    watch_endpoint: Option<WatchEndpoint>,
    watch_playlist_endpoint: Option<WatchPlaylistEndpoint>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WatchPlaylistEndpoint {
    playlist_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct BrowseEndpoint {
    browse_id: Option<String>,
    browse_endpoint_context_supported_configs: Option<BrowseConfigs>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WatchEndpoint {
    video_id: Option<String>,
    watch_endpoint_music_supported_configs: Option<WatchConfigs>,
}

impl WatchEndpoint {
    /// Innertube's `MUSIC_VIDEO_TYPE_*` for what the video is.
    fn music_video_type(&self) -> Option<&str> {
        self.watch_endpoint_music_supported_configs
            .as_ref()?
            .watch_endpoint_music_config
            .as_ref()?
            .music_video_type
            .as_deref()
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WatchConfigs {
    watch_endpoint_music_config: Option<WatchMusicConfig>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WatchMusicConfig {
    music_video_type: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct BrowseConfigs {
    browse_endpoint_context_music_config: Option<MusicConfig>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct MusicConfig {
    page_type: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Thumbnail {
    #[serde(rename = "musicThumbnailRenderer")]
    renderer: Option<ThumbnailRenderer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ThumbnailRenderer {
    thumbnail: Option<ThumbnailList>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ThumbnailList {
    thumbnails: Vec<Thumb>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Thumb {
    url: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

fn collect_items(v: &Value, out: &mut Vec<ListItem>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == "musicResponsiveListItemRenderer" {
                    if let Ok(item) = ListItem::deserialize(child) {
                        out.push(item);
                    }
                } else {
                    collect_items(child, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| collect_items(c, out)),
        _ => {}
    }
}

/// `m:ss` or `h:mm:ss`.
fn parse_duration(s: &str) -> Option<Duration> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let secs = parts
        .iter()
        .try_fold(0u64, |acc, p| Some(acc * 60 + p.parse::<u64>().ok()?))?;
    Some(Duration::from_secs(secs))
}

const SEPARATOR: &str = " \u{2022} ";

/// Whether a card or row carries a live badge: a stream, not a song that can be played.
fn is_live(badges: &[Value], subtitle_badges: &[Value]) -> bool {
    badges
        .iter()
        .chain(subtitle_badges)
        .any(|b| b.get("liveBadgeRenderer").is_some())
}

/// What a subtitle's leading group says about the item, not who made it.
const TYPE_LABELS: [&str; 4] = ["Song", "Video", "Single", "EP"];
/// Unlinked runs between artists in a subtitle.
const ARTIST_SEPARATORS: [&str; 2] = [" & ", ", "];

/// A group of a subtitle between " \u{2022} " runs.
struct Group {
    text: String,
    /// The group's text split at its unlinked artist separators.
    parts: Vec<String>,
}

struct Subtitle {
    artists: Vec<String>,
    /// The artist page of each of `artists`, as linked runs give it.
    artist_ids: Vec<Option<CollectionId>>,
    album: Option<String>,
    duration: Option<Duration>,
}

/// Column 1: artists, album and duration separated by " \u{2022} " runs. Typed runs
/// (`pageType`) win; without any, the first group is the artist and the second the album.
/// A linked album with no linked artist takes its artists from the first group that is
/// neither a count, a type label nor the album, split at unlinked " & " and ", " runs.
fn parse_subtitle<'a>(runs: impl IntoIterator<Item = &'a Run>) -> Subtitle {
    let mut artists = Vec::new();
    let mut artist_ids = Vec::new();
    let mut album = None;
    let mut duration = None;
    let mut groups = vec![Group {
        text: String::new(),
        parts: vec![String::new()],
    }];
    for run in runs {
        let Some(text) = run.text.as_deref() else {
            continue;
        };
        if text == SEPARATOR {
            groups.push(Group {
                text: String::new(),
                parts: vec![String::new()],
            });
            continue;
        }
        if let Some(d) = parse_duration(text) {
            duration = Some(d);
            continue;
        }
        let browse = run
            .navigation_endpoint
            .as_ref()
            .and_then(|n| n.browse_endpoint.as_ref());
        let page_type = browse
            .and_then(|b| b.browse_endpoint_context_supported_configs.as_ref())
            .and_then(|c| c.browse_endpoint_context_music_config.as_ref())
            .and_then(|m| m.page_type.as_deref());
        let group = groups.last_mut().expect("non-empty");
        group.text.push_str(text);
        if page_type.is_none() && ARTIST_SEPARATORS.contains(&text) {
            group.parts.push(String::new());
        } else {
            group.parts.last_mut().expect("non-empty").push_str(text);
        }
        match page_type {
            Some("MUSIC_PAGE_TYPE_ARTIST") => {
                artists.push(text.to_owned());
                artist_ids.push(browse.and_then(|b| artist_id(b.browse_id.as_deref()?)));
            }
            Some("MUSIC_PAGE_TYPE_ALBUM") => album = Some(text.to_owned()),
            _ => {}
        }
    }
    let counted = |g: &Group| g.text.ends_with(" views") || g.text.ends_with(" plays");
    if artists.is_empty() && album.is_none() {
        // Video results put a view or play count where a song has its album.
        let mut untyped = groups
            .into_iter()
            .map(|g| g.text)
            .filter(|g| !g.is_empty() && !g.ends_with(" views") && !g.ends_with(" plays"));
        if let Some(a) = untyped.next() {
            artists.push(a);
        }
        album = untyped.next();
    } else if artists.is_empty() {
        let mut named = groups
            .into_iter()
            .filter(|g| !g.text.is_empty() && !counted(g) && Some(&g.text) != album.as_ref());
        let first = named
            .next()
            .filter(|g| !TYPE_LABELS.contains(&g.text.as_str()));
        if let Some(g) = first.or_else(|| named.next()) {
            artists = g
                .parts
                .iter()
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }
    // Untyped runs link nowhere.
    artist_ids.resize(artists.len(), None);
    Subtitle {
        artists,
        artist_ids,
        album,
        duration,
    }
}

/// The artist page browse id `browse_id` opens, if it is one.
fn artist_id(browse_id: &str) -> Option<CollectionId> {
    (collection_kind(browse_id) == CollectionKind::Artist).then(|| CollectionId {
        provider: ProviderId(PROVIDER.to_owned()),
        id: format!("{BROWSE_PREFIX}{browse_id}"),
    })
}

fn column_runs(item: &ListItem, i: usize) -> &[Run] {
    item.flex_columns
        .get(i)
        .and_then(|c| c.renderer.as_ref())
        .and_then(|r| r.text.as_ref())
        .map_or(&[], |t| t.runs.as_slice())
}

fn largest_thumbnail(item: &ListItem) -> Option<String> {
    largest(item.thumbnail.as_ref())
}

fn largest(thumbnail: Option<&Thumbnail>) -> Option<String> {
    thumbnail?
        .renderer
        .as_ref()?
        .thumbnail
        .as_ref()?
        .thumbnails
        .iter()
        .filter(|t| t.url.is_some())
        .max_by_key(|t| u64::from(t.width.unwrap_or(0)) * u64::from(t.height.unwrap_or(0)))
        .and_then(|t| t.url.clone())
}

/// A search response's rows.
fn search_items(body: &[u8]) -> Result<Vec<ListItem>, ProviderError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("search json: {e}")))?;
    let Some(obj) = root.as_object() else {
        return Err(parse_err("search response is not an object"));
    };
    // An empty result set still carries `contents`; its absence means the shape changed.
    if !obj.contains_key("contents") {
        return Err(parse_err("search response has no contents"));
    }
    let mut items = Vec::new();
    collect_items(&root, &mut items);
    Ok(items)
}

pub(crate) fn parse_search(body: &[u8], limit: usize) -> Result<Vec<Track>, ProviderError> {
    let items = search_items(body)?;
    let tracks = items
        .iter()
        .filter_map(|item| {
            let video_id = item
                .playlist_item_data
                .as_ref()?
                .video_id
                .as_deref()
                .filter(|v| !v.is_empty())?;
            let title: String = column_runs(item, 0)
                .iter()
                .filter_map(|r| r.text.as_deref())
                .collect();
            let sub = parse_subtitle(column_runs(item, 1));
            Some(Track {
                id: TrackId {
                    provider: ProviderId(PROVIDER.to_owned()),
                    id: video_id.to_owned(),
                },
                title,
                artists: sub.artists,
                artist_ids: sub.artist_ids,
                album: sub.album,
                duration: sub.duration,
                artwork_url: largest_thumbnail(item),
            })
        })
        .take(limit)
        .collect();
    Ok(tracks)
}

/// The rows of an artists, albums or playlists search that open as a collection of `kind`: the
/// row's browse id, its title, and its subtitle as shown ("Artist • 68.7M monthly audience",
/// "Album • Artist • 2013", "YouTube Music • 35 songs").
pub(crate) fn parse_search_collections(
    body: &[u8],
    kind: CollectionKind,
    limit: usize,
) -> Result<Vec<Collection>, ProviderError> {
    let items = search_items(body)?;
    let collections = items
        .iter()
        .filter_map(|item| {
            let first_run = column_runs(item, 0).first();
            let nav = item
                .navigation_endpoint
                .as_ref()
                .or_else(|| first_run?.navigation_endpoint.as_ref())?;
            let browse_id = nav
                .browse_endpoint
                .as_ref()?
                .browse_id
                .as_deref()
                .filter(|id| ["MPREb_", "VL", "UC"].iter().any(|p| id.starts_with(p)))?;
            if collection_kind(browse_id) != kind {
                return None;
            }
            let text = |i| -> String {
                column_runs(item, i)
                    .iter()
                    .filter_map(|r| r.text.as_deref())
                    .collect()
            };
            let subtitle = text(1);
            Some(Collection {
                id: CollectionId {
                    provider: ProviderId(PROVIDER.to_owned()),
                    id: format!("{BROWSE_PREFIX}{browse_id}"),
                },
                kind,
                title: text(0),
                subtitle: Some(subtitle.trim().to_owned()).filter(|s| !s.is_empty()),
                artwork_url: largest_thumbnail(item),
            })
        })
        .take(limit)
        .collect();
    Ok(collections)
}

/// The artist the first section of an unfiltered search is a card for, if it is one, as the
/// artists search lists it: the card for a song, album or playlist leaves this `None`.
pub(crate) fn parse_top_artist(body: &[u8]) -> Result<Option<Collection>, ProviderError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("search json: {e}")))?;
    if root.get("contents").is_none() {
        return Err(parse_err("search response has no contents"));
    }
    let Some(card) = root
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer")
    else {
        return Ok(None);
    };
    let runs = |key| card.get(key).and_then(|v| Runs::deserialize(v).ok());
    let run = runs("title").and_then(|t| t.runs.into_iter().next());
    let artist = run.and_then(|r| {
        let browse = r.navigation_endpoint?.browse_endpoint?;
        let page_type = browse
            .browse_endpoint_context_supported_configs?
            .browse_endpoint_context_music_config?
            .page_type?;
        let browse_id = browse.browse_id.filter(|id| !id.is_empty())?;
        let subtitle: String = runs("subtitle")
            .map(|s| s.runs.into_iter().filter_map(|r| r.text).collect())
            .unwrap_or_default();
        let thumbnail = card
            .get("thumbnail")
            .and_then(|t| Thumbnail::deserialize(t).ok());
        (page_type == "MUSIC_PAGE_TYPE_ARTIST").then(|| Collection {
            id: CollectionId {
                provider: ProviderId(PROVIDER.to_owned()),
                id: format!("{BROWSE_PREFIX}{browse_id}"),
            },
            kind: CollectionKind::Artist,
            title: r.text.unwrap_or_default(),
            subtitle: Some(subtitle.trim().to_owned()).filter(|s| !s.is_empty()),
            artwork_url: largest(thumbnail.as_ref()),
        })
    });
    Ok(artist)
}

/// `first` then `then`, a collection once however often it appears, at most `limit`.
pub(crate) fn merge_collections(
    first: Vec<Collection>,
    then: Vec<Collection>,
    limit: usize,
) -> Vec<Collection> {
    let mut seen = BTreeSet::new();
    first
        .into_iter()
        .chain(then)
        .filter(|c| seen.insert(c.id.id.clone()))
        .take(limit)
        .collect()
}

/// Byte length and the `*Renderer` key paths down to 10 object levels; never any text content.
pub(crate) fn structure_summary(body: &[u8]) -> String {
    fn walk(v: &Value, path: &str, depth: usize, out: &mut BTreeSet<String>) {
        if depth > 10 {
            return;
        }
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    let next = if k.ends_with("Renderer") {
                        let p = format!("{path}>{k}");
                        out.insert(p.clone());
                        p
                    } else {
                        path.to_owned()
                    };
                    walk(child, &next, depth + 1, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, path, depth, out)),
            _ => {}
        }
    }
    let mut paths = BTreeSet::new();
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => walk(&v, "", 0, &mut paths),
        Err(_) => return format!("{} bytes, not json", body.len()),
    }
    let list: Vec<String> = paths.into_iter().collect();
    format!("{} bytes, renderers: {}", body.len(), list.join(", "))
}

// ---------------------------------------------------------------- player

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PlayerResponse {
    playability_status: Option<Playability>,
    streaming_data: Option<StreamingData>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Playability {
    status: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StreamingData {
    adaptive_formats: Vec<Format>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Format {
    itag: Option<u32>,
    url: Option<String>,
    signature_cipher: Option<String>,
    cipher: Option<String>,
    mime_type: Option<String>,
    content_length: Option<String>,
    average_bitrate: Option<u64>,
}

/// A stream URL picked from a player response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamFormat {
    pub url: String,
    pub itag: u32,
    pub content_length: u64,
    pub average_bitrate: Option<u64>,
    pub hint: FormatHint,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlayerOutcome {
    Stream(StreamFormat),
    LoginRequired,
}

pub(crate) fn hint_for(itag: u32) -> FormatHint {
    if itag == 140 {
        FormatHint::Mp4Aac
    } else {
        FormatHint::WebmOpus
    }
}

pub(crate) fn itag_order(prefer_aac: bool) -> [u32; 4] {
    if prefer_aac {
        [140, 251, 250, 249]
    } else {
        [251, 250, 249, 140]
    }
}

fn is_audio(f: &Format) -> bool {
    f.mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("audio/"))
}

pub(crate) fn classify_player(
    body: &[u8],
    prefer_aac: bool,
) -> Result<PlayerOutcome, ProviderError> {
    let resp: PlayerResponse =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("player json: {e}")))?;
    let Some(playability) = resp.playability_status else {
        return Err(parse_err("player response has no playabilityStatus"));
    };
    let Some(status) = playability.status.as_deref() else {
        return Err(parse_err("playabilityStatus has no status"));
    };
    let reason = playability.reason.unwrap_or_else(|| status.to_owned());
    match status {
        "OK" => {}
        "LOGIN_REQUIRED" => return Ok(PlayerOutcome::LoginRequired),
        "UNPLAYABLE"
        | "ERROR"
        | "AGE_CHECK_REQUIRED"
        | "CONTENT_CHECK_REQUIRED"
        | "LIVE_STREAM_OFFLINE" => return Err(ProviderError::Unplayable(reason)),
        other => {
            crate::log::debug!("unknown playability status {other}");
            return Err(ProviderError::Unplayable(reason));
        }
    }

    let formats = resp.streaming_data.unwrap_or_default().adaptive_formats;
    let audio: Vec<&Format> = formats.iter().filter(|f| is_audio(f)).collect();
    if audio.is_empty() {
        return Err(ProviderError::Unplayable(
            "no audio formats offered".to_owned(),
        ));
    }
    for itag in itag_order(prefer_aac) {
        let Some(f) = audio.iter().find(|f| f.itag == Some(itag)) else {
            continue;
        };
        let (Some(url), Some(len)) = (
            f.url.as_deref().filter(|u| !u.is_empty()),
            f.content_length
                .as_deref()
                .and_then(|l| l.parse::<u64>().ok()),
        ) else {
            continue;
        };
        return Ok(PlayerOutcome::Stream(StreamFormat {
            url: url.to_owned(),
            itag,
            content_length: len,
            average_bitrate: f.average_bitrate,
            hint: hint_for(itag),
        }));
    }
    let ciphered = audio
        .iter()
        .any(|f| f.signature_cipher.is_some() || f.cipher.is_some());
    // Audio is offered but none of it is usable: the client no longer yields plain URLs
    // (cipher / SABR-only), or the itag set changed.
    Err(ProviderError::ClientBroken(if ciphered {
        "audio formats are ciphered".to_owned()
    } else {
        "no usable audio format (missing url, length or known itag)".to_owned()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = include_str!("../tests/fixtures/search_songs.json");
    // Live anonymous captures of "daft punk" with the Albums and Featured playlists filters,
    // 2026-10-23: the first rows, trimmed of menus and tracking, visitorData scrubbed.
    const SEARCH_ALBUMS: &str = include_str!("../tests/fixtures/search_albums.json");
    const SEARCH_PLAYLISTS: &str = include_str!("../tests/fixtures/search_playlists.json");
    // The same query unfiltered: the artist card first, then the first rows of two sections.
    const SEARCH_TOP_ARTIST: &str = include_str!("../tests/fixtures/search_top_artist.json");
    // The same query with the Artists filter, 2026-10-23: the first six rows, trimmed alike.
    const SEARCH_ARTISTS: &str = include_str!("../tests/fixtures/search_artists.json");
    const OK: &str = include_str!("../tests/fixtures/player_ok.json");
    const LOGIN: &str = include_str!("../tests/fixtures/player_login_required.json");
    const ERROR: &str = include_str!("../tests/fixtures/player_error.json");
    // Hand-edited from player_ok.json (status/reason swapped, streamingData removed):
    // UNPLAYABLE, AGE_CHECK_REQUIRED, no-audio (video formats only) and cipher-only.
    // The rest are real captures, scrubbed. ERROR comes from a nonexistent video id.
    const UNPLAYABLE: &str = include_str!("../tests/fixtures/player_unplayable.json");
    const AGE: &str = include_str!("../tests/fixtures/player_age_check.json");
    const NO_AUDIO: &str = include_str!("../tests/fixtures/player_no_audio.json");
    const CIPHER: &str = include_str!("../tests/fixtures/player_cipher_only.json");
    const MALFORMED: &str = include_str!("../tests/fixtures/player_malformed.json");

    #[test]
    fn search_fixture_fields() {
        let tracks = parse_search(SEARCH.as_bytes(), 50).unwrap();
        assert_eq!(tracks.len(), 8);
        let t = &tracks[0];
        assert_eq!(t.id.id, "Jb6gcoR266U");
        assert_eq!(t.id.provider, ProviderId("ytmusic".into()));
        assert_eq!(t.title, "Around the World");
        assert_eq!(t.artists, vec!["Daft Punk"]);
        assert_eq!(t.artist_ids, [Some(artist("UCRr1xG_2WIDs18a6cIiCxeA"))]);
        assert_eq!(t.album.as_deref(), Some("Homework"));
        assert_eq!(t.duration, Some(Duration::from_secs(7 * 60 + 10)));
        assert!(t.artwork_url.as_deref().unwrap().contains("w120-h120"));
        assert!(
            tracks
                .iter()
                .all(|t| !t.title.is_empty() && t.duration.is_some())
        );
        assert_eq!(
            tracks[1].artist_ids,
            [Some(artist("UCfARTWWQuS2V8Dsc-7mQ2Ug"))]
        );
    }

    fn artist(browse_id: &str) -> CollectionId {
        CollectionId {
            provider: ProviderId("ytmusic".into()),
            id: format!("browse:{browse_id}"),
        }
    }

    #[test]
    fn search_respects_limit() {
        assert_eq!(parse_search(SEARCH.as_bytes(), 3).unwrap().len(), 3);
    }

    #[test]
    fn search_skips_items_without_video_id() {
        let body = br#"{"contents":{"a":[
            {"musicResponsiveListItemRenderer":{"flexColumns":[]}},
            {"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"abc"}}}]}}"#;
        let tracks = parse_search(body, 10).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id.id, "abc");
        assert!(tracks[0].artists.is_empty());
    }

    #[test]
    fn search_untyped_subtitle_falls_back_to_groups() {
        let body = r#"{"contents":{"x":{"musicResponsiveListItemRenderer":{
            "playlistItemData":{"videoId":"v"},
            "flexColumns":[
              {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"T"}]}}},
              {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[
                {"text":"A"},{"text":" & "},{"text":"B"},{"text":" \u2022 "},
                {"text":"Alb"},{"text":" \u2022 "},{"text":"1:02:03"}]}}}]}}}}"#;
        let t = &parse_search(body.as_bytes(), 1).unwrap()[0];
        assert_eq!(t.artists, vec!["A & B"]);
        // Runs that link nowhere open no artist page.
        assert_eq!(t.artist_ids, [None]);
        assert_eq!(t.album.as_deref(), Some("Alb"));
        assert_eq!(t.duration, Some(Duration::from_secs(3723)));
    }

    #[test]
    fn search_untyped_view_count_is_not_an_album() {
        let body = r#"{"contents":{"x":{"musicResponsiveListItemRenderer":{
            "playlistItemData":{"videoId":"v"},
            "flexColumns":[
              {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"T"}]}}},
              {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[
                {"text":"A"},{"text":" • "},{"text":"4.8K views"},
                {"text":" • "},{"text":"2:32"}]}}}]}}}}"#;
        let t = &parse_search(body.as_bytes(), 1).unwrap()[0];
        assert_eq!(t.artists, vec!["A"]);
        assert_eq!(t.album, None);
    }

    fn subtitle_of(runs: &str) -> Subtitle {
        let runs: Vec<Run> = serde_json::from_str(runs).unwrap();
        parse_subtitle(&runs)
    }

    const ALBUM: &str = r#"{"text":"Alb","navigationEndpoint":{"browseEndpoint":{"browseId":"MPREb_x",
        "browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":
        {"pageType":"MUSIC_PAGE_TYPE_ALBUM"}}}}}"#;

    #[test]
    fn unlinked_artists_split_at_unlinked_separators() {
        let sub = subtitle_of(&format!(
            r#"[{{"text":"A"}},{{"text":" & "}},{{"text":"B"}},{{"text":", "}},{{"text":"C"}},
               {{"text":" \u2022 "}},{{"text":"12M plays"}},{{"text":" \u2022 "}},{ALBUM}]"#
        ));
        assert_eq!(sub.artists, ["A", "B", "C"]);
        assert_eq!(sub.album.as_deref(), Some("Alb"));

        let sub = subtitle_of(&format!(
            r#"[{{"text":"EP"}},{{"text":" \u2022 "}},{{"text":"Simon"}},{{"text":" \u2022 "}},{ALBUM},
               {{"text":" \u2022 "}},{{"text":"3:25"}}]"#
        ));
        assert_eq!(sub.artists, ["Simon"]);
        assert_eq!(sub.duration, Some(Duration::from_secs(205)));
    }

    #[test]
    fn linked_artists_win_over_unlinked_text() {
        let artist = r#"{"text":"Linked","navigationEndpoint":{"browseEndpoint":{"browseId":"UC",
            "browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":
            {"pageType":"MUSIC_PAGE_TYPE_ARTIST"}}}}}"#;
        let sub = subtitle_of(&format!(
            r#"[{artist},{{"text":" & "}},{{"text":"Plain"}},{{"text":" \u2022 "}},{ALBUM}]"#
        ));
        assert_eq!(sub.artists, ["Linked"]);
    }

    #[test]
    fn search_structurally_bad_is_parse_error() {
        for body in ["not json", "[]", "{}"] {
            assert!(
                matches!(
                    parse_search(body.as_bytes(), 5),
                    Err(ProviderError::Parse(_))
                ),
                "{body}"
            );
        }
        assert_eq!(parse_search(br#"{"contents":{}}"#, 5).unwrap(), vec![]);
    }

    #[test]
    fn album_search_fixture_fields() {
        let albums =
            parse_search_collections(SEARCH_ALBUMS.as_bytes(), CollectionKind::Album, 50).unwrap();
        assert_eq!(albums.len(), 6);
        let a = &albums[0];
        assert_eq!(a.id.provider, ProviderId("ytmusic".into()));
        assert_eq!(a.id.id, "browse:MPREb_K8qWMWVqXGi");
        assert_eq!(a.kind, CollectionKind::Album);
        assert_eq!(a.title, "Random Access Memories");
        assert_eq!(
            a.subtitle.as_deref(),
            Some("Album \u{2022} Daft Punk \u{2022} 2013")
        );
        assert!(a.artwork_url.as_deref().unwrap().contains("w544-h544"));
        let ep = &albums[4];
        assert_eq!(ep.id.id, "browse:MPREb_D8fwgSA15pm");
        assert_eq!(ep.title, "New Daft Punk");
        assert_eq!(
            ep.subtitle.as_deref(),
            Some("EP \u{2022} Grimelda \u{2022} 2021")
        );
        assert!(albums.iter().all(|a| a.kind == CollectionKind::Album
            && a.id.id.starts_with("browse:MPREb_")
            && !a.title.is_empty()
            && a.subtitle.is_some()
            && a.artwork_url.is_some()));
    }

    #[test]
    fn playlist_search_fixture_fields() {
        let playlists =
            parse_search_collections(SEARCH_PLAYLISTS.as_bytes(), CollectionKind::Playlist, 50)
                .unwrap();
        assert_eq!(playlists.len(), 6);
        let p = &playlists[0];
        assert_eq!(p.id.provider, ProviderId("ytmusic".into()));
        assert_eq!(
            p.id.id,
            "browse:VLRDCLAK5uy_n20FRYQXNt1p1wS55Nj2r14IouO5weaYU"
        );
        assert_eq!(p.kind, CollectionKind::Playlist);
        assert_eq!(p.title, "Presenting Daft Punk");
        assert_eq!(
            p.subtitle.as_deref(),
            Some("YouTube Music \u{2022} 35 songs")
        );
        assert!(p.artwork_url.as_deref().unwrap().contains("w544-h544"));
        assert!(playlists.iter().all(|p| p.kind == CollectionKind::Playlist
            && p.id.id.starts_with("browse:VL")
            && !p.title.is_empty()
            && p.subtitle.is_some()
            && p.artwork_url.is_some()));
    }

    #[test]
    fn collection_search_keeps_only_its_kind_up_to_the_limit() {
        let parse = |body: &str, kind| parse_search_collections(body.as_bytes(), kind, 50).unwrap();
        assert!(parse(SEARCH_ALBUMS, CollectionKind::Playlist).is_empty());
        assert!(parse(SEARCH_PLAYLISTS, CollectionKind::Album).is_empty());
        assert!(parse(SEARCH, CollectionKind::Album).is_empty());
        let two =
            parse_search_collections(SEARCH_ALBUMS.as_bytes(), CollectionKind::Album, 2).unwrap();
        assert_eq!(two.len(), 2);
    }

    #[test]
    fn collection_search_skips_rows_that_open_no_collection() {
        let body = r#"{"contents":{"a":[
            {"musicResponsiveListItemRenderer":{"navigationEndpoint":{"browseEndpoint":
                {"browseId":"UCartist"}}}},
            {"musicResponsiveListItemRenderer":{"flexColumns":[]}},
            {"musicResponsiveListItemRenderer":{"flexColumns":[
              {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"T",
                "navigationEndpoint":{"browseEndpoint":{"browseId":"VLPLx"}}}]}}}]}}]}}"#;
        let found =
            parse_search_collections(body.as_bytes(), CollectionKind::Playlist, 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id.id, "browse:VLPLx");
        assert_eq!(found[0].title, "T");
        assert_eq!(found[0].subtitle, None);
        for body in ["not json", "[]", "{}"] {
            assert!(matches!(
                parse_search_collections(body.as_bytes(), CollectionKind::Album, 5),
                Err(ProviderError::Parse(_))
            ));
        }
    }

    #[test]
    fn artists_search_lists_artists_in_order() {
        let found = parse_search_collections(SEARCH_ARTISTS.as_bytes(), CollectionKind::Artist, 12)
            .unwrap();
        let names: Vec<&str> = found.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            names,
            [
                "Daft Punk",
                "The Weeknd",
                "Pharrell",
                "The Strokes",
                "Pentatonix",
                "LCD Soundsystem"
            ]
        );
        assert_eq!(found[0].id.id, "browse:UCRr1xG_2WIDs18a6cIiCxeA");
        assert_eq!(
            found[0].subtitle.as_deref(),
            Some("Artist \u{2022} 68.7M monthly audience")
        );
        assert!(
            found[0]
                .artwork_url
                .as_deref()
                .unwrap()
                .contains("w120-h120")
        );
        assert!(found.iter().all(|c| c.kind == CollectionKind::Artist
            && c.id.id.starts_with("browse:UC")
            && c.artwork_url.is_some()));
        let first_two =
            parse_search_collections(SEARCH_ARTISTS.as_bytes(), CollectionKind::Artist, 2).unwrap();
        assert_eq!(first_two[..], found[..2]);
        // Artists are not albums, nor albums artists.
        assert!(
            parse_search_collections(SEARCH_ARTISTS.as_bytes(), CollectionKind::Album, 12)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_search_collections(SEARCH_ALBUMS.as_bytes(), CollectionKind::Artist, 12)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn top_artist_is_read_from_the_leading_card() {
        let artist = parse_top_artist(SEARCH_TOP_ARTIST.as_bytes()).unwrap();
        assert_eq!(
            artist,
            Some(Collection {
                id: CollectionId {
                    provider: ProviderId(PROVIDER.into()),
                    id: "browse:UCRr1xG_2WIDs18a6cIiCxeA".into(),
                },
                kind: CollectionKind::Artist,
                title: "Daft Punk".into(),
                subtitle: Some("Artist \u{2022} 68.7M monthly audience".into()),
                artwork_url: Some(
                    "https://lh3.googleusercontent.com/qLhu6Py_4_xoBsoubKQsXlhOQGqU9YU1ZRAbFusF0LlrPkXbbpu7bEh-k_ZtE4JwLgubvucAQqcK1hRk=w120-h120-p-l90-rj".into()
                ),
            })
        );
    }

    #[test]
    fn a_search_without_an_artist_card_has_no_top_artist() {
        assert_eq!(parse_top_artist(SEARCH.as_bytes()).unwrap(), None);
        assert_eq!(parse_top_artist(SEARCH_ALBUMS.as_bytes()).unwrap(), None);
        let album_card =
            SEARCH_TOP_ARTIST.replace("MUSIC_PAGE_TYPE_ARTIST", "MUSIC_PAGE_TYPE_ALBUM");
        assert_eq!(parse_top_artist(album_card.as_bytes()).unwrap(), None);
        for body in ["not json", "[]", "{}"] {
            assert!(matches!(
                parse_top_artist(body.as_bytes()),
                Err(ProviderError::Parse(_))
            ));
        }
    }

    fn collection(id: &str) -> Collection {
        Collection {
            id: CollectionId {
                provider: ProviderId(PROVIDER.into()),
                id: id.into(),
            },
            kind: CollectionKind::Album,
            title: id.into(),
            subtitle: None,
            artwork_url: None,
        }
    }

    #[test]
    fn merged_collections_lead_with_the_first_list_once_each_up_to_the_limit() {
        let list = |ids: &[&str]| ids.iter().map(|id| collection(id)).collect::<Vec<_>>();
        let ids = |cs: Vec<Collection>| cs.into_iter().map(|c| c.id.id).collect::<Vec<_>>();
        let merged = merge_collections(list(&["a", "b"]), list(&["c", "a", "d", "b", "e"]), 4);
        assert_eq!(ids(merged), ["a", "b", "c", "d"]);
        assert_eq!(
            ids(merge_collections(list(&[]), list(&["x", "y"]), 5)),
            ["x", "y"]
        );
        assert_eq!(
            ids(merge_collections(list(&["p", "q"]), list(&["x"]), 1)),
            ["p"]
        );
    }

    #[test]
    fn structure_summary_has_paths_but_no_text() {
        let s = structure_summary(SEARCH.as_bytes());
        assert!(s.contains("musicResponsiveListItemRenderer"), "{s}");
        assert!(!s.contains("Around the World"), "{s}");
        assert!(structure_summary(b"nope").contains("not json"));
    }

    #[test]
    fn logged_in_reads_the_tracking_param() {
        let with = |v: &str| {
            serde_json::json!({"responseContext": {"serviceTrackingParams": [
                {"service": "CSI", "params": [{"key": "c", "value": "WEB_REMIX"}]},
                {"service": "GFEEDBACK", "params": [{"key": "logged_in", "value": v}]}]}})
        };
        assert_eq!(logged_in(&with("1")), Some(true));
        assert_eq!(logged_in(&with("0")), Some(false));
        assert_eq!(logged_in(&serde_json::json!({"responseContext": {}})), None);
        assert_eq!(logged_in(&serde_json::json!([])), None);
    }

    #[test]
    fn visitor_data_from_fixtures() {
        assert_eq!(visitor_data(SEARCH.as_bytes()).as_deref(), Some("VISITOR"));
        assert_eq!(visitor_data(b"{}"), None);
        assert_eq!(visitor_data(b"nope"), None);
    }

    fn stream(body: &str, prefer_aac: bool) -> StreamFormat {
        match classify_player(body.as_bytes(), prefer_aac).unwrap() {
            PlayerOutcome::Stream(f) => f,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn format_order_opus_first() {
        let f = stream(OK, false);
        assert_eq!(f.itag, 251);
        assert_eq!(f.hint, FormatHint::WebmOpus);
        assert_eq!(f.content_length, 8859337);
        assert_eq!(f.average_bitrate, Some(165001));
        assert!(f.url.contains("ip=0.0.0.0"));
    }

    #[test]
    fn format_order_prefer_aac() {
        let f = stream(OK, true);
        assert_eq!(f.itag, 140);
        assert_eq!(f.hint, FormatHint::Mp4Aac);
    }

    #[test]
    fn format_falls_through_the_order() {
        let body = OK.replace("\"itag\": 251", "\"itag\": 9251");
        assert_eq!(stream(&body, false).itag, 250);
        let body = body.replace("\"itag\": 250", "\"itag\": 9250");
        assert_eq!(stream(&body, false).itag, 249);
        let body = body.replace("\"itag\": 249", "\"itag\": 9249");
        assert_eq!(stream(&body, false).itag, 140);
    }

    #[test]
    fn login_required_is_reported_not_failed() {
        assert_eq!(
            classify_player(LOGIN.as_bytes(), false).unwrap(),
            PlayerOutcome::LoginRequired
        );
    }

    #[test]
    fn unplayable_rows() {
        for (name, body) in [("error", ERROR), ("unplayable", UNPLAYABLE), ("age", AGE)] {
            match classify_player(body.as_bytes(), false) {
                Err(ProviderError::Unplayable(r)) => assert!(!r.is_empty(), "{name}"),
                other => panic!("{name}: {other:?}"),
            }
        }
        assert_eq!(
            classify_player(UNPLAYABLE.as_bytes(), false),
            Err(ProviderError::Unplayable(
                "Video unavailable. This content is no longer available.".into()
            ))
        );
    }

    #[test]
    fn other_statuses_are_unplayable() {
        for s in [
            "CONTENT_CHECK_REQUIRED",
            "LIVE_STREAM_OFFLINE",
            "SOMETHING_NEW",
        ] {
            let body = format!(r#"{{"playabilityStatus":{{"status":"{s}"}}}}"#);
            assert!(matches!(
                classify_player(body.as_bytes(), false),
                Err(ProviderError::Unplayable(_))
            ));
        }
    }

    #[test]
    fn ok_without_audio_is_unplayable() {
        assert!(matches!(
            classify_player(NO_AUDIO.as_bytes(), false),
            Err(ProviderError::Unplayable(_))
        ));
        let body = br#"{"playabilityStatus":{"status":"OK"}}"#;
        assert!(matches!(
            classify_player(body, false),
            Err(ProviderError::Unplayable(_))
        ));
    }

    #[test]
    fn cipher_only_is_client_broken() {
        assert!(matches!(
            classify_player(CIPHER.as_bytes(), false),
            Err(ProviderError::ClientBroken(_))
        ));
    }

    #[test]
    fn ok_without_url_is_client_broken() {
        let body = br#"{"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[
            {"itag":251,"mimeType":"audio/webm; codecs=\"opus\"","contentLength":"10"}]}}"#;
        assert!(matches!(
            classify_player(body, false),
            Err(ProviderError::ClientBroken(_))
        ));
    }

    #[test]
    fn malformed_or_missing_status_is_parse() {
        for body in [MALFORMED, "{}", r#"{"playabilityStatus":{}}"#, "[]"] {
            assert!(
                matches!(
                    classify_player(body.as_bytes(), false),
                    Err(ProviderError::Parse(_))
                ),
                "{body}"
            );
        }
    }
}
