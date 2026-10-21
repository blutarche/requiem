//! The signed-in library and Home through the YouTube Data API v3: the user's playlists and
//! liked videos. Every call costs one unit of the project's 10,000-unit daily quota.

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
/// Home's most recently liked music videos.
const HOME_LIKED: usize = 20;
/// Likes pages Home reads while looking for `HOME_LIKED` music videos.
const HOME_MAX_PAGES: usize = 4;
/// `snippet.categoryId` of Music.
const MUSIC_CATEGORY: &str = "10";
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
            Some((side("width").saturating_mul(side("height")), url))
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

struct VideoInfo {
    duration: Option<Duration>,
    category: Option<String>,
}

/// `videos.list?part=contentDetails,snippet`: video id to duration and category.
fn parse_video_info(body: &[u8]) -> Result<HashMap<String, VideoInfo>, ProviderError> {
    let v = json(body, "videos")?;
    Ok(items(&v)
        .iter()
        .filter_map(|it| {
            let info = VideoInfo {
                duration: text(it, "/contentDetails/duration").and_then(|d| iso8601_duration(&d)),
                category: text(it, "/snippet/categoryId"),
            };
            Some((text(it, "/id")?, info))
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
    // Pages can come back empty, so the item cap alone does not end a looping server.
    let max_pages = cap / PER_PAGE * 2 + 2;
    for _ in 0..max_pages {
        let mut q = query.to_vec();
        q.push(("maxResults", &per_page));
        if let Some(t) = &token {
            q.push(("pageToken", t));
        }
        let (page, next) = parse(&api.get(url, &q)?)?;
        out.extend(page);
        match next {
            Some(n) if out.len() < cap && token.as_deref() != Some(n.as_str()) => token = Some(n),
            _ => break,
        }
    }
    out.truncate(cap);
    Ok(out)
}

/// The liked-videos playlist, if the account has a YouTube channel to keep one.
fn likes(api: &dyn DataApi) -> Result<Option<String>, ProviderError> {
    match api.likes() {
        Ok(l) => Ok(Some(l)),
        Err(ProviderError::Unplayable(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Liked videos first, then up to 200 of the user's playlists.
fn collections(api: &dyn DataApi, likes: Option<&str>) -> Result<Vec<Collection>, ProviderError> {
    let playlists = paginate(
        api,
        PLAYLISTS_URL,
        &[("part", "snippet,contentDetails"), ("mine", "true")],
        MAX_PLAYLISTS,
        parse_playlists,
    )?;
    let liked = likes.map(|l| Collection {
        id: collection_id(l),
        kind: CollectionKind::Playlist,
        title: LIKED.to_owned(),
        subtitle: None,
        artwork_url: None,
    });
    Ok(liked.into_iter().chain(playlists).collect())
}

/// One shelf, "Playlists": liked videos first, then up to 200 of the user's playlists.
pub(crate) fn library(api: &dyn DataApi) -> Result<Vec<Shelf>, ProviderError> {
    let likes = likes(api)?;
    Ok(vec![Shelf {
        title: "Playlists".to_owned(),
        items: collections(api, likes.as_deref())?
            .into_iter()
            .map(Item::Collection)
            .collect(),
    }])
}

/// "Liked songs", the 20 most recently liked music videos, then "Your playlists", the
/// Library's collections; empty shelves are left out.
pub(crate) fn home(api: &dyn DataApi) -> Result<Vec<Shelf>, ProviderError> {
    let likes = likes(api)?;
    let liked = match &likes {
        Some(l) => liked_music(api, l)?,
        None => Vec::new(),
    };
    let own = collections(api, likes.as_deref())?;
    Ok([
        ("Liked songs", liked.into_iter().map(Item::Track).collect()),
        (
            "Your playlists",
            own.into_iter().map(Item::Collection).collect(),
        ),
    ]
    .into_iter()
    .filter(|(_, items): &(_, Vec<Item>)| !items.is_empty())
    .map(|(title, items)| Shelf {
        title: title.to_owned(),
        items,
    })
    .collect())
}

/// Walks the liked-videos playlist a page at a time, keeping music videos, until `HOME_LIKED`
/// are in, the likes run out or `HOME_MAX_PAGES` pages are read.
fn liked_music(api: &dyn DataApi, likes: &str) -> Result<Vec<Track>, ProviderError> {
    let per_page = PER_PAGE.to_string();
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..HOME_MAX_PAGES {
        let mut q = vec![
            ("part", "snippet,contentDetails"),
            ("playlistId", likes),
            ("maxResults", per_page.as_str()),
        ];
        if let Some(t) = &token {
            q.push(("pageToken", t));
        }
        let (page, next) = parse_playlist_items(&api.get(PLAYLIST_ITEMS_URL, &q)?)?;
        let mut info = video_info(api, &page)?;
        out.extend(page.into_iter().filter_map(|mut t| {
            let i = info.remove(&t.id.id)?;
            (i.category.as_deref() == Some(MUSIC_CATEGORY)).then(|| {
                t.duration = i.duration;
                t
            })
        }));
        match next {
            Some(n) if out.len() < HOME_LIKED && token.as_deref() != Some(n.as_str()) => {
                token = Some(n);
            }
            _ => break,
        }
    }
    out.truncate(HOME_LIKED);
    Ok(out)
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
    add_durations(api, &mut tracks)?;
    Ok(vec![Shelf {
        title,
        items: tracks.into_iter().map(Item::Track).collect(),
    }])
}

/// One `videos.list` call per 50 tracks.
fn video_info(
    api: &dyn DataApi,
    tracks: &[Track],
) -> Result<HashMap<String, VideoInfo>, ProviderError> {
    let ids: Vec<&str> = tracks.iter().map(|t| t.id.id.as_str()).collect();
    let mut info = HashMap::new();
    for chunk in ids.chunks(PER_PAGE) {
        let joined = chunk.join(",");
        let body = api.get(
            VIDEOS_URL,
            &[
                ("part", "contentDetails,snippet"),
                ("id", &joined),
                ("maxResults", "50"),
            ],
        )?;
        info.extend(parse_video_info(&body)?);
    }
    Ok(info)
}

fn add_durations(api: &dyn DataApi, tracks: &mut [Track]) -> Result<(), ProviderError> {
    let info = video_info(api, tracks)?;
    for t in tracks {
        t.duration = info.get(&t.id.id).and_then(|i| i.duration);
    }
    Ok(())
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

    /// Answers every request with an empty page; `fresh` gives each a new `nextPageToken`.
    struct Endless {
        fresh: bool,
        requests: std::cell::Cell<usize>,
    }

    impl DataApi for Endless {
        fn get(&self, _: &str, _: &[(&str, &str)]) -> Result<Vec<u8>, ProviderError> {
            let n = self.requests.get() + 1;
            self.requests.set(n);
            let token = if self.fresh {
                format!("t{n}")
            } else {
                "same".into()
            };
            Ok(json!({"items": [], "nextPageToken": token})
                .to_string()
                .into_bytes())
        }

        fn likes(&self) -> Result<String, ProviderError> {
            unreachable!()
        }
    }

    #[test]
    fn a_server_that_never_ends_the_listing_is_cut_off() {
        for (fresh, most) in [(false, 2), (true, 200 / PER_PAGE * 2 + 2)] {
            let api = Endless {
                fresh,
                requests: 0.into(),
            };
            let got = paginate(&api, PLAYLISTS_URL, &[], 200, parse_playlists).unwrap();
            assert!(got.is_empty());
            assert_eq!(api.requests.get(), most, "fresh tokens: {fresh}");
        }
    }

    #[test]
    fn thumbnail_area_saturates() {
        let snippet = json!({"thumbnails": {
            "a": {"url": "https://x/a", "width": u64::MAX, "height": u64::MAX},
            "b": {"url": "https://x/b", "width": 10, "height": 10},
        }});
        assert_eq!(best_thumbnail(&snippet).as_deref(), Some("https://x/a"));
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

    fn video(i: usize, category: &str) -> Value {
        json!({
            "id": format!("v{i}"),
            "contentDetails": {"duration": "PT1M"},
            "snippet": {"categoryId": category},
        })
    }

    fn videos_for(ids: std::ops::Range<usize>) -> String {
        let items: Vec<Value> = ids.map(|i| video(i, "10")).collect();
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

    const ONE_PLAYLIST: &str =
        r#"{"items": [{"id": "PLroadtrip", "snippet": {"title": "Road trip"}}]}"#;

    #[test]
    fn home_is_the_latest_likes_then_the_library_in_three_calls() {
        let f = Fake::new();
        f.api
            .script(PLAYLIST_ITEMS_URL, 200, &items_page(0, 50, "next"));
        f.api.script(VIDEOS_URL, 200, &videos_for(0..20));
        f.api.script(PLAYLISTS_URL, 200, ONE_PLAYLIST);
        let shelves = home(&f).unwrap();
        let titles: Vec<_> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Liked songs", "Your playlists"]);
        assert_eq!(shelves[0].items.len(), 20);
        let Item::Track(last) = &shelves[0].items[19] else {
            panic!()
        };
        assert_eq!(
            (last.id.id.as_str(), last.duration),
            ("v19", Some(Duration::from_secs(60)))
        );
        assert_eq!(track_ids(&shelves[1]), ["data:LLme", "data:PLroadtrip"]);
        let q = &f.api.queries(PLAYLIST_ITEMS_URL)[0];
        assert!(q.contains(&("playlistId".into(), "LLme".into())), "{q:?}");
        assert_eq!(f.api.calls().len(), 3);
    }

    fn liked_ids(shelf: &Shelf) -> Vec<String> {
        track_ids(shelf).into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn home_keeps_only_music_in_like_order() {
        let f = Fake::new();
        f.api
            .script(PLAYLIST_ITEMS_URL, 200, &items_page(0, 6, "next"));
        let mut videos: Vec<Value> = ["10", "20", "10", "22", "10"]
            .iter()
            .enumerate()
            .map(|(i, c)| video(i, c))
            .collect();
        // v5 has no entry in videos.list, v4 has no category.
        videos[4] = json!({"id": "v4", "contentDetails": {"duration": "PT1M"}});
        f.api
            .script(VIDEOS_URL, 200, &json!({ "items": videos }).to_string());
        f.api.script(PLAYLIST_ITEMS_URL, 200, r#"{"items": []}"#);
        f.api.script(PLAYLISTS_URL, 200, r#"{"items": []}"#);
        let shelves = home(&f).unwrap();
        assert_eq!(liked_ids(&shelves[0]), ["v0", "v2"]);
        let q = &f.api.queries(VIDEOS_URL)[0];
        assert!(q.contains(&("part".into(), "contentDetails,snippet".into())));
    }

    #[test]
    fn home_reads_on_until_it_has_20_music_videos() {
        let f = Fake::new();
        f.api
            .script(PLAYLIST_ITEMS_URL, 200, &items_page(0, 50, "p2"));
        let first: Vec<Value> = (0..50)
            .map(|i| video(i, if i % 5 == 0 { "10" } else { "20" }))
            .collect();
        f.api
            .script(VIDEOS_URL, 200, &json!({ "items": first }).to_string());
        f.api
            .script(PLAYLIST_ITEMS_URL, 200, &items_page(50, 50, "p3"));
        f.api.script(VIDEOS_URL, 200, &videos_for(50..100));
        f.api.script(PLAYLISTS_URL, 200, ONE_PLAYLIST);
        let shelves = home(&f).unwrap();
        let ids = liked_ids(&shelves[0]);
        assert_eq!(ids.len(), 20);
        assert_eq!(
            &ids[..10],
            [
                "v0", "v5", "v10", "v15", "v20", "v25", "v30", "v35", "v40", "v45"
            ]
        );
        assert_eq!(&ids[10..13], ["v50", "v51", "v52"]);
        assert_eq!(f.page_tokens(PLAYLIST_ITEMS_URL), [None, Some("p2".into())]);
    }

    #[test]
    fn home_gives_up_after_4_pages_of_likes() {
        let f = Fake::new();
        for p in 0..6 {
            f.api.script(
                PLAYLIST_ITEMS_URL,
                200,
                &items_page(p * 50, 50, &format!("t{p}")),
            );
            let none: Vec<Value> = (p * 50..p * 50 + 50).map(|i| video(i, "20")).collect();
            f.api
                .script(VIDEOS_URL, 200, &json!({ "items": none }).to_string());
        }
        f.api.script(PLAYLISTS_URL, 200, r#"{"items": []}"#);
        let shelves = home(&f).unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Your playlists");
        assert_eq!(f.api.queries(PLAYLIST_ITEMS_URL).len(), 4);
        assert_eq!(f.api.queries(VIDEOS_URL).len(), 4);
    }

    #[test]
    fn home_without_a_channel_or_playlists_leaves_out_empty_shelves() {
        let mut f = Fake::new();
        f.likes = Err(ProviderError::Unplayable("no channel".into()));
        f.api.script(PLAYLISTS_URL, 200, ONE_PLAYLIST);
        let shelves = home(&f).unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Your playlists");
        assert_eq!(f.api.queries(PLAYLIST_ITEMS_URL).len(), 0);

        f.api.script(PLAYLISTS_URL, 200, r#"{"items": []}"#);
        assert_eq!(home(&f).unwrap(), []);

        f.likes = Err(ProviderError::SignInRequired("sign in".into()));
        assert!(matches!(home(&f), Err(ProviderError::SignInRequired(_))));
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
