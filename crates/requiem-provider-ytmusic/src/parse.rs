//! Response parsers. Every field is optional so a missing one never fails deserialization;
//! a structurally unusable response is `ProviderError::Parse`.

use std::collections::BTreeSet;
use std::time::Duration;

use requiem_core::{FormatHint, ProviderError, ProviderId, Track, TrackId};
use serde::Deserialize;
use serde_json::Value;

mod browse;

pub(crate) use browse::{
    BROWSE_PREFIX, collection_kind, parse_collection, parse_home, parse_library_playlists,
};

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
    flex_columns: Vec<FlexColumn>,
    fixed_columns: Vec<FixedColumn>,
    thumbnail: Option<Thumbnail>,
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

struct Subtitle {
    artists: Vec<String>,
    album: Option<String>,
    duration: Option<Duration>,
}

/// Column 1: artists, album and duration separated by " • " runs. Typed runs
/// (`pageType`) win; without any, the first group is the artist and the second the album.
fn parse_subtitle<'a>(runs: impl IntoIterator<Item = &'a Run>) -> Subtitle {
    let mut artists = Vec::new();
    let mut album = None;
    let mut duration = None;
    let mut groups: Vec<String> = vec![String::new()];
    for run in runs {
        let Some(text) = run.text.as_deref() else {
            continue;
        };
        if text == SEPARATOR {
            groups.push(String::new());
            continue;
        }
        if let Some(d) = parse_duration(text) {
            duration = Some(d);
            continue;
        }
        groups.last_mut().expect("non-empty").push_str(text);
        let page_type = run
            .navigation_endpoint
            .as_ref()
            .and_then(|n| n.browse_endpoint.as_ref())
            .and_then(|b| b.browse_endpoint_context_supported_configs.as_ref())
            .and_then(|c| c.browse_endpoint_context_music_config.as_ref())
            .and_then(|m| m.page_type.as_deref());
        match page_type {
            Some("MUSIC_PAGE_TYPE_ARTIST") => artists.push(text.to_owned()),
            Some("MUSIC_PAGE_TYPE_ALBUM") => album = Some(text.to_owned()),
            _ => {}
        }
    }
    if artists.is_empty() && album.is_none() {
        // Video results put a view or play count where a song has its album.
        let mut untyped = groups
            .into_iter()
            .filter(|g| !g.is_empty() && !g.ends_with(" views") && !g.ends_with(" plays"));
        if let Some(a) = untyped.next() {
            artists.push(a);
        }
        album = untyped.next();
    }
    Subtitle {
        artists,
        album,
        duration,
    }
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

pub(crate) fn parse_search(body: &[u8], limit: usize) -> Result<Vec<Track>, ProviderError> {
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
                album: sub.album,
                duration: sub.duration,
                artwork_url: largest_thumbnail(item),
            })
        })
        .take(limit)
        .collect();
    Ok(tracks)
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
        assert_eq!(t.album.as_deref(), Some("Homework"));
        assert_eq!(t.duration, Some(Duration::from_secs(7 * 60 + 10)));
        assert!(t.artwork_url.as_deref().unwrap().contains("w120-h120"));
        assert!(
            tracks
                .iter()
                .all(|t| !t.title.is_empty() && t.duration.is_some())
        );
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
