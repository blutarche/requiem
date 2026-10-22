//! A YouTube Music browser session: the cookie of a signed-in music.youtube.com tab, checked
//! with YouTube Music and kept beside the Google sign-in. Innertube takes it where it refuses
//! OAuth tokens, so Home, the Library and private playlists come as music.youtube.com shows
//! them. Search and playback never carry it.

use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use requiem_core::{
    AccountState, Collection, CollectionId, CollectionKind, HOME_LIKED, HOME_MIXES, HOME_PLAYLISTS,
    Item, ProviderError, ProviderId, Shelf,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::IpFamily;
use crate::browser::{self, BrowserHeaders};
use crate::clients::{MUSIC_CLIENT_ID, MUSIC_CLIENT_VERSION, MUSIC_ORIGIN, MUSIC_UA};
use crate::google::{Clock, commit, private_dir, read_capped, stage_private, staged_path, tighten};
use crate::innertube::{InnertubeClient, status_error};
use crate::log::debug;
use crate::parse::{
    BROWSE_PREFIX, PROVIDER, collection_kind, logged_in, parse_collection, parse_home,
    parse_library_playlists,
};
use crate::transport::Transport;

const FILE_NAME: &str = "ytmusic-session.json";
const VERSION: u32 = 1;
const API: &str = "https://music.youtube.com/youtubei/v1/";
/// music.youtube.com's own key, which ytmusicapi sends with a browser session.
const API_KEY: &str = "AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
/// Home's shelves, its first page's and its next pages' together.
const HOME_SHELVES: usize = 6;
/// Tracks of the liked music playlist on Home.
const HOME_LIKED_TRACKS: usize = 20;
/// What YouTube Music calls its mixes shelf (requests ask for English).
const MIXED_FOR_YOU: &str = "Mixed for you";
const LIKED: &str = "LM";
/// YouTube Music's own Liked music picture, the same for everyone.
const LIKED_ARTWORK: &str =
    "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-music-@576.png";

const ENDED: &str = "Your YouTube Music browser session ended; paste a new one in Settings";
const NOT_ACCEPTED: &str = "Google didn't accept that session; copy it again from a browser where you're signed in to YouTube Music";
const WEB_PAGE: &str =
    "YouTube answered with a web page, not data; the session may need consent or a fresh sign-in";
const REDIRECTED: &str = "YouTube redirected a signed-in request";
const NO_NAME: &str = "YouTube Music named no account for that session";
const NO_HOME: &str = "YouTube Music showed no Home for that session";
const CHANGED: &str =
    "YouTube Music was signed out while that session was checked, so it wasn't kept";

/// The plist `tmutil addexclusion` writes, read back on macOS 26: Time Machine skips the file.
#[cfg(target_os = "macos")]
const BACKUP_EXCLUDE: [u8; 61] = [
    0x62, 0x70, 0x6C, 0x69, 0x73, 0x74, 0x30, 0x30, 0x5F, 0x10, 0x11, 0x63, 0x6F, 0x6D, 0x2E, 0x61,
    0x70, 0x70, 0x6C, 0x65, 0x2E, 0x62, 0x61, 0x63, 0x6B, 0x75, 0x70, 0x64, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C,
];
#[cfg(target_os = "macos")]
const BACKUP_EXCLUDE_NAME: &std::ffi::CStr = c"com.apple.metadata:com_apple_backup_excludeItem";

#[derive(Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    cookie: String,
    authuser: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visitor_id: Option<String>,
    account_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel_handle: Option<String>,
    saved_at_unix: u64,
}

impl fmt::Debug for SessionFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionFile")
            .field("version", &self.version)
            .field("cookie", &"<redacted>")
            .field("authuser", &self.authuser)
            .field("account_name", &self.account_name)
            .finish_non_exhaustive()
    }
}

/// A session YouTube Music took, as of `gen`.
#[derive(Debug)]
pub(crate) struct Signed {
    headers: BrowserHeaders,
    account_name: String,
    r#gen: u64,
}

#[derive(Default)]
struct Slot {
    current: Option<Arc<Signed>>,
    /// Bumped by every import, sign-out and expiry, so a request made with one session never
    /// ends another.
    r#gen: u64,
    /// Bumped by every sign-out only: an import checked across one isn't kept, while one
    /// checked across an expiry or another import is.
    sign_outs: u64,
}

/// Why a signed request failed.
enum Failure {
    /// A 401, or an answer that takes the request as signed out.
    SignedOut,
    /// A 403, which is as often a challenge or a rate limit as a refusal.
    Forbidden,
    Other(ProviderError),
}

pub(crate) struct BrowserSession {
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    family: IpFamily,
    /// `None` when there is no usable location for it.
    path: Option<PathBuf>,
    slot: Mutex<Slot>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the session is kept: beside the Google sign-in.
pub(crate) fn path_beside(token: Option<&Path>) -> Option<PathBuf> {
    Some(token?.parent()?.join(FILE_NAME))
}

fn load(path: &Path) -> Option<Signed> {
    tighten(path);
    let bytes = match read_capped(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            debug!("browser session unreadable: {e}");
            return None;
        }
    };
    // A file put back by a failed Restart to Update is a new file, without the exclusion.
    exclude_from_backups(path);
    // serde's errors can quote the file, so none is logged.
    let Ok(file) = serde_json::from_slice::<SessionFile>(&bytes) else {
        debug!("browser session unparsable; treating as signed out");
        return None;
    };
    if file.version != VERSION || file.account_name.is_empty() {
        debug!("browser session of version {} unusable", file.version);
        return None;
    }
    let Ok(headers) = browser::from_parts(
        &file.cookie,
        &file.authuser,
        file.page_id.as_deref(),
        file.visitor_id.as_deref(),
    ) else {
        debug!("browser session holds no usable cookie; treating as signed out");
        return None;
    };
    Some(Signed {
        headers,
        account_name: file.account_name,
        r#gen: 0,
    })
}

