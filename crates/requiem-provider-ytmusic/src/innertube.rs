//! Innertube client: search and browse (WEB_REMIX) and stream resolution (VISIONOS).

use std::sync::{Arc, Mutex};
use std::thread;

use requiem_core::{
    Collection, CollectionKind, ProviderError, SEARCH_COLLECTIONS, SearchResults, Track,
};
use serde_json::{Value, json};

use crate::IpFamily;
use crate::clients::*;
use crate::log::debug;
use crate::parse::{
    PlayerOutcome, StreamFormat, classify_player, parse_search, parse_search_collections,
    structure_summary, visitor_data,
};
use crate::range::{FetchError, RangeFetcher, RangeResponse, Refresher};
use crate::transport::{HttpResponse, Transport};

const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false";
// Smallest WEB_REMIX call that returns `responseContext.visitorData`: measured 2026-10-10
// at 609 bytes, against ~198 KB for a songs search.
const SUGGESTIONS_URL: &str =
    "https://music.youtube.com/youtubei/v1/music/get_search_suggestions?prettyPrint=false";
const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";

/// Why a resolve attempt on one family failed.
enum Failure {
    /// The player answered LOGIN_REQUIRED even with fresh visitorData: YouTube's bot
    /// check, which is per IP address.
    BotCheck,
    Other(ProviderError),
}

impl From<ProviderError> for Failure {
    fn from(e: ProviderError) -> Self {
        Self::Other(e)
    }
}

fn family_name(f: IpFamily) -> &'static str {
    match f {
        IpFamily::Any => "any",
        IpFamily::V4 => "v4",
        IpFamily::V6 => "v6",
    }
}

/// Stream URLs embed the requesting address (`ip=`), and the CDN rejects other addresses,
/// so a source must keep fetching on the family that produced its URL.
fn url_family(url: &str) -> IpFamily {
    let ip = url
        .split_once('?')
        .map(|(_, q)| q)
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("ip="));
    match ip {
        Some(ip) if ip.contains(':') || ip.contains("%3A") || ip.contains("%3a") => IpFamily::V6,
        Some(ip) if !ip.is_empty() => IpFamily::V4,
        _ => IpFamily::Any,
    }
}

/// A `RangeFetcher` bound to one family for the life of a source.
struct PinnedFetcher {
    transport: Arc<dyn Transport>,
    family: IpFamily,
}

impl RangeFetcher for PinnedFetcher {
    fn fetch(&self, url: &str, start: u64, end: u64) -> Result<RangeResponse, FetchError> {
        self.transport.fetch_on(self.family, url, start, end)
    }
}

pub(crate) struct InnertubeClient {
    transport: Arc<dyn Transport>,
    configured: IpFamily,
    /// Family used for API calls: `configured`, until a fallback from `Any` pins one.
    pinned: Mutex<IpFamily>,
    visitor: Mutex<Option<String>>,
}

pub(crate) fn status_error(what: &str, status: u16) -> ProviderError {
    if status == 429 || status >= 500 {
        ProviderError::Network(format!("{what}: http {status}"))
    } else {
        ProviderError::ClientBroken(format!("{what}: http {status}"))
    }
}

impl InnertubeClient {
    pub(crate) fn new(transport: Arc<dyn Transport>, configured: IpFamily) -> Self {
        Self {
            transport,
            configured,
            pinned: Mutex::new(configured),
            visitor: Mutex::new(None),
        }
    }

    pub(crate) fn fetcher(&self, family: IpFamily) -> Arc<dyn RangeFetcher> {
        Arc::new(PinnedFetcher {
            transport: self.transport.clone(),
            family,
        })
    }

