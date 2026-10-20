//! Live smoke tests against YouTube. Run with `-- --ignored live_ --test-threads=1`.
//! `live_home` and `live_library` need a signed-in account: point `REQUIEM_GOOGLE_CLIENT_FILE` and
//! `REQUIEM_GOOGLE_TOKEN_FILE` at the client JSON and at the token a `signin` saved.

use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use requiem_core::{AccountState, FormatHint, Item, Page, Provider, Shelf};

use crate::{IpFamily, YtMusicOptions, YtMusicProvider};

const FAMILIES: [IpFamily; 2] = [IpFamily::V4, IpFamily::V6];
const QUERY: &str = "daft punk around the world";

fn provider(prefer_aac: bool, ip_family: IpFamily) -> YtMusicProvider {
    YtMusicProvider::new(YtMusicOptions {
        prefer_aac,
        ip_family,
    })
}

#[test]
#[ignore]
fn live_search() {
    for family in FAMILIES {
        let tracks = provider(false, family).search(QUERY, 20).unwrap();
        println!("{family:?}: {} tracks", tracks.len());
        assert!(tracks.len() >= 5);
        for t in &tracks {
            assert!(
                !t.id.id.is_empty() && !t.title.is_empty() && t.duration.is_some(),
                "{t:?}"
            );
        }
    }
}

fn stream(prefer_aac: bool, hint: FormatHint) {
    for family in FAMILIES {
        let p = provider(prefer_aac, family);
        let track = p.search(QUERY, 5).unwrap().remove(0);
        let (format, mut src) = p.open(&track.id.id).unwrap();
        assert_eq!(format.hint, hint, "{family:?}");

        let t0 = Instant::now();
        let mut first = [0u8; 1];
        src.read_exact(&mut first).unwrap();
        let ttfb = t0.elapsed();

        let start = Instant::now();
        let mut total = 1u64;
        let mut buf = vec![0u8; 64 * 1024];
        while start.elapsed() < Duration::from_secs(10) {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        let rate = total as f64 / start.elapsed().as_secs_f64();
        let floor = 4.0 * format.average_bitrate.unwrap_or(0) as f64 / 8.0;
        println!(
            "{family:?} itag {}: ttfb {ttfb:?}, {total} bytes, {rate:.0} B/s (floor {floor:.0})",
            format.itag
        );
        assert!(rate >= floor, "{family:?}: {rate} < {floor}");

        let mid = format.content_length / 2;
        src.seek(SeekFrom::Start(mid)).unwrap();
        let mut got = 0;
        let mut chunk = vec![0u8; 16 * 1024];
        while got < 256 * 1024 {
            let n = src.read(&mut chunk).unwrap();
            assert!(n > 0, "{family:?}: EOF mid-stream");
            got += n;
        }
    }
}

#[test]
#[ignore]
fn live_stream_opus() {
    stream(false, FormatHint::WebmOpus);
}

#[test]
#[ignore]
fn live_stream_aac() {
    stream(true, FormatHint::Mp4Aac);
}

#[test]
#[ignore]
fn live_any_family_falls_back() {
    let p = provider(false, IpFamily::Any);
    let track = p.search(QUERY, 5).unwrap().remove(0);
    let (format, mut src) = p.open(&track.id.id).unwrap();
    let mut got = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    while got < 2 * 1024 * 1024 {
        let n = src.read(&mut buf).unwrap();
        assert!(n > 0, "EOF after {got} bytes");
        got += n;
    }
    println!("Any: itag {}, read {got} bytes", format.itag);
}

fn first_collection(shelves: &[Shelf]) -> Option<Page> {
    shelves.iter().flat_map(|s| &s.items).find_map(|i| match i {
        Item::Collection(c) => Some(Page::Collection(c.id.clone())),
        Item::Track(_) => None,
    })
}

fn summary(shelves: &[Shelf]) -> Vec<(String, usize)> {
    shelves
        .iter()
        .map(|s| (s.title.clone(), s.items.len()))
        .collect()
}

#[test]
#[ignore]
fn live_home() {
    let p = provider(false, IpFamily::Any);
    assert!(matches!(p.account(), AccountState::SignedIn { .. }));
    let home = p.browse(&Page::Home).unwrap();
    println!("home: {:?}", summary(&home));
    let titles: Vec<&str> = home.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Liked songs", "Your playlists"]);
    assert!(home[0].items.len() <= 20);
    assert!(home[0].items.iter().all(|i| matches!(i, Item::Track(_))));
    assert!(
        home[1]
            .items
            .iter()
            .all(|i| matches!(i, Item::Collection(_)))
    );
    let Some(Item::Collection(liked)) = home[1].items.first() else {
        panic!("no Liked videos first");
    };
    assert_eq!(liked.title, "Liked videos");
}

#[test]
#[ignore]
fn live_library() {
    let p = provider(false, IpFamily::Any);
    let account = p.account();
    println!("account: {account:?}");
    assert!(matches!(account, AccountState::SignedIn { .. }));
    let library = p.browse(&Page::Library).unwrap();
    println!("library: {:?}", summary(&library));
    let page = first_collection(&library).expect("liked videos");
    let liked = p.browse(&page).unwrap();
    println!("{page:?}: {:?}", summary(&liked));
    for item in liked.iter().flat_map(|s| &s.items).take(3) {
        let Item::Track(t) = item else {
            panic!("{item:?}")
        };
        assert!(!t.title.is_empty() && !t.artists.is_empty(), "{t:?}");
    }
}
