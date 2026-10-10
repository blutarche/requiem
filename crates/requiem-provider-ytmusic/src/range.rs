//! `HttpRangeSource`: lazy, seekable bytes over ranged GETs.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use requiem_core::{ByteSource, ProviderError};

use crate::log::{debug, host_of};

pub(crate) const CHUNK_SIZE: u64 = 512 * 1024;
const LRU_CHUNKS: usize = 2;
const MAX_REFRESHES: u32 = 3;
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_secs(1);

pub(crate) struct RangeResponse {
    pub status: u16,
    pub content_range: Option<String>,
    pub body: Vec<u8>,
}

pub(crate) enum FetchError {
    /// Connect failure, timeout or reset.
    Transient(String),
}

pub(crate) trait RangeFetcher: Send + Sync {
    /// GET `url` with `Range: bytes=start-end_inclusive`.
    fn fetch(&self, url: &str, start: u64, end_inclusive: u64)
    -> Result<RangeResponse, FetchError>;
}

pub(crate) trait Refresher: Send + Sync {
    /// Re-resolves the same video; returns a fresh (url, itag, content_length).
    fn refresh(&self) -> Result<(String, u32, u64), ProviderError>;
}

#[derive(Clone, Copy)]
pub(crate) struct RangeConfig {
    pub chunk_size: u64,
    pub min_refresh_interval: Duration,
    pub retry_delay: Duration,
}

impl Default for RangeConfig {
    fn default() -> Self {
        Self {
            chunk_size: CHUNK_SIZE,
            min_refresh_interval: MIN_REFRESH_INTERVAL,
            retry_delay: TRANSIENT_RETRY_DELAY,
        }
    }
}

pub(crate) struct HttpRangeSource {
    url: String,
    itag: u32,
    len: u64,
    pos: u64,
    fetcher: Arc<dyn RangeFetcher>,
    refresher: Arc<dyn Refresher>,
    cfg: RangeConfig,
    /// Container header, kept for the life of the source.
    chunk0: Option<Vec<u8>>,
    /// Most recently used first.
    lru: VecDeque<(u64, Vec<u8>)>,
    refreshes: u32,
    last_refresh: Option<Instant>,
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

impl HttpRangeSource {
    pub(crate) fn new(
        url: String,
        itag: u32,
        len: u64,
        fetcher: Arc<dyn RangeFetcher>,
        refresher: Arc<dyn Refresher>,
        cfg: RangeConfig,
    ) -> Self {
        Self {
            url,
            itag,
            len,
            pos: 0,
            fetcher,
            refresher,
            cfg,
            chunk0: None,
            lru: VecDeque::with_capacity(LRU_CHUNKS),
            refreshes: 0,
            last_refresh: None,
        }
    }

    fn cached(&mut self, idx: u64) -> Option<&[u8]> {
        if idx == 0 {
            return self.chunk0.as_deref();
        }
        let at = self.lru.iter().position(|(i, _)| *i == idx)?;
        let entry = self.lru.remove(at).expect("position is in range");
        self.lru.push_front(entry);
        self.lru.front().map(|(_, d)| d.as_slice())
    }

    fn store(&mut self, idx: u64, data: Vec<u8>) {
        if idx == 0 {
            self.chunk0 = Some(data);
        } else {
            self.lru.push_front((idx, data));
            self.lru.truncate(LRU_CHUNKS);
        }
    }

