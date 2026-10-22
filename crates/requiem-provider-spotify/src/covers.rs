//! Covers for the user's own playlists. Spotify's rootlist and the playlists themselves carry no
//! picture for them, so the cover is built the way Spotify's clients do: a mosaic of the covers
//! of the playlist's first four albums.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use librespot_core::SpotifyId;
use tokio::sync::{OnceCell, Semaphore, watch};

use crate::backend::{IMAGE_URL, METADATA_IN_FLIGHT};

const MOSAIC_URL: &str = "https://mosaic.scdn.co/640/";
/// The covers a mosaic is made of.
pub(crate) const MOSAIC_COVERS: usize = 4;
/// How many of a playlist's first tracks are read for albums at most.
pub(crate) const COVER_TRACKS: usize = 12;

/// The cover of the album a track is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlbumCover {
    pub(crate) album: String,
    /// The image's 40 hex digits.
    pub(crate) image: String,
}

/// A playlist's cover: its own `art` if it has one, else a mosaic of its first four distinct
/// albums' covers, else the first track's album cover, else none.
pub(crate) fn cover_choice(art: Option<String>, albums: &[AlbumCover]) -> Option<String> {
    if art.is_some() {
        return art;
    }
    let mut seen = HashSet::new();
    let distinct: Vec<&AlbumCover> = albums
        .iter()
        .filter(|a| seen.insert(a.album.as_str()))
        .collect();
    match distinct.first() {
        None => None,
        Some(_) if distinct.len() >= MOSAIC_COVERS => {
            let ids: String = distinct[..MOSAIC_COVERS]
                .iter()
                .map(|a| a.image.as_str())
                .collect();
            Some(format!("{MOSAIC_URL}{ids}"))
        }
        Some(first) => Some(format!("{IMAGE_URL}{}", first.image)),
    }
}

/// What fetching a whole playlist tells of it, for a rootlist row that left it incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Whole {
    Found {
        name: Option<String>,
        length: Option<i32>,
        art: Option<String>,
        /// Its first tracks, for a cover.
        first: Vec<SpotifyId>,
    },
    /// Spotify has no such playlist.
    NotFound,
}

/// Answers kept per playlist for the revision they were found at; an entry is replaced when
/// the playlist changes. Lookups of one playlist at one revision share one `find`.
struct Memo<V> {
    state: Mutex<MemoState<V>>,
}

/// One lookup's answer, which every lookup of the same playlist and revision waits on.
type Flight<V> = Arc<OnceCell<Option<V>>>;

struct MemoState<V> {
    found: HashMap<SpotifyId, (Vec<u8>, V)>,
    in_flight: HashMap<(SpotifyId, Vec<u8>), Flight<V>>,
}

