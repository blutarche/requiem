//! Blocking HTTP over one shared ureq agent.

use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use requiem_core::ProviderError;
use ureq::Agent;
use ureq::config::IpFamily as UreqIpFamily;

use crate::IpFamily;
use crate::clients::{MUSIC_UA, STREAM_UA};
use crate::range::{FetchError, RangeFetcher, RangeResponse};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Innertube bodies are ~100-200 KB; anything near this is not an API response.
const MAX_API_BODY: u64 = 8 * 1024 * 1024;

pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(crate) trait Transport: RangeFetcher {
    /// Statuses are returned as values; only transport failures are errors.
    fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError>;

    /// Drops pooled connections (a fresh agent).
    fn reset(&self);
}

pub(crate) struct UreqTransport {
    agent: Mutex<Agent>,
    ip_family: IpFamily,
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
    pub(crate) fn new(ip_family: IpFamily) -> Self {
        Self {
            agent: Mutex::new(build_agent(ip_family)),
            ip_family,
        }
    }

    fn agent(&self) -> Agent {
        self.agent.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError> {
        let mut req = self
            .agent()
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

    fn reset(&self) {
        *self.agent.lock().unwrap_or_else(|e| e.into_inner()) = build_agent(self.ip_family);
    }
}

impl RangeFetcher for UreqTransport {
    fn fetch(
        &self,
        url: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<RangeResponse, FetchError> {
        let transient = |e: &dyn std::fmt::Display| FetchError::Transient(e.to_string());
        let resp = self
            .agent()
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
}
