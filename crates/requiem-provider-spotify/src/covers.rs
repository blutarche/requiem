//! Covers for the user's own playlists. Spotify's rootlist and the playlists themselves carry no
//! picture for them, so the cover is built the way Spotify's clients do: a mosaic of the covers
//! of the playlist's first four albums.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use librespot_core::SpotifyId;
use tokio::sync::Semaphore;

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

/// A playlist's revision and the cover found for it, if it has one.
type Found = (Vec<u8>, Option<String>);

/// Covers found so far, kept for the player's lifetime. A playlist's entry is for the revision
/// it was found at and is replaced when the playlist changes.
pub(crate) struct Covers {
    found: Mutex<HashMap<SpotifyId, Found>>,
    /// Caps the requests made for covers, whatever number of playlists look for one.
    pub(crate) permits: Semaphore,
}

impl Covers {
    pub(crate) fn new() -> Self {
        Self {
            found: Mutex::new(HashMap::new()),
            permits: Semaphore::new(METADATA_IN_FLIGHT),
        }
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
        let kept = self
            .found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .filter(|(r, _)| r == revision)
            .map(|(_, cover)| cover.clone());
        if let Some(cover) = kept {
            return cover;
        }
        let cover = find().await?;
        self.found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, (revision.to_vec(), cover.clone()));
        cover
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

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
