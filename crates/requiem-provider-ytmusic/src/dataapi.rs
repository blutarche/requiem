//! The signed-in library through the YouTube Data API v3: the user's playlists and liked
//! videos. Every call costs one unit of the project's 10,000-unit daily quota.

use std::collections::HashMap;
use std::time::Duration;

use requiem_core::{
    Collection, CollectionId, CollectionKind, Item, ProviderError, ProviderId, Shelf, Track,
    TrackId,
};
use serde_json::Value;

use crate::google::Google;
use crate::parse::PROVIDER;

const PLAYLISTS_URL: &str = "https://www.googleapis.com/youtube/v3/playlists";
const PLAYLIST_ITEMS_URL: &str = "https://www.googleapis.com/youtube/v3/playlistItems";
const VIDEOS_URL: &str = "https://www.googleapis.com/youtube/v3/videos";
/// Prefix of collection ids served by the Data API.
pub(crate) const DATA_PREFIX: &str = "data:";
const PER_PAGE: usize = 50;
const MAX_PLAYLISTS: usize = 200;
const MAX_TRACKS: usize = 1000;
const LIKED: &str = "Liked videos";

/// Authorized Data API calls.
pub(crate) trait DataApi {
    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Vec<u8>, ProviderError>;
    fn likes(&self) -> Result<String, ProviderError>;
}

impl DataApi for Google {
    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Vec<u8>, ProviderError> {
        self.api_get(url, query)
    }

    fn likes(&self) -> Result<String, ProviderError> {
        Google::likes(self)
    }
}

fn parse_err(msg: impl Into<String>) -> ProviderError {
    ProviderError::Parse(msg.into())
}

fn json(body: &[u8], what: &str) -> Result<Value, ProviderError> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("{what} json: {e}")))?;
    if v.get("items").is_some_and(Value::is_array) {
        Ok(v)
    } else {
        Err(parse_err(format!("{what} response has no items")))
    }
}

