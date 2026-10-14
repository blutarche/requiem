//! A temporary directory, a signing key, and a fake release page on 127.0.0.1.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use semver::Version;

use crate::b64;
use crate::fetch::Config;
use crate::manifest::Target;

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rqu{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        // Canonical, so paths compare equal to what realpath returns (/var is /private/var).
        Self(fs::canonicalize(&path).unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Retries `f` for up to 5 s until it holds. A child that another test is spawning holds
/// copies of this process's descriptors until its exec closes them, and with them any `flock`
/// taken through one, so a dropped lock can stay held for a moment.
pub fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub struct Key(Ed25519KeyPair);

impl Key {
    pub fn new() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Self(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
    }

    pub fn public(&self) -> String {
        b64::encode(self.0.public_key().as_ref())
    }

    /// The `.sig` file's contents.
    pub fn sign(&self, bytes: &[u8]) -> String {
        b64::encode(self.0.sign(bytes).as_ref()) + "\n"
    }
}

pub fn manifest_json(version: &str, zip: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "format": 1,
        "version": version,
        "minimumMacOS": "26.0",
        "architectures": ["arm64"],
        "bundleIdentifier": "dev.blutarche.requiem",
        "file": format!("Requiem-{version}.zip"),
        "size": zip.len(),
        "sha256": sha256_hex(zip),
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn target() -> Target {
    Target {
        arch: "arm64".into(),
        macos: "26.6.2".into(),
    }
}

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn redirect(to: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".into(), to.into())],
            body: Vec::new(),
        }
    }
}

/// Serves fixed replies by path; anything else is a 404. Records the paths asked for.
pub struct Server {
    pub base: String,
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    pub hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Reply>>> = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let (r, h) = (routes.clone(), hits.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (r, h) = (r.clone(), h.clone());
                thread::spawn(move || serve(stream, &r, &h));
            }
        });
        Self { base, routes, hits }
    }

    pub fn route(&self, path: &str, reply: Reply) {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), reply);
    }

    /// Publishes a signed release of `version` whose zip is `zip`, as `releases/latest`.
    pub fn publish(&self, key: &Key, version: &str, zip: &[u8]) -> Vec<u8> {
        let manifest = serde_json::to_vec_pretty(&manifest_json(version, zip)).unwrap();
        self.publish_manifest(key, &manifest);
        self.route(
            &format!("/download/v{version}/Requiem-{version}.zip"),
            Reply::ok(zip),
        );
        manifest
    }

    pub fn publish_manifest(&self, key: &Key, manifest: &[u8]) {
        self.route("/latest/download/requiem-update.json", Reply::ok(manifest));
        self.route(
            "/latest/download/requiem-update.json.sig",
            Reply::ok(key.sign(manifest)),
        );
    }

    pub fn config(&self, key: &Key, running: &str) -> Config {
        Config {
            base: self.base.clone(),
            public_key: key.public(),
            running: Version::parse(running).unwrap(),
            target: target(),
        }
    }
}

fn serve(stream: TcpStream, routes: &Mutex<HashMap<String, Reply>>, hits: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => return,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
        }
    }
    hits.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(path.clone());
    let reply = routes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&path)
        .cloned()
        .unwrap_or(Reply {
            status: 404,
            headers: Vec::new(),
            body: b"not found".to_vec(),
        });
    let mut out = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut stream = &stream;
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.write_all(&reply.body);
}
