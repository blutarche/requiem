//! Test doubles for Google's endpoints.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{fs, process};

use requiem_core::ProviderError;

use crate::transport::{ApiHttp, HttpResponse};

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
