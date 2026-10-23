//! The helper's backend: librespot on a 2-worker tokio runtime, with one `Session` that every
//! connection shares. Connection threads block on the runtime for each call.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::{Display, Write as _};
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::audio_key::AudioKey;
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::error::ErrorKind;
use librespot_core::{Error, FileId, Session, SessionConfig, SpotifyId, SpotifyUri};
use librespot_metadata::album::AlbumType;
use librespot_metadata::artist::Artists;
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use librespot_metadata::image::{Image, ImageSize};
use librespot_metadata::{Album, Artist, Metadata, Track as SpotifyTrack};
use librespot_oauth::DeviceAuthClientBuilder;
use librespot_protocol::context_page::ContextPage;
use librespot_protocol::context_track::ContextTrack;
use librespot_protocol::playlist4_external::{ListAttributes, SelectedListContent};
use protobuf::Message;
use requiem_core::{
    Collection, CollectionId, CollectionKind, Item, Page, ProviderError, ProviderId, Shelf, Track,
    TrackId,
};
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::Notify;

use crate::covers::{AlbumCover, COVER_TRACKS, Covers, InTime, MOSAIC_COVERS, Whole, cover_choice};
use crate::keys::Keys;
use crate::logger::{debug, redact};
use crate::recent::{BACKOFF, Card, RecentCache, Shelves};
use crate::server::{Backend, Stream};
use crate::wire::Pending;
use crate::{PROVIDER_ID, creds};

pub(crate) const METADATA_IN_FLIGHT: usize = 8;
/// A prepared stream not attached by then is forgotten.
pub(crate) const PREPARED_EXPIRY: Duration = Duration::from_secs(30 * 60);
/// Best first, with each format's bytes per second at its nominal rate.
pub(crate) const FORMATS: [(AudioFileFormat, usize); 3] = [
    (AudioFileFormat::OGG_VORBIS_320, 40 * 1024),
    (AudioFileFormat::OGG_VORBIS_160, 20 * 1024),
    (AudioFileFormat::OGG_VORBIS_96, 12 * 1024),
];
/// Spotify's own header before the Ogg stream in every audio file.
pub(crate) const SPOTIFY_HEADER: u64 = 0xa7;
const SIGN_IN_SCOPE: &str = "streaming";
/// The device code's `expires_in`; librespot-oauth doesn't pass it on.
const CODE_LIFETIME: Duration = Duration::from_secs(3599);
const ROOTLIST_PAGE: usize = 120;
const MAX_PAGES: usize = 50;
/// One whole-playlist fetch for a rootlist row, librespot's own retries included.
const FULL_PLAYLIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an answer waits for the whole playlists its incomplete rows need; rows still
/// waiting then are left out of it, and told of once they land.
const INLINE_BUDGET: Duration = Duration::from_secs(5);
/// How long a background lookup for a cover or a whole playlist may take.
const BACKGROUND_BUDGET: Duration = Duration::from_secs(30);
/// As many as YouTube Music's provider shows.
const MAX_COLLECTION_TRACKS: usize = 1000;
/// Home's most recently liked tracks.
const HOME_LIKED: usize = 20;
/// Collections on Home's Listen again, each one a request.
const LISTEN_AGAIN: usize = 10;
/// Tracks on Home's Quick picks.
const QUICK_PICKS: usize = 20;
/// How long Home waits for Listen again and Quick picks; whatever isn't in by then is left out.
const RECENT_BUDGET: Duration = Duration::from_secs(5);
/// What the desktop client asks for; contexts only, each with the track of it played last.
const RECENTLY_PLAYED: &str =
    "recently-played?format=json&offset=0&limit=50&filter=default,collection-new-episodes";
/// The ids of the playlists Spotify makes start with one of these: its mixes, Daily Mixes,
/// "This Is" and editorial playlists, then Discover Weekly and Release Radar.
const MADE_FOR_YOU: [&str; 2] = ["37i9dQZF1", "37i9dQZEVX"];
pub(crate) const IMAGE_URL: &str = "https://i.scdn.co/image/";
const LIKED: &str = "Liked Songs";
const LIKED_ID: &str = "collection";
/// Spotify's own Liked Songs picture, the same for everyone.
const LIKED_ARTWORK: &str = "https://misc.scdn.co/liked-songs/liked-songs-640.png";
const PLAYLIST_PREFIX: &str = "playlist:";
const ALBUM_PREFIX: &str = "album:";
const ARTIST_PREFIX: &str = "spotify:artist:";
/// An artist page's top songs, at most.
const ARTIST_TOP_SONGS: usize = 10;
/// An artist page's albums, and its singles and EPs, at most each; as many as YouTube Music's
/// artist page shows.
const ARTIST_RELEASES: usize = 10;
/// The shelves of an artist's page, named as YouTube Music's provider names them.
const ARTIST_TOP: &str = "Top songs";
const ARTIST_ALBUMS: &str = "Albums";
const ARTIST_SINGLES: &str = "Singles & EPs";

pub(crate) struct Librespot {
    rt: Runtime,
    creds: Option<PathBuf>,
    /// Where librespot keeps the still-encrypted download; the daemon's 0700 socket dir.
    tmp_dir: PathBuf,
    session: tokio::sync::Mutex<Option<Session>>,
    keys: Keys<(SpotifyId, FileId), AudioKey>,
    prepared: Mutex<PreparedStreams<Prep>>,
    flow: SignInFlow,
    covers: Arc<Covers>,
    recent: Mutex<RecentCache>,
}

/// `AudioKey` prints itself in `Debug`, so this has none.
struct Prep {
    file: FileId,
    key: AudioKey,
    bytes_per_second: usize,
}

fn provider() -> ProviderId {
    ProviderId(PROVIDER_ID.into())
}

fn no_place() -> ProviderError {
    ProviderError::SignInRequired(format!(
        "no place to keep a Spotify sign-in: set HOME or {}",
        creds::ENV
    ))
}

/// An error from librespot or its HTTP stack, as text fit to log or send to the daemon.
fn shown(e: &impl Display) -> String {
    redact(&e.to_string())
}

/// Our own message, then librespot's, which names the failure but never a token or key.
fn api_error(what: &str, e: &Error) -> ProviderError {
    let msg = format!("Spotify {what}: {}", shown(&e));
    match e.kind {
        ErrorKind::Unauthenticated => ProviderError::SignInRequired(msg),
        ErrorKind::Cancelled
        | ErrorKind::Unknown
        | ErrorKind::DeadlineExceeded
        | ErrorKind::ResourceExhausted
        | ErrorKind::Aborted
        | ErrorKind::Internal
        | ErrorKind::Unavailable => ProviderError::Network(msg),
        _ => ProviderError::Unplayable(msg),
    }
}

/// librespot reports a refused login as `PermissionDenied`.
fn connect_error(e: &Error) -> ProviderError {
    match e.kind {
        ErrorKind::PermissionDenied | ErrorKind::Unauthenticated => {
            ProviderError::SignInRequired(format!(
                "Spotify did not accept the sign-in; sign in again ({})",
                shown(&e)
            ))
        }
        _ => ProviderError::Network(format!("cannot reach Spotify: {}", shown(&e))),
    }
}

fn cancelled() -> ProviderError {
    ProviderError::SignInRequired("sign-in was cancelled".into())
}

impl Librespot {
    /// Removes any sign-in scratch a killed helper left beside the credentials file.
    pub(crate) fn new(rt: Runtime, creds: Option<PathBuf>, tmp_dir: PathBuf) -> Self {
        if let Some(path) = &creds
            && let Err(e) = creds::remove_scratch(path)
        {
            eprintln!("[spotify] {e}");
        }
        Self {
            rt,
            creds,
            tmp_dir,
            session: tokio::sync::Mutex::new(None),
            keys: Keys::new(),
            prepared: Mutex::new(PreparedStreams::new()),
            flow: SignInFlow::default(),
            covers: Arc::new(Covers::new()),
            recent: Mutex::new(RecentCache::default()),
        }
    }

    fn config(&self) -> SessionConfig {
        SessionConfig {
            tmp_dir: self.tmp_dir.clone(),
            ..SessionConfig::default()
        }
    }

    /// The shared session, connecting with the saved sign-in if there is none or it died.
    async fn session(&self) -> Result<Session, ProviderError> {
        let mut slot = self.session.lock().await;
        if let Some(s) = slot.as_ref()
            && !s.is_invalid()
        {
            return Ok(s.clone());
        }
        let path = self.creds.as_deref().ok_or_else(no_place)?;
        let creds = match creds::load(path) {
            Ok(Some(c)) => c,
            Ok(None) => return Err(ProviderError::SignInRequired("sign in to Spotify".into())),
            Err(e) => {
                return Err(ProviderError::SignInRequired(format!(
                    "{}: {e}; sign in again",
                    path.display()
                )));
            }
        };
        let session = Session::new(self.config(), None);
        session
            .connect(creds, false)
            .await
            .map_err(|e| connect_error(&e))?;
        // An invalid session still serves the streams that hold it.
        *slot = Some(session.clone());
        Ok(session)
    }

    async fn search_async(&self, q: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        let q = q.trim();
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let session = self.session().await?;
        let ctx = session
            .spclient()
            .get_context(&format!("spotify:search:{}", search_term(q)))
            .await
            .map_err(|e| api_error("search", &e))?;
        let mut ids = context_tracks(&ctx.pages);
        ids.truncate(limit);
        tracks(&session, ids).await
    }

    async fn library(&self) -> Result<Vec<Shelf>, ProviderError> {
        let session = self.session().await?;
        let mut items = vec![Item::Collection(liked_card())];
        items.extend(
            playlists(&session, &self.covers)
                .await?
                .into_iter()
                .map(Item::Collection),
        );
        Ok(vec![Shelf {
            title: "Spotify".into(),
            items,
        }])
    }

    async fn home(&self) -> Result<Vec<Shelf>, ProviderError> {
        let session = self.session().await?;
        let session = &session;
        let (liked, playlists, recent) = tokio::join!(
            async {
                let ids = context_ids(session, &liked_uri(session), HOME_LIKED).await?;
                tracks(session, ids).await
            },
            playlists(session, &self.covers),
            recent(session, &self.covers, &self.recent),
        );
        // Without the liked songs, Home still has the playlists.
        let liked = liked.unwrap_or_else(|e| {
            eprintln!("[spotify] Home left out the liked songs: {}", shown(&e));
            Vec::new()
        });
        Ok(home_shelves(liked, playlists?, recent))
    }

