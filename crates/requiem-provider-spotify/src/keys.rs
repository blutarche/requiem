//! Audio-key pacing. Spotify rate-limits key requests and answers a burst with failures that
//! last minutes (librespot #1649), so requests go one at a time and spaced out, and a run of
//! failures stops them for a while.

use std::collections::{HashMap, VecDeque};
use std::fmt::Display;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use requiem_core::ProviderError;

/// Measured from the end of the previous request.
pub(crate) const KEY_SPACING: Duration = Duration::from_secs(2);
/// After a failed request, the waits before the second and the third attempt.
pub(crate) const KEY_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_secs(3), Duration::from_secs(10)];
pub(crate) const BREAKER_FAILURES: u32 = 5;
pub(crate) const BREAKER_PAUSE: Duration = Duration::from_secs(10 * 60);
pub(crate) const KEY_CACHE_LEN: usize = 512;

pub(crate) const REFUSED: &str = "Spotify refused the audio key";
pub(crate) const PAUSED: &str = "Spotify is refusing audio keys; try again in a few minutes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Go,
    Wait(Duration),
    /// The breaker is open: no requests until it closes.
    Paused,
}

/// When the next key request may go. After the pause one request is let through; another
/// failure pauses again at once, a success closes the breaker.
#[derive(Debug, Default)]
pub(crate) struct KeyPolicy {
    last_done: Option<Instant>,
    failures: u32,
    paused_until: Option<Instant>,
}

impl KeyPolicy {
    pub(crate) fn gate(&self, now: Instant) -> Gate {
        if self.paused_until.is_some_and(|until| now < until) {
            return Gate::Paused;
        }
        match self.last_done.map(|t| t + KEY_SPACING) {
            Some(due) if now < due => Gate::Wait(due - now),
            _ => Gate::Go,
        }
    }

    pub(crate) fn record(&mut self, now: Instant, ok: bool) {
        self.last_done = Some(now);
        if ok {
            self.failures = 0;
            self.paused_until = None;
        } else {
            self.failures += 1;
            if self.failures >= BREAKER_FAILURES {
                self.paused_until = Some(now + BREAKER_PAUSE);
            }
        }
    }
}

/// Oldest entry out first.
struct KeyCache<I, K> {
    map: HashMap<I, K>,
    order: VecDeque<I>,
}

impl<I: Hash + Eq + Clone, K: Clone> KeyCache<I, K> {
    fn get(&self, id: &I) -> Option<K> {
        self.map.get(id).cloned()
    }

    fn insert(&mut self, id: I, key: K) {
        if self.map.insert(id.clone(), key).is_none() {
            self.order.push_back(id);
            if self.order.len() > KEY_CACHE_LEN
                && let Some(old) = self.order.pop_front()
            {
                self.map.remove(&old);
            }
        }
    }
}

/// Keys for the helper's lifetime, fetched under [`KeyPolicy`].
pub(crate) struct Keys<I, K> {
    cache: Mutex<KeyCache<I, K>>,
    /// Held across a whole fetch, retries included, so at most one request is in flight.
    policy: tokio::sync::Mutex<KeyPolicy>,
}

impl<I: Hash + Eq + Clone, K: Clone> Keys<I, K> {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(KeyCache {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            policy: tokio::sync::Mutex::new(KeyPolicy::default()),
        }
    }