fn save(path: &Path, file: &SessionFile) -> io::Result<()> {
    private_dir(path)?;
    let json = serde_json::to_vec(file).map_err(io::Error::other)?;
    let tmp = stage_private(path, &json)?;
    exclude_from_backups(&tmp);
    commit(&tmp, path)
}

/// Marks `path` so Time Machine skips it, as `tmutil addexclusion` does; a failure is only logged.
#[cfg(target_os = "macos")]
pub(crate) fn exclude_from_backups(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: both strings are NUL-terminated and the value is a live buffer of its length.
    let rc = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            BACKUP_EXCLUDE_NAME.as_ptr(),
            BACKUP_EXCLUDE.as_ptr().cast(),
            BACKUP_EXCLUDE.len(),
            0,
            0,
        )
    };
    if rc != 0 {
        debug!(
            "cannot keep {} out of backups: {}",
            path.display(),
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn exclude_from_backups(_: &Path) {}

/// Deletes the session file and any copy a save left behind; a missing one is no error. The
/// copy goes first, so on an error the file is still there.
fn remove(path: &Path) -> io::Result<()> {
    for p in [staged_path(path), path.to_owned()] {
        match fs::remove_file(p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// A continuation token as a query value.
fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~%".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A browse that failed is left out of Home, but one that ended the session is not.
fn left_out_unless_ended<T>(
    result: Result<T, ProviderError>,
    what: &str,
) -> Result<Option<T>, ProviderError> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e @ ProviderError::SignInRequired(_)) => Err(e),
        Err(e) => {
            debug!("home without {what}: {e}");
            Ok(None)
        }
    }
}

/// A browse run on its own thread; a panic in it is a failed browse, as Home leaves out.
fn joined<T>(
    handle: thread::ScopedJoinHandle<'_, Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    handle
        .join()
        .unwrap_or_else(|_| Err(ProviderError::Parse("a browse panicked".into())))
}

fn browse_body(browse_id: &str) -> Value {
    json!({"context": InnertubeClient::music_context(), "browseId": browse_id})
}

/// The signed-in account's name and channel handle from `account/account_menu`.
fn account_of(body: &[u8]) -> Option<(String, Option<String>)> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let header = root.pointer(
        "/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer",
    )?;
    let text = |key: &str| {
        let runs = header.get(key)?.get("runs")?.as_array()?;
        let s: String = runs
            .iter()
            .filter_map(|r| r.get("text")?.as_str())
            .collect();
        Some(s.trim().to_owned()).filter(|s| !s.is_empty())
    };
    Some((text("accountName")?, text("channelHandle")))
}

impl BrowserSession {
    /// Reads the session kept at `path`; makes no network calls.
    pub(crate) fn new(
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
        family: IpFamily,
        path: Option<PathBuf>,
    ) -> Self {
        let current = path.as_deref().and_then(load).map(Arc::new);
        Self {
            transport,
            clock,
            family,
            path,
            slot: Mutex::new(Slot {
                current,
                r#gen: 0,
                sign_outs: 0,
            }),
        }
    }

    pub(crate) fn current(&self) -> Option<Arc<Signed>> {
        locked(&self.slot).current.clone()
    }

    pub(crate) fn account(&self) -> Option<AccountState> {
        self.current().map(|s| AccountState::SignedIn {
            name: Some(s.account_name.clone()),
        })
    }

    /// POSTs to music.youtube.com's Innertube with the session, never anywhere else.
    fn call(
        &self,
        headers: &BrowserHeaders,
        endpoint: &str,
        params: &str,
        body: &Value,
    ) -> Result<Vec<u8>, Failure> {
        let url = format!("{API}{endpoint}?alt=json&key={API_KEY}{params}");
        let cookie = headers.cookie.expose();
        // Every kept cookie has one: `browser` refuses any other.
        let sapisid = browser::sapisid(cookie).unwrap_or_default();
        let auth = browser::authorization(self.clock.unix(), sapisid);
        let mut sent = vec![
            ("Cookie", cookie),
            ("Authorization", auth.as_str()),
            ("Origin", MUSIC_ORIGIN),
            ("X-Goog-AuthUser", headers.authuser.as_str()),
            ("X-YouTube-Client-Name", MUSIC_CLIENT_ID),
            ("X-YouTube-Client-Version", MUSIC_CLIENT_VERSION),
            ("User-Agent", MUSIC_UA),
        ];
        if let Some(p) = &headers.page_id {
            sent.push(("X-Goog-PageId", p.as_str()));
        }
        if let Some(v) = &headers.visitor_id {
            sent.push(("X-Goog-Visitor-Id", v.as_str()));
        }
        let resp = self
            .transport
            .post_json_signed(self.family, &url, &sent, &body.to_string())
            .map_err(Failure::Other)?;
        match resp.status {
            200 => {}
            401 => return Err(Failure::SignedOut),
            403 => return Err(Failure::Forbidden),
            300..=399 => {
                return Err(Failure::Other(ProviderError::ClientBroken(
                    REDIRECTED.into(),
                )));
            }
            status => return Err(Failure::Other(status_error(endpoint, status))),
        }
        let Ok(root) = serde_json::from_slice::<Value>(&resp.body) else {
            return Err(Failure::Other(ProviderError::ClientBroken(WEB_PAGE.into())));
        };
        if logged_in(&root) == Some(false) {
            return Err(Failure::SignedOut);
        }
        Ok(resp.body)
    }

    /// Checks the pasted headers with YouTube Music and keeps them in place of any earlier
    /// session. Nothing is kept when YouTube Music does not take them. Errors never quote the
    /// paste.
    pub(crate) fn import(&self, paste: &str) -> Result<(), ProviderError> {
        let headers = browser::parse_paste(paste).map_err(ProviderError::SignInRequired)?;
        let Some(path) = self.path.clone() else {
            return Err(ProviderError::SignInRequired(
                "no usable HOME: set REQUIEM_GOOGLE_TOKEN_FILE to where the sign-in should be kept"
                    .into(),
            ));
        };
        let checked = |f: Failure| match f {
            Failure::SignedOut | Failure::Forbidden => {
                ProviderError::SignInRequired(NOT_ACCEPTED.into())
            }
            Failure::Other(e) => e,
        };
        // The checks run unlocked; a sign-out meanwhile must not be undone by the save.
        let sign_outs = locked(&self.slot).sign_outs;
        let menu = self
            .call(
                &headers,
                "account/account_menu",
                "",
                &json!({"context": InnertubeClient::music_context()}),
            )
            .map_err(checked)?;
        let (name, handle) =
            account_of(&menu).ok_or_else(|| ProviderError::SignInRequired(NO_NAME.into()))?;
        let home = self
            .call(&headers, "browse", "", &browse_body("FEmusic_home"))
            .map_err(checked)?;
        if !parse_home(&home).is_ok_and(|p| !p.shelves.is_empty()) {
            return Err(ProviderError::SignInRequired(NO_HOME.into()));
        }
        let file = SessionFile {
            version: VERSION,
            cookie: headers.cookie.expose().to_owned(),
            authuser: headers.authuser.clone(),
            page_id: headers.page_id.clone(),
            visitor_id: headers.visitor_id.clone(),
            account_name: name.clone(),
            channel_handle: handle,
            saved_at_unix: self.clock.unix(),
        };
        let mut slot = locked(&self.slot);
        if slot.sign_outs != sign_outs {
            return Err(ProviderError::SignInRequired(CHANGED.into()));
        }
        save(&path, &file).map_err(|e| {
            ProviderError::ClientBroken(format!(
                "cannot keep the session in {}: {e}",
                path.display()
            ))
        })?;
        slot.r#gen += 1;
        slot.current = Some(Arc::new(Signed {
            headers,
            account_name: name,
            r#gen: slot.r#gen,
        }));
        Ok(())
    }

    /// Forgets the session on this computer; the browser it came from stays signed in.
    /// Whether there was one. A file that can't be deleted keeps the session, as the next
    /// start would read it again.
    pub(crate) fn sign_out(&self) -> Result<bool, ProviderError> {
        let mut slot = locked(&self.slot);
        if let Some(path) = &self.path {
            remove(path).map_err(|e| {
                ProviderError::ClientBroken(format!(
                    "cannot delete the browser session in {}: {e}; still signed in",
                    path.display()
                ))
            })?;
        }
        slot.r#gen += 1;
        slot.sign_outs += 1;
        Ok(slot.current.take().is_some())
    }

    /// Forgets `s`, unless an import or sign-out came first, and says the session ended.
    fn expire(&self, s: &Signed) -> ProviderError {
        let mut slot = locked(&self.slot);
        if slot.r#gen == s.r#gen && slot.current.is_some() {
            slot.r#gen += 1;
            slot.current = None;
            // Google no longer takes it, so a file left behind only fails again.
            if let Some(path) = &self.path
                && let Err(e) = remove(path)
            {
                debug!("cannot delete the ended browser session: {e}");
            }
        }
        ProviderError::SignInRequired(ENDED.into())
    }

    fn browse(&self, s: &Signed, params: &str, body: &Value) -> Result<Vec<u8>, ProviderError> {
        self.call(&s.headers, "browse", params, body)
            .map_err(|f| match f {
                Failure::SignedOut => self.expire(s),
                Failure::Forbidden => status_error("browse", 403),
                Failure::Other(e) => e,
            })
    }

    /// Liked songs, the mixes, YouTube Music's other shelves, then the playlists, browsed at
    /// once. Liked songs and the playlists are left out when their browse fails, unless the
    /// session ended.
    pub(crate) fn home(&self, s: &Signed) -> Result<Vec<Shelf>, ProviderError> {
        let (feed, liked, playlists) = thread::scope(|scope| {
            let liked = scope.spawn(|| self.collection(s, &format!("VL{LIKED}")));
            let playlists = scope.spawn(|| self.library(s));
            let feed = self.home_feed(s);
            (feed, joined(liked), joined(playlists))
        });
        let liked = left_out_unless_ended(liked, "liked music");
        let playlists = left_out_unless_ended(playlists, "the library");
        // The session ending, wherever it showed, says more than the feed's own failure.
        let mut feed = match feed {
            Err(e) if !matches!(e, ProviderError::SignInRequired(_)) => {
                liked?;
                playlists?;
                return Err(e);
            }
            feed => feed?,
        };
        let (liked, playlists) = (liked?, playlists?);
        let mut shelves = Vec::new();
        if let Some(mut liked) = liked {
            liked.items.truncate(HOME_LIKED_TRACKS);
            liked.title = HOME_LIKED.to_owned();
            shelves.push(liked);
        }
        if let Some(i) = feed.iter().position(|s| s.title == MIXED_FOR_YOU) {
            let mut mixes = feed.remove(i);
            mixes.title = HOME_MIXES.to_owned();
            shelves.push(mixes);
        }
        shelves.extend(feed);
        for mut shelf in playlists.unwrap_or_default() {
            shelf.title = HOME_PLAYLISTS.to_owned();
            shelves.push(shelf);
        }
        shelves.retain(|s| !s.items.is_empty());
        Ok(shelves)
    }

    /// Home's first page, then its next pages until there are `HOME_SHELVES` shelves. A next
    /// page that fails ends Home early, unless the session ended.
    fn home_feed(&self, s: &Signed) -> Result<Vec<Shelf>, ProviderError> {
        let body = browse_body("FEmusic_home");
        let first = parse_home(&self.browse(s, "", &body)?)?;
        let mut shelves = first.shelves;
        let mut next = first.next;
        // Bounded by pages too: a page may add no shelf.
        for _ in 0..HOME_SHELVES {
            if shelves.len() >= HOME_SHELVES {
                break;
            }
            let Some(token) = next.take() else { break };
            let token = query_value(&token);
            let params = format!("&ctoken={token}&continuation={token}");
            match self.browse(s, &params, &body).and_then(|b| parse_home(&b)) {
                Ok(page) => {
                    shelves.extend(page.shelves);
                    next = page.next;
                }
                Err(e @ ProviderError::SignInRequired(_)) => return Err(e),
                Err(e) => {
                    debug!("home's next page: {e}");
                    break;
                }
            }
        }
        shelves.truncate(HOME_SHELVES);
        Ok(shelves)
    }

    /// Liked music, then the playlists on the Library's Playlists tab.
    pub(crate) fn library(&self, s: &Signed) -> Result<Vec<Shelf>, ProviderError> {
        let body = self.browse(s, "", &browse_body("FEmusic_liked_playlists"))?;
        let liked = format!("{BROWSE_PREFIX}VL{LIKED}");
        let playlists = parse_library_playlists(&body)?
            .into_iter()
            .filter(|c| c.id.id != liked)
            .map(Item::Collection);
        let mut items = vec![Item::Collection(Collection {
            id: CollectionId {
                provider: ProviderId(PROVIDER.to_owned()),
                id: liked.clone(),
            },
            kind: CollectionKind::Playlist,
            title: "Liked music".to_owned(),
            subtitle: None,
            artwork_url: Some(LIKED_ARTWORK.to_owned()),
        })];
        items.extend(playlists);
        Ok(vec![Shelf {
            title: "Playlists".to_owned(),
            items,
        }])
    }

    /// A playlist or album, private ones included.
    pub(crate) fn collection(&self, s: &Signed, browse_id: &str) -> Result<Shelf, ProviderError> {
        let body = self.browse(s, "", &browse_body(browse_id))?;
        parse_collection(&body, collection_kind(browse_id))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::google::SystemClock;
    use crate::testing::{FakeMusic, TempDir};

    // Synthetic responses built to ytmusicapi's paths; every value in them is made up.
    const MENU: &str = include_str!("../tests/fixtures/account_menu.json");
    const HOME: &str = include_str!("../tests/fixtures/signed_in_home.json");
    const HOME_NEXT: &str = include_str!("../tests/fixtures/signed_in_home_continuation.json");
    const LIBRARY: &str = include_str!("../tests/fixtures/signed_in_library_playlists.json");
    const PLAYLIST: &str = include_str!("../tests/fixtures/browse_playlist.json");

    const SECRET: &str = "fakeSID-MARKER";
    const COOKIE: &str = "SID=fakeSID-MARKER; __Secure-3PAPISID=FAKEsapisid-0123/AbCdEfGh";
    const MENU_URL: &str = "https://music.youtube.com/youtubei/v1/account/account_menu";
    const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse";
    const SIGNED_OUT: &str = r#"{"responseContext":{"serviceTrackingParams":[{"service":"GFEEDBACK","params":[{"key":"logged_in","value":"0"}]}]},"contents":{}}"#;

    fn paste() -> String {
        format!("Cookie: {COOKIE}\nX-Goog-AuthUser: 1\nX-Goog-PageId: 4242\nUser-Agent: x\n")
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

        fn path(&self) -> PathBuf {
            path_beside(Some(&self.tmp.path().join("requiem/google-token.json"))).unwrap()
        }

        fn session(&self) -> BrowserSession {
            BrowserSession::new(
                self.music.clone(),
                Arc::new(SystemClock),
                IpFamily::Any,
                Some(self.path()),
            )
        }

        /// A session that YouTube Music took, kept on disk.
        fn signed_in(&self) -> BrowserSession {
            self.music.script(MENU_URL, 200, MENU);
            self.music.script(BROWSE_URL, 200, HOME);
            let s = self.session();
            s.import(&paste()).unwrap();
            self.music.calls.lock().unwrap().clear();
            s
        }

        fn calls(&self) -> Vec<String> {
            self.music
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.url.clone())
                .collect()
        }
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn import_checks_with_youtube_music_then_keeps_the_session_privately() {
        let rig = Rig::new();
        let s = rig.signed_in();
        assert_eq!(
            s.account(),
            Some(AccountState::SignedIn {
                name: Some("Requiem Tester".into())
            })
        );

        let path = rig.path();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(!path.with_extension("json.tmp").exists());
        let kept: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(kept["version"], 1);
        assert_eq!(kept["cookie"], COOKIE);
        assert_eq!(kept["authuser"], "1");
        assert_eq!(kept["page_id"], "4242");
        assert!(kept.get("visitor_id").is_none());
        assert_eq!(kept["account_name"], "Requiem Tester");
        assert_eq!(kept["channel_handle"], "@requiemtester");
        assert!(kept["saved_at_unix"].as_u64().unwrap() > 1_700_000_000);

        // A provider started later reads it back.
        let again = rig.session();
        assert_eq!(again.account(), s.account());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_kept_session_is_out_of_backups() {
        use std::os::unix::ffi::OsStrExt;
        let rig = Rig::new();
        rig.signed_in();
        let path = std::ffi::CString::new(rig.path().as_os_str().as_bytes()).unwrap();
        let exclusion = || {
            let mut value = [0u8; 128];
            // SAFETY: both strings are NUL-terminated and `value` is a live buffer of its
            // length.
            let n = unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    BACKUP_EXCLUDE_NAME.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            usize::try_from(n).ok().map(|n| value[..n].to_vec())
        };
        assert_eq!(exclusion().as_deref(), Some(&BACKUP_EXCLUDE[..]));

        // Put back as a failed Restart to Update does, as a new file; loading excludes it again.
        let bytes = fs::read(rig.path()).unwrap();
        fs::remove_file(rig.path()).unwrap();
        fs::write(rig.path(), bytes).unwrap();
        assert_eq!(exclusion(), None);
        assert!(rig.session().current().is_some());
        assert_eq!(exclusion().as_deref(), Some(&BACKUP_EXCLUDE[..]));
    }

    #[test]
    fn signed_requests_go_only_to_music_youtube_com_with_the_session() {
        let rig = Rig::new();
        rig.music.script(MENU_URL, 200, MENU);
        rig.music.script(BROWSE_URL, 200, HOME);
        rig.session().import(&paste()).unwrap();
        let calls = rig.music.calls.lock().unwrap();
        let key = "?alt=json&key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
        assert_eq!(calls[0].url, format!("{MENU_URL}{key}"));
        assert_eq!(calls[1].url, format!("{BROWSE_URL}{key}"));
        for c in calls.iter() {
            assert!(c.signed, "{}", c.url);
            assert_eq!(header(&c.headers, "Cookie"), Some(COOKIE));
            assert_eq!(
                header(&c.headers, "Origin"),
                Some("https://music.youtube.com")
            );
            assert_eq!(header(&c.headers, "X-Goog-AuthUser"), Some("1"));
            assert_eq!(header(&c.headers, "X-Goog-PageId"), Some("4242"));
            assert_eq!(header(&c.headers, "X-Goog-Visitor-Id"), None);
            assert_eq!(header(&c.headers, "X-YouTube-Client-Name"), Some("67"));
            assert!(header(&c.headers, "User-Agent").is_some());
            let auth = header(&c.headers, "Authorization").unwrap();
            let ts: u64 = auth
                .strip_prefix("SAPISIDHASH ")
                .and_then(|a| a.split('_').next())
                .and_then(|t| t.parse().ok())
                .unwrap();
            assert_eq!(
                auth,
                browser::authorization(ts, "FAKEsapisid-0123/AbCdEfGh")
            );
            let body: Value = serde_json::from_str(&c.body).unwrap();
            assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        }
        let body: Value = serde_json::from_str(&calls[1].body).unwrap();
        assert_eq!(body["browseId"], "FEmusic_home");
    }

    #[test]
    fn a_refused_import_keeps_nothing_and_says_why() {
        let html = "<!DOCTYPE html><html>consent.youtube.com</html>";
        let nameless = r#"{"actions":[{"openPopupAction":{"popup":{}}}]}"#;
        let empty_home = r#"{"contents":{}}"#;
        type Replies<'a> = &'a [(&'a str, u16, &'a str)];
        let cases: [(Replies, &str); 8] = [
            (&[(MENU_URL, 401, "{}")], NOT_ACCEPTED),
            (&[(MENU_URL, 403, "{}")], NOT_ACCEPTED),
            (&[(MENU_URL, 200, SIGNED_OUT)], NOT_ACCEPTED),
            (&[(MENU_URL, 200, html)], WEB_PAGE),
            (&[(MENU_URL, 302, "")], REDIRECTED),
            (&[(MENU_URL, 200, nameless)], NO_NAME),
            (
                &[(MENU_URL, 200, MENU), (BROWSE_URL, 200, empty_home)],
                NO_HOME,
            ),
            (
                &[(MENU_URL, 200, MENU), (BROWSE_URL, 200, SIGNED_OUT)],
                NOT_ACCEPTED,
            ),
        ];
        for (replies, why) in cases {
            let rig = Rig::new();
            for (url, status, body) in replies {
                rig.music.script(url, *status, body);
            }
            let s = rig.session();
            let e = s.import(&paste()).unwrap_err();
            assert!(e.to_string().contains(why), "{why}: {e}");
            assert!(!e.to_string().contains(SECRET));
            assert_eq!(s.account(), None);
            assert!(!rig.path().exists(), "{why}");
        }
        // A network failure stays one, so the user can retry.
        let rig = Rig::new();
        let e = rig.session().import(&paste()).unwrap_err();
        assert!(matches!(e, ProviderError::Network(_)), "{e}");
        // A paste the parser refuses sends nothing.
        let e = rig.session().import("Accept: */*").unwrap_err();
        assert!(e.to_string().contains("no Cookie header"), "{e}");
        assert_eq!(rig.calls().len(), 1);
    }

    #[test]
    fn a_refused_import_leaves_the_earlier_session() {
        let rig = Rig::new();
        let s = rig.signed_in();
        rig.music.script(MENU_URL, 401, "{}");
        assert!(
            s.import("Cookie: SID=other; __Secure-3PAPISID=other")
                .is_err()
        );
        assert!(s.account().is_some());
        let kept: Value = serde_json::from_slice(&fs::read(rig.path()).unwrap()).unwrap();
        assert_eq!(kept["cookie"], COOKIE);
    }

    #[test]
    fn a_401_or_a_signed_out_answer_ends_the_session() {
        for (status, body) in [(401, "{}"), (200, SIGNED_OUT)] {
            let rig = Rig::new();
            let s = rig.signed_in();
            rig.music
                .script_browse(BROWSE_URL, "FEmusic_home", status, body);
            let e = s.home(&s.current().unwrap()).unwrap_err();
            assert_eq!(e, ProviderError::SignInRequired(ENDED.into()), "{status}");
            assert_eq!(s.account(), None, "{status}");
            assert!(!rig.path().exists(), "{status}");
            assert!(rig.session().current().is_none(), "{status}");
        }
    }

    #[test]
    fn an_ended_request_does_not_end_a_newer_session() {
        let rig = Rig::new();
        let s = rig.signed_in();
        let old = s.current().unwrap();
        rig.music.script(MENU_URL, 200, MENU);
        rig.music.script(BROWSE_URL, 200, HOME);
        s.import(&paste()).unwrap();
        rig.music.script(BROWSE_URL, 401, "{}");
        assert!(s.home(&old).is_err());
        assert!(s.account().is_some());
        assert!(rig.path().exists());
    }

    #[test]
    fn a_403_a_5xx_or_an_unreadable_answer_keeps_the_session() {
        for (status, body) in [(403, "{}"), (503, ""), (200, "<html></html>"), (200, "{}")] {
            let rig = Rig::new();
            let s = rig.signed_in();
            rig.music.script(BROWSE_URL, status, body);
            let e = s.library(&s.current().unwrap()).unwrap_err();
            assert!(
                !matches!(e, ProviderError::SignInRequired(_)),
                "{status}: {e}"
            );
            assert!(s.account().is_some(), "{status}");
            assert!(rig.path().exists(), "{status}");
        }
    }

    /// A liked music playlist page of `n` tracks.
    fn liked(n: usize) -> String {
        let rows: Vec<Value> = (0..n)
            .map(|i| {
                json!({"musicResponsiveListItemRenderer": {
                    "playlistItemData": {"videoId": format!("liked{i:02}")},
                    "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer":
                        {"text": {"runs": [{"text": format!("Liked {i}")}]}}}]}})
            })
            .collect();
        json!({"contents": {"musicPlaylistShelfRenderer": {"contents": rows}}}).to_string()
    }

    /// Home's two pages, then liked music's and the library's replies, if any, which Home
    /// browses at once.
    fn script_home(rig: &Rig, liked: Option<(u16, &str)>, library: Option<(u16, &str)>) {
        rig.music
            .script_browse(BROWSE_URL, "FEmusic_home", 200, HOME);
        rig.music
            .script_browse(BROWSE_URL, "FEmusic_home", 200, HOME_NEXT);
        if let Some((status, body)) = liked {
            rig.music.script_browse(BROWSE_URL, "VLLM", status, body);
        }
        if let Some((status, body)) = library {
            rig.music
                .script_browse(BROWSE_URL, "FEmusic_liked_playlists", status, body);
        }
    }

    fn titles(shelves: &[Shelf]) -> Vec<&str> {
        shelves.iter().map(|s| s.title.as_str()).collect()
    }

    #[test]
    fn home_follows_next_pages_up_to_six_shelves() {
        let rig = Rig::new();
        let s = rig.signed_in();
        rig.music
            .script_browse(BROWSE_URL, "FEmusic_home", 200, HOME);
        rig.music
            .script_browse(BROWSE_URL, "FEmusic_home", 200, HOME_NEXT);
        let shelves = s.home(&s.current().unwrap()).unwrap();
        assert_eq!(
            titles(&shelves),
            [
                "Mixes",
                "Quick picks",
                "Listen again",
                "Your favorites",
                "Forgotten favorites",
                "Recommended albums"
            ]
        );
        let calls = rig.calls();
        assert_eq!(
            calls.len(),
            4,
            "home's two pages, liked music and the library"
        );
        let next: Vec<_> = calls.iter().filter(|c| c.contains("ctoken")).collect();
        assert_eq!(next.len(), 1, "{calls:?}");
        assert!(
            next[0].ends_with("&ctoken=FAKE-CONTINUATION-1&continuation=FAKE-CONTINUATION-1"),
            "{}",
            next[0]
        );
        let Item::Collection(mix) = &shelves[0].items[0] else {
            panic!("{:?}", shelves[0].items[0])
        };
        assert_eq!(mix.id.id, "browse:VLRDTMAKfakeSupermix01");
    }

    #[test]
    fn home_is_liked_songs_then_mixes_then_the_feed_then_playlists() {
        let rig = Rig::new();
        let s = rig.signed_in();
        script_home(&rig, Some((200, &liked(25))), Some((200, LIBRARY)));
        let shelves = s.home(&s.current().unwrap()).unwrap();
        assert_eq!(
            titles(&shelves),
            [
                "Liked songs",
                "Mixes",
                "Quick picks",
                "Listen again",
                "Your favorites",
                "Forgotten favorites",
                "Recommended albums",
                "Your playlists"
            ]
        );
        let ids: Vec<&str> = shelves[0]
            .items
            .iter()
            .map(|i| match i {
                Item::Track(t) => t.id.id.as_str(),
                Item::Collection(c) => panic!("{c:?}"),
            })
            .collect();
        assert_eq!(ids.len(), 20);
        assert_eq!((ids[0], ids[19]), ("liked00", "liked19"));
        let Item::Collection(mix) = &shelves[1].items[0] else {
            panic!("{:?}", shelves[1].items[0])
        };
        assert_eq!(mix.id.id, "browse:VLRDTMAKfakeSupermix01");
        let playlists: Vec<&str> = shelves[7]
            .items
            .iter()
            .map(|i| match i {
                Item::Collection(c) => c.id.id.as_str(),
                Item::Track(t) => panic!("{t:?}"),
            })
            .collect();
        assert_eq!(
            playlists,
            [
                "browse:VLLM",
                "browse:VLPLfakeRoadTrip01",
                "browse:VLPLfakeNight02"
            ]
        );
        let calls = rig.music.calls.lock().unwrap();
        let mut browse_ids: Vec<String> = calls
            .iter()
            .map(|c| {
                let body: Value = serde_json::from_str(&c.body).unwrap();
                body["browseId"].as_str().unwrap().to_owned()
            })
            .collect();
        browse_ids.sort();
        assert_eq!(
            browse_ids,
            [
                "FEmusic_home",
                "FEmusic_home",
                "FEmusic_liked_playlists",
                "VLLM"
            ]
        );
    }

    #[test]
    fn a_failed_liked_or_library_browse_leaves_home_without_that_shelf() {
        let rig = Rig::new();
        let s = rig.signed_in();
        script_home(&rig, Some((503, "")), Some((200, LIBRARY)));
        let shelves = s.home(&s.current().unwrap()).unwrap();
        assert_eq!(titles(&shelves)[0], "Mixes");
        assert_eq!(titles(&shelves).last(), Some(&"Your playlists"));
        assert!(!titles(&shelves).contains(&"Liked songs"));

        let rig = Rig::new();
        let s = rig.signed_in();
        script_home(&rig, Some((200, &liked(2))), Some((200, "{}")));
        let shelves = s.home(&s.current().unwrap()).unwrap();
        assert_eq!(titles(&shelves)[..2], ["Liked songs", "Mixes"]);
        assert!(!titles(&shelves).contains(&"Your playlists"));
        assert!(s.account().is_some());
    }

    #[test]
    fn an_ended_session_in_liked_music_or_the_library_still_fails_home() {
        let liked = liked(1);
        for ended_at in [2, 3] {
            let rig = Rig::new();
            let s = rig.signed_in();
            let reply = |i| {
                if i == ended_at {
                    Some((401, "{}"))
                } else {
                    Some((200, liked.as_str()))
                }
            };
            script_home(&rig, reply(2), reply(3));
            let e = s.home(&s.current().unwrap()).unwrap_err();
            assert_eq!(e, ProviderError::SignInRequired(ENDED.into()), "{ended_at}");
            assert_eq!(s.account(), None, "{ended_at}");
        }
    }

    fn asked_for(music: &FakeMusic, browse_id: &str) -> bool {
        music.calls.lock().unwrap().iter().any(|c| {
            serde_json::from_str::<Value>(&c.body)
                .is_ok_and(|b| b["browseId"].as_str() == Some(browse_id))
        })
    }

    #[test]
    fn home_asks_for_liked_music_and_the_library_at_once() {
        let rig = Rig::new();
        let s = rig.signed_in();
        script_home(&rig, Some((200, &liked(2))), Some((200, LIBRARY)));
        let music = rig.music.clone();
        // Liked music answers only once the library and the feed have been asked for.
        rig.music.during_browse(BROWSE_URL, "VLLM", move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !(asked_for(&music, "FEmusic_liked_playlists")
                && asked_for(&music, "FEmusic_home"))
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "liked music held up the rest of Home"
                );
                thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let shelves = s.home(&s.current().unwrap()).unwrap();
        assert_eq!(titles(&shelves).first(), Some(&"Liked songs"));
        assert_eq!(titles(&shelves).last(), Some(&"Your playlists"));
    }

    #[test]
    fn a_failed_next_page_ends_home_early() {
        let rig = Rig::new();
        let s = rig.signed_in();
        rig.music
            .script_browse(BROWSE_URL, "FEmusic_home", 200, HOME);
        rig.music.script_browse(BROWSE_URL, "FEmusic_home", 503, "");
        let shelves = s.home(&s.current().unwrap()).unwrap();
        assert_eq!(titles(&shelves), ["Mixes", "Quick picks", "Listen again"]);
    }

    #[test]
    fn library_is_liked_music_then_the_playlists() {
        let rig = Rig::new();
        let s = rig.signed_in();
        rig.music.script(BROWSE_URL, 200, LIBRARY);
        let shelves = s.library(&s.current().unwrap()).unwrap();
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Playlists");
        let items: Vec<_> = shelves[0]
            .items
            .iter()
            .map(|i| match i {
                Item::Collection(c) => (c.id.id.as_str(), c.title.as_str()),
                Item::Track(t) => panic!("{t:?}"),
            })
            .collect();
        assert_eq!(
            items,
            [
                ("browse:VLLM", "Liked music"),
                ("browse:VLPLfakeRoadTrip01", "Road trip"),
                ("browse:VLPLfakeNight02", "Night drive"),
            ]
        );
        let Item::Collection(liked) = &shelves[0].items[0] else {
            panic!("no Liked music first");
        };
        assert_eq!(
            liked.artwork_url.as_deref(),
            Some("https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-music-@576.png")
        );
        let body: Value = serde_json::from_str(&rig.music.calls.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["browseId"], "FEmusic_liked_playlists");
    }

    #[test]
    fn collections_open_with_the_session() {
        let rig = Rig::new();
        let s = rig.signed_in();
        rig.music.script(BROWSE_URL, 200, PLAYLIST);
        let shelf = s.collection(&s.current().unwrap(), "VLLM").unwrap();
        assert!(!shelf.items.is_empty());
        let calls = rig.music.calls.lock().unwrap();
        assert!(calls[0].signed);
        let body: Value = serde_json::from_str(&calls[0].body).unwrap();
        assert_eq!(body["browseId"], "VLLM");
    }

    #[test]
    fn a_sign_out_while_a_session_is_checked_keeps_nothing() {
        let rig = Rig::new();
        rig.music.script(MENU_URL, 200, MENU);
        rig.music.script(BROWSE_URL, 200, HOME);
        let s = Arc::new(rig.session());
        let during = Arc::downgrade(&s);
        rig.music.during(BROWSE_URL, move || {
            during.upgrade().unwrap().sign_out().unwrap();
        });
        let e = s.import(&paste()).unwrap_err().to_string();
        assert!(e.contains("signed out while"), "{e}");
        assert_eq!(s.account(), None);
        assert!(!rig.path().exists());
        assert_eq!(rig.session().account(), None);
    }

    #[test]
    fn an_old_session_ending_while_a_new_one_is_checked_keeps_the_new_one() {
        let rig = Rig::new();
        let s = Arc::new(rig.signed_in());
        rig.music.script(MENU_URL, 200, MENU);
        rig.music.script(BROWSE_URL, 200, HOME);
        let during = Arc::downgrade(&s);
        rig.music.during(BROWSE_URL, move || {
            // A refresh with the old session got a 401 meanwhile.
            let s = during.upgrade().unwrap();
            s.expire(&s.current().unwrap());
            assert!(s.current().is_none());
        });
        s.import(&paste()).unwrap();
        assert!(s.current().is_some());
        assert!(rig.path().exists());
        assert!(rig.session().current().is_some());
    }

    #[test]
    fn sign_out_deletes_the_session() {
        let rig = Rig::new();
        let s = rig.signed_in();
        assert!(s.sign_out().unwrap());
        assert_eq!(s.account(), None);
        assert!(!rig.path().exists());
        assert!(!s.sign_out().unwrap());
    }

    #[test]
    fn sign_out_deletes_a_copy_a_save_left_behind() {
        let rig = Rig::new();
        let s = rig.signed_in();
        let leftover = staged_path(&rig.path());
        fs::write(&leftover, fs::read(rig.path()).unwrap()).unwrap();
        assert!(s.sign_out().unwrap());
        assert!(!leftover.exists());
        assert!(!rig.path().exists());

        // Nothing reads the copy, but it holds the cookie.
        fs::write(&leftover, COOKIE).unwrap();
        assert_eq!(rig.session().account(), None);
        assert!(!s.sign_out().unwrap());
        assert!(!leftover.exists());
    }

    #[test]
    fn a_session_that_cannot_be_deleted_stays_signed_in() {
        let rig = Rig::new();
        let s = rig.signed_in();
        let dir = rig.path().parent().unwrap().to_owned();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let out = s.sign_out();
        let restarted = rig.session().account();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let e = out.unwrap_err().to_string();
        assert!(e.contains("cannot delete the browser session"), "{e}");
        assert!(!e.contains(SECRET), "{e}");
        assert!(s.account().is_some());
        assert!(restarted.is_some());
        // Once it can be, it goes.
        assert!(s.sign_out().unwrap());
        assert!(!rig.path().exists());
    }

    #[test]
    fn a_broken_file_reads_as_no_session_and_is_made_private() {
        let rig = Rig::new();
        let path = rig.path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = json!({
            "version": 1, "cookie": COOKIE, "authuser": "0",
            "account_name": "Requiem Tester", "saved_at_unix": 1
        });
        for broken in [
            json!(SECRET).to_string(),
            format!("{{\"cookie\": \"{SECRET}"),
            json!({"version": 2, "cookie": COOKIE, "authuser": "0", "account_name": "x", "saved_at_unix": 1}).to_string(),
            json!({"version": 1, "cookie": "SID=x", "authuser": "0", "account_name": "x", "saved_at_unix": 1}).to_string(),
            json!({"version": 1, "cookie": COOKIE, "authuser": "0", "account_name": "", "saved_at_unix": 1}).to_string(),
        ] {
            fs::write(&path, &broken).unwrap();
            assert!(rig.session().current().is_none(), "{broken}");
        }
        fs::write(&path, good.to_string()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(rig.session().current().is_some());
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn nothing_shows_the_cookie() {
        let rig = Rig::new();
        let s = rig.signed_in();
        let signed = s.current().unwrap();
        let file = SessionFile {
            version: 1,
            cookie: COOKIE.into(),
            authuser: "0".into(),
            page_id: None,
            visitor_id: None,
            account_name: "n".into(),
            channel_handle: None,
            saved_at_unix: 0,
        };
        let mut shown = vec![
            format!("{signed:?}"),
            format!("{file:?}"),
            serde_json::to_string(&s.account()).unwrap(),
        ];
        for (status, body) in [(401, "{}"), (403, "{}"), (500, ""), (302, ""), (200, "<p>")] {
            let rig = Rig::new();
            rig.music.script(MENU_URL, status, body);
            let s = rig.session();
            let e = s.import(&paste()).unwrap_err();
            shown.extend([e.to_string(), format!("{e:?}")]);
            let s = rig.signed_in();
            rig.music
                .script_browse(BROWSE_URL, "FEmusic_home", status, body);
            let e = s.home(&s.current().unwrap()).unwrap_err();
            shown.extend([e.to_string(), format!("{e:?}")]);
            shown.push(serde_json::to_string(&s.account()).unwrap());
        }
        for text in shown {
            assert!(
                !text.contains(SECRET) && !text.contains("FAKEsapisid"),
                "{text}"
            );
        }
    }
}
