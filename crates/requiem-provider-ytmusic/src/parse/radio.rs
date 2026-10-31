//! The `next` endpoint's queue for a song's radio (`RDAMVM` + its video id).

use requiem_core::{ProviderError, ProviderId, Track, TrackId};
use serde::Deserialize;
use serde_json::Value;

use super::{PROVIDER, Runs, ThumbnailList, parse_duration, parse_err, parse_subtitle};

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PanelVideo {
    video_id: Option<String>,
    title: Option<Runs>,
    long_byline_text: Option<Runs>,
    length_text: Option<Runs>,
    thumbnail: Option<ThumbnailList>,
}

const QUEUE: &str = "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs/0/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer/contents";

/// The radio's tracks in order, without `seed`, which the queue starts with.
pub(crate) fn parse_radio(body: &[u8], seed: &str) -> Result<Vec<Track>, ProviderError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|e| parse_err(format!("radio json: {e}")))?;
    let items = root
        .pointer(QUEUE)
        .and_then(Value::as_array)
        .ok_or_else(|| parse_err("radio response has no queue"))?;
    let tracks = items
        .iter()
        .filter_map(|item| PanelVideo::deserialize(item.get("playlistPanelVideoRenderer")?).ok())
        .filter_map(|v| {
            let video_id = v.video_id.filter(|id| !id.is_empty() && id != seed)?;
            let title: String = v
                .title
                .iter()
                .flat_map(|t| &t.runs)
                .filter_map(|r| r.text.as_deref())
                .collect();
            let sub = parse_subtitle(v.long_byline_text.iter().flat_map(|t| &t.runs));
            let duration = v
                .length_text
                .iter()
                .flat_map(|t| &t.runs)
                .find_map(|r| parse_duration(r.text.as_deref()?));
            let artwork_url = v
                .thumbnail
                .iter()
                .flat_map(|t| &t.thumbnails)
                .filter(|t| t.url.is_some())
                .max_by_key(|t| u64::from(t.width.unwrap_or(0)) * u64::from(t.height.unwrap_or(0)))
                .and_then(|t| t.url.clone());
            Some(Track {
                id: TrackId {
                    provider: ProviderId(PROVIDER.to_owned()),
                    id: video_id,
                },
                title,
                artists: sub.artists,
                artist_ids: sub.artist_ids,
                album: sub.album,
                duration: duration.or(sub.duration),
                artwork_url,
            })
        })
        .collect();
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use requiem_core::CollectionId;

    use super::*;

    // Live anonymous capture of the radio of Daft Punk's "Around the World" (Jb6gcoR266U),
    // 2026-10-31: its first five of fifty, trimmed.
    const RADIO: &str = include_str!("../../tests/fixtures/next_radio.json");

    #[test]
    fn a_radio_lists_its_songs_without_the_seed() {
        let tracks = parse_radio(RADIO.as_bytes(), "Jb6gcoR266U").unwrap();
        let ids: Vec<_> = tracks.iter().map(|t| t.id.id.as_str()).collect();
        assert_eq!(
            ids,
            ["JhulBGMA7G4", "Mnu5-meFyfI", "MR3uP7IYz44", "NxxjLD2pmlk"]
        );
        let t = &tracks[0];
        assert_eq!(t.id.provider.0, PROVIDER);
        assert_eq!(t.title, "Harder, Better, Faster, Stronger");
        assert_eq!(t.artists, ["Daft Punk"]);
        assert_eq!(
            t.artist_ids,
            [Some(CollectionId {
                provider: ProviderId(PROVIDER.to_owned()),
                id: "browse:UCRr1xG_2WIDs18a6cIiCxeA".into(),
            })]
        );
        assert_eq!(t.album.as_deref(), Some("Discovery"));
        assert_eq!(t.duration, Some(Duration::from_secs(3 * 60 + 47)));
        assert!(
            t.artwork_url
                .as_deref()
                .is_some_and(|u| u.contains("w544-h544")),
            "{:?}",
            t.artwork_url
        );
        // Two linked artists, joined by an unlinked " & ".
        assert_eq!(tracks[1].artists, ["Aaron Smith", "Krono"]);
        assert_eq!(tracks[1].album.as_deref(), Some("Dancin (Krono Remix)"));
    }

    #[test]
    fn the_seed_is_kept_when_another_track_is_asked_about() {
        let tracks = parse_radio(RADIO.as_bytes(), "other").unwrap();
        assert_eq!(tracks.len(), 5);
        assert_eq!(tracks[0].title, "Around the World");
    }

    #[test]
    fn a_response_without_a_queue_is_a_parse_error() {
        assert!(matches!(
            parse_radio(br#"{"contents":{}}"#, "x"),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            parse_radio(b"not json", "x"),
            Err(ProviderError::Parse(_))
        ));
    }
}
