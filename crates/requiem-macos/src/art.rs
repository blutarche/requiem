//! Artwork bookkeeping without AppKit: URL sizing, the decoded-image cache, the request
//! queue and the disk cache of downloaded images.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// Image hosts whose URLs end in Google's `=w…-h…-…` options, which take a size.
const SIZED_HOSTS: [&str; 2] = ["googleusercontent.com", "ggpht.com"];

/// `url` asking for a `px`-pixel square image, for Google image hosts; any other URL as it is.
///
/// The options are the `-`-separated part after the last `=` of the last path segment:
/// `w`, `h` and `s` sizes are replaced and the rest (quality, format, crop) kept. A URL with
/// no size gets one.
pub fn sized_url(url: &str, px: u32) -> String {
    let unchanged = || url.to_owned();
    let Some(rest) = url.strip_prefix("https://") else {
        return unchanged();
    };
    let Some((host, _)) = rest.split_once('/') else {
        return unchanged();
    };
    let google = SIZED_HOSTS.iter().any(|h| {
        host == *h
            || host
                .strip_suffix(h)
                .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1)
    });
    if !google || url.contains(['?', '#']) {
        return unchanged();
    }
    let segment = url.rfind('/').map_or(0, |i| i + 1);
    let (base, options) = match url[segment..].rfind('=') {
        Some(i) => (&url[..segment + i], &url[segment + i + 1..]),
        None => (url, ""),
    };
    let is_size = |prefix: char, opt: &str| {
        opt.strip_prefix(prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut sized = false;
    let mut opts: Vec<String> = options
        .split('-')
        .filter(|o| !o.is_empty())
        .map(|o| {
            for prefix in ['w', 'h', 's'] {
                if is_size(prefix, o) {
                    sized = true;
                    return format!("{prefix}{px}");
                }
            }
            o.to_owned()
        })
        .collect();
    if !sized {
        opts.splice(0..0, [format!("w{px}"), format!("h{px}")]);
    }
    format!("{base}={}", opts.join("-"))
}

/// Bytes of decoded RGBA pixels in a `w` × `h` image.
pub fn pixel_cost(w: usize, h: usize) -> usize {
    w.saturating_mul(h).saturating_mul(4)
}

struct Entry<V> {
    value: V,
    cost: usize,
    used: u64,
}

/// Values by key, least recently used evicted first once their costs pass `cap`. The newest
/// entry stays even if it alone is over.
pub struct Lru<V> {
    cap: usize,
    total: usize,
    clock: u64,
    entries: HashMap<String, Entry<V>>,
}

impl<V: Clone> Lru<V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            total: 0,
            clock: 0,
            entries: HashMap::new(),
        }
    }

    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.evict();
    }

    /// Marks `key` used.
    pub fn get(&mut self, key: &str) -> Option<V> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(entry.value.clone())
    }

    pub fn insert(&mut self, key: String, value: V, cost: usize) {
        self.clock += 1;
        let entry = Entry {
            value,
            cost,
            used: self.clock,
        };
        if let Some(old) = self.entries.insert(key, entry) {
            self.total -= old.cost;
        }
        self.total += cost;
        self.evict();
    }

    fn evict(&mut self) {
        while self.total > self.cap && self.entries.len() > 1 {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            let Some(entry) = oldest.and_then(|k| self.entries.remove(&k)) else {
                return;
            };
            self.total -= entry.cost;
        }
    }

    #[cfg(test)]
    fn total(&self) -> usize {
        self.total
    }
}

/// Requests waiting for the fetcher, newest first, at most `limit`: rows scrolled into view
/// are fetched before those scrolled past.
pub struct Queue<T> {
    keys: VecDeque<T>,
    limit: usize,
}

