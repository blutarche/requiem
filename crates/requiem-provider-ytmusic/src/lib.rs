//! YouTube Music provider: Innertube client, parsers and ranged byte source, and the
//! signed-in library through a browser session, or else through Google sign-in and the
//! YouTube Data API.

mod browser;
mod clients;
mod dataapi;
mod google;
mod innertube;
mod log;
mod parse;
mod range;
mod session;
mod transport;

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod testing;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use requiem_core::{
    AccountState, AudioSource, Page, Provider, ProviderError, ProviderId, Shelf, Track, TrackId,
};

use dataapi::DATA_PREFIX;
use google::{Clock, Google, GoogleFiles, SystemClock};
use innertube::{ClientRefresher, InnertubeClient};
use parse::{BROWSE_PREFIX, PROVIDER, StreamFormat};
use range::{HttpRangeSource, RangeConfig};
use session::BrowserSession;
use transport::{ApiHttp, Transport, UreqTransport};

/// The most an `import_client` payload carrying a browser session may hold.
const MAX_SESSION_IMPORT: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IpFamily {
    #[default]
    Any,
    V4,
    V6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct YtMusicOptions {
    /// Prefer AAC (itag 140) over Opus.
    pub prefer_aac: bool,
    pub ip_family: IpFamily,
}

pub struct YtMusicProvider {
    client: Arc<InnertubeClient>,
    google: Google,
    session: BrowserSession,
    prefer_aac: bool,
}

/// The Google sign-in file [`YtMusicProvider::new`] reads, when there is a usable location.
pub fn token_file() -> Option<PathBuf> {
    GoogleFiles::from_env().token
}

/// The browser session file [`YtMusicProvider::new`] reads: beside [`token_file`].
pub fn session_file() -> Option<PathBuf> {
    session::path_beside(token_file().as_deref())
}

/// Keeps `path` out of Time Machine backups, as the browser session is; elsewhere, or on a
/// failure, it does nothing.
pub fn exclude_from_backups(path: &Path) {
    session::exclude_from_backups(path);
}

/// Parses the Google sign-in file at `path` without changing it; a missing file passes.
pub fn check_token_file(path: &Path) -> Result<(), String> {
    google::check_token(path)
}

impl YtMusicProvider {
    /// Reads the Google client and sign-in files named by `REQUIEM_GOOGLE_CLIENT_FILE` and
    /// `REQUIEM_GOOGLE_TOKEN_FILE`, or their defaults, and the browser session beside the
    /// sign-in; makes no network calls.
    pub fn new(opts: YtMusicOptions) -> YtMusicProvider {
        let transport = Arc::new(UreqTransport::new());
        Self::with(
            opts,
            transport.clone(),
            transport,
            Arc::new(SystemClock),
            GoogleFiles::from_env(),
        )
    }

    fn with(
        opts: YtMusicOptions,
        transport: Arc<dyn Transport>,
        api: Arc<dyn ApiHttp>,
        clock: Arc<dyn Clock>,
        files: GoogleFiles,
    ) -> YtMusicProvider {
        let session = BrowserSession::new(
            transport.clone(),
            clock.clone(),
            opts.ip_family,
            session::path_beside(files.token.as_deref()),
        );
        Self {
            client: Arc::new(InnertubeClient::new(transport, opts.ip_family)),
            google: Google::new(api, clock, files),
            session,
            prefer_aac: opts.prefer_aac,
        }
    }

    pub(crate) fn open(
        &self,
        video_id: &str,
    ) -> Result<(StreamFormat, HttpRangeSource), ProviderError> {
        let (format, family) = self.client.resolve(video_id, self.prefer_aac)?;
        let source = HttpRangeSource::new(
            format.url.clone(),
            format.itag,
            format.content_length,
            self.client.fetcher(family),
            Arc::new(ClientRefresher {
                client: self.client.clone(),
                video_id: video_id.to_owned(),
                prefer_aac: self.prefer_aac,
                family,
            }),
            RangeConfig::default(),
        );
        Ok((format, source))
    }
}

impl Provider for YtMusicProvider {
    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER.to_owned())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        self.client.search(query, limit)
    }

    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError> {
        let (format, source) = self.open(&track.id)?;
        Ok(AudioSource::Encoded {
            bytes: Box::new(source),
            hint: format.hint,
        })
    }

    fn browse(&self, page: &Page) -> Result<Vec<Shelf>, ProviderError> {
        let session = self.session.current();
        match page {
            Page::Home => match session {
                Some(s) => self.session.home(&s),
                None => dataapi::home(&self.google),
            },
            Page::Library => match session {
                Some(s) => self.session.library(&s),
                None => dataapi::library(&self.google),
            },
            Page::Collection(c) if c.provider.0 == PROVIDER => {
                if let Some(id) = c.id.strip_prefix(DATA_PREFIX) {
                    dataapi::playlist(&self.google, id)
                } else if let Some(id) = c.id.strip_prefix(BROWSE_PREFIX) {
                    if let Some(s) = session {
                        return Ok(vec![self.session.collection(&s, id)?]);
                    }
                    let body = self.client.browse(id)?;
                    Ok(vec![parse::parse_collection(
                        &body,
                        parse::collection_kind(id),
                    )?])
                } else {
                    Err(ProviderError::Unplayable(format!(
                        "unknown collection {}",
                        c.id
                    )))
                }
            }
            Page::Collection(c) => Err(ProviderError::Unplayable(format!(
                "collection {} belongs to {}",
                c.id, c.provider.0
            ))),
        }
    }

    fn has_home(&self) -> bool {
        true
    }

    fn account(&self) -> AccountState {
        self.session
            .account()
            .unwrap_or_else(|| self.google.account())
    }

    fn sign_in(&self, on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        self.google.sign_in(on_state)
    }

    /// Forgets the browser session, which tells Google nothing, and signs out of Google. A
    /// session that can't be deleted leaves both signed in.
    fn sign_out(&self) -> Result<(), ProviderError> {
        self.session.sign_out()?;
        self.google.sign_out()
    }

    /// `{"browser_session": "<pasted headers>"}` signs in with a browser session; anything else
    /// is a Google OAuth client.
    fn import_client(&self, json: &str) -> Result<(), ProviderError> {
        match browser_session(json) {
            Some(paste) => self.session.import(&paste?),
            None => self.google.import_client(json),
        }
    }
}

