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

    /// `post_json` for a request that carries the browser session: a redirect comes back as its
    /// status instead of being followed, so the session goes nowhere but `url`.
    fn post_json_signed(
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

    fn post(
        &self,
        family: IpFamily,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        follow_redirects: bool,
    ) -> Result<HttpResponse, ProviderError> {
        let mut req = self
            .agent(family)
            .post(url)
            .header("Content-Type", "application/json");
        if !follow_redirects {
            req = req.config().max_redirects(0).build();
        }
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
        self.post(family, url, headers, body, true)
    }

    fn post_json_signed(
        &self,
        family: IpFamily,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError> {
        self.post(family, url, headers, body, false)
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    use super::*;

    /// A server on loopback that answers `/start` with a redirect to `/next`, and `/next` with
    /// 200; returns its address and the paths it was asked for.
    fn redirecting() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let next = format!("{base}/next");
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                let reply = if path == "/start" {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {next}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_owned()
                };
                seen.lock().unwrap().push(path);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        (base, asked)
    }

    #[test]
    fn a_signed_request_does_not_follow_a_redirect() {
        let (base, asked) = redirecting();
        let t = UreqTransport::new();
        let url = format!("{base}/start");
        let signed = t
            .post_json_signed(IpFamily::Any, &url, &[("Cookie", "a=b")], "{}")
            .unwrap();
        assert_eq!(signed.status, 302);
        assert_eq!(*asked.lock().unwrap(), ["/start"]);
        // An anonymous request follows it, as before.
        let anonymous = t.post_json(IpFamily::Any, &url, &[], "{}").unwrap();
        assert_eq!(anonymous.status, 200);
        assert_eq!(*asked.lock().unwrap(), ["/start", "/start", "/next"]);
    }
}