    async fn collection(&self, id: &str) -> Result<Vec<Shelf>, ProviderError> {
        if let Some(artist) = artist_of(id) {
            return self.artist(artist).await;
        }
        let session = self.session().await?;
        let opens = Opens::from_id(id)
            .ok_or_else(|| ProviderError::Unplayable(format!("no Spotify collection {id}")))?;
        let ids = context_ids(&session, &opens.uri(&session), MAX_COLLECTION_TRACKS).await?;
        let title = match opens {
            Opens::Liked => LIKED.to_owned(),
            Opens::Playlist(pid) => playlist_name(&session, &pid)
                .await
                .unwrap_or_else(|| "Playlist".into()),
            Opens::Album(aid) => Album::get(&session, &SpotifyUri::Album { id: aid })
                .await
                .ok()
                .map(|a| a.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Album".into()),
        };
        let tracks = tracks(&session, ids).await?;
        Ok(vec![Shelf {
            title,
            items: tracks.into_iter().map(Item::Track).collect(),
        }])
    }

    /// Artist `id`'s page: its top songs where the account is, then its albums, and its singles
    /// and EPs, each in its latest release.
    async fn artist(&self, id: SpotifyId) -> Result<Vec<Shelf>, ProviderError> {
        let session = self.session().await?;
        let artist = Artist::get(&session, &SpotifyUri::Artist { id })
            .await
            .map_err(|e| api_error("artist", &e))?;
        let wants = ArtistWants::of(&artist, &session.country());
        let releases = wants.albums.iter().chain(&wants.singles).copied().collect();
        let (top, releases) = tokio::join!(tracks(&session, wants.top), albums(&session, releases));
        let (albums, singles) = releases?
            .into_iter()
            .partition(|(id, _)| wants.albums.contains(id));
        Ok(artist_shelves(top?, albums, singles))
    }

    async fn prepare_async(&self, track: &str) -> Result<String, ProviderError> {
        let id = SpotifyId::from_base62(track)
            .map_err(|_| ProviderError::Unplayable(format!("{track} is not a Spotify track id")))?;
        let session = self.session().await?;
        let (file, bytes_per_second) = pick_file(&session, id).await?;
        // librespot's player also asks with the requested track's id when an alternative plays.
        let key = self
            .keys
            .get((id, file), || session.audio_key().request(id, file))
            .await?;
        let stream = self
            .prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                Instant::now(),
                Prep {
                    file,
                    key,
                    bytes_per_second,
                },
            );
        Ok(stream)
    }

    async fn attach_async(&self, prep: Prep) -> Result<Box<dyn Stream>, ProviderError> {
        let session = self.session().await?;
        let file = AudioFile::open(&session, prep.file, prep.bytes_per_second)
            .await
            .map_err(|e| {
                ProviderError::Network(format!("cannot open the Spotify stream: {}", shown(&e)))
            })?;
        let len = file
            .get_stream_loader_controller()
            .map_err(|e| {
                ProviderError::Network(format!("cannot open the Spotify stream: {}", shown(&e)))
            })?
            .len() as u64;
        Ok(Box::new(Open {
            stream: Skipped::new(AudioDecrypt::new(Some(prep.key), file), len),
            _session: session,
        }))
    }

    async fn sign_in_async(
        &self,
        path: &Path,
        r#gen: u64,
        pending: &mut dyn FnMut(Pending),
    ) -> Result<(), ProviderError> {
        let stop = self.flow.cancel.notified();
        tokio::pin!(stop);
        if !self.flow.is_current(r#gen) {
            return Err(cancelled());
        }
        let config = self.config();
        let client = DeviceAuthClientBuilder::new(&config.client_id, vec![SIGN_IN_SCOPE])
            .build()
            .map_err(|e| ProviderError::ClientBroken(format!("Spotify sign-in: {}", shown(&e))))?;
        let auth = tokio::select! {
            a = client.request_device_code_async() => a.map_err(|e| {
                ProviderError::Network(format!("Spotify sign-in: {}", shown(&e)))
            })?,
            () = &mut stop => return Err(cancelled()),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        pending(Pending {
            url: auth.url().to_owned(),
            code: auth.user_code().to_owned(),
            expires_at_unix: (now + CODE_LIFETIME).as_secs(),
        });
        let token = tokio::select! {
            t = client.poll_for_token_async(&auth) => t.map_err(|e| {
                ProviderError::SignInRequired(format!("Spotify sign-in did not finish: {}", shown(&e)))
            })?,
            () = &mut stop => return Err(cancelled()),
        };
        let (session, reusable) = tokio::select! {
            r = self.reusable(path, token.access_token) => r?,
            () = &mut stop => return Err(cancelled()),
        };
        let mut slot = self.session.lock().await;
        // A sign-out while the session connected wins.
        if !self.flow.is_current(r#gen) {
            return Err(cancelled());
        }
        creds::save(path, &reusable).map_err(|e| {
            ProviderError::ClientBroken(format!(
                "cannot keep the sign-in in {}: {e}",
                path.display()
            ))
        })?;
        *slot = Some(session);
        Ok(())
    }

    /// Connects with the access token and returns librespot's reusable credentials, which it
    /// hands over only by saving them in a cache: a scratch one, removed at once.
    async fn reusable(
        &self,
        path: &Path,
        access_token: String,
    ) -> Result<(Session, Credentials), ProviderError> {
        let broken = |e: io::Error| ProviderError::ClientBroken(format!("Spotify sign-in: {e}"));
        let scratch = Scratch::new(path).map_err(broken)?;
        let cache = Cache::new(Some(scratch.0.as_path()), None, None, None)
            .map_err(|e| ProviderError::ClientBroken(format!("Spotify sign-in: {}", shown(&e))))?;
        let session = Session::new(self.config(), Some(cache));
        session
            .connect(Credentials::with_access_token(access_token), true)
            .await
            .map_err(|e| connect_error(&e))?;
        let reusable = session
            .cache()
            .and_then(|c| c.credentials())
            .ok_or_else(|| {
                ProviderError::ClientBroken("Spotify sent no reusable sign-in".into())
            })?;
        Ok((session, reusable))
    }
}

impl Backend for Librespot {
    fn search(&self, q: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        self.rt.block_on(self.search_async(q, limit))
    }

    fn browse(&self, page: &Page) -> Result<Vec<Shelf>, ProviderError> {
        match page {
            Page::Home => self.rt.block_on(self.home()),
            Page::Library => self.rt.block_on(self.library()),
            Page::Collection(c) => self.rt.block_on(self.collection(&c.id)),
        }
    }

    fn prepare(&self, track: &str) -> Result<String, ProviderError> {
        self.rt.block_on(self.prepare_async(track))
    }

    fn attach(&self, stream: &str) -> Result<Box<dyn Stream>, ProviderError> {
        let prep = self
            .prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(Instant::now(), stream)
            .ok_or_else(|| ProviderError::Network("the Spotify stream expired".into()))?;
        self.rt.block_on(self.attach_async(prep))
    }

