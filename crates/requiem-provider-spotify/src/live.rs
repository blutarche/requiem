//! Against Spotify with the real sign-in file, by hand only. librespot runs in this process
//! behind a socket, and everything goes through the daemon's side of it. No audio output.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use requiem_core::{
    AccountState, AudioSource, ByteSource, CollectionId, FormatHint, Item, Page, Provider,
    ProviderId, TrackId,
};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::backend::Librespot;
use crate::server::{self, Backend};
use crate::testing::TempDir;
use crate::{PROVIDER_ID, SpotifyProvider};

const AROUND_THE_WORLD: &str = "1pKYYY0dkg23sQQXi0Q5zN";
/// Lists no files of its own; plays through an alternative.
const BOHEMIAN_RHAPSODY: &str = "4u7EnebtmKWzUH433cf5Qv";

struct Media(Box<dyn ByteSource>);

impl Read for Media {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for Media {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl MediaSource for Media {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.0.byte_len()
    }
}

fn decode_secs(
    fmt: &mut dyn FormatReader,
    dec: &mut dyn AudioDecoder,
    track: u32,
    secs: u64,
) -> (u64, u32) {
    let mut frames = 0u64;
    let mut rate = 0u32;
    while rate == 0 || frames < u64::from(rate) * secs {
        let packet = fmt.next_packet().unwrap().expect("ended early");
        if packet.track_id != track {
            continue;
        }
        match dec.decode(&packet) {
            Ok(buf) => {
                rate = buf.spec().rate();
                frames += buf.frames() as u64;
            }
            Err(SymError::DecodeError(_)) => {}
            Err(e) => panic!("decode: {e}"),
        }
    }
    (frames, rate)
}

/// Decodes `first` seconds, seeks to `seek_to` seconds, then decodes one more.
fn decode(src: AudioSource, first: u64, seek_to: i64) {
    let AudioSource::Encoded { bytes, hint } = src;
    assert_eq!(hint, FormatHint::OggVorbis);
    let mut codecs = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut codecs);
    let mss = MediaSourceStream::new(Box::new(Media(bytes)), MediaSourceStreamOptions::default());
    let mut h = Hint::new();
    h.with_extension("ogg");
    let t = Instant::now();
    let mut fmt = symphonia::default::get_probe()
        .probe(
            &h,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("probe");
    let track = fmt.default_track(TrackType::Audio).expect("audio track");
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .expect("audio params")
        .clone();
    let mut dec = codecs
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .expect("vorbis decoder");
    println!("  opened in {:?}", t.elapsed());
    let t = Instant::now();
    let (frames, rate) = decode_secs(&mut *fmt, &mut *dec, id, first);
    println!(
        "  decoded {frames} frames at {rate} Hz in {:?}",
        t.elapsed()
    );
    assert!(frames >= u64::from(rate) * first);
    let t = Instant::now();
    fmt.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time: Time::from_millis(seek_to * 1000),
            track_id: Some(id),
        },
    )
    .expect("seek");
    dec.reset();
    let (frames, rate) = decode_secs(&mut *fmt, &mut *dec, id, 1);
    println!(
        "  seek to {seek_to} s, then {frames} frames at {rate} Hz, in {:?}",
        t.elapsed()
    );
}

/// Run with `cargo test -p requiem-provider-spotify live -- --ignored --nocapture`. Uses the
/// sign-in file at its default place; never prints or copies it.
#[test]
#[ignore = "talks to Spotify with the real sign-in; run by hand"]
fn live() {
    let creds = crate::credentials_file().expect("no place for the sign-in file");
    assert!(creds.exists(), "sign in to Spotify first");
    let tmp = TempDir::new();
    let socket = tmp.path().join("spotify.sock");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(crate::HELPER_WORKERS)
        .enable_all()
        .build()
        .unwrap();
    let backend: Arc<dyn Backend> = Arc::new(Librespot::new(
        rt,
        Some(creds.clone()),
        tmp.path().to_owned(),
    ));
    let listener = server::bind(&socket).unwrap();
    thread::spawn(move || server::accept_loop(&listener, &backend));
    let p = SpotifyProvider::with_running_helper(socket, Some(creds));
    let spotify = ProviderId(PROVIDER_ID.into());
    assert_eq!(p.account(), AccountState::SignedIn { name: None });

    println!("1. search \"daft punk\"");
    let found = p.search("daft punk", 10).unwrap();
    assert!(!found.is_empty() && found.len() <= 10);
    for t in &found {
        println!("  {} - {}", t.title, t.artists.join(", "));
        assert_eq!(t.id.provider, spotify);
    }
    assert!(found.iter().all(|t| t.duration.is_some()));
    assert!(found.iter().any(|t| t.artwork_url.is_some()));

    println!("2. Library, then Liked Songs");
    let library = p.browse(&Page::Library).unwrap();
    assert_eq!(library.len(), 1);
    assert_eq!(library[0].title, "Spotify");
    let Some(Item::Collection(liked)) = library[0].items.first() else {
        panic!("no Liked Songs first");
    };
    assert_eq!(liked.title, "Liked Songs");
    println!("  {} collections", library[0].items.len());
    let liked = p
        .browse(&Page::Collection(CollectionId {
            provider: spotify.clone(),
            id: "collection".into(),
        }))
        .unwrap();
    assert_eq!(liked[0].title, "Liked Songs");
    assert!(!liked[0].items.is_empty());
    println!("  {} liked tracks", liked[0].items.len());

    println!("3-4. resolve, decode 10 s, seek to 60 s");
    for (id, first) in [(AROUND_THE_WORLD, 10), (BOHEMIAN_RHAPSODY, 2)] {
        println!(" {id}");
        let t = Instant::now();
        let src = p
            .resolve(&TrackId {
                provider: spotify.clone(),
                id: id.into(),
            })
            .unwrap();
        println!("  resolved in {:?}", t.elapsed());
        decode(src, first, 60);
    }
}
