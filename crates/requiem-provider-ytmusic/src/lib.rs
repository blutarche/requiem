//! YouTube Music provider: Innertube client, parsers and ranged byte source, and the
//! signed-in library through a browser session.

mod browser;
mod clients;
mod files;
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

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use requiem_core::{
    AccountState, AudioSource, Browsed, Page, Provider, ProviderError, ProviderId, SearchResults,
    Shelf, TrackId,
};

use innertube::{ClientRefresher, InnertubeClient};
use log::debug;
use parse::{BROWSE_PREFIX, PROVIDER, StreamFormat};
use range::{HttpRangeSource, RangeConfig};
use session::{BrowserSession, Clock, SystemClock};
use transport::{Transport, UreqTransport};

/// The most an `import_client` payload carrying a browser session may hold.
const MAX_SESSION_IMPORT: usize = 256 * 1024;
/// The Google sign-in that versions before browser-session-only sign-in kept in the provider's
/// folder, and `REQUIEM_GOOGLE_TOKEN_FILE` could put elsewhere.
const OLD_TOKEN_NAME: &str = "google-token.json";
const OLD_TOKEN_ENV: &str = "REQUIEM_GOOGLE_TOKEN_FILE";
/// What a sign-in, which YouTube Music takes only as a pasted browser session, answers.
const SIGN_IN_BY_SESSION: &str = "YouTube Music signs in with a browser session: paste one in Settings, or run `requiem signin-browser ytmusic <path|->`";
const SIGNED_OUT: &str = "sign in to YouTube Music with a browser session to see your library";

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
    session: BrowserSession,
    /// No usable `HOME`, so there is nowhere to keep a session.
    homeless: bool,
    prefer_aac: bool,
}

/// The browser session file [`YtMusicProvider::new`] reads, when there is a usable location:
/// in `~/Library/Application Support/requiem` on macOS, `$XDG_STATE_HOME/requiem` on Linux.
pub fn session_file() -> Option<PathBuf> {
    session::path_in(files::data_dir().as_deref())
}

/// Deletes the Google sign-in that earlier versions kept, which nothing reads any more, and a
/// copy a save of it left behind. Nothing is revoked online. The user's own Google client file
/// stays.
pub fn remove_old_google_sign_in() {
    let var = std::env::var_os(OLD_TOKEN_ENV).map(PathBuf::from);
    if let Some(path) = old_token_file(var, files::data_dir())
        && let Err(e) = remove_old_token(&path)
    {
        eprintln!("[ytmusic] cannot delete {}: {e}", path.display());
    }
}

/// `REQUIEM_GOOGLE_TOKEN_FILE` as `var` when it is absolute, else the file in `dir`.
fn old_token_file(var: Option<PathBuf>, dir: Option<PathBuf>) -> Option<PathBuf> {
    match var {
        Some(p) if p.is_absolute() => Some(p),
        _ => Some(dir?.join(OLD_TOKEN_NAME)),
    }
}

fn remove_old_token(path: &Path) -> io::Result<()> {
    let existed = path.exists();
    files::remove(path)?;
    if existed {
        debug!("deleted the old Google sign-in {}", path.display());
    }
    Ok(())
}

/// Keeps `path` out of Time Machine backups, as the browser session is; elsewhere, or on a
/// failure, it does nothing.
pub fn exclude_from_backups(path: &Path) {
    session::exclude_from_backups(path);
}

impl YtMusicProvider {
    /// Reads the browser session at [`session_file`]; makes no network calls.
    pub fn new(opts: YtMusicOptions) -> YtMusicProvider {
        Self::with(
            opts,
            Arc::new(UreqTransport::new()),
            Arc::new(SystemClock),
            session_file(),
        )
    }