impl<V: Clone> Memo<V> {
    fn new() -> Self {
        Self {
            state: Mutex::new(MemoState {
                found: HashMap::new(),
                in_flight: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MemoState<V>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn kept(&self, id: SpotifyId, revision: &[u8]) -> Option<V> {
        self.lock()
            .found
            .get(&id)
            .filter(|(r, _)| r == revision)
            .map(|(_, v)| v.clone())
    }

    fn running(&self, id: SpotifyId, revision: &[u8]) -> bool {
        self.lock().in_flight.contains_key(&(id, revision.to_vec()))
    }

    /// The answer kept, else what `find` makes of it: `None` when it couldn't tell, which is
    /// not kept. Nor is anything found at an empty revision, which can't tell a changed
    /// playlist from the one it was.
    async fn resolve<F>(
        &self,
        id: SpotifyId,
        revision: &[u8],
        find: impl FnOnce() -> F,
    ) -> Option<V>
    where
        F: Future<Output = Option<V>>,
    {
        if let Some(v) = self.kept(id, revision) {
            return Some(v);
        }
        let key = (id, revision.to_vec());
        let cell = self
            .lock()
            .in_flight
            .entry(key.clone())
            .or_default()
            .clone();
        let got = cell
            .get_or_init(|| async {
                let got = find().await;
                if let Some(v) = &got
                    && !revision.is_empty()
                {
                    self.lock().found.insert(id, (revision.to_vec(), v.clone()));
                }
                got
            })
            .await
            .clone();
        // Kept before it leaves the flight, so a lookup between the two finds it either way.
        let mut st = self.lock();
        if st
            .in_flight
            .get(&key)
            .is_some_and(|c| Arc::ptr_eq(c, &cell))
        {
            st.in_flight.remove(&key);
        }
        got
    }
}

/// Covers and whole playlists found so far, kept for the player's lifetime, and the lookups
/// still running for them in the background.
pub(crate) struct Covers {
    covers: Memo<Option<String>>,
    wholes: Memo<Whole>,
    /// Caps the requests made for covers, whatever number of playlists look for one.
    pub(crate) permits: Semaphore,
    settle: Mutex<Settle>,
    /// Counts the batches of background lookups that found something an answer would show.
    changes: watch::Sender<u64>,
}

#[derive(Default)]
struct Settle {
    running: usize,
    changed: bool,
}

/// A background lookup running; when the last one ends, a change any of them made is told.
struct Busy(Arc<Covers>);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut st = self.0.settle();
        st.running -= 1;
        self.0.tell_if_settled(&mut st);
    }
}

/// How a whole playlist fetched for an answer turned out by its deadline.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InTime {
    /// `None` when the fetch failed.
    Got(Option<Whole>),
    /// Still running; it goes on in the background.
    Late,
}

impl Covers {
    pub(crate) fn new() -> Self {
        Self {
            covers: Memo::new(),
            wholes: Memo::new(),
            permits: Semaphore::new(METADATA_IN_FLIGHT),
            settle: Mutex::new(Settle::default()),
            changes: watch::Sender::new(0),
        }
    }

    fn settle(&self) -> MutexGuard<'_, Settle> {
        self.settle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tell_if_settled(&self, st: &mut Settle) {
        if st.running == 0 && st.changed {
            st.changed = false;
            self.changes.send_modify(|n| *n += 1);
        }
    }

    fn busy(self: &Arc<Self>) -> Busy {
        self.settle().running += 1;
        Busy(self.clone())
    }

    /// Something a fresh answer would show was found.
    fn mark_changed(&self) {
        let mut st = self.settle();
        st.changed = true;
        self.tell_if_settled(&mut st);
    }

    /// Changes once each time background lookups have all ended having found something.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The cover of playlist `id` at `revision`: the one kept, else what `find` makes of it.
    /// `find` answers `None` when it couldn't tell, which is not kept, and `Some(None)` for a
    /// playlist that has no cover to be had, which is.
    pub(crate) async fn resolve<F>(
        &self,
        id: SpotifyId,
        revision: &[u8],
        find: impl FnOnce() -> F,
    ) -> Option<String>
    where
        F: Future<Output = Option<Option<String>>>,
    {
        self.covers.resolve(id, revision, find).await.flatten()
    }

    /// The cover kept for playlist `id` at `revision`, without waiting: on a miss, `find` runs
    /// in the background, unless a lookup already does, and a cover it finds is told through
    /// [`Self::subscribe`]. At an empty revision there is no looking, as nothing found is kept.
    pub(crate) fn cover_now<F>(
        self: &Arc<Self>,
        id: SpotifyId,
        revision: &[u8],
        find: impl FnOnce() -> F + Send + 'static,
    ) -> Option<String>
    where
        F: Future<Output = Option<Option<String>>> + Send + 'static,
    {
        if revision.is_empty() {
            return None;
        }
        if let Some(cover) = self.covers.kept(id, revision) {
            return cover;
        }
        if self.covers.running(id, revision) {
            return None;
        }
        let busy = self.busy();
        let covers = self.clone();
        let revision = revision.to_vec();
        tokio::spawn(async move {
            let _busy = busy;
            if covers.resolve(id, &revision, find).await.is_some() {
                covers.mark_changed();
            }
        });
        None
    }