impl<T> Queue<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            keys: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// Returns the oldest key when `key` pushes it out.
    pub fn push(&mut self, key: T) -> Option<T> {
        self.keys.push_front(key);
        if self.keys.len() > self.limit {
            self.keys.pop_back()
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The newest key.
    pub fn pop(&mut self) -> Option<T> {
        self.keys.pop_front()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Not loading yet: queue a fetch.
    Fetch,
    /// Already loading; the waiter joined it.
    Joined,
    /// Failed earlier this session; not fetched again.
    Failed,
}

/// Fetches in progress, with whoever waits on each, and the keys that failed.
pub struct Book<W> {
    waiting: HashMap<String, Vec<W>>,
    failed: HashSet<String>,
}

impl<W> Default for Book<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W> Book<W> {
    pub fn new() -> Self {
        Self {
            waiting: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    /// For a key not in the cache.
    pub fn want(&mut self, key: &str, waiter: W) -> Want {
        if self.failed.contains(key) {
            return Want::Failed;
        }
        if let Some(waiters) = self.waiting.get_mut(key) {
            waiters.push(waiter);
            return Want::Joined;
        }
        self.waiting.insert(key.to_owned(), vec![waiter]);
        Want::Fetch
    }

    /// `key` failed earlier, so `want` would not fetch it.
    pub fn has_failed(&self, key: &str) -> bool {
        self.failed.contains(key)
    }

    /// The fetch of `key` ended; returns its waiters. A failure is remembered.
    pub fn finish(&mut self, key: &str, ok: bool) -> Vec<W> {
        if !ok {
            self.failed.insert(key.to_owned());
        }
        self.waiting.remove(key).unwrap_or_default()
    }

    /// `key` left the queue unfetched, so a later `want` fetches it.
    pub fn dropped(&mut self, key: &str) {
        self.waiting.remove(key);
    }
}

/// Overrides where downloaded artwork is kept, so tests and private daemons leave the user's
/// cache alone.
pub const CACHE_DIR_ENV: &str = "REQUIEM_ART_CACHE_DIR";

/// Where downloaded artwork is kept: `REQUIEM_ART_CACHE_DIR`, or
/// `~/Library/Caches/<bundle id>/artwork`, `requiem` standing in for a missing bundle id.
/// `None` turns the disk cache off, as does an override that is not absolute: falling back to
/// the real cache would defeat it.
pub fn cache_dir(
    over: Option<OsString>,
    home: Option<PathBuf>,
    bundle_id: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = over.filter(|v| !v.is_empty()).map(PathBuf::from) {
        if dir.is_absolute() {
            return Some(dir);
        }
        eprintln!("requiem: not keeping artwork on disk: {CACHE_DIR_ENV} is not absolute");
        return None;
    }
    let home = home.filter(|h| h.is_absolute())?;
    let id = bundle_id
        .filter(|id| !id.is_empty() && !id.contains('/') && !id.starts_with('.'))
        .unwrap_or("requiem");
    Some(home.join("Library/Caches").join(id).join("artwork"))
}

/// The file `url`'s bytes are kept in: the hex SHA-256 of the URL, which unlike `std`'s hasher
/// stays the same across toolchains.
pub fn file_name(url: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, url.as_bytes());
    let mut name = String::with_capacity(64);
    for b in digest.as_ref() {
        let _ = write!(name, "{b:02x}");
    }
    name
}

fn is_cache_file(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

const TMP_SUFFIX: &str = ".tmp";
/// A temporary file this old was left by a write that never finished.
const STALE_TMP: Duration = Duration::from_secs(600);
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Downloaded images by URL, the least recently used removed once their bytes pass `cap`.
/// Threads and processes may share it: files are replaced whole, by rename.
pub struct DiskCache {
    dir: PathBuf,
    cap: u64,
    /// Bytes on disk as of the last prune plus those written since; `None` before the first
    /// prune. Held through a prune, so only one runs at a time.
    total: Mutex<Option<u64>>,
}

impl DiskCache {
    pub fn new(dir: PathBuf, cap: u64) -> Self {
        Self {
            dir,
            cap,
            total: Mutex::new(None),
        }
    }

    pub fn path(&self, url: &str) -> PathBuf {
        self.dir.join(file_name(url))
    }

    /// `url`'s bytes, marking them used.
    pub fn read(&self, url: &str) -> Option<Vec<u8>> {
        let mut file = File::open(self.path(url)).ok()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let _ = file.set_modified(SystemTime::now());
        (!bytes.is_empty()).then_some(bytes)
    }

    /// Drops `url`'s bytes, as for a file that no longer decodes.
    pub fn remove(&self, url: &str) {
        let _ = fs::remove_file(self.path(url));
    }

    /// Keeps `bytes` as `url`'s, so a reader sees the old file or the new one, never part of
    /// one. Prunes once past the cap.
    pub fn write(&self, url: &str, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let name = file_name(url);
        let tmp = self.dir.join(format!(
            "{name}.{}.{}{TMP_SUFFIX}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let written = File::create(&tmp)
            .and_then(|mut f| f.write_all(bytes))
            .and_then(|()| fs::rename(&tmp, self.dir.join(&name)));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        let over = {
            let mut total = self.total.lock().unwrap_or_else(PoisonError::into_inner);
            match total.as_mut() {
                Some(t) => {
                    *t += bytes.len() as u64;
                    *t > self.cap
                }
                None => true,
            }
        };
        if over {
            self.prune();
        }
        Ok(())
    }

    /// Once over the cap, removes the least recently used files until a quarter of it is free,
    /// so a full cache is not scanned on every write. Also removes what writes that never
    /// finished left; other files in the directory stay.
    pub fn prune(&self) {
        let mut total = self.total.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(entries) = fs::read_dir(&self.dir) else {
            *total = Some(0);
            return;
        };
        let now = SystemTime::now();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|_| meta.is_file()) else {
                continue;
            };
            let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if is_cache_file(name) {
                files.push((used, meta.len(), entry.path()));
            } else if name.ends_with(TMP_SUFFIX)
                && name.get(..64).is_some_and(is_cache_file)
                && now.duration_since(used).is_ok_and(|age| age > STALE_TMP)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mut sum: u64 = files.iter().map(|(_, len, _)| len).sum();
        if sum > self.cap {
            files.sort_by_key(|(used, _, _)| *used);
            let target = self.cap / 4 * 3;
            for (_, len, path) in files {
                if sum <= target {
                    break;
                }
                if fs::remove_file(&path).is_ok() {
                    sum -= len;
                }
            }
        }
        *total = Some(sum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YT3: &str = "https://yt3.googleusercontent.com/1gXa4i8m_NecS7xNqLqzxy6rdOWBh_L6U91YYv5ULNdmvvYT1Pr6ld7_SyUs5L52pEQheVyfU9SnXUm9";

    #[test]
    fn parser_urls_take_the_drawn_size() {
        let url = format!("{YT3}=w120-h120-l90-rj");
        assert_eq!(sized_url(&url, 64), format!("{YT3}=w64-h64-l90-rj"));
        let url = format!("{YT3}=w60-h60-l90-rj");
        assert_eq!(sized_url(&url, 536), format!("{YT3}=w536-h536-l90-rj"));
    }

    #[test]
    fn other_google_hosts_and_size_forms() {
        assert_eq!(
            sized_url("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj", 96),
            "https://lh3.googleusercontent.com/abc=w96-h96-l90-rj"
        );
        assert_eq!(
            sized_url("https://yt3.ggpht.com/abc=s88-c-k-c0x00ffffff-no-rj", 64),
            "https://yt3.ggpht.com/abc=s64-c-k-c0x00ffffff-no-rj"
        );
        assert_eq!(
            sized_url("https://lh3.googleusercontent.com/a/b=l90-rj", 64),
            "https://lh3.googleusercontent.com/a/b=w64-h64-l90-rj"
        );
    }

    #[test]
    fn a_url_without_options_gets_a_size() {
        assert_eq!(
            sized_url("https://lh3.googleusercontent.com/abc", 64),
            "https://lh3.googleusercontent.com/abc=w64-h64"
        );
    }

    #[test]
    fn other_urls_stay_as_they_are() {
        for url in [
            "https://i.ytimg.com/vi/abc/hqdefault.jpg",
            "http://yt3.googleusercontent.com/abc=w60-h60",
            "https://lh3.googleusercontent.com/abc=w60-h60?x=1",
            "https://evilgoogleusercontent.com/abc=w60-h60",
            "https://googleusercontent.com.example.com/abc=w60-h60",
            "https://yt3.googleusercontent.com",
            "demo:03",
            "",
        ] {
            assert_eq!(sized_url(url, 64), url, "{url}");
        }
    }

    #[test]
    fn lru_evicts_least_recently_used_by_bytes() {
        let mut lru = Lru::new(pixel_cost(64, 64) * 3);
        for k in ["a", "b", "c"] {
            lru.insert(k.into(), k, pixel_cost(64, 64));
        }
        assert_eq!(lru.get("a"), Some("a"));
        lru.insert("d".into(), "d", pixel_cost(64, 64));
        assert_eq!(lru.get("b"), None, "b was the least recently used");
        assert_eq!(lru.get("a"), Some("a"));
        assert_eq!(lru.get("c"), Some("c"));
        assert_eq!(lru.get("d"), Some("d"));
        assert_eq!(lru.total(), pixel_cost(64, 64) * 3);
    }

    #[test]
    fn a_large_image_evicts_as_many_small_ones_as_it_needs() {
        let mut lru = Lru::new(1000);
        for k in ["a", "b", "c", "d"] {
            lru.insert(k.into(), k, 250);
        }
        lru.insert("big".into(), "big", 600);
        assert_eq!(lru.get("a"), None);
        assert_eq!(lru.get("b"), None);
        assert_eq!(lru.get("c"), None);
        assert_eq!(lru.get("d"), Some("d"));
        assert_eq!(lru.total(), 850);
    }

    #[test]
    fn the_newest_entry_stays_even_over_the_cap() {
        let mut lru = Lru::new(100);
        lru.insert("a".into(), "a", 50);
        lru.insert("huge".into(), "huge", 500);
        assert_eq!(lru.get("a"), None);
        assert_eq!(lru.get("huge"), Some("huge"));
        assert_eq!(lru.total(), 500);
    }

    #[test]
    fn replacing_a_key_counts_its_new_cost_once() {
        let mut lru = Lru::new(1000);
        lru.insert("a".into(), 1, 300);
        lru.insert("a".into(), 2, 400);
        assert_eq!(lru.total(), 400);
        assert_eq!(lru.get("a"), Some(2));
    }

    #[test]
    fn lowering_the_cap_evicts() {
        let mut lru = Lru::new(1000);
        lru.insert("a".into(), "a", 400);
        lru.insert("b".into(), "b", 400);
        lru.set_cap(500);
        assert_eq!(lru.get("a"), None);
        assert_eq!(lru.get("b"), Some("b"));
    }

    #[test]
    fn requests_for_one_key_share_a_fetch() {
        let mut book = Book::new();
        assert_eq!(book.want("u", 1), Want::Fetch);
        assert_eq!(book.want("u", 2), Want::Joined);
        assert_eq!(book.want("v", 3), Want::Fetch);
        assert_eq!(book.finish("u", true), vec![1, 2]);
        assert_eq!(book.finish("v", true), vec![3]);
        // Once loaded the cache answers; should it have evicted the image, it is fetched again.
        assert_eq!(book.want("u", 4), Want::Fetch);
    }

    #[test]
    fn a_failed_key_is_never_fetched_again() {
        let mut book = Book::new();
        assert_eq!(book.want("u", 1), Want::Fetch);
        assert_eq!(book.want("u", 2), Want::Joined);
        assert_eq!(book.finish("u", false), vec![1, 2]);
        for w in 3..10 {
            assert_eq!(book.want("u", w), Want::Failed);
        }
        assert_eq!(book.want("v", 10), Want::Fetch);
    }

    #[test]
    fn a_dropped_request_can_be_made_again() {
        let mut book = Book::new();
        assert_eq!(book.want("u", 1), Want::Fetch);
        book.dropped("u");
        assert_eq!(book.want("u", 2), Want::Fetch);
        assert_eq!(book.finish("u", true), vec![2]);
    }

    #[test]
    fn the_queue_serves_the_newest_and_drops_the_oldest() {
        let mut q = Queue::<String>::new(2);
        assert_eq!(q.push("a".into()), None);
        assert_eq!(q.push("b".into()), None);
        assert_eq!(q.push("c".into()), Some("a".into()));
        assert_eq!(q.pop().as_deref(), Some("c"));
        assert_eq!(q.push("d".into()), None);
        assert_eq!(q.pop().as_deref(), Some("d"));
        assert_eq!(q.pop().as_deref(), Some("b"));
        assert_eq!(q.pop(), None);
    }

    /// A fresh directory under the system's temporary one, never the real cache.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("requiem-art-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn last_used(cache: &DiskCache, url: &str, secs_ago: u64) {
        let file = File::options().write(true).open(cache.path(url)).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
            .unwrap();
    }

    fn total(cache: &DiskCache) -> Option<u64> {
        *cache.total.lock().unwrap()
    }

    #[test]
    fn a_url_is_kept_under_its_hash() {
        let url = "https://i.scdn.co/image/ab67616d00001e02";
        let name = file_name(url);
        assert!(is_cache_file(&name), "{name}");
        assert_eq!(name, file_name(url));
        assert_ne!(name, file_name("https://i.scdn.co/image/ab67616d00001e03"));
        // SHA-256 of the empty string: a name never changes with the toolchain.
        assert_eq!(
            file_name(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let cache = DiskCache::new(PathBuf::from("/cache"), 1);
        assert_eq!(cache.path(url), PathBuf::from("/cache").join(name));
        assert!(!is_cache_file(&file_name("").to_uppercase()));
        assert!(!is_cache_file("notes.txt"));
    }

    #[test]
    fn a_write_is_read_back_whole() {
        let dir = scratch("write");
        let cache = DiskCache::new(dir.clone(), 1 << 20);
        assert_eq!(cache.read("https://a/1"), None);
        cache.write("https://a/1", b"first").unwrap();
        assert_eq!(cache.read("https://a/1").as_deref(), Some(&b"first"[..]));
        cache.write("https://a/1", b"second").unwrap();
        assert_eq!(cache.read("https://a/1").as_deref(), Some(&b"second"[..]));
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            left,
            vec![file_name("https://a/1")],
            "no temporary file is left"
        );
        cache.remove("https://a/1");
        assert_eq!(cache.read("https://a/1"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn past_the_cap_the_least_recently_used_go() {
        let dir = scratch("evict");
        let cache = DiskCache::new(dir.clone(), 400);
        for (i, url) in ["https://a/1", "https://a/2", "https://a/3"]
            .iter()
            .enumerate()
        {
            cache.write(url, &[0; 100]).unwrap();
            last_used(&cache, url, 300 - 100 * i as u64);
        }
        // Reading 1 marks it used, so 2 is now the oldest.
        assert!(cache.read("https://a/1").is_some());
        cache.write("https://a/4", &[0; 100]).unwrap();
        assert_eq!(total(&cache), Some(400), "at the cap, nothing goes");
        cache.write("https://a/5", &[0; 100]).unwrap();
        // 500 bytes is over 400, so files go until 300 are left: 2, then 3.
        assert_eq!(cache.read("https://a/2"), None);
        assert_eq!(cache.read("https://a/3"), None);
        for url in ["https://a/1", "https://a/4", "https://a/5"] {
            assert!(cache.read(url).is_some(), "{url}");
        }
        assert_eq!(total(&cache), Some(300));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prune_leaves_other_files_and_unfinished_writes_alone() {
        let dir = scratch("prune");
        fs::create_dir_all(&dir).unwrap();
        let other = dir.join("notes.txt");
        fs::write(&other, [0; 500]).unwrap();
        let name = file_name("https://a/1");
        let stale = dir.join(format!("{name}.1.0{TMP_SUFFIX}"));
        let writing = dir.join(format!("{name}.1.1{TMP_SUFFIX}"));
        fs::write(&stale, b"x").unwrap();
        fs::write(&writing, b"x").unwrap();
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_TMP * 2)
            .unwrap();
        let cache = DiskCache::new(dir.clone(), 100);
        cache.prune();
        assert!(other.exists());
        assert!(writing.exists(), "another writer may still be writing it");
        assert!(!stale.exists());
        assert_eq!(total(&cache), Some(0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_override_keeps_tests_off_the_real_cache() {
        let home = || Some(PathBuf::from("/Users/me"));
        assert_eq!(
            cache_dir(Some("/tmp/art".into()), home(), Some("dev.x")),
            Some(PathBuf::from("/tmp/art"))
        );
        assert_eq!(
            cache_dir(Some("relative/art".into()), home(), Some("dev.x")),
            None,
            "a bad override turns the cache off rather than use the real one"
        );
        assert_eq!(
            cache_dir(Some("".into()), home(), Some("dev.x")),
            Some(PathBuf::from("/Users/me/Library/Caches/dev.x/artwork"))
        );
        assert_eq!(
            cache_dir(None, home(), None),
            Some(PathBuf::from("/Users/me/Library/Caches/requiem/artwork"))
        );
        assert_eq!(
            cache_dir(None, home(), Some("../x")),
            Some(PathBuf::from("/Users/me/Library/Caches/requiem/artwork"))
        );
        assert_eq!(cache_dir(None, None, Some("dev.x")), None);
        assert_eq!(cache_dir(None, Some("me".into()), Some("dev.x")), None);
    }
}