    fn with(
        opts: YtMusicOptions,
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
        session_file: Option<PathBuf>,
    ) -> YtMusicProvider {
        let homeless = session_file.is_none();
        let session = BrowserSession::new(transport.clone(), clock, opts.ip_family, session_file);
        Self {
            client: Arc::new(InnertubeClient::new(transport, opts.ip_family)),
            session,
            homeless,
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

    fn search(&self, query: &str, limit: usize) -> Result<SearchResults, ProviderError> {
        self.client.search_all(query, limit)
    }

    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError> {
        let (format, source) = self.open(&track.id)?;
        Ok(AudioSource::Encoded {
            bytes: Box::new(source),
            hint: format.hint,
        })
    }

    fn browse(&self, page: &Page) -> Result<Browsed, ProviderError> {
        let session = self.session.current();
        let shelves = |shelves: Result<Vec<Shelf>, ProviderError>| {
            shelves.map(|shelves| Browsed {
                about: None,
                shelves,
            })
        };
        match page {
            Page::Home => match session {
                Some(s) => shelves(self.session.home(&s)),
                None => Err(ProviderError::SignInRequired(SIGNED_OUT.into())),
            },
            Page::Library => match session {
                Some(s) => shelves(self.session.library(&s)),
                None => Err(ProviderError::SignInRequired(SIGNED_OUT.into())),
            },
            Page::Collection(c) if c.provider.0 == PROVIDER => {
                if let Some(id) = c.id.strip_prefix(BROWSE_PREFIX) {
                    if let Some(s) = session {
                        return self.session.page(&s, id);
                    }
                    parse::parse_page(&self.client.browse(id)?, id, |playlist| {
                        self.client.browse(playlist)
                    })
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

    fn fetch_ahead(&self) -> bool {
        true
    }

    fn account(&self) -> AccountState {
        match self.session.account() {
            Some(state) => state,
            None if self.homeless => AccountState::Unavailable(session::NO_HOME_DIR.into()),
            None => AccountState::SignedOut,
        }
    }

    /// YouTube Music signs in only by `import_client` with a browser session, so this says
    /// how.
    fn sign_in(&self, _on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        Err(ProviderError::SignInRequired(SIGN_IN_BY_SESSION.into()))
    }

    /// Forgets the browser session, which tells Google nothing. A session that can't be
    /// deleted stays signed in.
    fn sign_out(&self) -> Result<(), ProviderError> {
        self.session.sign_out().map(drop)
    }

    /// `{"browser_session": "<pasted headers>"}` signs in with a browser session.
    fn import_client(&self, json: &str) -> Result<(), ProviderError> {
        match browser_session(json) {
            Some(paste) => self.session.import(&paste?),
            None => Err(ProviderError::SignInRequired(SIGN_IN_BY_SESSION.into())),
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

    use requiem_core::{CollectionId, CollectionKind, ProviderId, SEARCH_COLLECTIONS};

    use super::*;
    use crate::clients::{ALBUMS_PARAMS, ARTISTS_PARAMS, PLAYLISTS_PARAMS, SONGS_PARAMS};
    use crate::testing::{FakeMusic, TempDir};

    // Synthetic responses; every value in them is made up.
    const MENU: &str = include_str!("../tests/fixtures/account_menu.json");
    const HOME: &str = include_str!("../tests/fixtures/signed_in_home.json");
    const SEARCH: &str = include_str!("../tests/fixtures/search_songs.json");
    const ARTISTS: &str = include_str!("../tests/fixtures/search_artists.json");
    const ALBUMS: &str = include_str!("../tests/fixtures/search_albums.json");
    const PLAYLISTS: &str = include_str!("../tests/fixtures/search_playlists.json");
    const TOP_ARTIST: &str = include_str!("../tests/fixtures/search_top_artist.json");
    const ARTIST: &str = include_str!("../tests/fixtures/browse_artist.json");
    const PLAYER: &str = include_str!("../tests/fixtures/player_ok.json");
    const PLAYLIST: &str = include_str!("../tests/fixtures/browse_playlist.json");
    const TOP_SONGS: &str = include_str!("../tests/fixtures/browse_artist_top_songs.json");
    const TOP_SONGS_ID: &str = "VLOLAK5uy_lNVBcjNtiCwq-n95uoxJ-Gd4vsfMkyZNs";
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
        tmp: TempDir,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                music: Arc::new(FakeMusic::default()),
                tmp: TempDir::new(),
            }
        }

        fn dir(&self) -> PathBuf {
            self.tmp.path().join("requiem")
        }

        fn session_file(&self) -> PathBuf {
            self.dir().join("ytmusic-session.json")
        }

        fn provider(&self) -> YtMusicProvider {
            YtMusicProvider::with(
                YtMusicOptions::default(),
                self.music.clone(),
                Arc::new(SystemClock),
                session::path_in(Some(&self.dir())),
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
        rig.music
            .script_search(SEARCH_URL, SONGS_PARAMS, 200, SEARCH);
        rig.music.script(PLAYER_URL, 200, PLAYER);
        rig.music.script(BROWSE_URL, 200, HOME);
        rig.music
            .script(BROWSE_URL, 200, r#"{"continuationContents":{}}"#);
        // Home's liked music and the library, then the collection.
        rig.music.script(BROWSE_URL, 200, PLAYLIST);
        rig.music.script(BROWSE_URL, 200, LIBRARY);
        rig.music.script(BROWSE_URL, 200, PLAYLIST);
        // Artists, albums and playlists go unanswered, which leaves them out.
        let tracks = p.search("fake query", 5).unwrap().tracks;
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
        assert_eq!(
            anonymous_urls,
            [
                SEARCH_URL, SEARCH_URL, SEARCH_URL, SEARCH_URL, SEARCH_URL, PLAYER_URL
            ]
        );
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
    fn only_a_browser_session_signs_in() {
        let rig = Rig::new();
        let p = rig.provider();
        assert_eq!(p.account(), AccountState::SignedOut);
        let e = p.sign_in(&|_| panic!("no state to report")).unwrap_err();
        assert!(e.to_string().contains("signin-browser ytmusic"), "{e}");
        assert!(rig.music.calls.lock().unwrap().is_empty());

        let p = rig.signed_in();
        assert_eq!(
            p.account(),
            AccountState::SignedIn {
                name: Some("Requiem Tester".into())
            }
        );
        assert!(rig.session_file().exists());

        // An OAuth client, as earlier versions took, is refused and changes nothing.
        let e = p
            .import_client(r#"{"installed":{"client_id":"cid2","client_secret":"s2"}}"#)
            .unwrap_err();
        assert_eq!(e, ProviderError::SignInRequired(SIGN_IN_BY_SESSION.into()));
        assert!(p.account() != AccountState::SignedOut);
        assert!(rig.session_file().exists());

        let e = p.import_client(r#"{"browser_session": 42}"#).unwrap_err();
        assert!(e.to_string().contains("browser_session"), "{e}");
        // Too big to be a session import.
        let big = serde_json::json!({ "browser_session": "x".repeat(MAX_SESSION_IMPORT) });
        let e = p.import_client(&big.to_string()).unwrap_err();
        assert_eq!(e, ProviderError::SignInRequired(SIGN_IN_BY_SESSION.into()));
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
        rig.music.calls.lock().unwrap().clear();
        p.sign_out().unwrap();
        assert_eq!(p.account(), AccountState::SignedOut);
        assert!(!rig.session_file().exists());
        assert!(rig.music.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn signed_out_home_and_library_ask_for_a_session() {
        let rig = Rig::new();
        let p = rig.provider();
        for page in [Page::Home, Page::Library] {
            assert_eq!(
                p.browse(&page),
                Err(ProviderError::SignInRequired(SIGNED_OUT.into()))
            );
        }
        assert!(rig.music.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn with_nowhere_to_keep_a_session_sign_in_is_unavailable() {
        let rig = Rig::new();
        let p = YtMusicProvider::with(
            YtMusicOptions::default(),
            rig.music.clone(),
            Arc::new(SystemClock),
            None,
        );
        assert_eq!(
            p.account(),
            AccountState::Unavailable(session::NO_HOME_DIR.into())
        );
    }

    /// The files a 0.3 Google sign-in left in the provider's folder.
    fn old_google_files(dir: &Path) -> [PathBuf; 3] {
        fs::create_dir_all(dir).unwrap();
        let files = [
            dir.join(OLD_TOKEN_NAME),
            files::staged_path(&dir.join(OLD_TOKEN_NAME)),
            dir.join("google-client.json"),
        ];
        for f in &files {
            fs::write(f, r#"{"refresh_token":"RT-FAKE"}"#).unwrap();
        }
        files
    }

    #[test]
    fn a_google_sign_in_from_before_reads_as_signed_out_and_is_deleted() {
        let rig = Rig::new();
        let [token, staged, client] = old_google_files(&rig.dir());
        let p = rig.provider();
        assert_eq!(p.account(), AccountState::SignedOut);
        assert!(matches!(
            p.browse(&Page::Library),
            Err(ProviderError::SignInRequired(_))
        ));

        let path = old_token_file(None, Some(rig.dir())).unwrap();
        assert_eq!(path, token);
        remove_old_token(&path).unwrap();
        assert!(!token.exists() && !staged.exists());
        // The user's own client file stays.
        assert!(client.exists());
        // Once is enough; again finds nothing and is no error.
        remove_old_token(&path).unwrap();
    }

    #[test]
    fn the_old_sign_in_is_looked_for_where_its_variable_put_it() {
        let dir = Some(PathBuf::from("/st/requiem"));
        assert_eq!(
            old_token_file(Some("/elsewhere/t.json".into()), dir.clone()),
            Some(PathBuf::from("/elsewhere/t.json"))
        );
        assert_eq!(
            old_token_file(Some("t.json".into()), dir.clone()),
            Some(PathBuf::from("/st/requiem/google-token.json"))
        );
        assert_eq!(
            old_token_file(None, dir),
            Some(PathBuf::from("/st/requiem/google-token.json"))
        );
        assert_eq!(old_token_file(None, None), None);
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

    fn search_params(rig: &Rig) -> Vec<String> {
        let mut params: Vec<String> = rig
            .music
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                let body: serde_json::Value = serde_json::from_str(&c.body).ok()?;
                Some(body["params"].as_str()?.to_owned())
            })
            .collect();
        params.sort();
        params
    }

    #[test]
    fn search_finds_albums_and_playlists_beside_the_songs() {
        let rig = Rig::new();
        rig.music
            .script_search(SEARCH_URL, SONGS_PARAMS, 200, SEARCH);
        rig.music
            .script_search(SEARCH_URL, ALBUMS_PARAMS, 200, ALBUMS);
        rig.music
            .script_search(SEARCH_URL, PLAYLISTS_PARAMS, 200, PLAYLISTS);
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!(found.tracks.len(), 5);
        assert_eq!(found.albums.len(), 6);
        assert!(found.albums.iter().all(|a| a.kind == CollectionKind::Album));
        assert_eq!(found.albums[0].id.id, "browse:MPREb_K8qWMWVqXGi");
        assert_eq!(found.playlists.len(), 6);
        assert!(
            found
                .playlists
                .iter()
                .all(|p| p.kind == CollectionKind::Playlist)
        );
        let mut want = vec![
            SONGS_PARAMS,
            ARTISTS_PARAMS,
            ALBUMS_PARAMS,
            PLAYLISTS_PARAMS,
        ];
        want.sort_unstable();
        assert_eq!(search_params(&rig), want);
    }

    fn script_filtered_searches(rig: &Rig) {
        rig.music
            .script_search(SEARCH_URL, ARTISTS_PARAMS, 200, ARTISTS);
        rig.music
            .script_search(SEARCH_URL, SONGS_PARAMS, 200, SEARCH);
        rig.music
            .script_search(SEARCH_URL, ALBUMS_PARAMS, 200, ALBUMS);
        rig.music
            .script_search(SEARCH_URL, PLAYLISTS_PARAMS, 200, PLAYLISTS);
    }

    fn ids(found: &[requiem_core::Collection]) -> Vec<&str> {
        found.iter().map(|c| c.id.id.as_str()).collect()
    }

    #[test]
    fn search_leads_with_the_artists_own_albums_and_playlists() {
        let rig = Rig::new();
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 200, TOP_ARTIST);
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, ARTIST);
        let found = rig.provider().search("daft punk", 5).unwrap();

        let titles: Vec<&str> = found.albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(found.albums.len(), SEARCH_COLLECTIONS);
        assert_eq!(titles[0], "Random Access Memories (Drumless Edition)");
        assert!(
            titles.contains(&"Discovery")
                && titles.contains(&"Homework (25th Anniversary Edition)")
        );
        // Ten albums, then the first singles; the title matches fall off the end.
        assert_eq!(found.albums[10].title, "GLBTM (Studio Outtakes)");
        assert_eq!(
            found.albums[10].subtitle.as_deref(),
            Some("Single \u{2022} Daft Punk \u{2022} 2023")
        );
        assert!(!titles.contains(&"New Daft Punk"));

        // Four featured playlists, then the four title matches the page did not already list.
        let listed = ids(&found.playlists);
        assert_eq!(found.playlists[0].title, "Presenting Daft Punk");
        assert_eq!(listed.len(), 4 + 6 - 2);
        let mut unique = listed.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), listed.len());
    }

    #[test]
    fn search_is_unchanged_without_an_artist_card() {
        let album_card = TOP_ARTIST.replace("MUSIC_PAGE_TYPE_ARTIST", "MUSIC_PAGE_TYPE_ALBUM");
        for unfiltered in [SEARCH, album_card.as_str()] {
            let rig = Rig::new();
            script_filtered_searches(&rig);
            rig.music.script(SEARCH_URL, 200, unfiltered);
            let found = rig.provider().search("one more time", 5).unwrap();
            assert_eq!(names(&found.artists), ARTIST_NAMES);
            assert!(!found.top_artist, "the search is about none of them");
            assert_eq!(found.albums.len(), 6);
            assert_eq!(found.albums[0].id.id, "browse:MPREb_K8qWMWVqXGi");
            assert_eq!(found.playlists.len(), 6);
            let browses = rig.music.calls.lock().unwrap();
            assert!(browses.iter().all(|c| !c.url.contains("/browse")));
        }
    }

    #[test]
    fn search_keeps_the_title_matches_when_the_artist_cannot_be_had() {
        let rig = Rig::new();
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 200, TOP_ARTIST);
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 500, "{}");
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!(found.albums.len(), 6);
        assert_eq!(found.albums[0].title, "Random Access Memories");
        assert_eq!(found.playlists.len(), 6);

        // An artist page nobody can read, and an unfiltered search that fails.
        let rig = Rig::new();
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 200, TOP_ARTIST);
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, "not json");
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!((found.albums.len(), found.playlists.len()), (6, 6));

        let rig = Rig::new();
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 503, "{}");
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!((found.albums.len(), found.playlists.len()), (6, 6));
    }

    /// The artists the Artists filter finds for "daft punk", in its order.
    const ARTIST_NAMES: [&str; 6] = [
        "Daft Punk",
        "The Weeknd",
        "Pharrell",
        "The Strokes",
        "Pentatonix",
        "LCD Soundsystem",
    ];

    fn names(found: &[requiem_core::Collection]) -> Vec<&str> {
        found.iter().map(|c| c.title.as_str()).collect()
    }

    /// The Artists filter's answer with its first and third rows swapped.
    fn artists_with_daft_punk_third() -> String {
        let mut v: serde_json::Value = serde_json::from_str(ARTISTS).unwrap();
        v.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicShelfRenderer/contents")
            .and_then(serde_json::Value::as_array_mut)
            .unwrap()
            .swap(0, 2);
        v.to_string()
    }

    #[test]
    fn search_finds_artists_with_the_top_card_first_and_once() {
        let rig = Rig::new();
        rig.music.script_search(
            SEARCH_URL,
            ARTISTS_PARAMS,
            200,
            &artists_with_daft_punk_third(),
        );
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 200, TOP_ARTIST);
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, ARTIST);
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!(
            names(&found.artists),
            [
                "Daft Punk",
                "Pharrell",
                "The Weeknd",
                "The Strokes",
                "Pentatonix",
                "LCD Soundsystem"
            ]
        );
        assert!(found.top_artist, "the search is about the card's artist");
        let lead = &found.artists[0];
        assert_eq!(lead.id.id, "browse:UCRr1xG_2WIDs18a6cIiCxeA");
        assert_eq!(lead.kind, CollectionKind::Artist);
        assert_eq!(
            lead.subtitle.as_deref(),
            Some("Artist \u{2022} 68.7M monthly audience")
        );
        assert!(lead.artwork_url.is_some());
        assert!(
            found
                .artists
                .iter()
                .all(|a| a.kind == CollectionKind::Artist)
        );
    }

    #[test]
    fn search_keeps_everything_else_when_the_artists_search_fails() {
        let rig = Rig::new();
        rig.music
            .script_search(SEARCH_URL, ARTISTS_PARAMS, 500, "{}");
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 200, SEARCH);
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert!(found.artists.is_empty());
        assert!(!found.top_artist);
        assert_eq!(found.tracks.len(), 5);
        assert_eq!((found.albums.len(), found.playlists.len()), (6, 6));

        // The card's artist still leads, alone.
        let rig = Rig::new();
        rig.music
            .script_search(SEARCH_URL, ARTISTS_PARAMS, 200, "not json");
        script_filtered_searches(&rig);
        rig.music.script(SEARCH_URL, 200, TOP_ARTIST);
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, ARTIST);
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!(names(&found.artists), ["Daft Punk"]);
        assert!(found.top_artist);
        assert_eq!(found.tracks.len(), 5);
        assert_eq!(found.albums.len(), SEARCH_COLLECTIONS);
    }

    fn daft_punk() -> Page {
        Page::Collection(CollectionId {
            provider: ProviderId(PROVIDER.to_owned()),
            id: "browse:UCRr1xG_2WIDs18a6cIiCxeA".into(),
        })
    }

    fn titles(shelves: &[Shelf]) -> Vec<(&str, usize)> {
        shelves
            .iter()
            .map(|s| (s.title.as_str(), s.items.len()))
            .collect()
    }

    const ARTIST_SHELVES: [(&str, usize); 4] = [
        ("Top songs", 5),
        ("Albums", 10),
        ("Singles & EPs", 10),
        ("Featured on", 4),
    ];

    fn durations(shelves: &[Shelf]) -> Vec<Option<u64>> {
        shelves[0]
            .items
            .iter()
            .map(|item| match item {
                requiem_core::Item::Track(t) => t.duration.map(|d| d.as_secs()),
                requiem_core::Item::Collection(_) => panic!("top songs are tracks"),
            })
            .collect()
    }

    const TOP_SONGS_SECS: [Option<u64>; 5] =
        [Some(338), Some(249), Some(321), Some(430), Some(346)];

    #[test]
    fn an_artist_opens_as_its_page_signed_in_or_not() {
        let rig = Rig::new();
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, ARTIST);
        rig.music
            .script_browse(BROWSE_URL, TOP_SONGS_ID, 200, TOP_SONGS);
        let page = rig.provider().browse(&daft_punk()).unwrap();
        assert_eq!(titles(&page.shelves), ARTIST_SHELVES);
        assert_eq!(durations(&page.shelves), TOP_SONGS_SECS);
        assert_eq!(page.about.unwrap().title, "Daft Punk");
        assert!(rig.music.calls.lock().unwrap().iter().all(|c| !c.signed));

        let rig = Rig::new();
        let p = rig.signed_in();
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, ARTIST);
        rig.music
            .script_browse(BROWSE_URL, TOP_SONGS_ID, 200, TOP_SONGS);
        let page = p.browse(&daft_punk()).unwrap();
        assert_eq!(titles(&page.shelves), ARTIST_SHELVES);
        assert_eq!(durations(&page.shelves), TOP_SONGS_SECS);
        assert_eq!(page.about.unwrap().title, "Daft Punk");
        let calls = rig.music.calls.lock().unwrap();
        let asked = |id: &str| calls.iter().find(|c| c.body.contains(id)).unwrap().signed;
        assert!(asked("UCRr1xG_2WIDs18a6cIiCxeA") && asked(TOP_SONGS_ID));
    }

