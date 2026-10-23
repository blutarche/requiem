//! Innertube client constants. When YouTube kills the client below, the next working one
//! is in yt-dlp's `yt_dlp/extractor/youtube/_base.py` `INNERTUBE_CLIENTS`.

/// WEB_REMIX: music.youtube.com web client, used for search and for visitorData.
pub(crate) const MUSIC_ORIGIN: &str = "https://music.youtube.com";
pub(crate) const MUSIC_CLIENT_NAME: &str = "WEB_REMIX";
pub(crate) const MUSIC_CLIENT_ID: &str = "67";
pub(crate) const MUSIC_CLIENT_VERSION: &str = "1.20260707.12.00";
pub(crate) const MUSIC_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
/// Songs-only filter for `search`.
pub(crate) const SONGS_PARAMS: &str = "EgWKAQIIAWoQEAMQBBAJEAoQBRAREBAQFQ==";
/// The web client's Albums and Featured playlists chips, as a search response lists them
/// (2026-10-23), URL-decoded.
pub(crate) const ALBUMS_PARAMS: &str = "EgWKAQIYAWoMEA4QChADEAQQCRAF";
pub(crate) const PLAYLISTS_PARAMS: &str = "EgeKAQQoADgBagwQDhAKEAMQBBAJEAU=";
/// The web client's Artists chip, from an unfiltered search for "daft punk"
/// (`chipCloudRenderer` chip "Artists", 2026-10-23), URL-decoded.
pub(crate) const ARTISTS_PARAMS: &str = "EgWKAQIgAWoSEAUQCRAKEAMQBBAQEA4QFRAR";

/// VISIONOS: the only stream client that ships. Its URLs serve ranged GETs to EOF.
pub(crate) const STREAM_ORIGIN: &str = "https://www.youtube.com";
pub(crate) const STREAM_CLIENT_NAME: &str = "VISIONOS";
pub(crate) const STREAM_CLIENT_ID: &str = "101";
pub(crate) const STREAM_CLIENT_VERSION: &str = "1.02";
pub(crate) const STREAM_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
pub(crate) const STREAM_DEVICE_MAKE: &str = "Apple";
pub(crate) const STREAM_DEVICE_MODEL: &str = "RealityDevice17,1";
pub(crate) const STREAM_OS_NAME: &str = "visionOS";
pub(crate) const STREAM_OS_VERSION: &str = "26.5.23O471";