    /// Playlist `id` at `revision` whole: the one kept, else what `fetch` gets by `until`. A
    /// fetch still running then goes on in the background, and once it lands that is told
    /// through [`Self::subscribe`].
    pub(crate) async fn whole_by<F>(
        self: &Arc<Self>,
        id: SpotifyId,
        revision: &[u8],
        until: tokio::time::Instant,
        fetch: impl FnOnce() -> F + Send + 'static,
    ) -> InTime
    where
        F: Future<Output = Option<Whole>> + Send + 'static,
    {
        if let Some(whole) = self.wholes.kept(id, revision) {
            return InTime::Got(Some(whole));
        }
        let late = Arc::new(AtomicBool::new(false));
        let task = {
            let busy = self.busy();
            let covers = self.clone();
            let revision = revision.to_vec();
            let late = late.clone();
            tokio::spawn(async move {
                let _busy = busy;
                let whole = covers.wholes.resolve(id, &revision, fetch).await;
                if whole.is_some() && !revision.is_empty() && late.load(Ordering::SeqCst) {
                    covers.mark_changed();
                }
                whole
            })
        };
        match tokio::time::timeout_at(until, task).await {
            Ok(whole) => InTime::Got(whole.ok().flatten()),
            Err(_) => {
                late.store(true, Ordering::SeqCst);
                // Landed between the deadline and the flag: the task didn't see it.
                if self.wholes.kept(id, revision).is_some() {
                    self.mark_changed();
                }
                InTime::Late
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use super::*;

    fn album(n: u8) -> AlbumCover {
        AlbumCover {
            album: format!("album{n}"),
            image: format!("{n:02x}").repeat(20),
        }
    }

    #[test]
    fn a_playlists_own_picture_wins() {
        let albums: Vec<_> = (1..=5).map(album).collect();
        assert_eq!(
            cover_choice(Some("own".into()), &albums).as_deref(),
            Some("own")
        );
    }

    #[test]
    fn four_distinct_albums_make_a_mosaic() {
        // Two tracks of the first album, then more: the first four distinct ones count.
        let albums = [album(1), album(1), album(2), album(3), album(4), album(5)];
        let want = format!(
            "https://mosaic.scdn.co/640/{}{}{}{}",
            album(1).image,
            album(2).image,
            album(3).image,
            album(4).image
        );
        assert_eq!(cover_choice(None, &albums), Some(want));
    }

    #[test]
    fn fewer_than_four_albums_show_the_first_tracks_cover() {
        let albums = [album(7), album(7), album(8), album(9)];
        assert_eq!(
            cover_choice(None, &albums),
            Some(format!("https://i.scdn.co/image/{}", album(7).image))
        );
    }

    #[test]
    fn a_playlist_with_no_albums_stays_blank() {
        assert_eq!(cover_choice(None, &[]), None);
    }

    fn id(b62: &str) -> SpotifyId {
        SpotifyId::from_base62(b62).unwrap()
    }

    #[tokio::test]
    async fn a_second_load_makes_no_request() {
        let covers = Covers::new();
        let calls = AtomicUsize::new(0);
        let pl = id("37i9dQZF1DXcBWIGoYBM5M");
        let find = |cover: Option<&str>| {
            calls.fetch_add(1, Ordering::SeqCst);
            let cover = cover.map(str::to_owned);
            async move { Some(cover) }
        };
        let first = covers.resolve(pl, b"r1", || find(Some("art"))).await;
        let second = covers.resolve(pl, b"r1", || find(Some("other"))).await;
        assert_eq!(
            (first.as_deref(), second.as_deref()),
            (Some("art"), Some("art"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A changed playlist is looked at again, and the new answer replaces the old.
        let third = covers.resolve(pl, b"r2", || find(Some("new"))).await;
        let fourth = covers.resolve(pl, b"r2", || find(Some("other"))).await;
        assert_eq!(
            (third.as_deref(), fourth.as_deref()),
            (Some("new"), Some("new"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn two_lookups_of_one_playlist_at_once_find_it_once() {
        let covers = Covers::new();
        let calls = AtomicUsize::new(0);
        let pl = id("37i9dQZF1DXcBWIGoYBM5M");
        let gate = tokio::sync::Semaphore::new(0);
        let find = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                let _open = gate.acquire().await;
                Some(Some("art".to_owned()))
            }
        };
        let release = async {
            tokio::task::yield_now().await;
            gate.add_permits(2);
        };
        let (first, second, ()) = tokio::join!(
            covers.resolve(pl, b"r1", find),
            covers.resolve(pl, b"r1", find),
            release
        );
        assert_eq!(
            (first.as_deref(), second.as_deref()),
            (Some("art"), Some("art"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nothing_found_at_an_empty_revision_is_kept() {
        let covers = Arc::new(Covers::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let pl = id("37i9dQZF1DXcBWIGoYBM5M");
        for _ in 0..2 {
            let got = covers
                .resolve(pl, b"", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Some(Some("art".to_owned()))
                })
                .await;
            assert_eq!(got.as_deref(), Some("art"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Nor is one looked for without waiting, as it would be looked for on every answer.
        let counted = calls.clone();
        let got = covers.cover_now(pl, b"", move || async move {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(Some("art".to_owned()))
        });
        assert_eq!(got, None);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_answer_never_waits_for_a_cover_nor_looks_for_it_twice() {
        let covers = Arc::new(Covers::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let pl = id("37i9dQZF1DXcBWIGoYBM5M");
        for _ in 0..2 {
            let counted = calls.clone();
            let got = covers.cover_now(pl, b"r1", move || {
                counted.fetch_add(1, Ordering::SeqCst);
                std::future::pending()
            });
            assert_eq!(got, None);
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn covers_found_in_the_background_are_kept_and_told_once_they_all_land() {
        let covers = Arc::new(Covers::new());
        let mut changes = covers.subscribe();
        let gate = Arc::new(Semaphore::new(0));
        let (a, b) = (id("37i9dQZF1DXcBWIGoYBM5M"), id("1pKYYY0dkg23sQQXi0Q5zN"));
        for (pl, art) in [(a, "art-a"), (b, "art-b")] {
            let gate = gate.clone();
            let got = covers.cover_now(pl, b"r1", move || async move {
                gate.acquire().await.unwrap().forget();
                Some(Some(art.to_owned()))
            });
            assert_eq!(got, None);
        }
        gate.add_permits(1);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!changes.has_changed().unwrap(), "told before both landed");
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .unwrap()
            .unwrap();
        let never = || async { unreachable!("looked for a kept cover") };
        assert_eq!(covers.cover_now(a, b"r1", never).as_deref(), Some("art-a"));
        assert_eq!(covers.cover_now(b, b"r1", never).as_deref(), Some("art-b"));
        assert!(!changes.has_changed().unwrap());
    }

    fn found(name: &str) -> Whole {
        Whole::Found {
            name: Some(name.into()),
            length: Some(3),
            art: None,
            first: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_whole_playlist_late_for_an_answer_is_told_once_it_lands() {
        let covers = Arc::new(Covers::new());
        let mut changes = covers.subscribe();
        let pl = id("37i9dQZF1DXcBWIGoYBM5M");
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        let got = covers
            .whole_by(pl, b"r1", until, || async {
                tokio::time::sleep(Duration::from_secs(8)).await;
                Some(found("Late"))
            })
            .await;
        assert_eq!(got, InTime::Late);
        assert!(!changes.has_changed().unwrap());
        changes.changed().await.unwrap();
        let never = || async { unreachable!("fetched a kept playlist") };
        let until = tokio::time::Instant::now();
        assert_eq!(
            covers.whole_by(pl, b"r1", until, never).await,
            InTime::Got(Some(found("Late")))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_whole_playlist_in_time_is_no_change() {
        let covers = Arc::new(Covers::new());
        let changes = covers.subscribe();
        let pl = id("37i9dQZF1DXcBWIGoYBM5M");
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        let got = covers
            .whole_by(pl, b"r1", until, || async { Some(found("Soon")) })
            .await;
        assert_eq!(got, InTime::Got(Some(found("Soon"))));
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn an_empty_playlist_is_kept_blank_and_a_failure_is_tried_again() {
        let covers = Covers::new();
        let calls = AtomicUsize::new(0);
        let empty = id("37i9dQZF1DXcBWIGoYBM5M");
        let flaky = id("1pKYYY0dkg23sQQXi0Q5zN");
        for _ in 0..2 {
            let got = covers
                .resolve(empty, b"r", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Some(None)
                })
                .await;
            assert_eq!(got, None);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for _ in 0..2 {
            let got = covers
                .resolve(flaky, b"r", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    None
                })
                .await;
            assert_eq!(got, None);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
