//! Blocking HTTP over one shared ureq agent.

use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use requiem_core::ProviderError;
use ureq::Agent;
use ureq::config::IpFamily as UreqIpFamily;

use crate::IpFamily;
use crate::clients::{MUSIC_UA, STREAM_UA};
use crate::range::{FetchError, RangeResponse};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Innertube bodies are ~100-200 KB; anything near this is not an API response.
const MAX_API_BODY: u64 = 8 * 1024 * 1024;

pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(crate) trait Transport: Send + Sync {
    /// Statuses are returned as values; only transport failures are errors.
    fn post_json(
        &self,
        family: IpFamily,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError>;

    /// GET `url` with `Range: bytes=start-end_inclusive` on `family`.
    fn fetch_on(
        &self,
        family: IpFamily,
        url: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<RangeResponse, FetchError>;

    /// Drops pooled connections (fresh agents).
    fn reset(&self);
}

/// Google's OAuth and YouTube Data API endpoints.
pub(crate) trait ApiHttp: Send + Sync {
    /// GET `url` with `query` percent-encoded onto it. Statuses are returned as values.
    fn get(
        &self,
        url: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, ProviderError>;

    /// POST `form` url-encoded. Secrets belong here and never in a URL, which transport
    /// errors may quote.
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<HttpResponse, ProviderError>;
}

/// One agent per family, built lazily: ureq fixes the family at agent construction.
pub(crate) struct UreqTransport {
    agents: Mutex<[Option<Agent>; 3]>,
}

fn slot(family: IpFamily) -> usize {
    match family {
        IpFamily::Any => 0,
        IpFamily::V4 => 1,
        IpFamily::V6 => 2,
    }
}

fn build_agent(ip_family: IpFamily) -> Agent {
    Agent::config_builder()
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(IO_TIMEOUT))
        .timeout_recv_response(Some(IO_TIMEOUT))
        .timeout_recv_body(Some(IO_TIMEOUT))
        .ip_family(match ip_family {
            IpFamily::Any => UreqIpFamily::Any,
            IpFamily::V4 => UreqIpFamily::Ipv4Only,
            IpFamily::V6 => UreqIpFamily::Ipv6Only,
        })
        .http_status_as_error(false)
        .user_agent(MUSIC_UA)
        .build()
        .into()
}

impl UreqTransport {
    pub(crate) fn new() -> Self {
        Self {
            agents: Mutex::new([None, None, None]),
        }
    }

    fn agent(&self, family: IpFamily) -> Agent {
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        agents[slot(family)]
            .get_or_insert_with(|| build_agent(family))
            .clone()
    }
}

fn read_limited(body: ureq::Body, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    body.into_reader().take(limit).read_to_end(&mut out)?;
    Ok(out)
}

impl Transport for UreqTransport {
    fn post_json(
        &self,
        family: IpFamily,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError> {
        let mut req = self
            .agent(family)
            .post(url)
            .header("Content-Type", "application/json");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req
            .send(body)
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = read_limited(resp.into_body(), MAX_API_BODY)
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }

    fn fetch_on(
        &self,
        family: IpFamily,
        url: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<RangeResponse, FetchError> {
        let transient = |e: &dyn std::fmt::Display| FetchError::Transient(e.to_string());
        let resp = self
            .agent(family)
            .get(url)
            .header("Range", format!("bytes={start}-{end_inclusive}"))
            .header("User-Agent", STREAM_UA)
            .call()
            .map_err(|e| transient(&e))?;
        let status = resp.status().as_u16();
        let content_range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // One byte over the request lets validation see an oversized body.
        let want = end_inclusive - start + 1;
        let body = read_limited(resp.into_body(), want + 1).map_err(|e| transient(&e))?;
        Ok(RangeResponse {
            status,
            content_range,
            body,
        })
    }

    fn reset(&self) {
        *self.agents.lock().unwrap_or_else(|e| e.into_inner()) = [None, None, None];
    }
}

fn api_response(
    resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<HttpResponse, ProviderError> {
    let resp = resp.map_err(|e| ProviderError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let body = read_limited(resp.into_body(), MAX_API_BODY)
        .map_err(|e| ProviderError::Network(e.to_string()))?;
    Ok(HttpResponse { status, body })
}

impl ApiHttp for UreqTransport {
    fn get(
        &self,
        url: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, ProviderError> {
        let mut req = self
            .agent(IpFamily::Any)
            .get(url)
            .query_pairs(query.iter().copied());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        api_response(req.call())
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<HttpResponse, ProviderError> {
        api_response(
            self.agent(IpFamily::Any)
                .post(url)
                .send_form(form.iter().copied()),
        )
    }
}