    fn sign_in(&self, pending: &mut dyn FnMut(Pending)) -> Result<(), ProviderError> {
        let path = self.creds.clone().ok_or_else(no_place)?;
        let r#gen = self.flow.start();
        self.rt.block_on(self.sign_in_async(&path, r#gen, pending))
    }

    /// Home and Library show covers and playlists that background lookups find.
    fn watch(&self, changed: &mut dyn FnMut(Page) -> bool) {
        let mut changes = self.covers.subscribe();
        self.rt.block_on(async {
            while changes.changed().await.is_ok() {
                if !changed(Page::Home) || !changed(Page::Library) {
                    return;
                }
            }
        });
    }

    fn sign_out(&self) -> Result<(), ProviderError> {
        self.flow.cancel();
        let old = self.rt.block_on(self.session.lock()).take();
        // Streams still hold their clones, so components outlive this; they just stop working.
        if let Some(s) = old {
            s.shutdown();
        }
        // The next account must not see this one's listening.
        *lock(&self.recent) = RecentCache::default();
        match &self.creds {
            Some(path) => creds::forget(path).map_err(ProviderError::ClientBroken),
            None => Ok(()),
        }
    }
}

/// Each `start` or `cancel` ends the flows started before it.
#[derive(Default)]
struct SignInFlow {
    r#gen: AtomicU64,
    cancel: Notify,
}

impl SignInFlow {
    fn start(&self) -> u64 {
        let r#gen = self.r#gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.cancel.notify_waiters();
        r#gen
    }

    fn cancel(&self) {
        self.r#gen.fetch_add(1, Ordering::SeqCst);
        self.cancel.notify_waiters();
    }

    fn is_current(&self, r#gen: u64) -> bool {
        self.r#gen.load(Ordering::SeqCst) == r#gen
    }
}

/// A 0700 directory beside the credentials file, removed on drop. A helper killed while
/// holding one leaves it for the next helper's start or a sign-out to remove.
struct Scratch(PathBuf);

impl Scratch {
    fn new(creds: &Path) -> io::Result<Self> {
        let scratch = creds::scratch_dir(creds)?;
        if let Some(dir) = scratch.parent() {
            creds::create_dirs(dir)?;
        }
        match fs::remove_dir_all(&scratch) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        fs::DirBuilder::new().mode(0o700).create(&scratch)?;
        Ok(Self(scratch))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An open file and the session its download task needs alive.
struct Open<R> {
    stream: Skipped<R>,
    _session: Session,
}

impl<R: Read + Seek + Send> Stream for Open<R> {
    fn byte_len(&self) -> u64 {
        self.stream.byte_len()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read_at(offset, buf)
    }
}

/// `inner` less its first [`SPOTIFY_HEADER`] bytes. Seeks reach `inner` only as
/// `SeekFrom::Start`, as librespot's `SeekFrom::End` is off by one.
pub(crate) struct Skipped<R> {
    inner: R,
    inner_len: u64,
    /// `inner`'s position when known.
    at: Option<u64>,
}

impl<R: Read + Seek> Skipped<R> {
    pub(crate) fn new(inner: R, inner_len: u64) -> Self {
        Self {
            inner,
            inner_len,
            at: None,
        }
    }
}

impl<R: Read + Seek + Send> Stream for Skipped<R> {
    fn byte_len(&self) -> u64 {
        self.inner_len.saturating_sub(SPOTIFY_HEADER)
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.byte_len();
        if offset >= len {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(len - offset).unwrap_or(usize::MAX));
        let start = offset + SPOTIFY_HEADER;
        if self.at != Some(start) {
            self.at = None;
            self.inner.seek(SeekFrom::Start(start))?;
            self.at = Some(start);
        }
        let mut got = 0;
        while got < want {
            match self.inner.read(&mut buf[got..want]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.at = None;
                    return Err(e);
                }
            }
        }
        self.at = Some(start + got as u64);
        Ok(got)
    }
}

/// Prepared streams by id, each taken at most once and forgotten after
/// [`PREPARED_EXPIRY`].
pub(crate) struct PreparedStreams<T> {
    /// Differs per helper, so an id from a helper that died never names a new helper's stream.
    nonce: u64,
    next: u64,
    entries: HashMap<String, (Instant, T)>,
}

impl<T> PreparedStreams<T> {
    pub(crate) fn new() -> Self {
        Self {
            nonce: RandomState::new().build_hasher().finish(),
            next: 0,
            entries: HashMap::new(),
        }
    }

    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|_, (made, _)| now.saturating_duration_since(*made) < PREPARED_EXPIRY);
    }

    pub(crate) fn insert(&mut self, now: Instant, value: T) -> String {
        self.expire(now);
        self.next += 1;
        let id = format!("{:016x}-{}", self.nonce, self.next);
        self.entries.insert(id.clone(), (now, value));
        id
    }

    pub(crate) fn take(&mut self, now: Instant, id: &str) -> Option<T> {
        self.expire(now);
        self.entries.remove(id).map(|(_, v)| v)
    }
}

/// The best Ogg Vorbis file of `id`, else of its first alternative that has one.
async fn pick_file(session: &Session, id: SpotifyId) -> Result<(FileId, usize), ProviderError> {
    let item = AudioItem::get_file(session, SpotifyUri::Track { id })
        .await
        .map_err(|e| api_error("track", &e))?;
    if let Err(reason) = item.availability {
        return Err(ProviderError::Unplayable(format!(
            "not available on Spotify: {reason}"
        )));
    }
    if let Some(f) = best_file(&item.files) {
        return Ok(f);
    }
    for alt in item.alternatives.iter().flat_map(|t| t.0.iter()) {
        let Ok(alt) = AudioItem::get_file(session, alt.clone()).await else {
            continue;
        };
        if alt.availability.is_ok()
            && let Some(f) = best_file(&alt.files)
        {
            return Ok(f);
        }
    }
    Err(ProviderError::Unplayable(
        "no playable version of this track on Spotify".into(),
    ))
}

pub(crate) fn best_file(files: &AudioFiles) -> Option<(FileId, usize)> {
    FORMATS
        .iter()
        .find_map(|(format, rate)| files.get(format).map(|id| (*id, *rate)))
}

/// Track details for `ids`, in order and at most [`METADATA_IN_FLIGHT`] requests at a time.
async fn tracks(session: &Session, ids: Vec<SpotifyId>) -> Result<Vec<Track>, ProviderError> {
    let found = all_found(
        "track details",
        futures_util::stream::iter(ids)
            .map(|id| async move {
                SpotifyTrack::get(session, &SpotifyUri::Track { id })
                    .await
                    .map(|t| (id, t))
            })
            .buffered(METADATA_IN_FLIGHT),
    )
    .await?;
    Ok(found.into_iter().map(|(id, t)| to_track(id, t)).collect())
}

/// The lookups that found something, in order. One that failed for now, as when rate
/// limited, fails them all and stops the rest: a list cut short would pass for the whole one.
/// A track Spotify no longer has is left out; only if every one is gone is that an error.
async fn all_found<T>(
    what: &str,
    lookups: impl futures_util::Stream<Item = Result<T, Error>>,
) -> Result<Vec<T>, ProviderError> {
    let mut lookups = std::pin::pin!(lookups);
    let mut out = Vec::new();
    let mut gone = None;
    while let Some(r) = lookups.next().await {
        match r {
            Ok(t) => out.push(t),
            Err(e) => match api_error(what, &e) {
                ProviderError::Unplayable(_) => {
                    gone.get_or_insert(e);
                }
                failed => return Err(failed),
            },
        }
    }
    match gone {
        Some(e) if out.is_empty() => Err(api_error(what, &e)),
        Some(e) => {
            eprintln!("[spotify] left out some without {what}: {}", shown(&e));
            Ok(out)
        }
        None => Ok(out),
    }
}

fn to_track(id: SpotifyId, t: SpotifyTrack) -> Track {
    let artwork_url = artwork(&t.album.covers).or_else(|| artwork(&t.album.cover_group));
    Track {
        id: TrackId {
            provider: provider(),
            id: id.to_base62(),
        },
        title: t.name,
        artist_ids: artist_ids(&t.artists),
        artists: t.artists.0.into_iter().map(|a| a.name).collect(),
        album: Some(t.album.name).filter(|n| !n.is_empty()),
        duration: u64::try_from(t.duration)
            .ok()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis),
        artwork_url,
    }
}

/// The page of each of `artists`.
fn artist_ids(artists: &Artists) -> Vec<Option<CollectionId>> {
    artists
        .iter()
        .map(|a| match a.id {
            SpotifyUri::Artist { id } => Some(artist_page(id)),
            _ => None,
        })
        .collect()
}

fn artist_page(id: SpotifyId) -> CollectionId {
    CollectionId {
        provider: provider(),
        id: format!("{ARTIST_PREFIX}{}", id.to_base62()),
    }
}

/// The artist a collection id opens, if it is an artist's.
fn artist_of(id: &str) -> Option<SpotifyId> {
    SpotifyId::from_base62(id.strip_prefix(ARTIST_PREFIX)?).ok()
}

/// Album details for `ids`, in order and at most [`METADATA_IN_FLIGHT`] requests at a time.
async fn albums(
    session: &Session,
    ids: Vec<SpotifyId>,
) -> Result<Vec<(SpotifyId, Album)>, ProviderError> {
    all_found(
        "album details",
        futures_util::stream::iter(ids)
            .map(|id| async move {
                Album::get(session, &SpotifyUri::Album { id })
                    .await
                    .map(|a| (id, a))
            })
            .buffered(METADATA_IN_FLIGHT),
    )
    .await
}

/// What an artist's page needs looked up.
#[derive(Debug, PartialEq)]
struct ArtistWants {
    top: Vec<SpotifyId>,
    albums: Vec<SpotifyId>,
    singles: Vec<SpotifyId>,
}

impl ArtistWants {
    /// The top songs for `country`, else for everywhere, else for the first country listed;
    /// and the latest release of each album, and of each single and EP.
    fn of(artist: &Artist, country: &str) -> Self {
        let mut top = artist.top_tracks.for_country(country).0;
        if top.is_empty()
            && let Some(first) = artist.top_tracks.first()
        {
            top.clone_from(&first.tracks.0);
        }
        let top = top
            .iter()
            .filter_map(|uri| match uri {
                SpotifyUri::Track { id } => Some(*id),
                _ => None,
            })
            .take(ARTIST_TOP_SONGS)
            .collect();
        Self {
            top,
            albums: releases(artist.albums_current()),
            singles: releases(artist.singles_current()),
        }
    }
}

fn releases<'a>(uris: impl Iterator<Item = &'a SpotifyUri>) -> Vec<SpotifyId> {
    uris.filter_map(|uri| match uri {
        SpotifyUri::Album { id } => Some(*id),
        _ => None,
    })
    .take(ARTIST_RELEASES)
    .collect()
}

/// An artist's page, each shelf left out when empty. The release cards leave out the artist,
/// whose page it is: "Album • 2013", "EP • 2020".
fn artist_shelves(
    top: Vec<Track>,
    albums: Vec<(SpotifyId, Album)>,
    singles: Vec<(SpotifyId, Album)>,
) -> Vec<Shelf> {
    let cards = |releases: Vec<(SpotifyId, Album)>| -> Vec<Item> {
        releases
            .into_iter()
            .map(|(id, album)| Item::Collection(release_card(id, &album)))
            .collect()
    };
    let shelves: [(&str, Vec<Item>); 3] = [
        (ARTIST_TOP, top.into_iter().map(Item::Track).collect()),
        (ARTIST_ALBUMS, cards(albums)),
        (ARTIST_SINGLES, cards(singles)),
    ];
    shelves
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(title, items)| Shelf {
            title: title.to_owned(),
            items,
        })
        .collect()
}

fn release_card(id: SpotifyId, album: &Album) -> Collection {
    let kind = match album.album_type {
        AlbumType::SINGLE => "Single",
        AlbumType::EP => "EP",
        AlbumType::COMPILATION => "Compilation",
        _ => "Album",
    };
    let year = album.date.year();
    let subtitle = if year > 0 {
        format!("{kind} \u{2022} {year}")
    } else {
        kind.to_owned()
    };
    Collection {
        subtitle: Some(subtitle),
        ..album_card(id, album)
    }
}

pub(crate) fn artwork(images: &[Image]) -> Option<String> {
    widest(images).map(|i| format!("{IMAGE_URL}{}", i.id.to_base16()))
}

/// The widest image, by declared size when widths are missing.
fn widest(images: &[Image]) -> Option<&Image> {
    let rank = |s: ImageSize| match s {
        ImageSize::SMALL => 0,
        ImageSize::DEFAULT => 1,
        ImageSize::LARGE => 2,
        ImageSize::XLARGE => 3,
    };
    images.iter().max_by_key(|i| (i.width, rank(i.size)))
}

/// Track ids in context order; episodes, local files and anything else are left out.
fn context_tracks(pages: &[ContextPage]) -> Vec<SpotifyId> {
    pages
        .iter()
        .flat_map(|p| p.tracks.iter())
        .filter_map(context_track)
        .collect()
}

fn context_track(t: &ContextTrack) -> Option<SpotifyId> {
    match t.uri.as_deref() {
        Some(uri) if !uri.is_empty() => track_id(uri),
        _ => t
            .gid
            .as_deref()
            .filter(|g| g.len() == 16)
            .and_then(|g| SpotifyId::from_raw(g).ok()),
    }
}

fn track_id(uri: &str) -> Option<SpotifyId> {
    match SpotifyUri::from_uri(uri).ok()? {
        SpotifyUri::Track { id } => Some(id),
        _ => None,
    }
}

/// A `next_page_url` answer: JSON of a context page.
pub(crate) fn parse_json_page(
    bytes: &[u8],
) -> Result<(Vec<SpotifyId>, Option<String>), ProviderError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| ProviderError::Parse(format!("Spotify collection page: {e}")))?;
    let ids = v
        .get("tracks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("uri")?.as_str())
        .filter_map(track_id)
        .collect();
    let next = ["next_page_url", "nextPageUrl"]
        .iter()
        .find_map(|k| v.get(*k)?.as_str())
        .filter(|u| !u.is_empty())
        .map(str::to_owned);
    Ok((ids, next))
}

/// Context `uri`'s track ids in order, its later pages followed until there are `cap`.
async fn context_ids(
    session: &Session,
    uri: &str,
    cap: usize,
) -> Result<Vec<SpotifyId>, ProviderError> {
    let ctx = session
        .spclient()
        .get_context(uri)
        .await
        .map_err(|e| api_error("collection", &e))?;
    let mut ids = context_tracks(&ctx.pages);
    let mut next: VecDeque<String> = ctx
        .pages
        .iter()
        .filter_map(|p| p.next_page_url.clone())
        .filter(|u| !u.is_empty())
        .collect();
    let mut seen = HashSet::new();
    let mut pages = 0;
    while let Some(url) = next.pop_front() {
        if ids.len() >= cap || pages >= MAX_PAGES {
            break;
        }
        if !seen.insert(url.clone()) {
            continue;
        }
        pages += 1;
        let bytes = session
            .spclient()
            .get_next_page(&url)
            .await
            .map_err(|e| api_error("collection", &e))?;
        let (more, then) = parse_json_page(&bytes)?;
        ids.extend(more);
        next.extend(then);
    }
    ids.truncate(cap);
    Ok(ids)
}

/// Liked Songs' context, which lists the most recently liked first.
fn liked_uri(session: &Session) -> String {
    format!("spotify:user:{}:collection", session.username())
}

fn liked_card() -> Collection {
    Collection {
        id: CollectionId {
            provider: provider(),
            id: LIKED_ID.into(),
        },
        kind: CollectionKind::Playlist,
        title: LIKED.into(),
        subtitle: None,
        artwork_url: Some(LIKED_ARTWORK.into()),
    }
}

