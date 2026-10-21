//! Test doubles for Google's and YouTube Music's endpoints.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{fs, process};

use requiem_core::ProviderError;

use crate::IpFamily;
use crate::range::{FetchError, RangeResponse};
use crate::transport::{ApiHttp, HttpResponse, Transport};

type Pairs = Vec<(String, String)>;

pub(crate) struct Call {
    pub url: String,
    pub query: Pairs,
    pub headers: Pairs,
    pub form: Pairs,
}

/// Replies are scripted per URL and served in order; an unscripted call is a network error.
#[derive(Default)]
pub(crate) struct FakeApi {
    replies: Mutex<HashMap<String, VecDeque<(u16, String)>>>,
    calls: Mutex<Vec<Call>>,
}

fn owned(pairs: &[(&str, &str)]) -> Pairs {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

impl FakeApi {
    pub(crate) fn script(&self, url: &str, status: u16, body: &str) {
        self.replies
            .lock()
            .unwrap()
            .entry(url.to_owned())
            .or_default()
            .push_back((status, body.to_owned()));
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.url.clone())
            .collect()
    }

    fn pick(&self, url: &str, f: impl Fn(&Call) -> Pairs) -> Vec<Pairs> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.url == url)
            .map(f)
            .collect()
    }

    pub(crate) fn forms(&self, url: &str) -> Vec<Pairs> {
        self.pick(url, |c| c.form.clone())
    }

    pub(crate) fn headers(&self, url: &str) -> Vec<Pairs> {
        self.pick(url, |c| c.headers.clone())
    }

    pub(crate) fn queries(&self, url: &str) -> Vec<Pairs> {
        self.pick(url, |c| c.query.clone())
    }

    fn reply(&self, call: Call) -> Result<HttpResponse, ProviderError> {
        let url = call.url.clone();
        self.calls.lock().unwrap().push(call);
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .get_mut(&url)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| ProviderError::Network(format!("unscripted {url}")))?;
        Ok(HttpResponse {
            status,
            body: body.into_bytes(),
        })
    }
}

impl ApiHttp for FakeApi {
    fn get(
        &self,
        url: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, ProviderError> {
        self.reply(Call {
            url: url.to_owned(),
            query: owned(query),
            headers: owned(headers),
            form: Vec::new(),
        })
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<HttpResponse, ProviderError> {
        self.reply(Call {
            url: url.to_owned(),
            query: Vec::new(),
            headers: Vec::new(),
            form: owned(form),
        })
    }
}

pub(crate) struct MusicCall {
    pub url: String,
    pub headers: Pairs,
    pub body: String,
    /// Sent through `post_json_signed`.
    pub signed: bool,
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// Innertube's endpoints: replies are scripted per URL without its query and served in order;
/// an unscripted call is a network error.
#[derive(Default)]
pub(crate) struct FakeMusic {
    replies: Mutex<HashMap<String, VecDeque<(u16, String)>>>,
    pub calls: Mutex<Vec<MusicCall>>,
    /// Run once by the next call to their URL, before it is answered.
    hooks: Mutex<HashMap<String, Hook>>,
}

impl FakeMusic {
    pub(crate) fn script(&self, path: &str, status: u16, body: &str) {
        self.replies
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .push_back((status, body.to_owned()));
    }

    /// Runs `f` while the next call to `path` is in flight.
    pub(crate) fn during(&self, path: &str, f: impl Fn() + Send + Sync + 'static) {
        self.hooks
            .lock()
            .unwrap()
            .insert(path.to_owned(), Box::new(f));
    }

    fn reply(&self, call: MusicCall) -> Result<HttpResponse, ProviderError> {
        let path = call.url.split('?').next().unwrap_or_default().to_owned();
        self.calls.lock().unwrap().push(call);
        let hook = self.hooks.lock().unwrap().remove(&path);
        if let Some(hook) = hook {
            hook();
        }
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .get_mut(&path)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| ProviderError::Network(format!("unscripted {path}")))?;
        Ok(HttpResponse {
            status,
            body: body.into_bytes(),
        })
    }
}

impl Transport for FakeMusic {
    fn post_json(
        &self,
        _: IpFamily,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError> {
        self.reply(MusicCall {
            url: url.to_owned(),
            headers: owned(headers),
            body: body.to_owned(),
            signed: false,
        })
    }

    fn post_json_signed(
        &self,
        _: IpFamily,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<HttpResponse, ProviderError> {
        self.reply(MusicCall {
            url: url.to_owned(),
            headers: owned(headers),
            body: body.to_owned(),
            signed: true,
        })
    }

    fn fetch_on(
        &self,
        _: IpFamily,
        url: &str,
        _: u64,
        _: u64,
    ) -> Result<RangeResponse, FetchError> {
        Err(FetchError::Transient(format!("unscripted fetch of {url}")))
    }

    fn reset(&self) {}
}

/// A fresh directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("requiem-ytm-{}-{n}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
