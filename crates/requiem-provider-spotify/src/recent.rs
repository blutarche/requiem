//! What Home's Listen again and Quick picks keep between fetches, so a revalidation asks
//! Spotify only for what it hasn't seen lately, and nothing while Spotify limits the rate.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use librespot_core::SpotifyId;
use requiem_core::{Collection, Track};

/// How long a Listen again card's name and art are kept.
pub(crate) const CARD_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a radio for Quick picks is kept.
pub(crate) const RADIO_TTL: Duration = Duration::from_secs(30 * 60);
/// How long the shelves ask nothing of Spotify after it limited the rate.
pub(crate) const BACKOFF: Duration = Duration::from_secs(10 * 60);

/// A Listen again card, with what a playlist's later cover lookup needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Card {
    pub(crate) collection: Collection,
    /// The playlist's revision; empty for an album.
    pub(crate) revision: Vec<u8>,
    /// The playlist's first tracks, for a cover made of their albums'.
    pub(crate) first: Vec<SpotifyId>,
}

/// Home's shelves of what was played lately.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Shelves {
    pub(crate) listen_again: Vec<Collection>,
    pub(crate) quick_picks: Vec<Track>,
}

impl Shelves {
    fn is_empty(&self) -> bool {
        self.listen_again.is_empty() && self.quick_picks.is_empty()
    }
}

#[derive(Default)]
pub(crate) struct RecentCache {
    /// By the card's collection id.
    cards: HashMap<String, (Instant, Card)>,
    /// By the radio's seed track.
    radios: HashMap<SpotifyId, (Instant, Vec<Track>)>,
    limited_until: Option<Instant>,
    last_good: Option<Shelves>,
}

impl RecentCache {
    pub(crate) fn card(&self, id: &str, now: Instant) -> Option<Card> {
        let (at, card) = self.cards.get(id)?;
        fresh(*at, now, CARD_TTL).then(|| card.clone())
    }

    pub(crate) fn keep_card(&mut self, id: String, card: Card, now: Instant) {
        self.cards.retain(|_, (at, _)| fresh(*at, now, CARD_TTL));
        self.cards.insert(id, (now, card));
    }

    pub(crate) fn radio(&self, seed: SpotifyId, now: Instant) -> Option<Vec<Track>> {
        let (at, tracks) = self.radios.get(&seed)?;
        fresh(*at, now, RADIO_TTL).then(|| tracks.clone())
    }

    pub(crate) fn keep_radio(&mut self, seed: SpotifyId, tracks: Vec<Track>, now: Instant) {
        self.radios.retain(|_, (at, _)| fresh(*at, now, RADIO_TTL));
        self.radios.insert(seed, (now, tracks));
    }

    /// While backing off: the last good shelves, else none. `None` when Spotify may be asked.
    pub(crate) fn backing_off(&self, now: Instant) -> Option<Shelves> {
        let until = self.limited_until?;
        (now < until).then(|| self.last_good.clone().unwrap_or_default())
    }

    /// Starts a backoff after Spotify limited the rate; answers with the last good shelves,
    /// else `got`.
    pub(crate) fn limited(&mut self, now: Instant, got: Shelves) -> Shelves {
        self.limited_until = Some(now + BACKOFF);
        self.last_good.clone().unwrap_or(got)
    }

    /// Keeps `got` as the last good shelves unless it is empty.
    pub(crate) fn answered(&mut self, got: &Shelves) {
        if !got.is_empty() {
            self.last_good = Some(got.clone());
        }
    }
}

fn fresh(at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(at) < ttl
}

#[cfg(test)]
mod tests {
    use requiem_core::{CollectionId, CollectionKind, ProviderId, TrackId};

    use super::*;

    fn collection(title: &str) -> Collection {
        Collection {
            id: CollectionId {
                provider: ProviderId("spotify".into()),
                id: format!("playlist:{title}"),
            },
            kind: CollectionKind::Playlist,
            title: title.into(),
            subtitle: None,
            artwork_url: None,
        }
    }

    fn card(title: &str) -> Card {
        Card {
            collection: collection(title),
            revision: vec![1],
            first: Vec::new(),
        }
    }

    fn track(id: &str) -> Track {
        Track {
            id: TrackId {
                provider: ProviderId("spotify".into()),
                id: id.into(),
            },
            title: id.into(),
            artists: Vec::new(),
            album: None,
            duration: None,
            artwork_url: None,
        }
    }

    fn seed(n: u128) -> SpotifyId {
        SpotifyId::from_raw(&n.to_be_bytes()).unwrap()
    }

    #[test]
    fn a_card_is_kept_for_six_hours() {
        let t0 = Instant::now();
        let mut c = RecentCache::default();
        assert_eq!(c.card("playlist:a", t0), None);
        c.keep_card("playlist:a".into(), card("a"), t0);
        assert_eq!(c.card("playlist:a", t0 + CARD_TTL / 2), Some(card("a")));
        assert_eq!(c.card("playlist:b", t0), None);
        assert_eq!(c.card("playlist:a", t0 + CARD_TTL), None);
    }

    #[test]
    fn keeping_a_card_drops_the_expired_ones() {
        let t0 = Instant::now();
        let mut c = RecentCache::default();
        c.keep_card("playlist:a".into(), card("a"), t0);
        c.keep_card("playlist:b".into(), card("b"), t0 + CARD_TTL);
        assert_eq!(c.cards.len(), 1);
        assert!(c.card("playlist:b", t0 + CARD_TTL).is_some());
    }

    #[test]
    fn a_radio_is_kept_for_thirty_minutes_by_its_seed() {
        let t0 = Instant::now();
        let mut c = RecentCache::default();
        c.keep_radio(seed(1), vec![track("x")], t0);
        assert_eq!(c.radio(seed(1), t0 + RADIO_TTL / 2), Some(vec![track("x")]));
        assert_eq!(c.radio(seed(2), t0), None);
        assert_eq!(c.radio(seed(1), t0 + RADIO_TTL), None);
        c.keep_radio(seed(2), vec![track("y")], t0 + RADIO_TTL);
        assert_eq!(c.radios.len(), 1);
    }

    #[test]
    fn a_rate_limit_serves_the_last_good_shelves_for_ten_minutes() {
        let t0 = Instant::now();
        let mut c = RecentCache::default();
        assert_eq!(c.backing_off(t0), None);
        let good = Shelves {
            listen_again: vec![collection("a")],
            quick_picks: vec![track("x")],
        };
        c.answered(&good);
        // An empty answer is no good one.
        c.answered(&Shelves::default());
        let partial = Shelves {
            listen_again: vec![collection("b")],
            quick_picks: Vec::new(),
        };
        assert_eq!(c.limited(t0, partial), good);
        assert_eq!(c.backing_off(t0 + BACKOFF / 2), Some(good.clone()));
        assert_eq!(c.backing_off(t0 + BACKOFF), None);
    }

    #[test]
    fn a_rate_limit_without_good_shelves_serves_what_it_got_then_nothing() {
        let t0 = Instant::now();
        let mut c = RecentCache::default();
        let partial = Shelves {
            listen_again: vec![collection("b")],
            quick_picks: Vec::new(),
        };
        assert_eq!(c.limited(t0, partial.clone()), partial);
        assert_eq!(c.backing_off(t0), Some(Shelves::default()));
    }
}