/// The rootlist's playlists in order, each completed where the rootlist left it incomplete and
/// given the cover found for it where it has none. Never waits for a cover, and for a whole
/// playlist only [`INLINE_BUDGET`].
async fn playlists(
    session: &Session,
    covers: &Arc<Covers>,
) -> Result<Vec<Collection>, ProviderError> {
    let mut rows = Vec::new();
    let mut from = 0;
    for _ in 0..MAX_PAGES {
        let bytes = session
            .spclient()
            .get_rootlist(from, Some(ROOTLIST_PAGE))
            .await
            .map_err(|e| api_error("playlists", &e))?;
        let list = SelectedListContent::parse_from_bytes(&bytes)
            .map_err(|e| ProviderError::Parse(format!("Spotify playlists: {e}")))?;
        let got = list.contents.items.len();
        rows.extend(rootlist_rows(&list));
        from += got;
        let done = match list.length {
            Some(total) => from >= total.max(0) as usize,
            None => got < ROOTLIST_PAGE,
        };
        if got == 0 || done {
            break;
        }
    }
    let until = tokio::time::Instant::now() + INLINE_BUDGET;
    let collections = futures_util::future::join_all(
        rows.into_iter()
            .map(|row| listed(session, covers, row, until)),
    )
    .await;
    Ok(collections.into_iter().flatten().collect())
}

/// `row` as a Library card. A row that is incomplete is completed from the whole playlist, or
/// left out if that isn't had by `until`; one still without artwork gets the cover found for
/// it, if any.
async fn listed(
    session: &Session,
    covers: &Arc<Covers>,
    row: RootlistRow,
    until: tokio::time::Instant,
) -> Option<Collection> {
    let RootlistRow {
        id,
        mut collection,
        incomplete,
        revision,
    } = row;
    let mut first = None;
    if incomplete {
        let fetch = {
            let session = session.clone();
            let covers = covers.clone();
            move || async move {
                let _permit = covers.permits.acquire().await.ok()?;
                let deadline = tokio::time::Instant::now() + BACKGROUND_BUDGET;
                whole_playlist(&session, &id, deadline).await
            }
        };
        let InTime::Got(whole) = covers.whole_by(id, &revision, until, fetch).await else {
            return None;
        };
        if let Some(Whole::Found { first: f, .. }) = &whole {
            first = Some(f.clone());
        }
        collection = completed(collection, whole)?;
    }
    if collection.artwork_url.is_none() {
        collection.artwork_url = cover_later(session, covers, id, &revision, first);
    }
    Some(collection)
}

/// The cover kept for playlist `id` at `revision`; on a miss, none for now while a lookup runs
/// in the background, from the playlist's `first` tracks when they are known.
fn cover_later(
    session: &Session,
    covers: &Arc<Covers>,
    id: SpotifyId,
    revision: &[u8],
    first: Option<Vec<SpotifyId>>,
) -> Option<String> {
    let find = {
        let session = session.clone();
        let covers = covers.clone();
        move || async move {
            let deadline = tokio::time::Instant::now() + BACKGROUND_BUDGET;
            find_cover(&session, &covers, id, first, deadline).await
        }
    };
    covers.cover_now(id, revision, find)
}

/// The ids of a playlist's first tracks.
fn first_tracks(list: &SelectedListContent) -> Vec<SpotifyId> {
    list.contents
        .items
        .iter()
        .filter_map(|i| track_id(i.uri()))
        .take(COVER_TRACKS)
        .collect()
}

/// Playlist `id`'s cover, given its `first` tracks if they are known, else fetching the playlist.
/// `None` if that couldn't be told by `deadline`; `Some(None)` if the playlist has no cover to be
/// had.
async fn find_cover(
    session: &Session,
    covers: &Covers,
    id: SpotifyId,
    first: Option<Vec<SpotifyId>>,
    deadline: tokio::time::Instant,
) -> Option<Option<String>> {
    let ids = match first {
        Some(ids) => ids,
        None => {
            let whole = {
                let _permit = covers.permits.acquire().await.ok()?;
                whole_playlist(session, &id, deadline).await
            };
            let Some(Whole::Found { art, first, .. }) = whole else {
                return None;
            };
            if art.is_some() {
                return Some(art);
            }
            first
        }
    };
    let until = deadline.min(tokio::time::Instant::now() + FULL_PLAYLIST_TIMEOUT);
    let albums = tokio::time::timeout_at(until, album_covers(session, covers, ids))
        .await
        .ok()??;
    Some(cover_choice(None, &albums))
}

/// The album covers of `ids`' tracks in order, read [`MOSAIC_COVERS`] tracks at a time until
/// there are that many distinct albums. `None` if a track's details failed: the cover might
/// not be the playlist's.
async fn album_covers(
    session: &Session,
    covers: &Covers,
    ids: Vec<SpotifyId>,
) -> Option<Vec<AlbumCover>> {
    let mut found = Vec::new();
    for chunk in ids.chunks(MOSAIC_COVERS) {
        let results = futures_util::future::join_all(chunk.iter().map(|id| async move {
            let _permit = covers.permits.acquire().await.ok()?;
            SpotifyTrack::get(session, &SpotifyUri::Track { id: *id })
                .await
                .ok()
        }))
        .await;
        for t in results {
            found.extend(album_cover(&t?));
        }
        let distinct: HashSet<_> = found.iter().map(|a| &a.album).collect();
        if distinct.len() >= MOSAIC_COVERS {
            return Some(found);
        }
    }
    Some(found)
}

fn album_cover(t: &SpotifyTrack) -> Option<AlbumCover> {
    let image = widest(&t.album.covers).or_else(|| widest(&t.album.cover_group))?;
    Some(AlbumCover {
        album: t.album.id.to_id(),
        image: image.id.to_base16(),
    })
}

/// What a Spotify collection id opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opens {
    Liked,
    Playlist(SpotifyId),
    Album(SpotifyId),
}

impl Opens {
    fn from_id(id: &str) -> Option<Self> {
        if id == LIKED_ID {
            return Some(Self::Liked);
        }
        let b62 = |prefix| SpotifyId::from_base62(id.strip_prefix(prefix)?).ok();
        b62(PLAYLIST_PREFIX)
            .map(Self::Playlist)
            .or_else(|| b62(ALBUM_PREFIX).map(Self::Album))
    }

    /// From a played context's URI; artists, shows and the rest open as nothing.
    fn from_context(uri: &str) -> Option<Self> {
        // The user's Liked Songs; an artist's songs within it end in more.
        if uri.starts_with("spotify:user:") && uri.ends_with(":collection") {
            return Some(Self::Liked);
        }
        match SpotifyUri::from_uri(uri).ok()? {
            SpotifyUri::Playlist { id, .. } => Some(Self::Playlist(id)),
            SpotifyUri::Album { id } => Some(Self::Album(id)),
            _ => None,
        }
    }

    fn uri(self, session: &Session) -> String {
        match self {
            Self::Liked => liked_uri(session),
            Self::Playlist(id) => format!("spotify:playlist:{}", id.to_base62()),
            Self::Album(id) => format!("spotify:album:{}", id.to_base62()),
        }
    }

    fn id(self) -> CollectionId {
        let id = match self {
            Self::Liked => LIKED_ID.to_owned(),
            Self::Playlist(id) => format!("{PLAYLIST_PREFIX}{}", id.to_base62()),
            Self::Album(id) => format!("{ALBUM_PREFIX}{}", id.to_base62()),
        };
        CollectionId {
            provider: provider(),
            id,
        }
    }
}

/// A context played lately, and the track of it played last.
#[derive(Debug, PartialEq)]
pub(crate) struct Played {
    pub(crate) context: String,
    pub(crate) track: Option<SpotifyId>,
}

/// A recently-played answer: its contexts, the latest first.
pub(crate) fn parse_recently_played(bytes: &[u8]) -> Result<Vec<Played>, ProviderError> {
    let bad = |e: String| ProviderError::Parse(format!("Spotify recently played: {e}"));
    let v: Value = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
    let contexts = v
        .get("playContexts")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("no playContexts".into()))?;
    Ok(contexts
        .iter()
        .filter_map(|c| {
            Some(Played {
                context: c.get("uri")?.as_str()?.to_owned(),
                track: c
                    .get("lastPlayedTrackUri")
                    .and_then(Value::as_str)
                    .and_then(track_id),
            })
        })
        .collect())
}

/// The first [`LISTEN_AGAIN`] of `played` that open as collections, each once.
fn listen_again_picks(played: &[Played]) -> Vec<Opens> {
    let mut picks = Vec::new();
    for opens in played
        .iter()
        .filter_map(|p| Opens::from_context(&p.context))
    {
        if picks.len() == LISTEN_AGAIN {
            break;
        }
        if !picks.contains(&opens) {
            picks.push(opens);
        }
    }
    picks
}

/// A radio answer's tracks, with the details it sends along: one artist and no duration,
/// which saves a request per track.
pub(crate) fn parse_station(bytes: &[u8]) -> Result<Vec<Track>, ProviderError> {
    let bad = |e: String| ProviderError::Parse(format!("Spotify radio: {e}"));
    let v: Value = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
    let tracks = v
        .get("tracks")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("no tracks".into()))?;
    Ok(tracks.iter().filter_map(station_track).collect())
}

fn station_track(t: &Value) -> Option<Track> {
    let id = track_id(t.get("uri")?.as_str()?)?;
    let text = |v: Option<&Value>, key: &str| {
        v?.get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let meta = t.get("metadata");
    let artists: Vec<String> = text(meta, "artist_name").into_iter().collect();
    let artist_uri = text(meta, "artist_uri").or_else(|| text(Some(t), "artist_uri"));
    let artist_ids = artists
        .iter()
        .map(|_| artist_of(artist_uri.as_deref()?).map(artist_page))
        .collect();
    Some(Track {
        id: TrackId {
            provider: provider(),
            id: id.to_base62(),
        },
        title: text(meta, "title").or_else(|| text(Some(t), "name"))?,
        artists,
        artist_ids,
        album: text(meta, "album_title"),
        duration: None,
        artwork_url: text(meta, "image_url")
            .and_then(|u| Some(format!("{IMAGE_URL}{}", u.strip_prefix("spotify:image:")?))),
    })
}

fn lock(cache: &Mutex<RecentCache>) -> std::sync::MutexGuard<'_, RecentCache> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What one fetch of the recent shelves asked of Spotify.
#[derive(Default)]
struct Asked {
    requests: AtomicUsize,
    limited: AtomicBool,
}

impl Asked {
    /// Sends `request`, given up on at `until`, counting it and noting whether Spotify answered
    /// by limiting the rate. The error is fit to log.
    async fn ask<T>(
        &self,
        what: &str,
        until: tokio::time::Instant,
        request: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, String> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        match tokio::time::timeout_at(until, request).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                if e.kind == ErrorKind::ResourceExhausted {
                    self.limited.store(true, Ordering::Relaxed);
                }
                Err(format!("Spotify {what}: {}", shown(&e)))
            }
            Err(_) => Err(format!("Spotify {what}: timed out")),
        }
    }
}

