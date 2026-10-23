//! Test doubles for YouTube Music's endpoints.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{fs, process};

use requiem_core::ProviderError;

use crate::IpFamily;
use crate::range::{FetchError, RangeResponse};
use crate::transport::{HttpResponse, Transport};

type Pairs = Vec<(String, String)>;

fn owned(pairs: &[(&str, &str)]) -> Pairs {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

pub(crate) struct MusicCall {
    pub url: String,
    pub headers: Pairs,
    pub body: String,
    /// Sent through `post_json_signed`.
    pub signed: bool,
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// Innertube's endpoints: replies are scripted per URL without its query and served in order,
/// or per URL and `browseId` for browses made at once; an unscripted call is a network error.
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

    /// Like `script`, for calls to `path` whose body asks for `browse_id` only.
    pub(crate) fn script_browse(&self, path: &str, browse_id: &str, status: u16, body: &str) {
        self.script(&format!("{path}#{browse_id}"), status, body);
    }

    /// Like `during`, for the next call to `path` that asks for `browse_id`.
    pub(crate) fn during_browse(
        &self,
        path: &str,
        browse_id: &str,
        f: impl Fn() + Send + Sync + 'static,
    ) {
        self.during(&format!("{path}#{browse_id}"), f);
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
        let browse_id = serde_json::from_str::<serde_json::Value>(&call.body)
            .ok()
            .and_then(|b| b["browseId"].as_str().map(str::to_owned));
        self.calls.lock().unwrap().push(call);
        let hook = {
            let mut hooks = self.hooks.lock().unwrap();
            browse_id
                .as_ref()
                .and_then(|id| hooks.remove(&format!("{path}#{id}")))
                .or_else(|| hooks.remove(&path))
        };
        if let Some(hook) = hook {
            hook();
        }
        let mut replies = self.replies.lock().unwrap();
        let by_id = browse_id
            .and_then(|id| replies.get_mut(&format!("{path}#{id}")))
            .and_then(VecDeque::pop_front);
        let (status, body) = by_id
            .or_else(|| replies.get_mut(&path).and_then(VecDeque::pop_front))
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
