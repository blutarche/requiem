//! Innertube client: search (WEB_REMIX) and stream resolution (VISIONOS).

use std::sync::{Arc, Mutex};

use requiem_core::{ProviderError, Track};
use serde_json::{Value, json};

use crate::clients::*;
use crate::log::debug;
use crate::parse::{PlayerOutcome, StreamFormat, classify_player, parse_search, visitor_data};
use crate::range::{RangeFetcher, Refresher};
use crate::transport::{HttpResponse, Transport};

const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
// Smallest WEB_REMIX call that returns `responseContext.visitorData`: measured 2026-10-10
// at 609 bytes, against ~198 KB for a songs search.
const SUGGESTIONS_URL: &str =
    "https://music.youtube.com/youtubei/v1/music/get_search_suggestions?prettyPrint=false";
const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";

pub(crate) struct InnertubeClient {
    transport: Arc<dyn Transport>,
    visitor: Mutex<Option<String>>,
}

fn status_error(what: &str, status: u16) -> ProviderError {
    if status == 429 || status >= 500 {
        ProviderError::Network(format!("{what}: http {status}"))
    } else {
        ProviderError::ClientBroken(format!("{what}: http {status}"))
    }
}

impl InnertubeClient {
    pub(crate) fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            visitor: Mutex::new(None),
        }
    }

    pub(crate) fn fetcher(&self) -> Arc<dyn RangeFetcher> {
        self.transport.clone()
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
        what: &str,
        url: &str,
        body: &Value,
    ) -> Result<HttpResponse, ProviderError> {
        let resp = self.transport.post_json(
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

    fn music_context() -> Value {
        json!({"client": {
            "clientName": MUSIC_CLIENT_NAME,
            "clientVersion": MUSIC_CLIENT_VERSION,
            "hl": "en",
            "gl": "US",
        }})
    }

    pub(crate) fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        let body = json!({
            "context": Self::music_context(),
            "query": query,
            "params": SONGS_PARAMS,
        });
        let resp = self.post_music("search", SEARCH_URL, &body)?;
        if let Some(v) = visitor_data(&resp.body) {
            self.set_visitor(Some(v));
        }
        parse_search(&resp.body, limit)
    }

    /// Forgets the cached visitorData and the connection pool, so the next resolve
    /// starts from a fresh visitorData.
    pub(crate) fn reset_visitor(&self) {
        self.set_visitor(None);
        self.transport.reset();
    }

    fn fetch_visitor(&self) -> Result<String, ProviderError> {
        let body = json!({"context": Self::music_context(), "input": ""});
        let resp = self.post_music("visitor data", SUGGESTIONS_URL, &body)?;
        let v = visitor_data(&resp.body).ok_or_else(|| {
            ProviderError::Parse("response carries no responseContext.visitorData".to_owned())
        })?;
        debug!("fetched visitorData ({} chars)", v.len());
        self.set_visitor(Some(v.clone()));
        Ok(v)
    }

    fn player_call(
        &self,
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

    /// Resolves the preferred audio format. On `LOGIN_REQUIRED` the visitorData is stale:
    /// refetch it on a fresh connection pool and retry once.
    pub(crate) fn resolve(
        &self,
        video_id: &str,
        prefer_aac: bool,
    ) -> Result<StreamFormat, ProviderError> {
        let visitor = match self.cached_visitor() {
            Some(v) => v,
            None => self.fetch_visitor()?,
        };
        let mut outcome = self.player_call(video_id, &visitor, prefer_aac)?;
        if outcome == PlayerOutcome::LoginRequired {
            debug!("LOGIN_REQUIRED: refreshing visitorData and connection pool");
            self.reset_visitor();
            let visitor = self.fetch_visitor()?;
            outcome = self.player_call(video_id, &visitor, prefer_aac)?;
        }
        match outcome {
            PlayerOutcome::Stream(f) => {
                debug!(
                    "resolved itag {} ({} bytes) host {}",
                    f.itag,
                    f.content_length,
                    crate::log::host_of(&f.url)
                );
                Ok(f)
            }
            PlayerOutcome::LoginRequired => Err(ProviderError::ClientBroken(
                "player still answers LOGIN_REQUIRED after a visitorData refresh".to_owned(),
            )),
        }
    }
}

/// Re-resolves one video for `HttpRangeSource`. Holds the client, never the session.
pub(crate) struct ClientRefresher {
    pub client: Arc<InnertubeClient>,
    pub video_id: String,
    pub prefer_aac: bool,
}

impl Refresher for ClientRefresher {
    fn refresh(&self) -> Result<(String, u32, u64), ProviderError> {
        // Some visitorData buckets get URLs capped at 1 MiB (403 from byte 1048576), and
        // re-resolving with the same visitorData returns another capped URL.
        self.client.reset_visitor();
        let f = self.client.resolve(&self.video_id, self.prefer_aac)?;
        Ok((f.url, f.itag, f.content_length))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::range::{FetchError, RangeResponse};

    const OK: &str = include_str!("../tests/fixtures/player_ok.json");
    const LOGIN: &str = include_str!("../tests/fixtures/player_login_required.json");
    const SEARCH: &str = include_str!("../tests/fixtures/search_songs.json");

    #[derive(Default)]
    struct FakeTransport {
        replies: Mutex<VecDeque<Result<(u16, String), ProviderError>>>,
        /// (url, X-Goog-Visitor-Id, body)
        requests: Mutex<Vec<(String, Option<String>, String)>>,
        resets: Mutex<u32>,
    }

    impl FakeTransport {
        fn with(replies: Vec<(u16, &str)>) -> Arc<Self> {
            let t = Self::default();
            *t.replies.lock().unwrap() = replies
                .into_iter()
                .map(|(s, b)| Ok((s, b.to_owned())))
                .collect();
            Arc::new(t)
        }
        fn urls(&self) -> Vec<String> {
            let r = self.requests.lock().unwrap();
            r.iter()
                .map(|(u, _, _)| {
                    u.split('?')
                        .next()
                        .unwrap()
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        }
    }

    impl RangeFetcher for FakeTransport {
        fn fetch(&self, _: &str, _: u64, _: u64) -> Result<RangeResponse, FetchError> {
            unreachable!()
        }
    }

    impl Transport for FakeTransport {
        fn post_json(
            &self,
            url: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> Result<HttpResponse, ProviderError> {
            let vd = headers
                .iter()
                .find(|(k, _)| *k == "X-Goog-Visitor-Id")
                .map(|(_, v)| (*v).to_owned());
            self.requests
                .lock()
                .unwrap()
                .push((url.to_owned(), vd, body.to_owned()));
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted")?;
            Ok(HttpResponse {
                status,
                body: body.into_bytes(),
            })
        }
        fn reset(&self) {
            *self.resets.lock().unwrap() += 1;
        }
    }

    fn vd_reply(v: &str) -> String {
        format!(r#"{{"responseContext":{{"visitorData":"{v}"}}}}"#)
    }

    fn client(t: &Arc<FakeTransport>) -> InnertubeClient {
        InnertubeClient::new(t.clone())
    }

    #[test]
    fn resolve_without_visitor_fetches_one_first() {
        let t = FakeTransport::with(vec![(200, &vd_reply("VD1")), (200, OK)]);
        let f = client(&t).resolve("vid", false).unwrap();
        assert_eq!(f.itag, 251);
        assert_eq!(t.urls(), vec!["get_search_suggestions", "player"]);
        let reqs = t.requests.lock().unwrap();
        assert_eq!(reqs[1].1.as_deref(), Some("VD1"));
        assert!(reqs[1].2.contains(r#""visitorData":"VD1""#));
        assert!(reqs[1].2.contains(r#""clientName":"VISIONOS""#));
        assert!(reqs[1].2.contains(r#""videoId":"vid""#));
    }

    #[test]
    fn search_fills_visitor_cache() {
        let t = FakeTransport::with(vec![(200, SEARCH), (200, OK)]);
        let c = client(&t);
        assert_eq!(c.search("q", 3).unwrap().len(), 3);
        c.resolve("vid", false).unwrap();
        assert_eq!(t.urls(), vec!["search", "player"]);
        assert_eq!(t.requests.lock().unwrap()[1].1.as_deref(), Some("VISITOR"));
    }

    #[test]
    fn login_required_refreshes_visitor_and_pool_once() {
        let t = FakeTransport::with(vec![
            (200, &vd_reply("VD1")),
            (200, LOGIN),
            (200, &vd_reply("VD2")),
            (200, OK),
        ]);
        let f = client(&t).resolve("vid", false).unwrap();
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
        assert_eq!(t.requests.lock().unwrap()[3].1.as_deref(), Some("VD2"));
    }

    #[test]
    fn login_required_twice_is_client_broken() {
        let t = FakeTransport::with(vec![
            (200, &vd_reply("VD1")),
            (200, LOGIN),
            (200, &vd_reply("VD2")),
            (200, LOGIN),
        ]);
        assert!(matches!(
            client(&t).resolve("vid", false),
            Err(ProviderError::ClientBroken(_))
        ));
        assert_eq!(*t.resets.lock().unwrap(), 1);
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
        t.replies
            .lock()
            .unwrap()
            .push_back(Err(ProviderError::Network("timeout".into())));
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
        assert_eq!(reqs[1].1.as_deref(), Some("VD1"));
        assert_eq!(reqs[3].1.as_deref(), Some("VD2"));
        assert_eq!(*t.resets.lock().unwrap(), 1);
        assert_eq!(client.cached_visitor().as_deref(), Some("VD2"));
    }
}