    fn family(&self) -> IpFamily {
        *self.pinned.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cached_visitor(&self) -> Option<String> {
        self.visitor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_visitor(&self, v: Option<String>) {
        *self.visitor.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }

    fn post_music(
        &self,
        family: IpFamily,
        what: &str,
        url: &str,
        body: &Value,
    ) -> Result<HttpResponse, ProviderError> {
        let resp = self.transport.post_json(
            family,
            url,
            &[
                ("X-YouTube-Client-Name", MUSIC_CLIENT_ID),
                ("X-YouTube-Client-Version", MUSIC_CLIENT_VERSION),
                ("Origin", MUSIC_ORIGIN),
                ("User-Agent", MUSIC_UA),
            ],
            &body.to_string(),
        )?;
        if resp.status != 200 {
            return Err(status_error(what, resp.status));
        }
        Ok(resp)
    }

    pub(crate) fn music_context() -> Value {
        json!({"client": {
            "clientName": MUSIC_CLIENT_NAME,
            "clientVersion": MUSIC_CLIENT_VERSION,
            "hl": "en",
            "gl": "US",
        }})
    }

    fn search_once(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        let body = json!({
            "context": Self::music_context(),
            "query": query,
            "params": SONGS_PARAMS,
        });
        let resp = self.post_music(self.family(), "search", SEARCH_URL, &body)?;
        if let Some(v) = visitor_data(&resp.body) {
            self.set_visitor(Some(v));
        }
        let tracks = parse_search(&resp.body, limit)?;
        if tracks.is_empty() && crate::log::enabled() {
            debug!("empty search: {}", structure_summary(&resp.body));
        }
        Ok(tracks)
    }

    pub(crate) fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        let tracks = self.search_once(query, limit)?;
        // Real queries are never empty; a zero-item response is an anomaly tied to the
        // visitorData / connection, so retry once from a fresh one.
        if tracks.is_empty() && !query.trim().is_empty() && limit > 0 {
            self.reset_visitor();
            return self.search_once(query, limit);
        }
        Ok(tracks)
    }

    /// At most `limit` of the albums or playlists a search for `query` finds.
    fn search_collections(
        &self,
        query: &str,
        kind: CollectionKind,
        limit: usize,
    ) -> Result<Vec<Collection>, ProviderError> {
        let (what, params) = match kind {
            CollectionKind::Album => ("album search", ALBUMS_PARAMS),
            CollectionKind::Playlist => ("playlist search", PLAYLISTS_PARAMS),
        };
        let body = json!({
            "context": Self::music_context(),
            "query": query,
            "params": params,
        });
        let resp = self.post_music(self.family(), what, SEARCH_URL, &body)?;
        parse_search_collections(&resp.body, kind, limit)
    }

    /// Songs, albums and playlists, searched at once. Albums or playlists that could not be
    /// had are left out; only a failed song search fails.
    pub(crate) fn search_all(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<SearchResults, ProviderError> {
        let found = |kind| self.search_collections(query, kind, SEARCH_COLLECTIONS);
        let (tracks, albums, playlists) = thread::scope(|scope| {
            let albums = scope.spawn(|| found(CollectionKind::Album));
            let playlists = scope.spawn(|| found(CollectionKind::Playlist));
            let tracks = self.search(query, limit);
            (tracks, albums.join(), playlists.join())
        });
        let left_out =
            |joined: thread::Result<Result<Vec<Collection>, ProviderError>>, what| match joined {
                Ok(Ok(found)) => found,
                Ok(Err(e)) => {
                    debug!("search without {what}: {e}");
                    Vec::new()
                }
                Err(_) => {
                    debug!("search without {what}: the search panicked");
                    Vec::new()
                }
            };
        Ok(SearchResults {
            tracks: tracks?,
            albums: left_out(albums, "albums"),
            playlists: left_out(playlists, "playlists"),
        })
    }

    /// The first page of an anonymous browse of `browse_id`.
    pub(crate) fn browse(&self, browse_id: &str) -> Result<Vec<u8>, ProviderError> {
        let body = json!({"context": Self::music_context(), "browseId": browse_id});
        Ok(self
            .post_music(self.family(), "browse", BROWSE_URL, &body)?
            .body)
    }

    /// Forgets the cached visitorData and the connection pool, so the next resolve
    /// starts from a fresh visitorData.
    pub(crate) fn reset_visitor(&self) {
        self.set_visitor(None);
        self.transport.reset();
    }

    fn fetch_visitor(&self, family: IpFamily) -> Result<String, ProviderError> {
        let body = json!({"context": Self::music_context(), "input": ""});
        let resp = self.post_music(family, "visitor data", SUGGESTIONS_URL, &body)?;
        let v = visitor_data(&resp.body).ok_or_else(|| {
            ProviderError::Parse("response carries no responseContext.visitorData".to_owned())
        })?;
        debug!("fetched visitorData ({} chars)", v.len());
        self.set_visitor(Some(v.clone()));
        Ok(v)
    }

    fn player_call(
        &self,
        family: IpFamily,
        video_id: &str,
        visitor: &str,
        prefer_aac: bool,
    ) -> Result<PlayerOutcome, ProviderError> {
        let body = json!({
            "context": {"client": {
                "clientName": STREAM_CLIENT_NAME,
                "clientVersion": STREAM_CLIENT_VERSION,
                "deviceMake": STREAM_DEVICE_MAKE,
                "deviceModel": STREAM_DEVICE_MODEL,
                "userAgent": STREAM_UA,
                "osName": STREAM_OS_NAME,
                "osVersion": STREAM_OS_VERSION,
                "hl": "en",
                "gl": "US",
                "visitorData": visitor,
            }},
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
        });
        let resp = self.transport.post_json(
            family,
            PLAYER_URL,
            &[
                ("X-YouTube-Client-Name", STREAM_CLIENT_ID),
                ("X-YouTube-Client-Version", STREAM_CLIENT_VERSION),
                ("Origin", STREAM_ORIGIN),
                ("X-Goog-Visitor-Id", visitor),
                ("User-Agent", STREAM_UA),
            ],
            &body.to_string(),
        )?;
        if resp.status != 200 {
            return Err(status_error("player", resp.status));
        }
        classify_player(&resp.body, prefer_aac)
    }

    /// One resolve attempt on `family`. With `refresh_on_login`, a `LOGIN_REQUIRED` first
    /// gets fresh visitorData on a fresh connection pool and one retry.
    fn resolve_on(
        &self,
        family: IpFamily,
        video_id: &str,
        prefer_aac: bool,
        refresh_on_login: bool,
    ) -> Result<StreamFormat, Failure> {
        let visitor = match self.cached_visitor() {
            Some(v) => v,
            None => self.fetch_visitor(family)?,
        };
        let mut outcome = self.player_call(family, video_id, &visitor, prefer_aac)?;
        if outcome == PlayerOutcome::LoginRequired && refresh_on_login {
            debug!(
                "LOGIN_REQUIRED on {}: refreshing visitorData",
                family_name(family)
            );
            self.reset_visitor();
            let visitor = self.fetch_visitor(family)?;
            outcome = self.player_call(family, video_id, &visitor, prefer_aac)?;
        }
        match outcome {
            PlayerOutcome::Stream(f) => {
                debug!(
                    "resolved itag {} ({} bytes) host {} on {}",
                    f.itag,
                    f.content_length,
                    crate::log::host_of(&f.url),
                    family_name(family)
                );
                Ok(f)
            }
            PlayerOutcome::LoginRequired => Err(Failure::BotCheck),
        }
    }

    fn bot_check_error(tried: &[IpFamily]) -> ProviderError {
        let names: Vec<&str> = tried.iter().map(|f| family_name(*f)).collect();
        ProviderError::ClientBroken(format!(
            "YouTube bot check (LOGIN_REQUIRED) on every IP family: {}",
            names.join(", ")
        ))
    }

    /// Resolves the preferred audio format and returns the family its URL must be
    /// fetched on. When the default family is bot-checked and `Any` is configured, tries
    /// V4 then V6, each with fresh visitorData, and pins the first that works.
    pub(crate) fn resolve(
        &self,
        video_id: &str,
        prefer_aac: bool,
    ) -> Result<(StreamFormat, IpFamily), ProviderError> {
        let current = self.family();
        match self.resolve_on(current, video_id, prefer_aac, true) {
            Ok(f) => return Ok(self.with_fetch_family(f, current)),
            Err(Failure::Other(e)) => return Err(e),
            Err(Failure::BotCheck) => {}
        }
        let mut tried = vec![current];
        if self.configured == IpFamily::Any {
            for next in [IpFamily::V4, IpFamily::V6] {
                if next == current {
                    continue;
                }
                tried.push(next);
                self.reset_visitor();
                match self.resolve_on(next, video_id, prefer_aac, false) {
                    Ok(f) => {
                        debug!("bot check avoided: pinned to {}", family_name(next));
                        *self.pinned.lock().unwrap_or_else(|e| e.into_inner()) = next;
                        return Ok(self.with_fetch_family(f, next));
                    }
                    Err(Failure::Other(e)) => return Err(e),
                    Err(Failure::BotCheck) => {}
                }
            }
        }
        Err(Self::bot_check_error(&tried))
    }

    fn with_fetch_family(
        &self,
        f: StreamFormat,
        resolved_on: IpFamily,
    ) -> (StreamFormat, IpFamily) {
        let family = if resolved_on == IpFamily::Any {
            url_family(&f.url)
        } else {
            resolved_on
        };
        (f, family)
    }
}

/// Re-resolves one video for `HttpRangeSource`. Holds the client, never the session.
/// Stays on the family of the original URL, which embeds that address.
pub(crate) struct ClientRefresher {
    pub client: Arc<InnertubeClient>,
    pub video_id: String,
    pub prefer_aac: bool,
    pub family: IpFamily,
}

impl Refresher for ClientRefresher {
    fn refresh(&self) -> Result<(String, u32, u64), ProviderError> {
        // Some visitorData buckets get URLs capped at 1 MiB (403 from byte 1048576), and
        // re-resolving with the same visitorData returns another capped URL.
        self.client.reset_visitor();
        match self
            .client
            .resolve_on(self.family, &self.video_id, self.prefer_aac, true)
        {
            Ok(f) => Ok((f.url, f.itag, f.content_length)),
            Err(Failure::Other(e)) => Err(e),
            Err(Failure::BotCheck) => Err(InnertubeClient::bot_check_error(&[self.family])),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use super::*;
    use crate::range::{FetchError, RangeResponse};

    const OK: &str = include_str!("../tests/fixtures/player_ok.json");
    const LOGIN: &str = include_str!("../tests/fixtures/player_login_required.json");
    const SEARCH: &str = include_str!("../tests/fixtures/search_songs.json");

    type Reply = Result<(u16, String), ProviderError>;

    struct Request {
        family: IpFamily,
        url: String,
        visitor: Option<String>,
        body: String,
    }

    #[derive(Default)]
    struct FakeTransport {
        replies: Mutex<HashMap<IpFamily, VecDeque<Reply>>>,
        requests: Mutex<Vec<Request>>,
        resets: Mutex<u32>,
    }

    impl FakeTransport {
        fn with(replies: Vec<(u16, &str)>) -> Arc<Self> {
            Self::keyed(vec![(IpFamily::Any, replies)])
        }
        fn keyed(per_family: Vec<(IpFamily, Vec<(u16, &str)>)>) -> Arc<Self> {
            let t = Self::default();
            for (family, replies) in per_family {
                t.replies.lock().unwrap().insert(
                    family,
                    replies
                        .into_iter()
                        .map(|(s, b)| Ok((s, b.to_owned())))
                        .collect(),
                );
            }
            Arc::new(t)
        }
        fn urls(&self) -> Vec<String> {
            let r = self.requests.lock().unwrap();
            r.iter()
                .map(|q| {
                    q.url
                        .split('?')
                        .next()
                        .unwrap()
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        }
        fn families(&self) -> Vec<IpFamily> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|q| q.family)
                .collect()
        }
    }

    impl Transport for FakeTransport {
        fn post_json(
            &self,
            family: IpFamily,
            url: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> Result<HttpResponse, ProviderError> {
            let visitor = headers
                .iter()
                .find(|(k, _)| *k == "X-Goog-Visitor-Id")
                .map(|(_, v)| (*v).to_owned());
            self.requests.lock().unwrap().push(Request {
                family,
                url: url.to_owned(),
                visitor,
                body: body.to_owned(),
            });
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .get_mut(&family)
                .and_then(VecDeque::pop_front)
                .expect("scripted")?;
            Ok(HttpResponse {
                status,
                body: body.into_bytes(),
            })
        }
        fn post_json_signed(
            &self,
            _: IpFamily,
            _: &str,
            _: &[(&str, &str)],
            _: &str,
        ) -> Result<HttpResponse, ProviderError> {
            unreachable!("the anonymous client never signs a request")
        }
        fn fetch_on(
            &self,
            _: IpFamily,
            _: &str,
            _: u64,
            _: u64,
        ) -> Result<RangeResponse, FetchError> {
            unreachable!()
        }
        fn reset(&self) {
            *self.resets.lock().unwrap() += 1;
        }
    }

    fn vd_reply(v: &str) -> String {
        format!(r#"{{"responseContext":{{"visitorData":"{v}"}}}}"#)
    }

    fn client(t: &Arc<FakeTransport>) -> InnertubeClient {
        client_on(t, IpFamily::Any)
    }

    fn client_on(t: &Arc<FakeTransport>, family: IpFamily) -> InnertubeClient {
        InnertubeClient::new(t.clone(), family)
    }

    #[test]
    fn resolve_without_visitor_fetches_one_first() {
        let t = FakeTransport::with(vec![(200, &vd_reply("VD1")), (200, OK)]);
        let f = client(&t).resolve("vid", false).unwrap().0;
        assert_eq!(f.itag, 251);
        assert_eq!(t.urls(), vec!["get_search_suggestions", "player"]);
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[1].visitor.as_deref(), Some("VD1"));
        assert!(reqs[1].body.contains(r#""visitorData":"VD1""#));
        assert!(reqs[1].body.contains(r#""clientName":"VISIONOS""#));
        assert!(reqs[1].body.contains(r#""videoId":"vid""#));
    }

    #[test]
    fn browse_posts_the_browse_id_as_web_remix() {
        let t = FakeTransport::with(vec![(200, r#"{"contents":{}}"#), (404, "{}")]);
        let c = client(&t);
        assert_eq!(c.browse("VLPLx").unwrap(), br#"{"contents":{}}"#);
        assert!(matches!(
            c.browse("VLgone"),
            Err(ProviderError::ClientBroken(_))
        ));
        assert_eq!(t.urls(), vec!["browse", "browse"]);
        let reqs = t.requests.lock().unwrap();
        assert!(reqs[0].body.contains(r#""browseId":"VLPLx""#));
        assert!(reqs[0].body.contains(r#""clientName":"WEB_REMIX""#));
    }

    #[test]
    fn empty_search_retries_once_with_fresh_visitor() {
        let t = FakeTransport::with(vec![(200, r#"{"contents":{"x":[]}}"#), (200, SEARCH)]);
        let tracks = client(&t).search("daft punk", 3).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(t.urls(), vec!["search", "search"]);
        assert_eq!(*t.resets.lock().unwrap(), 1);
    }

    #[test]
    fn empty_search_after_retry_is_returned_without_third_attempt() {
        let empty = r#"{"contents":{}}"#;
        let t = FakeTransport::with(vec![(200, empty), (200, empty)]);
        assert!(client(&t).search("daft punk", 3).unwrap().is_empty());
        assert_eq!(t.urls().len(), 2);
    }

    #[test]
    fn search_fills_visitor_cache() {
        let t = FakeTransport::with(vec![(200, SEARCH), (200, OK)]);
        let c = client(&t);
        assert_eq!(c.search("q", 3).unwrap().len(), 3);
        c.resolve("vid", false).unwrap();
        assert_eq!(t.urls(), vec!["search", "player"]);
        assert_eq!(
            t.requests.lock().unwrap()[1].visitor.as_deref(),
            Some("VISITOR")
        );
    }

    #[test]
    fn login_required_refreshes_visitor_and_pool_once() {
        let t = FakeTransport::with(vec![
            (200, &vd_reply("VD1")),
            (200, LOGIN),
            (200, &vd_reply("VD2")),
            (200, OK),
        ]);
        let f = client(&t).resolve("vid", false).unwrap().0;
        assert_eq!(f.itag, 251);
        assert_eq!(*t.resets.lock().unwrap(), 1);
        assert_eq!(
            t.urls(),
            vec![
                "get_search_suggestions",
                "player",
                "get_search_suggestions",
                "player"
            ]
        );
        assert_eq!(
            t.requests.lock().unwrap()[3].visitor.as_deref(),
            Some("VD2")
        );
    }

    fn bot_checked() -> Vec<(u16, String)> {
        vec![
            (200, vd_reply("VD1")),
            (200, LOGIN.to_owned()),
            (200, vd_reply("VD2")),
            (200, LOGIN.to_owned()),
        ]
    }

    fn script(replies: &[(u16, String)]) -> Vec<(u16, &str)> {
        replies.iter().map(|(s, b)| (*s, b.as_str())).collect()
    }

    #[test]
    fn explicit_family_bot_check_is_client_broken_without_fallback() {
        let replies = bot_checked();
        let t = FakeTransport::keyed(vec![(IpFamily::V6, script(&replies))]);
        let err = client_on(&t, IpFamily::V6)
            .resolve("vid", false)
            .unwrap_err();
        assert!(matches!(err, ProviderError::ClientBroken(_)), "{err}");
        assert_eq!(*t.resets.lock().unwrap(), 1);
        assert!(t.families().iter().all(|f| *f == IpFamily::V6));
        assert_eq!(t.families().len(), 4);
    }

    #[test]
    fn any_falls_back_to_v4_and_pins_it() {
        let replies = bot_checked();
        let t = FakeTransport::keyed(vec![
            (IpFamily::Any, script(&replies)),
            (
                IpFamily::V4,
                vec![(200, &vd_reply("VD4")), (200, OK), (200, OK)],
            ),
        ]);
        let c = client(&t);
        let (f, family) = c.resolve("vid", false).unwrap();
        assert_eq!((f.itag, family), (251, IpFamily::V4));
        assert_eq!(
            t.families(),
            [
                IpFamily::Any,
                IpFamily::Any,
                IpFamily::Any,
                IpFamily::Any,
                IpFamily::V4,
                IpFamily::V4
            ]
        );
        assert_eq!(
            t.requests.lock().unwrap()[5].visitor.as_deref(),
            Some("VD4")
        );
        // pinned: the next resolve goes straight to V4 with the cached visitorData
        let (_, family) = c.resolve("vid2", false).unwrap();
        assert_eq!(family, IpFamily::V4);
        assert_eq!(t.families().len(), 7);
        assert_eq!(t.families()[6], IpFamily::V4);
        assert_eq!(t.urls()[6], "player");
    }

    #[test]
    fn every_family_bot_checked_names_all_tried() {
        let replies = bot_checked();
        let t = FakeTransport::keyed(vec![
            (IpFamily::Any, script(&replies)),
            (IpFamily::V4, vec![(200, &vd_reply("A")), (200, LOGIN)]),
            (IpFamily::V6, vec![(200, &vd_reply("B")), (200, LOGIN)]),
        ]);
        match client(&t).resolve("vid", false) {
            Err(ProviderError::ClientBroken(m)) => {
                assert!(m.contains("any, v4, v6"), "{m}");
                assert!(m.contains("LOGIN_REQUIRED"), "{m}");
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn url_family_reads_ip_param() {
        assert_eq!(
            url_family("https://h/videoplayback?a=b&ip=1.2.3.4&c=d"),
            IpFamily::V4
        );
        assert_eq!(
            url_family("https://h/videoplayback?ip=2001:db8::1"),
            IpFamily::V6
        );
        assert_eq!(
            url_family("https://h/videoplayback?ip=2001%3Adb8%3A%3A1"),
            IpFamily::V6
        );
        assert_eq!(url_family("https://h/videoplayback?x=1"), IpFamily::Any);
        assert_eq!(url_family("https://h/videoplayback"), IpFamily::Any);
    }

    #[test]
    fn http_statuses_map_as_specified() {
        for (status, network) in [
            (429, true),
            (500, true),
            (503, true),
            (400, false),
            (403, false),
        ] {
            let t = FakeTransport::with(vec![(status, "{}")]);
            let err = client(&t).search("q", 1).unwrap_err();
            assert_eq!(
                matches!(err, ProviderError::Network(_)),
                network,
                "{status}"
            );
            if !network {
                assert!(matches!(err, ProviderError::ClientBroken(_)), "{status}");
            }
        }
        let t = FakeTransport::with(vec![(200, &vd_reply("V")), (403, "{}")]);
        assert!(matches!(
            client(&t).resolve("v", false),
            Err(ProviderError::ClientBroken(_))
        ));
    }

    #[test]
    fn transport_errors_pass_through_as_network() {
        let t = Arc::new(FakeTransport::default());
        t.replies.lock().unwrap().insert(
            IpFamily::Any,
            VecDeque::from([Err(ProviderError::Network("timeout".into()))]),
        );
        assert!(matches!(
            client(&t).search("q", 1),
            Err(ProviderError::Network(_))
        ));
    }

    #[test]
    fn missing_visitor_in_response_is_parse_error() {
        let t = FakeTransport::with(vec![(200, "{}")]);
        assert!(matches!(
            client(&t).resolve("v", false),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn refresher_returns_fresh_url_itag_len() {
        let t = FakeTransport::with(vec![(200, &vd_reply("V")), (200, OK)]);
        let r = ClientRefresher {
            client: Arc::new(client(&t)),
            video_id: "vid".into(),
            prefer_aac: true,
            family: IpFamily::Any,
        };
        let (url, itag, len) = r.refresh().unwrap();
        assert!(url.contains("videoplayback"));
        assert_eq!((itag, len), (140, 6953417));
    }

    #[test]
    fn refresh_uses_fresh_visitor_data() {
        let t = FakeTransport::with(vec![
            (200, &vd_reply("VD1")),
            (200, OK),
            (200, &vd_reply("VD2")),
            (200, OK),
        ]);
        let client = Arc::new(client(&t));
        client.resolve("vid", false).unwrap();
        let r = ClientRefresher {
            client: client.clone(),
            video_id: "vid".into(),
            prefer_aac: false,
            family: IpFamily::Any,
        };
        r.refresh().unwrap();
        assert_eq!(
            t.urls(),
            vec![
                "get_search_suggestions",
                "player",
                "get_search_suggestions",
                "player"
            ]
        );
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[1].visitor.as_deref(), Some("VD1"));
        assert_eq!(reqs[3].visitor.as_deref(), Some("VD2"));
        assert_eq!(*t.resets.lock().unwrap(), 1);
        assert_eq!(client.cached_visitor().as_deref(), Some("VD2"));
    }
}