fn items(v: &Value) -> &[Value] {
    v.get("items")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn next_page(v: &Value) -> Option<String> {
    v.get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

fn text(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn best_thumbnail(snippet: &Value) -> Option<String> {
    snippet
        .get("thumbnails")?
        .as_object()?
        .values()
        .filter_map(|t| {
            let url = t.get("url")?.as_str()?;
            let side = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
            Some((side("width") * side("height"), url))
        })
        .max_by_key(|(area, _)| *area)
        .map(|(_, url)| url.to_owned())
}

fn collection_id(playlist: &str) -> CollectionId {
    CollectionId {
        provider: ProviderId(PROVIDER.to_owned()),
        id: format!("{DATA_PREFIX}{playlist}"),
    }
}

/// One page of results and the token of the next.
type ApiPage<T> = Result<(Vec<T>, Option<String>), ProviderError>;

/// One page of `playlists.list`.
fn parse_playlists(body: &[u8]) -> ApiPage<Collection> {
    let v = json(body, "playlists")?;
    let found = items(&v)
        .iter()
        .filter_map(|p| {
            let id = text(p, "/id")?;
            let snippet = p.get("snippet")?;
            let count = p
                .pointer("/contentDetails/itemCount")
                .and_then(Value::as_u64);
            Some(Collection {
                id: collection_id(&id),
                kind: CollectionKind::Playlist,
                title: text(snippet, "/title").unwrap_or_else(|| id.clone()),
                subtitle: count.map(|n| match n {
                    1 => "1 video".to_owned(),
                    n => format!("{n} videos"),
                }),
                artwork_url: best_thumbnail(snippet),
            })
        })
        .collect();
    Ok((found, next_page(&v)))
}

/// One page of `playlistItems.list`, without durations. Deleted and private videos, which
/// have no owner, are left out.
fn parse_playlist_items(body: &[u8]) -> ApiPage<Track> {
    let v = json(body, "playlistItems")?;
    let found = items(&v)
        .iter()
        .filter_map(|it| {
            let snippet = it.get("snippet")?;
            let owner = text(snippet, "/videoOwnerChannelTitle")?;
            let video = text(it, "/contentDetails/videoId")
                .or_else(|| text(snippet, "/resourceId/videoId"))?;
            let artist = owner.strip_suffix(" - Topic").unwrap_or(&owner).to_owned();
            Some(Track {
                id: TrackId {
                    provider: ProviderId(PROVIDER.to_owned()),
                    id: video,
                },
                title: text(snippet, "/title").unwrap_or_default(),
                artists: vec![artist],
                album: None,
                duration: None,
                artwork_url: best_thumbnail(snippet),
            })
        })
        .collect();
    Ok((found, next_page(&v)))
}

/// `videos.list?part=contentDetails`: video id to duration.
fn parse_durations(body: &[u8]) -> Result<HashMap<String, Duration>, ProviderError> {
    let v = json(body, "videos")?;
    Ok(items(&v)
        .iter()
        .filter_map(|it| {
            let d = iso8601_duration(&text(it, "/contentDetails/duration")?)?;
            Some((text(it, "/id")?, d))
        })
        .collect())
}

/// `PT4M48S`, `PT1H2M`, `P1DT2H`. Zero (`P0D`, a live stream) is unknown.
fn iso8601_duration(s: &str) -> Option<Duration> {
    let rest = s.strip_prefix('P')?;
    let mut secs = 0u64;
    let mut num = String::new();
    let mut in_time = false;
    let mut any = false;
    for c in rest.chars() {
        match c {
            '0'..='9' => num.push(c),
            'T' if !in_time && num.is_empty() => in_time = true,
            _ => {
                let n: u64 = num.parse().ok()?;
                num.clear();
                let unit = match (in_time, c) {
                    (false, 'W') => 7 * 86_400,
                    (false, 'D') => 86_400,
                    (true, 'H') => 3600,
                    (true, 'M') => 60,
                    (true, 'S') => 1,
                    _ => return None,
                };
                secs = secs.checked_add(n.checked_mul(unit)?)?;
                any = true;
            }
        }
    }
    (any && num.is_empty() && secs > 0).then(|| Duration::from_secs(secs))
}

/// Pages through `url` until there is no next page or `cap` items are in.
fn paginate<T>(
    api: &dyn DataApi,
    url: &str,
    query: &[(&str, &str)],
    cap: usize,
    parse: fn(&[u8]) -> ApiPage<T>,
) -> Result<Vec<T>, ProviderError> {
    let per_page = PER_PAGE.to_string();
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut q = query.to_vec();
        q.push(("maxResults", &per_page));
        if let Some(t) = &token {
            q.push(("pageToken", t));
        }
        let (page, next) = parse(&api.get(url, &q)?)?;
        out.extend(page);
        match next {
            Some(n) if out.len() < cap => token = Some(n),
            _ => break,
        }
    }
    out.truncate(cap);
    Ok(out)
}

/// One shelf, "Playlists": liked videos first, then up to 200 of the user's playlists.
pub(crate) fn library(api: &dyn DataApi) -> Result<Vec<Shelf>, ProviderError> {
    let likes = match api.likes() {
        Ok(l) => Some(l),
        // An account without a YouTube channel has no liked-videos playlist.
        Err(ProviderError::Unplayable(_)) => None,
        Err(e) => return Err(e),
    };
    let playlists = paginate(
        api,
        PLAYLISTS_URL,
        &[("part", "snippet,contentDetails"), ("mine", "true")],
        MAX_PLAYLISTS,
        parse_playlists,
    )?;
    let liked = likes.map(|l| Collection {
        id: collection_id(&l),
        kind: CollectionKind::Playlist,
        title: LIKED.to_owned(),
        subtitle: None,
        artwork_url: None,
    });
    let items = liked
        .into_iter()
        .chain(playlists)
        .map(Item::Collection)
        .collect();
    Ok(vec![Shelf {
        title: "Playlists".to_owned(),
        items,
    }])
}

fn playlist_title(api: &dyn DataApi, id: &str) -> Result<String, ProviderError> {
    let body = api.get(PLAYLISTS_URL, &[("part", "snippet"), ("id", id)])?;
    let v = json(&body, "playlists")?;
    // The liked-videos playlist is not listed under its own id.
    Ok(items(&v)
        .first()
        .and_then(|p| text(p, "/snippet/title"))
        .unwrap_or_else(|| {
            if id.starts_with("LL") {
                LIKED
            } else {
                "Playlist"
            }
            .to_owned()
        }))
}

/// The first 1000 videos of playlist `id`, as one shelf named after it.
pub(crate) fn playlist(api: &dyn DataApi, id: &str) -> Result<Vec<Shelf>, ProviderError> {
    let title = playlist_title(api, id)?;
    let mut tracks = paginate(
        api,
        PLAYLIST_ITEMS_URL,
        &[("part", "snippet,contentDetails"), ("playlistId", id)],
        MAX_TRACKS,
        parse_playlist_items,
    )?;
    let ids: Vec<String> = tracks.iter().map(|t| t.id.id.clone()).collect();
    let mut durations = HashMap::new();
    for chunk in ids.chunks(PER_PAGE) {
        let joined = chunk.join(",");
        let body = api.get(
            VIDEOS_URL,
            &[
                ("part", "contentDetails"),
                ("id", &joined),
                ("maxResults", "50"),
            ],
        )?;
        durations.extend(parse_durations(&body)?);
    }
    for t in &mut tracks {
        t.duration = durations.get(&t.id.id).copied();
    }
    Ok(vec![Shelf {
        title,
        items: tracks.into_iter().map(Item::Track).collect(),
    }])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::google::api_error;
    use crate::testing::FakeApi;
    use crate::transport::ApiHttp;

    const PLAYLISTS: &str = include_str!("../tests/fixtures/data_playlists.json");
    const ITEMS: &str = include_str!("../tests/fixtures/data_playlist_items.json");
    const VIDEOS: &str = include_str!("../tests/fixtures/data_videos.json");

    struct Fake {
        api: FakeApi,
        likes: Result<String, ProviderError>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                api: FakeApi::default(),
                likes: Ok("LLme".into()),
            }
        }

        fn page_tokens(&self, url: &str) -> Vec<Option<String>> {
            self.api
                .queries(url)
                .iter()
                .map(|q| {
                    q.iter()
                        .find(|(k, _)| k == "pageToken")
                        .map(|(_, v)| v.clone())
                })
                .collect()
        }
    }

    impl DataApi for Fake {
        fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Vec<u8>, ProviderError> {
            let r = self.api.get(url, query, &[])?;
            if r.status == 200 {
                Ok(r.body)
            } else {
                Err(api_error(r.status, &r.body))
            }
        }

        fn likes(&self) -> Result<String, ProviderError> {
            self.likes.clone()
        }
    }

    fn track_ids(shelf: &Shelf) -> Vec<&str> {
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
    fn library_is_liked_videos_then_every_playlist_page() {
        let f = Fake::new();
        f.api.script(PLAYLISTS_URL, 200, PLAYLISTS);
        f.api.script(
            PLAYLISTS_URL,
            200,
            &json!({"items": [{"id": "PLlast", "snippet": {"title": "Last"}}]}).to_string(),
        );
        let shelves = library(&f).unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Playlists");
        assert_eq!(
            track_ids(&shelves[0]),
            [
                "data:LLme",
                "data:PLroadtrip",
                "data:PLfocus",
                "data:PLlast"
            ]
        );
        let Item::Collection(liked) = &shelves[0].items[0] else {
            panic!()
        };
        assert_eq!(liked.title, "Liked videos");
        let Item::Collection(c) = &shelves[0].items[1] else {
            panic!()
        };
        assert_eq!(c.title, "Road trip");
        assert_eq!(c.kind, CollectionKind::Playlist);
        assert_eq!(c.id.provider, ProviderId("ytmusic".into()));
        assert_eq!(c.subtitle.as_deref(), Some("42 videos"));
        assert_eq!(
            c.artwork_url.as_deref(),
            Some("https://i.ytimg.com/vi/kkppQnFl2-8/maxresdefault.jpg")
        );
        assert_eq!(f.page_tokens(PLAYLISTS_URL), [None, Some("CAIQAA".into())]);
        let q = &f.api.queries(PLAYLISTS_URL)[0];
        for kv in [
            ("mine", "true"),
            ("part", "snippet,contentDetails"),
            ("maxResults", "50"),
        ] {
            assert!(q.contains(&(kv.0.into(), kv.1.into())), "{q:?}");
        }
    }

    fn playlists_page(n: usize, token: &str) -> String {
        let items: Vec<Value> = (0..n)
            .map(|i| json!({"id": format!("PL{token}{i}"), "snippet": {"title": "p"}}))
            .collect();
        json!({"items": items, "nextPageToken": token}).to_string()
    }

    #[test]
    fn library_stops_at_200_playlists() {
        let f = Fake::new();
        for p in 0..10 {
            f.api
                .script(PLAYLISTS_URL, 200, &playlists_page(50, &format!("t{p}")));
        }
        let shelves = library(&f).unwrap();
        assert_eq!(shelves[0].items.len(), 1 + 200);
        assert_eq!(f.api.calls().len(), 4);
    }

    #[test]
    fn library_without_a_channel_has_no_liked_videos() {
        let mut f = Fake::new();
        f.likes = Err(ProviderError::Unplayable("no channel".into()));
        f.api.script(PLAYLISTS_URL, 200, r#"{"items": []}"#);
        assert_eq!(library(&f).unwrap()[0].items, []);

        f.likes = Err(ProviderError::SignInRequired("sign in".into()));
        assert!(matches!(library(&f), Err(ProviderError::SignInRequired(_))));
    }

    #[test]
    fn playlist_tracks_with_durations_and_no_deleted_videos() {
        let f = Fake::new();
        f.api.script(
            PLAYLISTS_URL,
            200,
            r#"{"items": [{"id": "PLroadtrip", "snippet": {"title": "Road trip"}}]}"#,
        );
        f.api.script(PLAYLIST_ITEMS_URL, 200, ITEMS);
        f.api.script(VIDEOS_URL, 200, VIDEOS);
        let shelves = playlist(&f, "PLroadtrip").unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Road trip");
        assert_eq!(track_ids(&shelves[0]), ["kkppQnFl2-8", "bP7rQabXjuw"]);
        let Item::Track(t) = &shelves[0].items[0] else {
            panic!()
        };
        assert_eq!(t.title, "ฤดูฝน");
        assert_eq!(t.artists, ["Paradox"]);
        assert_eq!(t.album, None);
        assert_eq!(t.duration, Some(Duration::from_secs(4 * 60 + 48)));
        assert_eq!(
            t.artwork_url.as_deref(),
            Some("https://i.ytimg.com/vi/kkppQnFl2-8/sddefault.jpg")
        );
        let Item::Track(t) = &shelves[0].items[1] else {
            panic!()
        };
        // Not a "- Topic" channel: the owner's name as is.
        assert_eq!(t.artists, ["Silly Fools Official"]);
        assert_eq!(t.duration, Some(Duration::from_secs(5 * 60 + 9)));
        let q = &f.api.queries(VIDEOS_URL)[0];
        assert!(q.contains(&("id".into(), "kkppQnFl2-8,bP7rQabXjuw".into())));
    }

    fn items_page(start: usize, n: usize, token: &str) -> String {
        let items: Vec<Value> = (start..start + n)
            .map(|i| {
                json!({
                    "snippet": {"title": format!("t{i}"), "videoOwnerChannelTitle": "A - Topic"},
                    "contentDetails": {"videoId": format!("v{i}")},
                })
            })
            .collect();
        json!({"items": items, "nextPageToken": token}).to_string()
    }

    fn videos_for(ids: std::ops::Range<usize>) -> String {
        let items: Vec<Value> = ids
            .map(|i| json!({"id": format!("v{i}"), "contentDetails": {"duration": "PT1M"}}))
            .collect();
        json!({ "items": items }).to_string()
    }

    #[test]
    fn playlist_stops_at_1000_tracks_and_asks_durations_50_at_a_time() {
        let f = Fake::new();
        f.api.script(PLAYLISTS_URL, 200, r#"{"items": []}"#);
        for p in 0..25 {
            f.api.script(
                PLAYLIST_ITEMS_URL,
                200,
                &items_page(p * 50, 50, &format!("t{p}")),
            );
        }
        for p in 0..20 {
            f.api
                .script(VIDEOS_URL, 200, &videos_for(p * 50..p * 50 + 50));
        }
        let shelves = playlist(&f, "PLbig").unwrap();
        assert_eq!(shelves[0].title, "Playlist");
        assert_eq!(shelves[0].items.len(), 1000);
        assert_eq!(f.api.queries(PLAYLIST_ITEMS_URL).len(), 20);
        let tokens = f.page_tokens(PLAYLIST_ITEMS_URL);
        assert_eq!(tokens[0], None);
        assert_eq!(tokens[19].as_deref(), Some("t18"));
        assert_eq!(f.api.queries(VIDEOS_URL).len(), 20);
        let Item::Track(last) = &shelves[0].items[999] else {
            panic!()
        };
        assert_eq!(
            (last.id.id.as_str(), last.duration),
            ("v999", Some(Duration::from_secs(60)))
        );
        assert_eq!(last.artists, ["A"]);
    }

    #[test]
    fn liked_videos_are_named_when_the_api_does_not_list_them() {
        let f = Fake::new();
        f.api.script(PLAYLISTS_URL, 200, r#"{"items": []}"#);
        f.api.script(PLAYLIST_ITEMS_URL, 200, r#"{"items": []}"#);
        assert_eq!(playlist(&f, "LLme").unwrap()[0].title, "Liked videos");
    }

    #[test]
    fn a_missing_playlist_is_unplayable() {
        let f = Fake::new();
        f.api.script(
            PLAYLISTS_URL,
            404,
            r#"{"error":{"code":404,"message":"The playlist identified with the request's playlistId parameter cannot be found.","errors":[{"reason":"playlistNotFound"}]}}"#,
        );
        assert!(matches!(
            playlist(&f, "PLgone"),
            Err(ProviderError::Unplayable(m)) if m.contains("playlistNotFound")
        ));
    }

    #[test]
    fn iso8601_durations() {
        let d = |s| iso8601_duration(s).map(|d| d.as_secs());
        assert_eq!(d("PT4M48S"), Some(288));
        assert_eq!(d("PT45S"), Some(45));
        assert_eq!(d("PT1H2M3S"), Some(3723));
        assert_eq!(d("PT2H"), Some(7200));
        assert_eq!(d("P1DT2H"), Some(93_600));
        assert_eq!(d("P0D"), None);
        assert_eq!(d("PT"), None);
        assert_eq!(d("4:48"), None);
        assert_eq!(d("PT4X"), None);
        assert_eq!(d("PT4"), None);
    }

    #[test]
    fn malformed_responses_are_parse_errors() {
        assert!(matches!(
            parse_playlists(b"{}"),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_playlist_items(b"not json"),
            Err(ProviderError::Parse(_))
        ));
    }
}
