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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::audio_key::AudioKey;
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::error::ErrorKind;
use librespot_core::{Error, FileId, Session, SessionConfig, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use librespot_metadata::image::{Image, ImageSize};
use librespot_metadata::{Metadata, Track as SpotifyTrack};
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

use crate::keys::Keys;
use crate::logger::redact;
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
/// All of a Library's whole-playlist fetches; rows still waiting then keep the rootlist's
/// details, so the Library answers well within the daemon's call timeout.
const FULL_PLAYLISTS_BUDGET: Duration = Duration::from_secs(30);
/// As many as YouTube Music's provider shows.
const MAX_COLLECTION_TRACKS: usize = 1000;
/// Home's most recently liked tracks.
const HOME_LIKED: usize = 20;
/// The ids of the playlists Spotify makes start with this: its mixes, Daily Mixes, "This Is"
/// and editorial playlists alike.
const MADE_FOR_YOU: &str = "37i9dQZF1";
const IMAGE_URL: &str = "https://i.scdn.co/image/";
const LIKED: &str = "Liked Songs";
const LIKED_ID: &str = "collection";
const PLAYLIST_PREFIX: &str = "playlist:";

pub(crate) struct Librespot {
    rt: Runtime,
    creds: Option<PathBuf>,
    /// Where librespot keeps the still-encrypted download; the daemon's 0700 socket dir.
    tmp_dir: PathBuf,
    session: tokio::sync::Mutex<Option<Session>>,
    keys: Keys<(SpotifyId, FileId), AudioKey>,
    prepared: Mutex<PreparedStreams<Prep>>,
    flow: SignInFlow,
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
        items.extend(playlists(&session).await?.into_iter().map(Item::Collection));
        Ok(vec![Shelf {
            title: "Spotify".into(),
            items,
        }])
    }

    async fn home(&self) -> Result<Vec<Shelf>, ProviderError> {
        let session = self.session().await?;
        let session = &session;
        let (liked, playlists) = tokio::join!(
            async {
                let ids = context_ids(session, &liked_uri(session), HOME_LIKED).await?;
                tracks(session, ids).await
            },
            playlists(session),
        );
        Ok(home_shelves(liked?, playlists?))
    }

    async fn collection(&self, id: &str) -> Result<Vec<Shelf>, ProviderError> {
        let session = self.session().await?;
        let (uri, playlist) = if id == LIKED_ID {
            (liked_uri(&session), None)
        } else {
            let pid = id
                .strip_prefix(PLAYLIST_PREFIX)
                .and_then(|b62| SpotifyId::from_base62(b62).ok())
                .ok_or_else(|| ProviderError::Unplayable(format!("no Spotify collection {id}")))?;
            (format!("spotify:playlist:{}", pid.to_base62()), Some(pid))
        };
        let ids = context_ids(&session, &uri, MAX_COLLECTION_TRACKS).await?;
        let title = match playlist {
            None => LIKED.to_owned(),
            Some(pid) => playlist_name(&session, &pid)
                .await
                .unwrap_or_else(|| "Playlist".into()),
        };
        let tracks = tracks(&session, ids).await?;
        Ok(vec![Shelf {
            title,
            items: tracks.into_iter().map(Item::Track).collect(),
        }])
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

    fn sign_out(&self) -> Result<(), ProviderError> {
        self.flow.cancel();
        let old = self.rt.block_on(self.session.lock()).take();
        // Streams still hold their clones, so components outlive this; they just stop working.
        if let Some(s) = old {
            s.shutdown();
        }
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
/// A track without details is left out; only if every one fails is that an error.
async fn tracks(session: &Session, ids: Vec<SpotifyId>) -> Result<Vec<Track>, ProviderError> {
    let results: Vec<_> = futures_util::stream::iter(ids)
        .map(|id| async move {
            let t = SpotifyTrack::get(session, &SpotifyUri::Track { id }).await;
            (id, t)
        })
        .buffered(METADATA_IN_FLIGHT)
        .collect()
        .await;
    let mut out = Vec::with_capacity(results.len());
    let mut first_err = None;
    for (id, r) in results {
        match r {
            Ok(t) => out.push(to_track(id, t)),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) if out.is_empty() => Err(api_error("track details", &e)),
        Some(e) => {
            eprintln!("[spotify] left out tracks without details: {}", shown(&e));
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
        artists: t.artists.0.into_iter().map(|a| a.name).collect(),
        album: Some(t.album.name).filter(|n| !n.is_empty()),
        duration: u64::try_from(t.duration)
            .ok()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis),
        artwork_url,
    }
}

/// The widest image, by declared size when widths are missing.
pub(crate) fn artwork(images: &[Image]) -> Option<String> {
    let rank = |s: ImageSize| match s {
        ImageSize::SMALL => 0,
        ImageSize::DEFAULT => 1,
        ImageSize::LARGE => 2,
        ImageSize::XLARGE => 3,
    };
    images
        .iter()
        .max_by_key(|i| (i.width, rank(i.size)))
        .map(|i| format!("{IMAGE_URL}{}", i.id.to_base16()))
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
        artwork_url: None,
    }
}

/// The rootlist's playlists in order, each completed where the rootlist left it incomplete.
async fn playlists(session: &Session) -> Result<Vec<Collection>, ProviderError> {
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
    let deadline = tokio::time::Instant::now() + FULL_PLAYLISTS_BUDGET;
    let collections: Vec<_> = futures_util::stream::iter(rows)
        .map(|row| async move {
            if row.incomplete {
                completed(
                    row.collection,
                    full_playlist(session, &row.id, deadline).await,
                )
            } else {
                Some(row.collection)
            }
        })
        .buffered(METADATA_IN_FLIGHT)
        .collect()
        .await;
    Ok(collections.into_iter().flatten().collect())
}

/// Home, without empty shelves: the `liked` tracks, the playlists Spotify made, then the
/// Liked Songs card and the rest of the playlists.
fn home_shelves(liked: Vec<Track>, playlists: Vec<Collection>) -> Vec<Shelf> {
    let (made, own): (Vec<_>, Vec<_>) = playlists.into_iter().partition(|c| {
        c.id.id
            .strip_prefix(PLAYLIST_PREFIX)
            .is_some_and(|id| id.starts_with(MADE_FOR_YOU))
    });
    let own = std::iter::once(liked_card()).chain(own);
    [
        ("Liked songs", liked.into_iter().map(Item::Track).collect()),
        (
            "Made for you",
            made.into_iter().map(Item::Collection).collect(),
        ),
        ("Your playlists", own.map(Item::Collection).collect()),
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

/// A whole playlist fetched for an incomplete rootlist row.
pub(crate) enum Full {
    Playlist(Box<SelectedListContent>),
    /// Spotify has no such playlist.
    NotFound,
    /// Timed out, refused or unreadable.
    Failed,
}

/// `row` completed from `full`: its name, size and artwork where the playlist has them, else
/// the rootlist's. `None` drops a playlist that doesn't exist.
pub(crate) fn completed(mut row: Collection, full: Full) -> Option<Collection> {
    let list = match full {
        Full::Playlist(list) => list,
        Full::NotFound => return None,
        Full::Failed => return Some(row),
    };
    let attrs = list.attributes.as_ref();
    if let Some(name) = attrs.map(ListAttributes::name).filter(|n| !n.is_empty()) {
        row.title = name.to_owned();
    }
    if let Some(n) = list.length {
        row.subtitle = Some(songs(n));
    }
    if let Some(art) = attrs.and_then(playlist_artwork) {
        row.artwork_url = Some(art);
    }
    Some(row)
}

/// The whole playlist `id`, given up on at `deadline` at the latest.
async fn full_playlist(session: &Session, id: &SpotifyId, deadline: tokio::time::Instant) -> Full {
    let until = deadline.min(tokio::time::Instant::now() + FULL_PLAYLIST_TIMEOUT);
    let fetched = tokio::time::timeout_at(until, session.spclient().get_playlist(id)).await;
    let why = match fetched {
        Ok(Ok(bytes)) => match SelectedListContent::parse_from_bytes(&bytes) {
            Ok(list) => return Full::Playlist(Box::new(list)),
            Err(e) => e.to_string(),
        },
        Ok(Err(e)) if e.kind == ErrorKind::NotFound => return Full::NotFound,
        Ok(Err(e)) => shown(&e),
        Err(_) => "timed out".to_owned(),
    };
    eprintln!("[spotify] kept a playlist's rootlist details: {why}");
    Full::Failed
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

    use librespot_protocol::playlist4_external::{ListItems, MetaItem, PictureSize};

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
            Full::Playlist(whole("Hardcore Rock Mix", Some(50), Some("new"))),
        )
        .unwrap();
        assert_eq!(got.title, "Hardcore Rock Mix");
        assert_eq!(got.subtitle.as_deref(), Some("50 songs"));
        assert_eq!(got.artwork_url.as_deref(), Some("new"));

        let got = completed(
            row("Playlist", Some("30 songs"), Some("old")),
            Full::Playlist(whole("Named", None, None)),
        )
        .unwrap();
        assert_eq!(got.title, "Named");
        assert_eq!(got.subtitle.as_deref(), Some("30 songs"));
        assert_eq!(got.artwork_url.as_deref(), Some("old"));

        // The whole playlist's own 0 is real.
        let got = completed(
            row("Empty", None, None),
            Full::Playlist(whole("", Some(0), None)),
        );
        assert_eq!(got.unwrap().subtitle.as_deref(), Some("0 songs"));
    }

    #[test]
    fn a_missing_playlist_goes_and_a_failed_fetch_keeps_the_row() {
        assert_eq!(completed(row("Playlist", None, None), Full::NotFound), None);
        let kept = row("Metalcore Mix", None, Some("art"));
        assert_eq!(completed(kept.clone(), Full::Failed), Some(kept));
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
        );
        let got: Vec<_> = shelves.iter().map(|s| (&*s.title, titles(s))).collect();
        assert_eq!(
            got,
            [
                ("Liked songs", vec!["Song"]),
                ("Made for you", vec!["Daily Mix 2", "This Is Kado"]),
                (
                    "Your playlists",
                    vec!["Liked Songs", "Mine", "Discover Weekly"]
                ),
            ]
        );
    }

    #[test]
    fn home_leaves_out_empty_shelves() {
        let shelves = home_shelves(Vec::new(), vec![playlist("1Ab2", "Mine")]);
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