/// The paste in a browser session import, or `None` for anything else.
fn browser_session(json: &str) -> Option<Result<String, ProviderError>> {
    if json.len() > MAX_SESSION_IMPORT {
        return None;
    }
    let serde_json::Value::Object(mut obj) = serde_json::from_str(json).ok()? else {
        return None;
    };
    Some(match obj.remove("browser_session")? {
        serde_json::Value::String(paste) => Ok(paste),
        _ => Err(ProviderError::SignInRequired(
            "browser_session must be the pasted headers, as text".into(),
        )),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use requiem_core::{CollectionId, ProviderId};

    use super::*;
    use crate::testing::{FakeApi, FakeMusic, TempDir};

    // Synthetic responses; every value in them is made up.
    const MENU: &str = include_str!("../tests/fixtures/account_menu.json");
    const HOME: &str = include_str!("../tests/fixtures/signed_in_home.json");
    const SEARCH: &str = include_str!("../tests/fixtures/search_songs.json");
    const PLAYER: &str = include_str!("../tests/fixtures/player_ok.json");
    const PLAYLIST: &str = include_str!("../tests/fixtures/browse_playlist.json");
    const LIBRARY: &str = include_str!("../tests/fixtures/signed_in_library_playlists.json");

    const SECRET: &str = "fakeSID-MARKER";
    const SAPISID: &str = "FAKEsapisid-0123/AbCdEfGh";
    const SESSION_VISITOR: &str = "fakeSessionVisitor";
    const MENU_URL: &str = "https://music.youtube.com/youtubei/v1/account/account_menu";
    const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse";
    const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search";
    const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player";

    fn paste() -> String {
        format!(
            "cookie: SID={SECRET}; __Secure-3PAPISID={SAPISID}\n\
             x-goog-authuser: 0\nx-goog-visitor-id: {SESSION_VISITOR}\n"
        )
    }

    fn import() -> String {
        serde_json::json!({ "browser_session": paste() }).to_string()
    }

    struct Rig {
        music: Arc<FakeMusic>,
        api: Arc<FakeApi>,
        tmp: TempDir,
    }

    impl Rig {
        fn new() -> Self {
            let tmp = TempDir::new();
            fs::write(
                tmp.path().join("client.json"),
                r#"{"installed":{"client_id":"cid","client_secret":"csecret"}}"#,
            )
            .unwrap();
            Self {
                music: Arc::new(FakeMusic::default()),
                api: Arc::new(FakeApi::default()),
                tmp,
            }
        }

        fn files(&self) -> GoogleFiles {
            GoogleFiles {
                client: Some(self.tmp.path().join("client.json")),
                token: Some(self.tmp.path().join("requiem/google-token.json")),
            }
        }

        fn session_file(&self) -> PathBuf {
            self.tmp.path().join("requiem/ytmusic-session.json")
        }

        fn provider(&self) -> YtMusicProvider {
            YtMusicProvider::with(
                YtMusicOptions::default(),
                self.music.clone(),
                self.api.clone(),
                Arc::new(SystemClock),
                self.files(),
            )
        }

        fn signed_in(&self) -> YtMusicProvider {
            self.music.script(MENU_URL, 200, MENU);
            self.music.script(BROWSE_URL, 200, HOME);
            let p = self.provider();
            p.import_client(&import()).unwrap();
            p
        }
    }

    #[test]
    fn the_session_never_reaches_search_or_playback() {
        let rig = Rig::new();
        let p = rig.signed_in();
        rig.music.script(SEARCH_URL, 200, SEARCH);
        rig.music.script(PLAYER_URL, 200, PLAYER);
        rig.music.script(BROWSE_URL, 200, HOME);
        rig.music
            .script(BROWSE_URL, 200, r#"{"continuationContents":{}}"#);
        // Home's liked music and the library, then the collection.
        rig.music.script(BROWSE_URL, 200, PLAYLIST);
        rig.music.script(BROWSE_URL, 200, LIBRARY);
        rig.music.script(BROWSE_URL, 200, PLAYLIST);
        let tracks = p.search("fake query", 5).unwrap();
        assert!(!tracks.is_empty());
        p.resolve(&tracks[0].id).unwrap();
        p.browse(&Page::Home).unwrap();
        p.browse(&Page::Collection(CollectionId {
            provider: ProviderId(PROVIDER.to_owned()),
            id: "browse:VLPLfakeRoadTrip01".into(),
        }))
        .unwrap();

        let calls = rig.music.calls.lock().unwrap();
        let anonymous: Vec<_> = calls.iter().filter(|c| !c.signed).collect();
        let anonymous_urls: Vec<_> = anonymous
            .iter()
            .map(|c| c.url.split('?').next().unwrap())
            .collect();
        assert_eq!(anonymous_urls, [SEARCH_URL, PLAYER_URL]);
        for c in anonymous {
            for (k, v) in &c.headers {
                assert!(
                    !["Cookie", "Authorization", "X-Goog-AuthUser"].contains(&k.as_str()),
                    "{} sent {k}",
                    c.url
                );
                for secret in [SECRET, SAPISID, SESSION_VISITOR, "SAPISIDHASH"] {
                    assert!(!v.contains(secret), "{} sent {k}", c.url);
                }
            }
            assert!(!c.body.contains(SECRET) && !c.body.contains(SAPISID));
        }
        let signed: Vec<_> = calls
            .iter()
            .filter(|c| c.signed)
            .map(|c| c.url.split('?').next().unwrap())
            .collect();
        assert_eq!(
            signed,
            [
                MENU_URL, BROWSE_URL, BROWSE_URL, BROWSE_URL, BROWSE_URL, BROWSE_URL, BROWSE_URL
            ]
        );
    }

    #[test]
    fn a_session_import_is_told_apart_from_a_google_client() {
        let rig = Rig::new();
        let p = rig.signed_in();
        assert_eq!(
            p.account(),
            AccountState::SignedIn {
                name: Some("Requiem Tester".into())
            }
        );
        assert!(rig.session_file().exists());
        // The Google client and sign-in stay as they were.
        assert!(rig.api.calls().is_empty());
        assert!(!rig.files().token.unwrap().exists());

        // A client goes to Google, leaving the session.
        p.import_client(r#"{"installed":{"client_id":"cid2","client_secret":"s2"}}"#)
            .unwrap();
        let client = fs::read_to_string(rig.files().client.unwrap()).unwrap();
        assert!(client.contains("cid2"));
        assert!(rig.session_file().exists());

        let e = p.import_client(r#"{"browser_session": 42}"#).unwrap_err();
        assert!(e.to_string().contains("browser_session"), "{e}");
        // Too big to be a session import, so the Google client path refuses it.
        let big = serde_json::json!({ "browser_session": "x".repeat(MAX_SESSION_IMPORT) });
        let e = p.import_client(&big.to_string()).unwrap_err();
        assert!(e.to_string().contains("Google OAuth client"), "{e}");
        // A session over the paste's own cap but under the payload's.
        let e = p
            .import_client(
                &serde_json::json!({ "browser_session": "x".repeat(65 * 1024) }).to_string(),
            )
            .unwrap_err();
        assert!(e.to_string().contains("over 64 KiB"), "{e}");
        assert!(rig.session_file().exists());
    }

    #[test]
    fn signing_out_of_a_session_tells_google_nothing() {
        let rig = Rig::new();
        let p = rig.signed_in();
        p.sign_out().unwrap();
        assert_eq!(p.account(), AccountState::SignedOut);
        assert!(!rig.session_file().exists());
        assert!(rig.api.calls().is_empty());
    }

    #[test]
    fn an_ended_session_signs_out() {
        let rig = Rig::new();
        let p = rig.signed_in();
        rig.music.script(BROWSE_URL, 401, "{}");
        let e = p.browse(&Page::Home).unwrap_err();
        assert_eq!(
            e,
            ProviderError::SignInRequired(
                "Your YouTube Music browser session ended; paste a new one in Settings".into()
            )
        );
        assert_eq!(p.account(), AccountState::SignedOut);
        assert!(!rig.session_file().exists());
    }
}