/// Listen again, the collections played lately, and Quick picks, a radio of the track played
/// last. Each is left out, or left short, when its requests fail or aren't answered within
/// [`RECENT_BUDGET`]; Home never fails for them. Cards and radios are kept in `cache`, and
/// once Spotify limits the rate the last good shelves stand in for a while.
async fn recent(session: &Session, covers: &Arc<Covers>, cache: &Mutex<RecentCache>) -> Shelves {
    let backing_off = lock(cache).backing_off(Instant::now());
    if let Some(kept) = backing_off {
        debug(format_args!(
            "Listen again and Quick picks wait out a rate limit"
        ));
        return kept;
    }
    let until = tokio::time::Instant::now() + RECENT_BUDGET;
    let asked = Asked::default();
    let path = format!(
        "/recently-played/v3/user/{}/{RECENTLY_PLAYED}",
        session.username()
    );
    // librespot's plain JSON GET.
    let played = asked
        .ask(
            "recently played",
            until,
            session.spclient().get_next_page(&path),
        )
        .await
        .and_then(|b| parse_recently_played(&b).map_err(|e| e.to_string()));
    let got = match played {
        Ok(played) => {
            let seed = played.iter().find_map(|p| p.track);
            let (listen_again, quick_picks) = tokio::join!(
                listen_again(session, covers, cache, &asked, &played, until),
                quick_picks(session, cache, &asked, seed, until),
            );
            Shelves {
                listen_again,
                quick_picks,
            }
        }
        Err(e) => {
            eprintln!("[spotify] Home left out Listen again and Quick picks: {e}");
            Shelves::default()
        }
    };
    let requests = asked.requests.load(Ordering::Relaxed);
    debug(format_args!(
        "Listen again and Quick picks made {requests} requests"
    ));
    let mut kept = lock(cache);
    if asked.limited.load(Ordering::Relaxed) {
        eprintln!(
            "[spotify] Spotify is limiting requests; Listen again and Quick picks ask nothing of it for {} minutes",
            BACKOFF.as_secs() / 60
        );
        return kept.limited(Instant::now(), got);
    }
    kept.answered(&got);
    got
}