    fn fetch_once(&self, start: u64, end: u64) -> io::Result<RangeResponse> {
        let mut transient = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                thread::sleep(self.cfg.retry_delay);
            }
            match self.fetcher.fetch(&self.url, start, end) {
                Ok(r) if r.status >= 500 => transient = format!("http {}", r.status),
                Ok(r) => return Ok(r),
                Err(FetchError::Transient(e)) => transient = e,
            }
            debug!("transient fetch failure ({transient}), attempt {attempt}");
        }
        Err(other(format!(
            "range fetch failed after retry: {transient}"
        )))
    }

    fn validate(&self, r: &RangeResponse, start: u64, end: u64) -> io::Result<()> {
        if r.status != 206 {
            return Err(other(format!(
                "ranged request returned status {}",
                r.status
            )));
        }
        let expected = format!("bytes {start}-{end}/{}", self.len);
        let got = r.content_range.as_deref().map(str::trim);
        if got != Some(expected.as_str()) {
            return Err(other(format!(
                "Content-Range mismatch: expected {expected}, got {got:?}"
            )));
        }
        let want = end - start + 1;
        if r.body.len() as u64 != want {
            return Err(other(format!(
                "body length {} != requested {want}",
                r.body.len()
            )));
        }
        Ok(())
    }

    fn refresh(&mut self) -> io::Result<()> {
        if self.refreshes >= MAX_REFRESHES {
            return Err(other("stream URL refresh limit reached"));
        }
        if self
            .last_refresh
            .is_some_and(|t| t.elapsed() < self.cfg.min_refresh_interval)
        {
            return Err(other("stream URL rejected again right after a refresh"));
        }
        self.refreshes += 1;
        self.last_refresh = Some(Instant::now());
        let (url, itag, len) = self
            .refresher
            .refresh()
            .map_err(|e| other(format!("stream URL refresh failed: {e}")))?;
        if itag != self.itag || len != self.len {
            return Err(other(format!(
                "refreshed format differs (itag {itag}, {len} bytes; was itag {}, {} bytes)",
                self.itag, self.len
            )));
        }
        debug!("refreshed url for itag {itag}, host {}", host_of(&url));
        self.url = url;
        Ok(())
    }

    fn load_chunk(&mut self, idx: u64) -> io::Result<Vec<u8>> {
        let start = idx * self.cfg.chunk_size;
        let end = (start + self.cfg.chunk_size).min(self.len) - 1;
        loop {
            let resp = self.fetch_once(start, end)?;
            match resp.status {
                403 | 410 => self.refresh()?,
                _ => {
                    self.validate(&resp, start, end)?;
                    return Ok(resp.body);
                }
            }
        }
    }

    fn ensure_chunk(&mut self, idx: u64) -> io::Result<()> {
        if self.cached(idx).is_none() {
            let data = self.load_chunk(idx)?;
            self.store(idx, data);
        }
        Ok(())
    }
}

impl Read for HttpRangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        let idx = self.pos / self.cfg.chunk_size;
        let offset = (self.pos - idx * self.cfg.chunk_size) as usize;
        self.ensure_chunk(idx)?;
        let chunk = self.cached(idx).expect("chunk was just ensured");
        let n = buf.len().min(chunk.len() - offset);
        buf[..n].copy_from_slice(&chunk[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpRangeSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of stream")
        })?;
        Ok(self.pos)
    }
}

impl ByteSource for HttpRangeSource {
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const CHUNK: u64 = 16;
    const LEN: u64 = 100;

