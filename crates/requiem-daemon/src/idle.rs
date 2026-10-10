use std::time::{Duration, Instant};

/// Tracks how long the daemon has been idle. The clock is the caller's `now`.
pub struct IdleTimer {
    limit: Duration,
    idle_since: Option<Instant>,
}

impl IdleTimer {
    pub fn new(limit: Duration) -> Self {
        Self {
            limit,
            idle_since: None,
        }
    }

    /// Records whether the daemon is idle at `now`; true once it has been idle
    /// continuously for the limit.
    pub fn expired(&mut self, now: Instant, idle: bool) -> bool {
        if !idle {
            self.idle_since = None;
            return false;
        }
        let since = *self.idle_since.get_or_insert(now);
        now.saturating_duration_since(since) >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(300);

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    #[test]
    fn expires_after_the_limit_of_continuous_idle() {
        let t0 = Instant::now();
        let mut t = IdleTimer::new(LIMIT);
        assert!(!t.expired(at(t0, 0), true));
        assert!(!t.expired(at(t0, 299), true));
        assert!(t.expired(at(t0, 300), true));
        assert!(t.expired(at(t0, 400), true));
    }

    #[test]
    fn activity_restarts_the_clock() {
        let t0 = Instant::now();
        let mut t = IdleTimer::new(LIMIT);
        assert!(!t.expired(at(t0, 0), true));
        assert!(!t.expired(at(t0, 200), false));
        assert!(!t.expired(at(t0, 250), true));
        assert!(!t.expired(at(t0, 549), true));
        assert!(t.expired(at(t0, 550), true));
    }

    #[test]
    fn never_expires_while_busy() {
        let t0 = Instant::now();
        let mut t = IdleTimer::new(LIMIT);
        for s in (0..2000).step_by(5) {
            assert!(!t.expired(at(t0, s), false));
        }
    }

    #[test]
    fn zero_limit_expires_on_the_first_idle_sample() {
        let t0 = Instant::now();
        let mut t = IdleTimer::new(Duration::ZERO);
        assert!(!t.expired(t0, false));
        assert!(t.expired(t0, true));
    }
}