    #[test]
    fn an_artist_page_opens_without_durations_when_its_playlist_fails() {
        let rig = Rig::new();
        rig.music
            .script_browse(BROWSE_URL, "UCRr1xG_2WIDs18a6cIiCxeA", 200, ARTIST);
        rig.music.script_browse(BROWSE_URL, TOP_SONGS_ID, 500, "{}");
        let shelves = rig.provider().browse(&daft_punk()).unwrap().shelves;
        assert_eq!(titles(&shelves), ARTIST_SHELVES);
        assert_eq!(durations(&shelves), [None; 5]);
    }

    #[test]
    fn search_keeps_the_songs_and_albums_when_the_playlists_fail() {
        let rig = Rig::new();
        rig.music
            .script_search(SEARCH_URL, SONGS_PARAMS, 200, SEARCH);
        rig.music
            .script_search(SEARCH_URL, ALBUMS_PARAMS, 200, ALBUMS);
        rig.music
            .script_search(SEARCH_URL, PLAYLISTS_PARAMS, 500, "{}");
        let found = rig.provider().search("daft punk", 5).unwrap();
        assert_eq!(found.tracks.len(), 5);
        assert_eq!(found.albums.len(), 6);
        assert!(found.playlists.is_empty());
    }

    #[test]
    fn search_fails_when_the_songs_fail_whatever_else_is_found() {
        let rig = Rig::new();
        rig.music.script_search(SEARCH_URL, SONGS_PARAMS, 403, "{}");
        rig.music
            .script_search(SEARCH_URL, ALBUMS_PARAMS, 200, ALBUMS);
        rig.music
            .script_search(SEARCH_URL, PLAYLISTS_PARAMS, 200, PLAYLISTS);
        assert!(matches!(
            rig.provider().search("daft punk", 5),
            Err(ProviderError::ClientBroken(_))
        ));
    }
}