    fn data() -> Vec<u8> {
        (0..LEN).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[derive(Clone)]
    enum Step {
        Ok,
        Status(u16),
        Status200,
        BadRange,
        Short,
        Long,
        Transient,
    }

    /// Scripted fetcher: pops a step per call (default `Ok`), records (url, start, end).
    struct Fake {
        data: Vec<u8>,
        script: Mutex<VecDeque<Step>>,
        calls: Mutex<Vec<(String, u64, u64)>>,
        /// Status for any URL listed here, regardless of the script.
        dead_urls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(script: Vec<Step>) -> Arc<Self> {
            Arc::new(Self {
                data: data(),
                script: Mutex::new(script.into()),
                calls: Mutex::default(),
                dead_urls: Mutex::default(),
            })
        }
        fn starts(&self) -> Vec<u64> {
            self.calls.lock().unwrap().iter().map(|c| c.1).collect()
        }
    }

    impl RangeFetcher for Fake {
        fn fetch(&self, url: &str, start: u64, end: u64) -> Result<RangeResponse, FetchError> {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_owned(), start, end));
            let step = if self.dead_urls.lock().unwrap().iter().any(|u| u == url) {
                Step::Status(403)
            } else {
                self.script.lock().unwrap().pop_front().unwrap_or(Step::Ok)
            };
            let body = self.data[start as usize..=end as usize].to_vec();
            let range = Some(format!("bytes {start}-{end}/{LEN}"));
            let resp = |status, content_range, body| RangeResponse {
                status,
                content_range,
                body,
            };
            Ok(match step {
                Step::Ok => resp(206, range, body),
                Step::Status(s) => resp(s, None, vec![]),
                Step::Status200 => resp(200, None, self.data.clone()),
                Step::BadRange => resp(206, Some(format!("bytes {}-{end}/{LEN}", start + 1)), body),
                Step::Short => resp(206, range, body[..body.len() - 1].to_vec()),
                Step::Long => {
                    let mut b = body;
                    b.push(0);
                    resp(206, range, b)
                }
                Step::Transient => return Err(FetchError::Transient("timeout".into())),
            })
        }
    }

    type Refreshed = Result<(String, u32, u64), ProviderError>;

    struct FakeRefresher {
        results: Mutex<VecDeque<Refreshed>>,
        count: Mutex<u32>,
    }

    impl FakeRefresher {
        fn new(results: Vec<Refreshed>) -> Arc<Self> {
            Arc::new(Self {
                results: Mutex::new(results.into()),
                count: Mutex::new(0),
            })
        }
        fn count(&self) -> u32 {
            *self.count.lock().unwrap()
        }
    }

    impl Refresher for FakeRefresher {
        fn refresh(&self) -> Result<(String, u32, u64), ProviderError> {
            *self.count.lock().unwrap() += 1;
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(("fresh".into(), 251, LEN)))
        }
    }

    fn cfg(min_refresh_interval: Duration) -> RangeConfig {
        RangeConfig {
            chunk_size: CHUNK,
            min_refresh_interval,
            retry_delay: Duration::ZERO,
        }
    }

    fn source(f: &Arc<Fake>, r: &Arc<FakeRefresher>, interval: Duration) -> HttpRangeSource {
        HttpRangeSource::new("old".into(), 251, LEN, f.clone(), r.clone(), cfg(interval))
    }

    fn simple(script: Vec<Step>) -> (Arc<Fake>, Arc<FakeRefresher>, HttpRangeSource) {
        let f = Fake::new(script);
        let r = FakeRefresher::new(vec![]);
        let s = source(&f, &r, Duration::ZERO);
        (f, r, s)
    }

    fn read_chunk_at(s: &mut HttpRangeSource, pos: u64) {
        s.seek(SeekFrom::Start(pos)).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(s.read(&mut b).unwrap(), 1);
    }

    #[test]
    fn lazy_until_first_read() {
        let (f, _, mut s) = simple(vec![]);
        assert_eq!(s.byte_len(), Some(LEN));
        s.seek(SeekFrom::Start(40)).unwrap();
        s.seek(SeekFrom::End(0)).unwrap();
        assert!(f.calls.lock().unwrap().is_empty());
        s.seek(SeekFrom::Start(0)).unwrap();
        s.read_exact(&mut [0u8; 4]).unwrap();
        assert_eq!(f.starts(), vec![0]);
    }

    #[test]
    fn whole_stream_across_chunk_boundaries() {
        let (f, _, mut s) = simple(vec![]);
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        assert_eq!(out, data());
        assert_eq!(f.starts(), vec![0, 16, 32, 48, 64, 80, 96]);
        // the last chunk is clamped to the end of the stream
        assert_eq!(f.calls.lock().unwrap().last().unwrap().2, LEN - 1);
    }

    #[test]
    fn large_buffer_reads_never_cross_chunks_but_stay_exact() {
        let (_, _, mut s) = simple(vec![]);
        let mut buf = vec![0u8; 1000];
        let mut out = Vec::new();
        loop {
            let n = s.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data());
    }

    #[test]
    fn seek_then_read_fetches_only_needed_chunk() {
        let (f, _, mut s) = simple(vec![]);
        s.seek(SeekFrom::Start(50)).unwrap();
        let mut b = [0u8; 3];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, &data()[50..53]);
        assert_eq!(f.starts(), vec![48]);
        assert_eq!(s.seek(SeekFrom::Current(-3)).unwrap(), 50);
        assert_eq!(s.seek(SeekFrom::End(-1)).unwrap(), LEN - 1);
    }

    #[test]
    fn seek_past_eof_reads_zero_and_before_start_errors() {
        let (f, _, mut s) = simple(vec![]);
        assert_eq!(s.seek(SeekFrom::Start(LEN + 50)).unwrap(), LEN + 50);
        assert_eq!(s.read(&mut [0u8; 8]).unwrap(), 0);
        assert!(s.seek(SeekFrom::Current(-1000)).is_err());
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn chunk_zero_pinned_and_lru_holds_two() {
        let (f, _, mut s) = simple(vec![]);
        for pos in [0, 16, 32, 48] {
            read_chunk_at(&mut s, pos);
        }
        assert_eq!(f.starts(), vec![0, 16, 32, 48]);
        // chunks 2 and 3 are cached, chunk 1 was evicted, chunk 0 is pinned
        for pos in [0, 48, 32] {
            read_chunk_at(&mut s, pos);
        }
        assert_eq!(f.starts().len(), 4);
        read_chunk_at(&mut s, 16);
        assert_eq!(f.starts(), vec![0, 16, 32, 48, 16]);
        // reading chunk 2 refreshed its recency, so 3 was evicted by 1
        read_chunk_at(&mut s, 32);
        assert_eq!(f.starts().len(), 5);
        read_chunk_at(&mut s, 48);
        assert_eq!(f.starts(), vec![0, 16, 32, 48, 16, 48]);
    }

    #[test]
    fn status_200_to_ranged_request_errors() {
        let (_, _, mut s) = simple(vec![Step::Status200]);
        assert!(s.read(&mut [0u8; 4]).is_err());
    }

    #[test]
    fn bad_content_range_errors() {
        let (_, _, mut s) = simple(vec![Step::BadRange]);
        assert!(s.read(&mut [0u8; 4]).is_err());
    }

    #[test]
    fn short_and_long_bodies_error() {
        for step in [Step::Short, Step::Long] {
            let (_, _, mut s) = simple(vec![step]);
            assert!(s.read(&mut [0u8; 4]).is_err());
        }
    }

    #[test]
    fn unexpected_status_errors() {
        let (_, _, mut s) = simple(vec![Step::Status(404)]);
        assert!(s.read(&mut [0u8; 4]).is_err());
    }

    #[test]
    fn transient_then_success_retries_same_range() {
        let (f, _, mut s) = simple(vec![Step::Transient]);
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, &data()[..4]);
        assert_eq!(f.starts(), vec![0, 0]);
    }

    #[test]
    fn http_5xx_is_transient() {
        let (f, _, mut s) = simple(vec![Step::Status(503)]);
        s.read_exact(&mut [0u8; 4]).unwrap();
        assert_eq!(f.starts(), vec![0, 0]);
    }

    #[test]
    fn transient_twice_errors() {
        let (f, _, mut s) = simple(vec![Step::Transient, Step::Transient]);
        assert!(s.read(&mut [0u8; 4]).is_err());
        assert_eq!(f.starts().len(), 2);
    }

    #[test]
    fn forbidden_refreshes_same_format_and_resumes() {
        let f = Fake::new(vec![]);
        let r = FakeRefresher::new(vec![Ok(("fresh".into(), 251, LEN))]);
        f.dead_urls.lock().unwrap().push("old".into());
        let mut s = source(&f, &r, Duration::ZERO);
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        assert_eq!(out, data());
        assert_eq!(r.count(), 1);
        let calls = f.calls.lock().unwrap();
        assert_eq!(calls[0], ("old".into(), 0, CHUNK - 1));
        assert_eq!(calls[1], ("fresh".into(), 0, CHUNK - 1));
        assert!(calls[2..].iter().all(|c| c.0 == "fresh"));
    }

    #[test]
    fn gone_410_also_refreshes() {
        let f = Fake::new(vec![Step::Status(410)]);
        let r = FakeRefresher::new(vec![]);
        let mut s = source(&f, &r, Duration::ZERO);
        s.read_exact(&mut [0u8; 4]).unwrap();
        assert_eq!(r.count(), 1);
    }

    #[test]
    fn refresh_with_different_length_or_itag_errors() {
        for fresh in [
            ("fresh".to_string(), 251, LEN + 1),
            ("fresh".to_string(), 140, LEN),
        ] {
            let f = Fake::new(vec![Step::Status(403)]);
            let r = FakeRefresher::new(vec![Ok(fresh)]);
            let mut s = source(&f, &r, Duration::ZERO);
            assert!(s.read(&mut [0u8; 4]).is_err());
            assert_eq!(f.starts().len(), 1);
        }
    }

    #[test]
    fn refresh_failure_errors() {
        let f = Fake::new(vec![Step::Status(403)]);
        let r = FakeRefresher::new(vec![Err(ProviderError::Network("down".into()))]);
        let mut s = source(&f, &r, Duration::ZERO);
        let e = s.read(&mut [0u8; 4]).unwrap_err();
        assert!(e.to_string().contains("down"), "{e}");
    }

    #[test]
    fn fourth_refresh_errors() {
        let f = Fake::new(vec![Step::Status(403); 10]);
        let r = FakeRefresher::new(vec![]);
        let mut s = source(&f, &r, Duration::ZERO);
        assert!(s.read(&mut [0u8; 4]).is_err());
        assert_eq!(r.count(), 3);
        assert_eq!(f.starts().len(), 4);
    }

    #[test]
    fn refresh_within_min_interval_errors() {
        // first 403 refreshes; the refreshed URL 403s again immediately
        let f = Fake::new(vec![Step::Status(403), Step::Status(403)]);
        let r = FakeRefresher::new(vec![]);
        let mut s = source(&f, &r, Duration::from_secs(3600));
        assert!(s.read(&mut [0u8; 4]).is_err());
        assert_eq!(r.count(), 1);
    }

    #[test]
    fn refresh_after_min_interval_is_allowed() {
        let f = Fake::new(vec![Step::Status(403)]);
        let r = FakeRefresher::new(vec![]);
        let mut s = source(&f, &r, Duration::from_millis(30));
        s.read_exact(&mut [0u8; 4]).unwrap();
        thread::sleep(Duration::from_millis(40));
        f.script.lock().unwrap().push_back(Step::Status(403));
        s.seek(SeekFrom::Start(64)).unwrap();
        s.read_exact(&mut [0u8; 4]).unwrap();
        assert_eq!(r.count(), 2);
    }
}
