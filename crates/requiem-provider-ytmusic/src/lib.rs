//! YouTube Music provider: Innertube client, parsers and ranged byte source, and the
//! signed-in library through Google sign-in and the YouTube Data API.

mod clients;
mod dataapi;
mod google;
mod innertube;
mod log;
mod parse;
mod range;
mod transport;

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod testing;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use requiem_core::{
    AccountState, AudioSource, Page, Provider, ProviderError, ProviderId, Shelf, Track, TrackId,
};

use dataapi::DATA_PREFIX;
use google::{Google, GoogleFiles, SystemClock};
use innertube::{ClientRefresher, InnertubeClient};
use parse::{BROWSE_PREFIX, PROVIDER, StreamFormat};
use range::{HttpRangeSource, RangeConfig};
use transport::UreqTransport;

const HOME: &str = "FEmusic_home";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IpFamily {
    #[default]
    Any,
    V4,
    V6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct YtMusicOptions {
    /// Prefer AAC (itag 140) over Opus.
    pub prefer_aac: bool,
    pub ip_family: IpFamily,
}

pub struct YtMusicProvider {
    client: Arc<InnertubeClient>,
    google: Google,
    prefer_aac: bool,
}

/// The Google sign-in file [`YtMusicProvider::new`] reads, when there is a usable location.
pub fn token_file() -> Option<PathBuf> {
    GoogleFiles::from_env().token
}

/// Parses the Google sign-in file at `path` without changing it; a missing file passes.
pub fn check_token_file(path: &Path) -> Result<(), String> {
    google::check_token(path)
}

impl YtMusicProvider {
    /// Reads the Google client and sign-in files named by `REQUIEM_GOOGLE_CLIENT_FILE` and
    /// `REQUIEM_GOOGLE_TOKEN_FILE`, or their defaults; makes no network calls.
    pub fn new(opts: YtMusicOptions) -> YtMusicProvider {
        let transport = Arc::new(UreqTransport::new());
        Self {
            client: Arc::new(InnertubeClient::new(transport.clone(), opts.ip_family)),
            google: Google::new(transport, Arc::new(SystemClock), GoogleFiles::from_env()),
            prefer_aac: opts.prefer_aac,
        }
    }

    pub(crate) fn open(
        &self,
        video_id: &str,
    ) -> Result<(StreamFormat, HttpRangeSource), ProviderError> {
        let (format, family) = self.client.resolve(video_id, self.prefer_aac)?;
        let source = HttpRangeSource::new(
            format.url.clone(),
            format.itag,
            format.content_length,
            self.client.fetcher(family),
            Arc::new(ClientRefresher {
                client: self.client.clone(),
                video_id: video_id.to_owned(),
                prefer_aac: self.prefer_aac,
                family,
            }),
            RangeConfig::default(),
        );
        Ok((format, source))
    }
}

impl Provider for YtMusicProvider {
    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER.to_owned())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        self.client.search(query, limit)
    }

    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError> {
        let (format, source) = self.open(&track.id)?;
        Ok(AudioSource::Encoded {
            bytes: Box::new(source),
            hint: format.hint,
        })
    }

    fn browse(&self, page: &Page) -> Result<Vec<Shelf>, ProviderError> {
        match page {
            Page::Home => parse::parse_home(&self.client.browse(HOME)?),
            Page::Library => dataapi::library(&self.google),
            Page::Collection(c) if c.provider.0 == PROVIDER => {
                if let Some(id) = c.id.strip_prefix(DATA_PREFIX) {
                    dataapi::playlist(&self.google, id)
                } else if let Some(id) = c.id.strip_prefix(BROWSE_PREFIX) {
                    let body = self.client.browse(id)?;
                    Ok(vec![parse::parse_collection(
                        &body,
                        parse::collection_kind(id),
                    )?])
                } else {
                    Err(ProviderError::Unplayable(format!(
                        "unknown collection {}",
                        c.id
                    )))
                }
            }
            Page::Collection(c) => Err(ProviderError::Unplayable(format!(
                "collection {} belongs to {}",
                c.id, c.provider.0
            ))),
        }
    }

    fn account(&self) -> AccountState {
        self.google.account()
    }

    fn sign_in(&self, on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        self.google.sign_in(on_state)
    }

    fn sign_out(&self) -> Result<(), ProviderError> {
        self.google.sign_out()
    }
}