async fn listen_again(
    session: &Session,
    covers: &Arc<Covers>,
    cache: &Mutex<RecentCache>,
    asked: &Asked,
    played: &[Played],
    until: tokio::time::Instant,
) -> Vec<Collection> {
    let picks = listen_again_picks(played);
    let wanted = picks.len();
    let cards: Vec<_> = futures_util::future::join_all(picks.into_iter().map(|opens| async move {
        let key = opens.id().id;
        let kept = lock(cache).card(&key, Instant::now());
        let card = match kept {
            Some(card) => card,
            None => {
                let card = fetch_card(session, asked, opens, until).await?;
                lock(cache).keep_card(key, card.clone(), Instant::now());
                card
            }
        };
        Some(with_cover(session, covers, opens, card))
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    if cards.len() < wanted {
        eprintln!(
            "[spotify] Listen again left out {} it couldn't get",
            wanted - cards.len()
        );
    }
    cards
}

async fn fetch_card(
    session: &Session,
    asked: &Asked,
    opens: Opens,
    until: tokio::time::Instant,
) -> Option<Card> {
    let card = |collection| Card {
        collection,
        revision: Vec::new(),
        first: Vec::new(),
    };
    match opens {
        Opens::Liked => Some(card(liked_card())),
        Opens::Playlist(id) => played_playlist(session, asked, id, until).await,
        Opens::Album(id) => {
            let album = asked
                .ask(
                    "album",
                    until,
                    Album::get(session, &SpotifyUri::Album { id }),
                )
                .await
                .ok()?;
            Some(card(album_card(id, &album)))
        }
    }
}

/// `card`, given the cover kept for its playlist if it has no artwork of its own, or none for
/// now while a lookup from the tracks it holds runs in the background.
fn with_cover(session: &Session, covers: &Arc<Covers>, opens: Opens, card: Card) -> Collection {
    let mut collection = card.collection;
    if let Opens::Playlist(id) = opens
        && collection.artwork_url.is_none()
    {
        collection.artwork_url = cover_later(session, covers, id, &card.revision, Some(card.first));
    }
    collection
}

async fn played_playlist(
    session: &Session,
    asked: &Asked,
    id: SpotifyId,
    until: tokio::time::Instant,
) -> Option<Card> {
    let bytes = asked
        .ask("playlist", until, session.spclient().get_playlist(&id))
        .await
        .ok()?;
    let list = SelectedListContent::parse_from_bytes(&bytes).ok()?;
    let whole = whole_of(&list);
    let first = match &whole {
        Whole::Found { first, .. } => first.clone(),
        Whole::NotFound => Vec::new(),
    };
    let row = Collection {
        id: Opens::Playlist(id).id(),
        kind: CollectionKind::Playlist,
        title: "Playlist".into(),
        subtitle: None,
        artwork_url: None,
    };
    Some(Card {
        collection: completed(row, Some(whole))?,
        revision: list.revision().to_vec(),
        first,
    })
}

fn album_card(id: SpotifyId, album: &Album) -> Collection {
    let artists: Vec<_> = album.artists.iter().map(|a| a.name.as_str()).collect();
    Collection {
        id: Opens::Album(id).id(),
        kind: CollectionKind::Album,
        title: Some(album.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Album".into()),
        subtitle: Some(artists.join(", ")).filter(|s| !s.is_empty()),
        artwork_url: artwork(&album.covers).or_else(|| artwork(&album.cover_group)),
    }
}

/// A radio of track `seed`, without it; the one kept for `seed` if there is one.
async fn quick_picks(
    session: &Session,
    cache: &Mutex<RecentCache>,
    asked: &Asked,
    seed: Option<SpotifyId>,
    until: tokio::time::Instant,
) -> Vec<Track> {
    let Some(seed) = seed else {
        return Vec::new();
    };
    let kept = lock(cache).radio(seed, Instant::now());
    if let Some(tracks) = kept {
        return tracks;
    }
    let uri = SpotifyUri::Track { id: seed }.to_uri();
    let station =
        session
            .spclient()
            .get_apollo_station("tracks", &uri, Some(QUICK_PICKS), vec![seed], true);
    let tracks = asked
        .ask("radio", until, station)
        .await
        .and_then(|b| parse_station(&b).map_err(|e| e.to_string()));
    match tracks {
        Ok(mut tracks) => {
            tracks.retain(|t| t.id.id != seed.to_base62());
            tracks.truncate(QUICK_PICKS);
            lock(cache).keep_radio(seed, tracks.clone(), Instant::now());
            tracks
        }
        Err(e) => {
            eprintln!("[spotify] Home left out Quick picks: {e}");
            Vec::new()
        }
    }
}

/// Home, without empty shelves: the `liked` tracks, the playlists Spotify made, the Liked
/// Songs card and the rest of the playlists, then what was played lately.
fn home_shelves(liked: Vec<Track>, playlists: Vec<Collection>, recent: Shelves) -> Vec<Shelf> {
    let (made, own): (Vec<_>, Vec<_>) = playlists.into_iter().partition(|c| {
        c.id.id
            .strip_prefix(PLAYLIST_PREFIX)
            .is_some_and(|id| MADE_FOR_YOU.iter().any(|p| id.starts_with(p)))
    });
    let own = std::iter::once(liked_card()).chain(own);
    [
        (
            requiem_core::HOME_LIKED,
            liked.into_iter().map(Item::Track).collect(),
        ),
        (
            requiem_core::HOME_MIXES,
            made.into_iter().map(Item::Collection).collect(),
        ),
        (
            requiem_core::HOME_PLAYLISTS,
            own.map(Item::Collection).collect(),
        ),
        (
            requiem_core::HOME_LISTEN_AGAIN,
            recent
                .listen_again
                .into_iter()
                .map(Item::Collection)
                .collect(),
        ),
        (
            requiem_core::HOME_QUICK_PICKS,
            recent.quick_picks.into_iter().map(Item::Track).collect(),
        ),
    ]
    .into_iter()
    .filter(|(_, items): &(_, Vec<Item>)| !items.is_empty())
    .map(|(title, items)| Shelf {
        title: title.into(),
        items,
    })
    .collect()
}

/// A rootlist playlist as a Library row.
pub(crate) struct RootlistRow {
    pub(crate) id: SpotifyId,
    pub(crate) collection: Collection,
    /// The rootlist gave no name, or a size of none or 0, so the whole playlist is fetched for
    /// them: some mixes list 0 there while holding 50, and a playlist that no longer exists
    /// comes with neither.
    pub(crate) incomplete: bool,
    /// The playlist's revision in the rootlist; empty when the rootlist gave none.
    pub(crate) revision: Vec<u8>,
}

/// The rootlist's playlists in order; folder markers are skipped, so folders flatten.
pub(crate) fn rootlist_rows(list: &SelectedListContent) -> Vec<RootlistRow> {
    let Some(contents) = list.contents.as_ref() else {
        return Vec::new();
    };
    // Names and sizes come in a parallel list, when it matches.
    let metas = (contents.meta_items.len() == contents.items.len()).then_some(&contents.meta_items);
    contents
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let SpotifyUri::Playlist { id, .. } = SpotifyUri::from_uri(item.uri()).ok()? else {
                return None;
            };
            let meta = metas.map(|m| &m[i]);
            let attrs = meta.and_then(|m| m.attributes.as_ref());
            let name = attrs.map(ListAttributes::name).filter(|n| !n.is_empty());
            let length = meta.and_then(|m| m.length).filter(|n| *n > 0);
            Some(RootlistRow {
                id,
                collection: Collection {
                    id: CollectionId {
                        provider: provider(),
                        id: format!("{PLAYLIST_PREFIX}{}", id.to_base62()),
                    },
                    kind: CollectionKind::Playlist,
                    title: name.unwrap_or("Playlist").to_owned(),
                    subtitle: length.map(songs),
                    artwork_url: attrs.and_then(playlist_artwork),
                },
                incomplete: name.is_none() || length.is_none(),
                revision: meta.map(|m| m.revision().to_vec()).unwrap_or_default(),
            })
        })
        .collect()
}

fn songs(n: i32) -> String {
    match n {
        1 => "1 song".to_owned(),
        n => format!("{n} songs"),
    }
}

/// `row` completed from `whole`: its name, size and artwork where the playlist has them, else
/// the rootlist's, which a failed fetch, `None`, keeps. `None` drops a playlist that doesn't
/// exist.
pub(crate) fn completed(mut row: Collection, whole: Option<Whole>) -> Option<Collection> {
    let (name, length, art) = match whole {
        Some(Whole::Found {
            name, length, art, ..
        }) => (name, length, art),
        Some(Whole::NotFound) => return None,
        None => return Some(row),
    };
    if let Some(name) = name {
        row.title = name;
    }
    if let Some(n) = length {
        row.subtitle = Some(songs(n));
    }
    if let Some(art) = art {
        row.artwork_url = Some(art);
    }
    Some(row)
}

/// What a fetched playlist tells of itself for its row.
pub(crate) fn whole_of(list: &SelectedListContent) -> Whole {
    let attrs = list.attributes.as_ref();
    Whole::Found {
        name: attrs
            .map(ListAttributes::name)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        length: list.length,
        art: attrs.and_then(playlist_artwork),
        first: first_tracks(list),
    }
}

/// The whole playlist `id`, given up on at `deadline` at the latest: `None` if timed out,
/// refused or unreadable.
async fn whole_playlist(
    session: &Session,
    id: &SpotifyId,
    deadline: tokio::time::Instant,
) -> Option<Whole> {
    let until = deadline.min(tokio::time::Instant::now() + FULL_PLAYLIST_TIMEOUT);
    let fetched = tokio::time::timeout_at(until, session.spclient().get_playlist(id)).await;
    let why = match fetched {
        Ok(Ok(bytes)) => match SelectedListContent::parse_from_bytes(&bytes) {
            Ok(list) => return Some(whole_of(&list)),
            Err(e) => e.to_string(),
        },
        Ok(Err(e)) if e.kind == ErrorKind::NotFound => return Some(Whole::NotFound),
        Ok(Err(e)) => shown(&e),
        Err(_) => "timed out".to_owned(),
    };
    eprintln!("[spotify] kept a playlist's rootlist details: {why}");
    None
}

fn playlist_artwork(a: &ListAttributes) -> Option<String> {
    let sized = |name: &str| {
        a.picture_size
            .iter()
            .find(|p| p.target_name() == name && !p.url().is_empty())
            .map(|p| p.url().to_owned())
    };
    sized("large")
        .or_else(|| sized("default"))
        .or_else(|| {
            a.picture_size
                .iter()
                .map(|p| p.url())
                .find(|u| !u.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            let id = a.picture();
            (!id.is_empty()).then(|| {
                let mut url = IMAGE_URL.to_owned();
                for b in id {
                    let _ = write!(url, "{b:02x}");
                }
                url
            })
        })
}

async fn playlist_name(session: &Session, id: &SpotifyId) -> Option<String> {
    let bytes = session.spclient().get_playlist(id).await.ok()?;
    let list = SelectedListContent::parse_from_bytes(&bytes).ok()?;
    let name = list.attributes.as_ref()?.name();
    (!name.is_empty()).then(|| name.to_owned())
}

/// `spotify:search:` takes the query form-encoded: spaces as `+`.
pub(crate) fn search_term(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use librespot_protocol::metadata as proto;
    use librespot_protocol::playlist4_external::{ListItems, MetaItem, PictureSize};
    use protobuf::{EnumOrUnknown, MessageField};

    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn stale_sign_in_scratch_goes_at_start_and_at_sign_out() {
        let tmp = TempDir::new();
        let path = tmp.path().join("requiem/spotify-credentials.json");
        let stale = |name: &str| {
            let d = tmp.path().join("requiem").join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("credentials.json"), "{}").unwrap();
            d
        };
        let (fixed, old) = (stale(".spotify-sign-in"), stale(".spotify-sign-in-4242"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let backend = Librespot::new(rt, Some(path.clone()), tmp.path().to_owned());
        assert!(!fixed.exists() && !old.exists());

        let fixed = stale(".spotify-sign-in");
        fs::write(&path, "{}").unwrap();
        backend.sign_out().unwrap();
        assert!(!fixed.exists() && !path.exists());
        assert!(tmp.path().join("requiem").exists());
    }

    #[test]
    fn a_track_lookup_that_fails_for_now_fails_the_list() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let found = |lookups: Vec<Result<u8, Error>>| {
            rt.block_on(all_found(
                "track details",
                futures_util::stream::iter(lookups),
            ))
        };
        let limited = || Error::resource_exhausted(io::Error::other("rate limited"));
        let gone = || Error::not_found(io::Error::other("no such track"));

        assert!(matches!(
            found(vec![Ok(1), Err(limited()), Ok(3)]),
            Err(ProviderError::Network(_))
        ));
        assert!(matches!(
            found(vec![
                Ok(1),
                Err(Error::unavailable(io::Error::other("down")))
            ]),
            Err(ProviderError::Network(_))
        ));
        assert_eq!(found(vec![Ok(1), Err(gone()), Ok(3)]).unwrap(), vec![1, 3]);
        assert!(matches!(
            found(vec![Err(gone())]),
            Err(ProviderError::Unplayable(_))
        ));
    }

    #[test]
    fn librespot_error_text_loses_url_queries() {
        let e = Error::unavailable(io::Error::other(
            "Fetching https://cdn.example/a?token=SECRET",
        ));
        let ProviderError::Network(m) = api_error("stream", &e) else {
            panic!("not Network");
        };
        assert!(m.ends_with("https://cdn.example/a?<redacted> }"), "{m}");
    }

    /// Hands out at most 5 bytes per read and refuses all but `SeekFrom::Start`.
    struct Picky(Cursor<Vec<u8>>);

    impl Read for Picky {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(5);
            self.0.read(&mut buf[..n])
        }
    }

    impl Seek for Picky {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            assert!(matches!(pos, SeekFrom::Start(_)), "{pos:?}");
            self.0.seek(pos)
        }
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn skipped(len: usize) -> Skipped<Picky> {
        let mut file = vec![0xEE; SPOTIFY_HEADER as usize];
        file.extend(payload(len));
        let total = file.len() as u64;
        Skipped::new(Picky(Cursor::new(file)), total)
    }

    #[test]
    fn offsets_skip_the_spotify_header() {
        let p = payload(1000);
        let mut s = skipped(1000);
        assert_eq!(s.byte_len(), 1000);
        let mut buf = [0u8; 64];
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 64);
        assert_eq!(buf[..], p[..64]);
        // Contiguous reads and a jump back both land on the right bytes.
        assert_eq!(s.read_at(64, &mut buf).unwrap(), 64);
        assert_eq!(buf[..], p[64..128]);
        assert_eq!(s.read_at(3, &mut buf[..10]).unwrap(), 10);
        assert_eq!(buf[..10], p[3..13]);
        // Short only at the end, and nothing past it.
        assert_eq!(s.read_at(990, &mut buf).unwrap(), 10);
        assert_eq!(buf[..10], p[990..]);
        assert_eq!(s.read_at(1000, &mut buf).unwrap(), 0);
        assert_eq!(s.read_at(5000, &mut buf).unwrap(), 0);
    }

    #[test]
    fn a_file_shorter_than_the_header_is_empty() {
        let mut s = Skipped::new(Picky(Cursor::new(vec![0; 100])), 100);
        assert_eq!(s.byte_len(), 0);
        assert_eq!(s.read_at(0, &mut [0; 8]).unwrap(), 0);
        let exact = Skipped::new(Picky(Cursor::new(vec![0; 0xa7])), 0xa7);
        assert_eq!(exact.byte_len(), 0);
    }

    #[test]
    fn prepared_streams_are_taken_once_and_expire() {
        let t0 = Instant::now();
        let mut p = PreparedStreams::new();
        let a = p.insert(t0, 'a');
        let b = p.insert(t0, 'b');
        assert_ne!(a, b);
        assert_eq!(p.take(t0, &a), Some('a'));
        assert_eq!(p.take(t0, &a), None);
        let late = t0 + PREPARED_EXPIRY;
        assert_eq!(p.take(late - Duration::from_millis(1), &b), Some('b'));
        let c = p.insert(t0, 'c');
        assert_eq!(p.take(late, &c), None);
        // Another helper's ids never match.
        assert_ne!(PreparedStreams::new().insert(t0, 'x'), c);
    }

    #[test]
    fn formats_prefer_320_then_160_then_96() {
        let f = |n: u8| FileId([n; 20]);
        let mut files = AudioFiles::default();
        assert_eq!(best_file(&files), None);
        files.insert(AudioFileFormat::AAC_24, f(1));
        assert_eq!(best_file(&files), None);
        files.insert(AudioFileFormat::OGG_VORBIS_96, f(2));
        assert_eq!(best_file(&files), Some((f(2), 12 * 1024)));
        files.insert(AudioFileFormat::OGG_VORBIS_160, f(3));
        assert_eq!(best_file(&files), Some((f(3), 20 * 1024)));
        files.insert(AudioFileFormat::OGG_VORBIS_320, f(4));
        assert_eq!(best_file(&files), Some((f(4), 40 * 1024)));
    }

    #[test]
    fn search_terms_are_form_encoded() {
        assert_eq!(search_term("daft punk"), "daft+punk");
        assert_eq!(search_term("AC/DC: live"), "AC%2FDC%3A+live");
        assert_eq!(search_term("é+&"), "%C3%A9%2B%26");
    }

    #[test]
    fn artwork_is_the_widest_cover() {
        let img = |n: u8, size, width| Image {
            id: FileId([n; 20]),
            size,
            width,
            height: width,
        };
        assert_eq!(artwork(&[]), None);
        let url = artwork(&[
            img(1, ImageSize::DEFAULT, 300),
            img(2, ImageSize::LARGE, 640),
            img(3, ImageSize::SMALL, 64),
        ])
        .unwrap();
        assert_eq!(url, format!("{IMAGE_URL}{}", "02".repeat(20)));
        let by_size = artwork(&[img(1, ImageSize::DEFAULT, 0), img(2, ImageSize::XLARGE, 0)]);
        assert!(by_size.unwrap().ends_with(&"02".repeat(20)));
    }

    #[test]
    fn the_rootlist_flattens_folders_and_reads_names() {
        let item = |uri: &str| {
            let mut i = librespot_protocol::playlist4_external::Item::new();
            i.set_uri(uri.into());
            i
        };
        let meta = |name: &str, len: Option<i32>, pic: &[(&str, &str)]| {
            let mut a = ListAttributes::new();
            a.set_name(name.into());
            for (t, u) in pic {
                let mut p = PictureSize::new();
                p.set_target_name((*t).into());
                p.set_url((*u).into());
                a.picture_size.push(p);
            }
            let mut m = MetaItem::new();
            m.attributes = Some(a).into();
            m.length = len;
            m
        };
        let a = "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M";
        let b = "spotify:playlist:1pKYYY0dkg23sQQXi0Q5zN";
        let mut contents = ListItems::new();
        contents.items = vec![
            item("spotify:start-group:abc:Folder"),
            item(a),
            item("spotify:end-group:abc"),
            item(b),
            item("spotify:track:1pKYYY0dkg23sQQXi0Q5zN"),
        ];
        contents.meta_items = vec![
            meta("", None, &[]),
            meta("Mix", Some(1), &[("small", "s"), ("large", "L")]),
            meta("", None, &[]),
            meta("", Some(30), &[]),
            meta("", None, &[]),
        ];
        let mut list = SelectedListContent::new();
        list.contents = Some(contents.clone()).into();
        let got = rootlist_rows(&list);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id.to_base62(), "37i9dQZF1DXcBWIGoYBM5M");
        assert!(!got[0].incomplete);
        let mix = &got[0].collection;
        assert_eq!(mix.id.id, "playlist:37i9dQZF1DXcBWIGoYBM5M");
        assert_eq!(mix.title, "Mix");
        assert_eq!(mix.subtitle.as_deref(), Some("1 song"));
        assert_eq!(mix.artwork_url.as_deref(), Some("L"));
        // Nameless, so the whole playlist names it; "Playlist" stands in until then.
        assert!(got[1].incomplete);
        let nameless = &got[1].collection;
        assert_eq!(nameless.id.id, "playlist:1pKYYY0dkg23sQQXi0Q5zN");
        assert_eq!(nameless.title, "Playlist");
        assert_eq!(nameless.subtitle.as_deref(), Some("30 songs"));
        assert_eq!(nameless.kind, CollectionKind::Playlist);
        assert_eq!(nameless.id.provider, provider());

        // Without a matching meta list, playlists still show, each to be fetched whole.
        contents.meta_items.pop();
        list.contents = Some(contents).into();
        let got = rootlist_rows(&list);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.incomplete));
        assert_eq!(got[0].collection.title, "Playlist");
        assert_eq!(got[0].collection.subtitle, None);
    }

    fn rootlist_of(metas: Vec<MetaItem>) -> SelectedListContent {
        let mut contents = ListItems::new();
        contents.items = (0..metas.len())
            .map(|_| {
                let mut i = librespot_protocol::playlist4_external::Item::new();
                i.set_uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into());
                i
            })
            .collect();
        contents.meta_items = metas;
        let mut list = SelectedListContent::new();
        list.contents = Some(contents).into();
        list
    }

    fn meta(name: &str, length: Option<i32>) -> MetaItem {
        let mut a = ListAttributes::new();
        a.set_name(name.into());
        let mut m = MetaItem::new();
        m.attributes = Some(a).into();
        m.length = length;
        m
    }

    #[test]
    fn rows_without_a_name_or_a_size_are_fetched_whole() {
        let got = rootlist_rows(&rootlist_of(vec![
            meta("Mix", Some(50)),
            meta("Hardcore Rock Mix", Some(0)),
            meta("Unsized", None),
            meta("", Some(12)),
            meta("", None),
        ]));
        let incomplete: Vec<bool> = got.iter().map(|r| r.incomplete).collect();
        assert_eq!(incomplete, [false, true, true, true, true]);
        // A rootlist size of 0 is never shown: it may be wrong.
        assert_eq!(got[1].collection.subtitle, None);
        assert_eq!(got[1].collection.title, "Hardcore Rock Mix");
        assert_eq!(got[3].collection.subtitle.as_deref(), Some("12 songs"));
    }

    #[test]
    fn rows_carry_the_rootlists_revision() {
        let mut at_r7 = meta("Mine", Some(3));
        at_r7.revision = Some(b"r7".to_vec());
        let got = rootlist_of(vec![at_r7, meta("Unrevised", Some(3))]);
        let got = rootlist_rows(&got);
        assert_eq!(got[0].revision, b"r7");
        assert!(got[1].revision.is_empty());
    }

    fn row(title: &str, subtitle: Option<&str>, art: Option<&str>) -> Collection {
        Collection {
            id: CollectionId {
                provider: provider(),
                id: "playlist:37i9dQZF1DXcBWIGoYBM5M".into(),
            },
            kind: CollectionKind::Playlist,
            title: title.into(),
            subtitle: subtitle.map(Into::into),
            artwork_url: art.map(Into::into),
        }
    }

    fn whole(name: &str, length: Option<i32>, pic: Option<&str>) -> Box<SelectedListContent> {
        let mut a = ListAttributes::new();
        a.set_name(name.into());
        if let Some(url) = pic {
            let mut p = PictureSize::new();
            p.set_target_name("large".into());
            p.set_url(url.into());
            a.picture_size.push(p);
        }
        let mut list = SelectedListContent::new();
        list.attributes = Some(a).into();
        list.length = length;
        Box::new(list)
    }

    #[test]
    fn a_whole_playlist_completes_its_row() {
        let got = completed(
            row("Hardcore Rock Mix", None, Some("old")),
            Some(whole_of(&whole("Hardcore Rock Mix", Some(50), Some("new")))),
        )
        .unwrap();
        assert_eq!(got.title, "Hardcore Rock Mix");
        assert_eq!(got.subtitle.as_deref(), Some("50 songs"));
        assert_eq!(got.artwork_url.as_deref(), Some("new"));

        let got = completed(
            row("Playlist", Some("30 songs"), Some("old")),
            Some(whole_of(&whole("Named", None, None))),
        )
        .unwrap();
        assert_eq!(got.title, "Named");
        assert_eq!(got.subtitle.as_deref(), Some("30 songs"));
        assert_eq!(got.artwork_url.as_deref(), Some("old"));

        // The whole playlist's own 0 is real.
        let got = completed(
            row("Empty", None, None),
            Some(whole_of(&whole("", Some(0), None))),
        );
        assert_eq!(got.unwrap().subtitle.as_deref(), Some("0 songs"));
    }

    #[test]
    fn a_missing_playlist_goes_and_a_failed_fetch_keeps_the_row() {
        assert_eq!(
            completed(row("Playlist", None, None), Some(Whole::NotFound)),
            None
        );
        let kept = row("Metalcore Mix", None, Some("art"));
        assert_eq!(completed(kept.clone(), None), Some(kept));
    }

    fn playlist(id: &str, title: &str) -> Collection {
        Collection {
            id: CollectionId {
                provider: provider(),
                id: format!("{PLAYLIST_PREFIX}{id}"),
            },
            ..row(title, None, None)
        }
    }

    fn titles(shelf: &Shelf) -> Vec<&str> {
        shelf
            .items
            .iter()
            .map(|i| match i {
                Item::Track(t) => t.title.as_str(),
                Item::Collection(c) => c.title.as_str(),
            })
            .collect()
    }

    #[test]
    fn home_splits_the_playlists_spotify_made_from_the_rest() {
        let liked = Track {
            id: TrackId {
                provider: provider(),
                id: "t".into(),
            },
            title: "Song".into(),
            artists: Vec::new(),
            artist_ids: Vec::new(),
            album: None,
            duration: None,
            artwork_url: None,
        };
        let shelves = home_shelves(
            vec![liked],
            vec![
                playlist("1Ab2Cd3Ef4Gh5Ij6Kl7Mn8", "Mine"),
                playlist("37i9dQZF1E39pZBVudb5vh", "Daily Mix 2"),
                playlist("37i9dQZF1DZ06evO1JiPRm", "This Is Kado"),
                playlist("37i9dQZEVXcJZyENOWUFo7", "Discover Weekly"),
            ],
            Shelves::default(),
        );
        let got: Vec<_> = shelves.iter().map(|s| (&*s.title, titles(s))).collect();
        assert_eq!(
            got,
            [
                ("Liked songs", vec!["Song"]),
                (
                    "Mixes",
                    vec!["Daily Mix 2", "This Is Kado", "Discover Weekly"]
                ),
                ("Your playlists", vec!["Liked Songs", "Mine"]),
            ]
        );
    }

    #[test]
    fn asking_counts_requests_and_notes_only_a_rate_limit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let asked = Asked::default();
            let until = tokio::time::Instant::now() + Duration::from_secs(5);
            let limited = || asked.limited.load(Ordering::Relaxed);
            assert_eq!(
                asked.ask("x", until, async { Ok::<_, Error>(1) }).await,
                Ok(1)
            );
            let down = asked
                .ask("x", until, async {
                    Err::<u8, _>(Error::unavailable(io::Error::other("down")))
                })
                .await;
            assert!(down.is_err() && !limited());
            let late = asked
                .ask(
                    "x",
                    tokio::time::Instant::now(),
                    std::future::pending::<Result<u8, Error>>(),
                )
                .await;
            assert_eq!(late, Err("Spotify x: timed out".into()));
            assert!(!limited());
            let refused = asked
                .ask("radio", until, async {
                    Err::<u8, _>(Error::resource_exhausted(io::Error::other("429")))
                })
                .await;
            assert!(refused.unwrap_err().starts_with("Spotify radio: "));
            assert!(limited());
            assert_eq!(asked.requests.load(Ordering::Relaxed), 4);
        });
    }

    #[test]
    fn home_ends_with_listen_again_then_quick_picks() {
        let picks = parse_station(RADIO.as_bytes()).unwrap();
        let shelves = home_shelves(
            Vec::new(),
            vec![playlist("1Ab2", "Mine")],
            Shelves {
                listen_again: vec![
                    liked_card(),
                    playlist("37i9dQZF1E39pZBVudb5vh", "Daily Mix 2"),
                ],
                quick_picks: picks,
            },
        );
        let got: Vec<_> = shelves.iter().map(|s| (&*s.title, titles(s))).collect();
        assert_eq!(
            got,
            [
                ("Your playlists", vec!["Liked Songs", "Mine"]),
                ("Listen again", vec!["Liked Songs", "Daily Mix 2"]),
                (
                    "Quick picks",
                    vec!["First Song", "Second Song", "Bare Song"]
                ),
            ]
        );
        assert_eq!(shelves[1].title, requiem_core::HOME_LISTEN_AGAIN);
        assert_eq!(shelves[2].title, requiem_core::HOME_QUICK_PICKS);
    }

    const RECENTLY_PLAYED_FIXTURE: &str = include_str!("../tests/fixtures/recently_played.json");
    const RADIO: &str = include_str!("../tests/fixtures/radio_tracks.json");

    fn b62(s: &str) -> SpotifyId {
        SpotifyId::from_base62(s).unwrap()
    }

    fn gid(s: &str) -> Option<Vec<u8>> {
        Some(b62(s).to_raw().to_vec())
    }

    fn proto_album(id: &str, name: &str, kind: proto::album::Type, year: i32) -> proto::Album {
        proto::Album {
            gid: gid(id),
            name: Some(name.into()),
            type_: Some(EnumOrUnknown::new(kind)),
            date: MessageField::some(proto::Date {
                year: Some(year),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn album(id: &str, name: &str, kind: proto::album::Type, year: i32) -> (SpotifyId, Album) {
        let album = Album::try_from(&proto_album(id, name, kind, year)).unwrap();
        (b62(id), album)
    }

    #[test]
    fn a_tracks_artists_open_their_spotify_pages() {
        let artist = |id: &str, name: &str| proto::Artist {
            gid: gid(id),
            name: Some(name.into()),
            ..Default::default()
        };
        let msg = proto::Track {
            gid: gid("0FakeTrack100000000000"),
            name: Some("Song".into()),
            album: MessageField::some(proto_album(
                "0FakeAlbum100000000000",
                "Record",
                proto::album::Type::ALBUM,
                2013,
            )),
            artist: vec![
                artist("0FakeArtist10000000000", "First"),
                artist("0FakeArtist20000000000", "Second"),
            ],
            duration: Some(200_000),
            ..Default::default()
        };
        let t = to_track(
            b62("0FakeTrack100000000000"),
            SpotifyTrack::try_from(&msg).unwrap(),
        );
        assert_eq!(t.artists, ["First", "Second"]);
        let page = |id: &str| CollectionId {
            provider: provider(),
            id: id.into(),
        };
        assert_eq!(
            t.artist_ids,
            [
                Some(page("spotify:artist:0FakeArtist10000000000")),
                Some(page("spotify:artist:0FakeArtist20000000000")),
            ]
        );
        assert_eq!(
            t.artist_pages().next(),
            Some(("First", &page("spotify:artist:0FakeArtist10000000000")))
        );
        // The id opens the artist's page, and only an artist id does.
        assert_eq!(
            artist_of("spotify:artist:0FakeArtist10000000000"),
            Some(b62("0FakeArtist10000000000"))
        );
        assert_eq!(artist_of("album:0FakeAlbum100000000000"), None);
        assert_eq!(artist_of("spotify:artist:not-base62"), None);
    }

    fn proto_artist(top: Vec<(&str, usize)>, albums: usize) -> proto::Artist {
        let top_track = top
            .into_iter()
            .map(|(country, n)| proto::TopTracks {
                country: Some(country.into()),
                track: (0..n)
                    .map(|i| proto::Track {
                        gid: gid(&format!("0Top{country:0>2}{i:016}")),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
        let release = |id: String| proto::Album {
            gid: gid(&id),
            ..Default::default()
        };
        // The first album has an older release too, which the page leaves out.
        let album_group = (0..albums)
            .map(|i| proto::AlbumGroup {
                album: (0..if i == 0 { 2 } else { 1 })
                    .map(|v| release(format!("0FakeAlbum{v}{i:011}")))
                    .collect(),
                ..Default::default()
            })
            .collect();
        let single_group = vec![proto::AlbumGroup {
            album: vec![release("0FakeSingle00000000000".into())],
            ..Default::default()
        }];
        proto::Artist {
            gid: gid("0FakeArtist10000000000"),
            name: Some("Band".into()),
            top_track,
            album_group,
            single_group,
            ..Default::default()
        }
    }

    #[test]
    fn an_artists_page_wants_its_top_songs_where_the_account_is_and_its_latest_releases() {
        let artist = Artist::try_from(&proto_artist(vec![("SE", 3), ("US", 12)], 11)).unwrap();
        let wants = ArtistWants::of(&artist, "US");
        let top: Vec<SpotifyId> = (0..10).map(|i| b62(&format!("0TopUS{i:016}"))).collect();
        assert_eq!(wants.top, top, "the account's country, at most 10");
        let albums: Vec<SpotifyId> = (0..10)
            .map(|i| b62(&format!("0FakeAlbum0{i:011}")))
            .collect();
        assert_eq!(
            wants.albums, albums,
            "each album's latest release, at most 10"
        );
        assert_eq!(wants.singles, [b62("0FakeSingle00000000000")]);

        // A country the artist has no top songs for takes the first listed...
        let first = ArtistWants::of(&artist, "FR");
        assert_eq!(first.top.len(), 3);
        assert_eq!(first.top[0], b62(&format!("0TopSE{:016}", 0)));
        // ...unless there are top songs for everywhere.
        let global = Artist::try_from(&proto_artist(vec![("SE", 3), ("", 2)], 0)).unwrap();
        let wants = ArtistWants::of(&global, "FR");
        assert_eq!(
            wants.top,
            [
                b62(&format!("0Top00{:016}", 0)),
                b62(&format!("0Top00{:016}", 1))
            ]
        );
        assert!(wants.albums.is_empty());

        let none = Artist::try_from(&proto_artist(Vec::new(), 0)).unwrap();
        assert!(ArtistWants::of(&none, "US").top.is_empty());
    }

    #[test]
    fn an_artists_page_has_its_top_songs_then_albums_then_singles_and_eps() {
        let song = Track {
            id: TrackId {
                provider: provider(),
                id: "0FakeTrack100000000000".into(),
            },
            title: "Song".into(),
            artists: vec!["Band".into()],
            artist_ids: vec![Some(artist_page(b62("0FakeArtist10000000000")))],
            album: None,
            duration: None,
            artwork_url: None,
        };
        let albums = vec![album(
            "0FakeAlbum100000000000",
            "Record",
            proto::album::Type::ALBUM,
            2013,
        )];
        let singles = vec![
            album(
                "0FakeSingle00000000000",
                "Hit",
                proto::album::Type::SINGLE,
                2005,
            ),
            album("0FakeEP000000000000000", "Four", proto::album::Type::EP, 0),
        ];
        let shelves = artist_shelves(vec![song.clone()], albums.clone(), singles);
        let titles: Vec<&str> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Top songs", "Albums", "Singles & EPs"]);
        assert_eq!(shelves[0].items, [Item::Track(song)]);
        let card = |shelf: usize, i: usize| match &shelves[shelf].items[i] {
            Item::Collection(c) => c.clone(),
            Item::Track(t) => panic!("{} is no card", t.title),
        };
        let record = card(1, 0);
        // It opens as the album's own page.
        assert_eq!(record.id, Opens::Album(b62("0FakeAlbum100000000000")).id());
        assert_eq!(record.kind, CollectionKind::Album);
        assert_eq!(record.title, "Record");
        assert_eq!(record.subtitle.as_deref(), Some("Album \u{2022} 2013"));
        assert_eq!(card(2, 0).subtitle.as_deref(), Some("Single \u{2022} 2005"));
        // Without a year, the kind alone.
        assert_eq!(card(2, 1).subtitle.as_deref(), Some("EP"));

        let shelves = artist_shelves(Vec::new(), albums, Vec::new());
        let titles: Vec<&str> = shelves.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Albums"], "empty shelves are left out");
    }

    #[test]
    fn recently_played_gives_contexts_and_the_track_played_last() {
        let played = parse_recently_played(RECENTLY_PLAYED_FIXTURE.as_bytes()).unwrap();
        assert_eq!(played.len(), 7);
        assert_eq!(
            played[0],
            Played {
                context: "spotify:playlist:0FakeMix10000000000000".into(),
                track: Some(b62("0FakeSeed1000000000000")),
            }
        );
        // An episode is no track, and a context may name none.
        assert_eq!(played[5].track, None);
        assert_eq!(played[6].track, None);
        assert!(matches!(
            parse_recently_played(b"{}"),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_recently_played(b"<html>"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn listen_again_takes_collections_once_each_and_leaves_out_artists_and_shows() {
        let played = parse_recently_played(RECENTLY_PLAYED_FIXTURE.as_bytes()).unwrap();
        assert_eq!(
            listen_again_picks(&played),
            [
                Opens::Playlist(b62("0FakeMix10000000000000")),
                Opens::Album(b62("0FakeAlbum100000000000")),
                Opens::Liked,
                Opens::Playlist(b62("0FakeOwn10000000000000")),
            ]
        );
        let many: Vec<_> = (0..LISTEN_AGAIN + 3)
            .map(|n| Played {
                context: format!("spotify:album:0FakeAlbum{n:0>12}"),
                track: None,
            })
            .collect();
        assert_eq!(listen_again_picks(&many).len(), LISTEN_AGAIN);
    }

    #[test]
    fn collection_ids_round_trip_and_artist_liked_songs_open_nothing() {
        for opens in [
            Opens::Liked,
            Opens::Playlist(b62("0FakeMix10000000000000")),
            Opens::Album(b62("0FakeAlbum100000000000")),
        ] {
            assert_eq!(Opens::from_id(&opens.id().id), Some(opens));
        }
        assert_eq!(
            Opens::Album(b62("0FakeAlbum100000000000")).id().id,
            "album:0FakeAlbum100000000000"
        );
        assert_eq!(Opens::from_id("artist:0FakeArtist10000000000"), None);
        assert_eq!(Opens::from_id("album:not-base62"), None);
        assert_eq!(
            Opens::from_context("spotify:user:u:collection:artist:0FakeArtist10000000000"),
            None
        );
    }

    #[test]
    fn a_radio_gives_its_tracks_with_the_details_it_sends() {
        let tracks = parse_station(RADIO.as_bytes()).unwrap();
        assert_eq!(tracks.len(), 3, "the episode is left out");
        assert_eq!(
            tracks[0],
            Track {
                id: TrackId {
                    provider: provider(),
                    id: "0FakeRadio100000000000".into(),
                },
                title: "First Song".into(),
                artists: vec!["First Band".into()],
                artist_ids: vec![Some(artist_page(b62("0FakeBand1000000000000")))],
                album: Some("First Record".into()),
                duration: None,
                artwork_url: Some(format!(
                    "{IMAGE_URL}ab67616d0000b2730000000000000000000000a1"
                )),
            }
        );
        // Without metadata, the name is the title.
        assert_eq!(tracks[2].title, "Bare Song");
        assert!(tracks[2].artists.is_empty() && tracks[2].artist_ids.is_empty());
        assert_eq!(tracks[2].artwork_url, None);
        assert!(matches!(
            parse_station(b"{\"tracks\":null}"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn the_liked_songs_card_has_spotifys_picture() {
        let card = liked_card();
        assert_eq!(card.title, "Liked Songs");
        assert_eq!(
            card.artwork_url.as_deref(),
            Some("https://misc.scdn.co/liked-songs/liked-songs-640.png")
        );
    }

    #[test]
    fn home_leaves_out_empty_shelves() {
        let shelves = home_shelves(
            Vec::new(),
            vec![playlist("1Ab2", "Mine")],
            Shelves::default(),
        );
        let got: Vec<_> = shelves.iter().map(|s| (&*s.title, titles(s))).collect();
        assert_eq!(got, [("Your playlists", vec!["Liked Songs", "Mine"])]);
    }

    #[test]
    fn json_pages_give_tracks_and_the_next_url() {
        let page = br#"{"tracks":[
            {"uri":"spotify:track:1pKYYY0dkg23sQQXi0Q5zN"},
            {"uri":"spotify:episode:1pKYYY0dkg23sQQXi0Q5zN"},
            {"uri":"spotify:local:a:b:c:10"},
            {"uid":"x"}
        ],"next_page_url":"hm://next"}"#;
        let (ids, next) = parse_json_page(page).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].to_base62(), "1pKYYY0dkg23sQQXi0Q5zN");
        assert_eq!(next.as_deref(), Some("hm://next"));
        let (ids, next) = parse_json_page(br#"{"tracks":[],"nextPageUrl":""}"#).unwrap();
        assert!(ids.is_empty());
        assert_eq!(next, None);
        assert!(matches!(
            parse_json_page(b"<html>"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn context_tracks_take_uris_or_raw_ids() {
        let mut by_uri = ContextTrack::new();
        by_uri.uri = Some("spotify:track:1pKYYY0dkg23sQQXi0Q5zN".into());
        let id = SpotifyId::from_base62("4u7EnebtmKWzUH433cf5Qv").unwrap();
        let mut by_gid = ContextTrack::new();
        by_gid.gid = Some(id.to_raw().to_vec());
        let mut episode = ContextTrack::new();
        episode.uri = Some("spotify:episode:1pKYYY0dkg23sQQXi0Q5zN".into());
        let mut page = ContextPage::new();
        page.tracks = vec![by_uri, episode, by_gid, ContextTrack::new()];
        let got = context_tracks(&[page]);
        assert_eq!(
            got.iter().map(SpotifyId::to_base62).collect::<Vec<_>>(),
            ["1pKYYY0dkg23sQQXi0Q5zN", "4u7EnebtmKWzUH433cf5Qv"]
        );
    }

    #[test]
    fn librespot_errors_map_to_provider_errors() {
        let e = |kind| Error {
            kind,
            error: Box::new(io::Error::other("boom")),
        };
        assert!(matches!(
            api_error("x", &e(ErrorKind::Unavailable)),
            ProviderError::Network(_)
        ));
        assert!(matches!(
            api_error("x", &e(ErrorKind::Unauthenticated)),
            ProviderError::SignInRequired(_)
        ));
        assert!(matches!(
            api_error("x", &e(ErrorKind::NotFound)),
            ProviderError::Unplayable(_)
        ));
        assert!(matches!(
            connect_error(&e(ErrorKind::PermissionDenied)),
            ProviderError::SignInRequired(_)
        ));
        assert!(matches!(
            connect_error(&e(ErrorKind::Unavailable)),
            ProviderError::Network(_)
        ));
    }
}