    fn cached(&self, id: &I) -> Option<K> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
    }

    /// The cached key, or one from `request`. Never logs the key.
    pub(crate) async fn get<F, Fut, E>(&self, id: I, mut request: F) -> Result<K, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<K, E>>,
        E: Display,
    {
        if let Some(k) = self.cached(&id) {
            return Ok(k);
        }
        let mut policy = self.policy.lock().await;
        // Another fetch of the same key may have finished while this one waited.
        if let Some(k) = self.cached(&id) {
            return Ok(k);
        }
        let now = || tokio::time::Instant::now().into_std();
        let mut retries = KEY_RETRY_DELAYS.iter();
        loop {
            loop {
                match policy.gate(now()) {
                    Gate::Go => break,
                    Gate::Wait(d) => tokio::time::sleep(d).await,
                    Gate::Paused => return Err(ProviderError::Unplayable(PAUSED.into())),
                }
            }
            let result = request().await;
            policy.record(now(), result.is_ok());
            match result {
                Ok(k) => {
                    self.cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(id, k.clone());
                    return Ok(k);
                }
                Err(e) => {
                    eprintln!("[spotify] audio key request failed: {e}");
                    if policy.gate(now()) == Gate::Paused {
                        return Err(ProviderError::Unplayable(PAUSED.into()));
                    }
                    match retries.next() {
                        Some(d) => tokio::time::sleep(*d).await,
                        None => return Err(ProviderError::Unplayable(REFUSED.into())),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::time::Instant as TokioInstant;

    use super::*;

    #[test]
    fn spacing_counts_from_the_end_of_the_last_request() {
        let t0 = Instant::now();
        let mut p = KeyPolicy::default();
        assert_eq!(p.gate(t0), Gate::Go);
        p.record(t0, true);
        assert_eq!(p.gate(t0), Gate::Wait(KEY_SPACING));
        assert_eq!(
            p.gate(t0 + Duration::from_millis(1500)),
            Gate::Wait(Duration::from_millis(500))
        );
        assert_eq!(p.gate(t0 + KEY_SPACING), Gate::Go);
    }

    #[test]
    fn five_failures_in_a_row_pause_for_ten_minutes() {
        let t0 = Instant::now();
        let mut p = KeyPolicy::default();
        let mut t = t0;
        for _ in 0..BREAKER_FAILURES - 1 {
            p.record(t, false);
            t += KEY_SPACING;
            assert_eq!(p.gate(t), Gate::Go);
        }
        // A success in between starts the count again.
        p.record(t, true);
        for _ in 0..BREAKER_FAILURES - 1 {
            t += KEY_SPACING;
            p.record(t, false);
        }
        assert_eq!(p.gate(t + KEY_SPACING), Gate::Go);
        p.record(t, false);
        assert_eq!(p.gate(t + KEY_SPACING), Gate::Paused);
        assert_eq!(
            p.gate(t + BREAKER_PAUSE - Duration::from_millis(1)),
            Gate::Paused
        );
        // Half open: one request goes; failing it pauses again at once.
        let reopened = t + BREAKER_PAUSE;
        assert_eq!(p.gate(reopened), Gate::Go);
        p.record(reopened, false);
        assert_eq!(p.gate(reopened + KEY_SPACING), Gate::Paused);
        let again = reopened + BREAKER_PAUSE;
        assert_eq!(p.gate(again), Gate::Go);
        p.record(again, true);
        assert_eq!(p.gate(again + KEY_SPACING), Gate::Go);
        p.record(again + KEY_SPACING, false);
        assert_eq!(p.gate(again + KEY_SPACING * 2), Gate::Go);
    }

    /// Answers from a script, recording when each request was made.
    struct Script {
        answers: Mutex<VecDeque<bool>>,
        times: Mutex<Vec<TokioInstant>>,
        calls: AtomicU32,
    }

    impl Script {
        fn new(answers: &[bool]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.iter().copied().collect()),
                times: Mutex::new(Vec::new()),
                calls: AtomicU32::new(0),
            })
        }

        async fn request(&self) -> Result<u8, &'static str> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.times.lock().unwrap().push(TokioInstant::now());
            match self.answers.lock().unwrap().pop_front() {
                Some(true) => Ok(7),
                _ => Err("refused"),
            }
        }

        fn gaps(&self, start: TokioInstant) -> Vec<Duration> {
            let times = self.times.lock().unwrap();
            let mut prev = start;
            times
                .iter()
                .map(|t| {
                    let d = *t - prev;
                    prev = *t;
                    d
                })
                .collect()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_after_three_then_ten_seconds_then_gives_up() {
        let keys = Keys::<u32, u8>::new();
        let s = Script::new(&[false, false, false]);
        let start = TokioInstant::now();
        let e = keys.get(1, || s.request()).await.unwrap_err();
        assert_eq!(e, ProviderError::Unplayable(REFUSED.into()));
        assert_eq!(
            s.gaps(start),
            [Duration::ZERO, KEY_RETRY_DELAYS[0], KEY_RETRY_DELAYS[1]]
        );

        // A retry that works is cached: asking again sends nothing.
        let s = Script::new(&[false, true]);
        assert_eq!(keys.get(2, || s.request()).await, Ok(7));
        assert_eq!(keys.get(2, || s.request()).await, Ok(7));
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_go_one_at_a_time_two_seconds_apart() {
        let keys = Arc::new(Keys::<u32, u8>::new());
        let s = Script::new(&[true; 3]);
        let start = TokioInstant::now();
        let mut tasks = Vec::new();
        for id in 0..3 {
            let keys = keys.clone();
            let s = s.clone();
            tasks.push(tokio::spawn(
                async move { keys.get(id, || s.request()).await },
            ));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), Ok(7));
        }
        assert_eq!(s.gaps(start), [Duration::ZERO, KEY_SPACING, KEY_SPACING]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_breaker_answers_without_asking_then_lets_one_through() {
        let keys = Keys::<u32, u8>::new();
        // Two tracks fail all three attempts: the fifth failure opens the breaker mid-retry.
        let s = Script::new(&[false; 5]);
        let e = keys.get(1, || s.request()).await.unwrap_err();
        assert_eq!(e, ProviderError::Unplayable(REFUSED.into()));
        let opened = keys.get(2, || s.request()).await.unwrap_err();
        assert_eq!(opened, ProviderError::Unplayable(PAUSED.into()));
        assert_eq!(s.calls.load(Ordering::SeqCst), 5);
        let paused_at = TokioInstant::now();

        let s = Script::new(&[true, true]);
        let e = keys.get(3, || s.request()).await.unwrap_err();
        assert_eq!(e, ProviderError::Unplayable(PAUSED.into()));
        assert_eq!(s.calls.load(Ordering::SeqCst), 0);

        tokio::time::sleep_until(paused_at + BREAKER_PAUSE).await;
        assert_eq!(keys.get(3, || s.request()).await, Ok(7));
        assert_eq!(keys.get(4, || s.request()).await, Ok(7));
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_cache_drops_its_oldest_key() {
        let mut c = KeyCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        };
        for i in 0..KEY_CACHE_LEN as u32 {
            c.insert(i, i);
        }
        c.insert(0, 99);
        assert_eq!(c.get(&0), Some(99));
        c.insert(KEY_CACHE_LEN as u32, 0);
        assert_eq!(c.get(&0), None);
        assert_eq!(c.get(&1), Some(1));
        assert_eq!(c.map.len(), KEY_CACHE_LEN);
        assert_eq!(c.order.len(), KEY_CACHE_LEN);
    }
}
