//! YouTube Music provider: Innertube client, parsers and ranged byte source.

mod clients;
mod innertube;
mod log;
mod parse;
mod range;
mod transport;

#[cfg(test)]
mod live_tests;

use std::sync::Arc;

use requiem_core::{AudioSource, Provider, ProviderError, ProviderId, Track, TrackId};

use innertube::{ClientRefresher, InnertubeClient};
use parse::{PROVIDER, StreamFormat};
use range::{HttpRangeSource, RangeConfig};
use transport::UreqTransport;

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
    prefer_aac: bool,
}

impl YtMusicProvider {
    pub fn new(opts: YtMusicOptions) -> YtMusicProvider {
        Self {
            client: Arc::new(InnertubeClient::new(
                Arc::new(UreqTransport::new()),
                opts.ip_family,
            )),
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
}
