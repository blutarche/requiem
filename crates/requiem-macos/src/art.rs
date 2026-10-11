//! Artwork bookkeeping without AppKit: URL sizing, the decoded-image cache and the request
//! queue.

use std::collections::{HashMap, HashSet, VecDeque};

/// Image hosts whose URLs end in Google's `=w…-h…-…` options, which take a size.
const SIZED_HOSTS: [&str; 2] = ["googleusercontent.com", "ggpht.com"];

/// `url` asking for a `px`-pixel square image, for Google image hosts; any other URL as it is.
///
/// The options are the `-`-separated part after the last `=` of the last path segment:
/// `w`, `h` and `s` sizes are replaced and the rest (quality, format, crop) kept. A URL with
/// no size gets one.
pub(crate) fn sized_url(url: &str, px: u32) -> String {
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
pub(crate) fn pixel_cost(w: usize, h: usize) -> usize {
    w.saturating_mul(h).saturating_mul(4)
}

struct Entry<V> {
    value: V,
    cost: usize,
    used: u64,
}

/// Values by key, least recently used evicted first once their costs pass `cap`. The newest
/// entry stays even if it alone is over.
pub(crate) struct Lru<V> {
    cap: usize,
    total: usize,
    clock: u64,
    entries: HashMap<String, Entry<V>>,
}

impl<V: Clone> Lru<V> {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            total: 0,
            clock: 0,
            entries: HashMap::new(),
        }
    }

    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.evict();
    }

    /// Marks `key` used.
    pub(crate) fn get(&mut self, key: &str) -> Option<V> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(entry.value.clone())
    }

    pub(crate) fn insert(&mut self, key: String, value: V, cost: usize) {
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

/// Keys waiting for the fetcher, newest first, at most `limit`: rows scrolled into view are
/// fetched before those scrolled past.
pub(crate) struct Queue {
    keys: VecDeque<String>,
    limit: usize,
}

impl Queue {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            keys: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// Returns the oldest key when `key` pushes it out.
    pub(crate) fn push(&mut self, key: String) -> Option<String> {
        self.keys.push_front(key);
        if self.keys.len() > self.limit {
            self.keys.pop_back()
        } else {
            None
        }
    }

    /// The newest key.
    pub(crate) fn pop(&mut self) -> Option<String> {
        self.keys.pop_front()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// Not loading yet: queue a fetch.
    Fetch,
    /// Already loading; the waiter joined it.
    Joined,
    /// Failed earlier this session; not fetched again.
    Failed,
}

/// Fetches in progress, with whoever waits on each, and the keys that failed.
pub(crate) struct Book<W> {
    waiting: HashMap<String, Vec<W>>,
    failed: HashSet<String>,
}

impl<W> Book<W> {
    pub(crate) fn new() -> Self {
        Self {
            waiting: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    /// For a key not in the cache.
    pub(crate) fn want(&mut self, key: &str, waiter: W) -> Want {
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

    /// The fetch of `key` ended; returns its waiters. A failure is remembered.
    pub(crate) fn finish(&mut self, key: &str, ok: bool) -> Vec<W> {
        if !ok {
            self.failed.insert(key.to_owned());
        }
        self.waiting.remove(key).unwrap_or_default()
    }

    /// `key` left the queue unfetched, so a later `want` fetches it.
    pub(crate) fn dropped(&mut self, key: &str) {
        self.waiting.remove(key);
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
        let mut q = Queue::new(2);
        assert_eq!(q.push("a".into()), None);
        assert_eq!(q.push("b".into()), None);
        assert_eq!(q.push("c".into()), Some("a".into()));
        assert_eq!(q.pop().as_deref(), Some("c"));
        assert_eq!(q.push("d".into()), None);
        assert_eq!(q.pop().as_deref(), Some("d"));
        assert_eq!(q.pop().as_deref(), Some("b"));
        assert_eq!(q.pop(), None);
    }
}
