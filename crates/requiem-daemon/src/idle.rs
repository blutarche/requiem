use std::collections::HashSet;
use std::time::{Duration, Instant};

use requiem_core::{AccountState, Event, PlayState, ProviderId};

/// What keeps the daemon up without clients: playback, and sign-ins waiting for the user
/// to enter their code on another device.
#[derive(Debug, Default)]
pub struct Activity {
    playing: bool,
    signing_in: HashSet<ProviderId>,
}

impl Activity {
    /// The daemon starts `Stopped`.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, ev: &Event) {
        match ev {
            Event::State(s) => self.playing = *s != PlayState::Stopped,
            Event::Account { provider, state } => {
                if matches!(state, AccountState::Pending { .. }) {
                    self.signing_in.insert(provider.clone());
                } else {
                    self.signing_in.remove(provider);
                }
            }
            _ => {}
        }
    }

    pub fn idle(&self) -> bool {
        !self.playing && self.signing_in.is_empty()
    }
}

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

    fn account(state: AccountState) -> Event {
        Event::Account {
            provider: ProviderId("spotify".into()),
            state,
        }
    }

    fn pending() -> AccountState {
        AccountState::Pending {
            verification_url: "https://example.com/device".into(),
            user_code: "GQVQ-JKEC".into(),
            expires_at_unix: 1_800_000_000,
        }
    }

    #[test]
    fn a_pending_sign_in_holds_off_the_idle_exit() {
        let t0 = Instant::now();
        let mut a = Activity::new();
        let mut t = IdleTimer::new(LIMIT);
        a.observe(&account(AccountState::SignedOut));
        assert!(a.idle());
        a.observe(&account(pending()));
        for s in (0..2000).step_by(5) {
            assert!(!t.expired(at(t0, s), a.idle()), "expired at {s}s");
        }
        a.observe(&account(AccountState::SignedIn { name: None }));
        assert!(!t.expired(at(t0, 2000), a.idle()));
        assert!(t.expired(at(t0, 2300), a.idle()));
    }

    #[test]
    fn playback_and_sign_in_each_keep_it_busy() {
        let mut a = Activity::new();
        a.observe(&Event::State(PlayState::Playing));
        a.observe(&account(pending()));
        a.observe(&Event::State(PlayState::Stopped));
        assert!(!a.idle());
        a.observe(&account(AccountState::SignedOut));
        assert!(a.idle());
        a.observe(&Event::State(PlayState::Paused));
        assert!(!a.idle());
    }

    #[test]
    fn zero_limit_expires_on_the_first_idle_sample() {
        let t0 = Instant::now();
        let mut t = IdleTimer::new(Duration::ZERO);
        assert!(!t.expired(t0, false));
        assert!(t.expired(t0, true));
    }
}
