use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use serde::{Deserialize, Serialize};

use crate::{
    AccountState, AudioSource, Command, Engine, EngineEvent, EngineEventSink, EnqueueMode, Event,
    FailureKind, Page, PlayState, Provider, ProviderError, ProviderId, SeekTarget, Shelf, Track,
    TrackId,
};

/// What a restart restores. `resume` is the queue index Play would start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistState {
    pub tracks: Vec<Track>,
    pub resume: Option<usize>,
    pub volume: u8,
}

/// Called on the session thread; must not block.
pub type PersistSink = Arc<dyn Fn(PersistState) + Send + Sync>;

#[derive(Clone)]
pub struct SessionConfig {
    pub resolve_retry_delay: Duration,
    pub search_limit: usize,
    pub max_consecutive_failures: u32,
    pub prev_restart_threshold: Duration,
    /// A subscriber with more queued events than this stops receiving `Position`.
    pub subscriber_position_backlog: usize,
    /// Called with a fresh state after every queue, volume or play-state event.
    pub persist: Option<PersistSink>,
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionConfig")
            .field("resolve_retry_delay", &self.resolve_retry_delay)
            .field("search_limit", &self.search_limit)
            .field("max_consecutive_failures", &self.max_consecutive_failures)
            .field("prev_restart_threshold", &self.prev_restart_threshold)
            .field(
                "subscriber_position_backlog",
                &self.subscriber_position_backlog,
            )
            .field("persist", &self.persist.is_some())
            .finish()
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            resolve_retry_delay: Duration::from_secs(1),
            search_limit: 20,
            max_consecutive_failures: 3,
            prev_restart_threshold: Duration::from_secs(3),
            subscriber_position_backlog: 64,
            persist: None,
        }
    }
}

enum Purpose {
    Start {
        r#gen: u64,
    },
    Pre {
        version: u64,
        index: usize,
        track: TrackId,
    },
}

enum Msg {
    Command(Command),
    /// The second sender is dropped once the subscriber has been sent the account states.
    Subscribe(Sender<Event>, Sender<()>),
    Engine(EngineEvent),
    Searched {
        req: u64,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// `accounts` are the queried providers' states after the browse, which may have signed it out.
    Browsed {
        req: u64,
        page: Page,
        result: Result<Vec<Shelf>, ProviderError>,
        accounts: Vec<(ProviderId, AccountState)>,
    },
    Account {
        provider: ProviderId,
        state: AccountState,
    },
    SignOutDone {
        provider: ProviderId,
        result: Result<(), ProviderError>,
        state: AccountState,
    },
    SignInDone {
        provider: ProviderId,
        result: Result<(), ProviderError>,
        state: AccountState,
    },
    Resolved {
        purpose: Purpose,
        result: Result<AudioSource, ProviderError>,
    },
}

pub struct Session {
    tx: Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl Session {
    pub fn spawn(
        providers: Vec<Arc<dyn Provider>>,
        make_engine: impl FnOnce(EngineEventSink) -> Box<dyn Engine> + Send + 'static,
        config: SessionConfig,
    ) -> Session {
        let (tx, rx) = unbounded();
        let actor_tx = tx.clone();
        let handle = thread::Builder::new()
            .name("requiem-session".into())
            .spawn(move || {
                let sink_tx = actor_tx.clone();
                let sink: EngineEventSink = Arc::new(move |ev| {
                    let _ = sink_tx.send(Msg::Engine(ev));
                });
                Actor::new(providers, make_engine(sink), config, actor_tx).run(rx);
            })
            .expect("spawn session thread");
        Session {
            tx,
            handle: Some(handle),
        }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Command(cmd));
    }

    /// Returns once the session has queued every provider's `Account` on the new receiver,
    /// ahead of anything else.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = unbounded();
        let (done_tx, done) = bounded(0);
        if self.tx.send(Msg::Subscribe(tx, done_tx)).is_ok() {
            let _ = done.recv();
        }
        rx
    }

    /// Workers blocked in a provider are detached and not waited for.
    pub fn shutdown(mut self) {
        self.send(Command::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.send(Command::Shutdown);
        }
    }
}

enum PreState {
    Pending,
    Ready(AudioSource),
}

struct PreSlot {
    version: u64,
    index: usize,
    track: TrackId,
    state: PreState,
    /// Load generation that started this index while the pre-resolve was in flight.
    waiting: Option<u64>,
}

impl PreSlot {
    fn matches(&self, version: u64, index: usize, track: &TrackId) -> bool {
        self.version == version && self.index == index && self.track == *track
    }
}

struct Actor {
    providers: Vec<Arc<dyn Provider>>,
    engine: Box<dyn Engine>,
    config: SessionConfig,
    tx: Sender<Msg>,
    subs: Vec<Sender<Event>>,

    queue: Vec<Track>,
    current: Option<usize>,
    /// `current` was set by `Cue` and nothing is loaded: Play starts it rather than its successor.
    cued: bool,
    /// Track handed to the engine; outlives a queue `Replace`.
    playing: Option<Track>,
    queue_version: u64,
    r#gen: u64,
    /// The engine holds this generation's track.
    loaded: bool,
    started: bool,
    state: PlayState,
    volume: u8,
    last_pos: Duration,
    duration: Option<Duration>,
    pre: Option<PreSlot>,

    strikes: u32,
    stream_strikes: u32,
    last_parse_failure: Option<TrackId>,

    /// The last state seen for each provider, in provider order.
    accounts: Vec<(ProviderId, AccountState)>,
    /// Providers with a sign-in worker running.
    signing_in: HashSet<ProviderId>,
    /// Providers whose running sign-in a `SignOut` ended: its failure is not an error.
    signed_out_while_signing_in: HashSet<ProviderId>,
}

/// The provider's own words, except that a network failure keeps its "network error" prefix.
fn sign_in_error(e: &ProviderError) -> String {
    match e {
        ProviderError::Network(_) => e.to_string(),
        ProviderError::Unplayable(m)
        | ProviderError::ClientBroken(m)
        | ProviderError::Parse(m)
        | ProviderError::SignInRequired(m) => m.clone(),
    }
}

/// Runs `call` on every provider in parallel and waits for all. Results are concatenated in
/// provider order; a sign-in failure is skipped silently and any other is logged. It fails only
/// when none succeeded, with the first error that isn't a sign-in error, else a sign-in error.
fn fan_out<T: Send + 'static>(
    what: &'static str,
    providers: Vec<Arc<dyn Provider>>,
    call: impl Fn(&dyn Provider) -> Result<Vec<T>, ProviderError> + Send + Sync + 'static,
) -> Result<Vec<T>, ProviderError> {
    let call = Arc::new(call);
    let workers: Vec<_> = providers
        .into_iter()
        .map(|p| {
            let call = call.clone();
            thread::spawn(move || (p.id(), call(p.as_ref())))
        })
        .collect();
    let mut merged = Vec::new();
    let mut succeeded = false;
    let mut hard = None;
    let mut sign_in = None;
    for worker in workers {
        let Ok((id, result)) = worker.join() else {
            eprintln!("requiem: {what} worker panicked");
            continue;
        };
        match result {
            Ok(items) => {
                succeeded = true;
                merged.extend(items);
            }
            Err(e @ ProviderError::SignInRequired(_)) => {
                sign_in.get_or_insert(e);
            }
            Err(e) => {
                eprintln!("requiem: {what} on {} failed: {e}", id.0);
                hard.get_or_insert(e);
            }
        }
    }
    if succeeded {
        return Ok(merged);
    }
    Err(hard
        .or(sign_in)
        .unwrap_or_else(|| ProviderError::Unplayable("no provider answered".into())))
}

fn resolve_with_retry(
    provider: &dyn Provider,
    track: &TrackId,
    delay: Duration,
) -> Result<AudioSource, ProviderError> {
    match provider.resolve(track) {
        Err(ProviderError::Network(_)) => {
            thread::sleep(delay);
            provider.resolve(track)
        }
        r => r,
    }
}

impl Actor {
    fn new(
        providers: Vec<Arc<dyn Provider>>,
        engine: Box<dyn Engine>,
        config: SessionConfig,
        tx: Sender<Msg>,
    ) -> Self {
        let accounts = providers.iter().map(|p| (p.id(), p.account())).collect();
        Self {
            providers,
            engine,
            config,
            tx,
            subs: Vec::new(),
            queue: Vec::new(),
            current: None,
            cued: false,
            playing: None,
            queue_version: 0,
            r#gen: 0,
            loaded: false,
            started: false,
            state: PlayState::Stopped,
            volume: 100,
            last_pos: Duration::ZERO,
            duration: None,
            pre: None,
            strikes: 0,
            stream_strikes: 0,
            last_parse_failure: None,
            accounts,
            signing_in: HashSet::new(),
            signed_out_while_signing_in: HashSet::new(),
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Command(Command::Shutdown) => {
                    self.engine.stop();
                    return;
                }
                Msg::Command(cmd) => self.on_command(cmd),
                Msg::Subscribe(s, _done) => {
                    for (provider, state) in &self.accounts {
                        let _ = s.send(Event::Account {
                            provider: provider.clone(),
                            state: state.clone(),
                        });
                    }
                    self.subs.push(s);
                }
                Msg::Engine(ev) => self.on_engine(ev),
                Msg::Searched { req, result } => match result {
                    Ok(tracks) => self.emit(Event::SearchResults { req, tracks }),
                    Err(e) => self.emit(Event::SearchFailed {
                        req,
                        error: e.to_string(),
                    }),
                },
                Msg::Resolved { purpose, result } => self.on_resolved(purpose, result),
                Msg::Browsed {
                    req,
                    page,
                    result,
                    accounts,
                } => {
                    match result {
                        Ok(shelves) => self.emit(Event::BrowseResults { req, page, shelves }),
                        Err(e) => self.emit(Event::BrowseFailed {
                            req,
                            sign_in_required: matches!(e, ProviderError::SignInRequired(_)),
                            error: e.to_string(),
                        }),
                    }
                    for (provider, state) in accounts {
                        self.set_account(provider, state, false);
                    }
                }
                Msg::Account { provider, state } => self.set_account(provider, state, false),
                Msg::SignOutDone {
                    provider,
                    result,
                    state,
                } => {
                    if let Err(e) = result {
                        self.emit(Event::AccountError {
                            provider: provider.clone(),
                            error: sign_in_error(&e),
                        });
                    }
                    // Announced even when it leaves the state as it was, so a client waiting on
                    // its `SignOut` hears the outcome.
                    self.set_account(provider, state, true);
                }
                Msg::SignInDone {
                    provider,
                    result,
                    state,
                } => {
                    self.signing_in.remove(&provider);
                    let signed_out = self.signed_out_while_signing_in.remove(&provider);
                    let failed = result.is_err();
                    if let Err(e) = result
                        && !signed_out
                    {
                        self.emit(Event::AccountError {
                            provider: provider.clone(),
                            error: sign_in_error(&e),
                        });
                    }
                    // A failure is announced even when it leaves the state as it was, so a
                    // client waiting on its `SignIn` hears the outcome; a success already was.
                    self.set_account(provider, state, failed);
                }
            }
        }
    }

    fn emit(&mut self, ev: Event) {
        let is_position = matches!(ev, Event::Position(_));
        let backlog = self.config.subscriber_position_backlog;
        self.subs.retain(|s| {
            if is_position && s.len() > backlog {
                return true;
            }
            s.send(ev.clone()).is_ok()
        });
        if matches!(
            ev,
            Event::QueueChanged { .. } | Event::Volume(_) | Event::State(_)
        ) && let Some(sink) = &self.config.persist
        {
            sink(PersistState {
                tracks: self.queue.clone(),
                resume: self.resume_index(),
                volume: self.volume,
            });
        }
    }

    /// The index Play would start.
    fn resume_index(&self) -> Option<usize> {
        if self.playing.is_some() || self.state != PlayState::Stopped {
            return self.current;
        }
        if self.queue.is_empty() {
            return None;
        }
        if self.cued
            && let Some(c) = self.current.filter(|c| *c < self.queue.len())
        {
            return Some(c);
        }
        let next = self.current.map_or(0, |c| c + 1);
        Some(if next < self.queue.len() { next } else { 0 })
    }

    fn set_state(&mut self, state: PlayState) {
        self.state = state;
        self.emit(Event::State(state));
    }

    fn emit_queue(&mut self) {
        let ev = Event::QueueChanged {
            tracks: self.queue.clone(),
            current: self.current,
        };
        self.emit(ev);
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Search { req, query } => self.search(req, query),
            Command::Enqueue { tracks, mode } => {
                match mode {
                    EnqueueMode::Replace => {
                        self.queue = tracks;
                        self.current = None;
                        self.cued = false;
                    }
                    EnqueueMode::Append => self.queue.extend(tracks),
                }
                self.queue_version += 1;
                self.drop_pre();
                self.emit_queue();
                if self.started {
                    self.ensure_pre_resolve();
                }
            }
            Command::PlayIndex(i) => {
                if i < self.queue.len() {
                    self.play(i);
                }
            }
            Command::Cue(i) => {
                if self.state == PlayState::Stopped
                    && self.playing.is_none()
                    && i < self.queue.len()
                {
                    self.current = Some(i);
                    self.cued = true;
                    self.emit_queue();
                }
            }
            Command::Remove(i) => self.remove(i),
            Command::Move { from, to } => self.move_track(from, to),
            Command::Clear => {
                self.queue.clear();
                self.current = None;
                self.cued = false;
                self.queue_version += 1;
                self.stop();
                self.emit_queue();
            }
            Command::Next => {
                let next = self.current.map_or(0, |c| c + 1);
                if next < self.queue.len() {
                    self.play(next);
                } else {
                    self.stop();
                }
            }
            Command::Prev => match self.current {
                Some(c) if c > 0 && self.last_pos <= self.config.prev_restart_threshold => {
                    self.play(c - 1)
                }
                _ => self.seek_to(Duration::ZERO),
            },
            Command::Pause => self.pause(),
            Command::Resume => self.resume(),
            Command::TogglePause => match self.state {
                PlayState::Playing | PlayState::Buffering => self.pause(),
                PlayState::Paused | PlayState::Stopped => self.resume(),
            },
            Command::Seek(target) => {
                let to = match target {
                    SeekTarget::Absolute(d) => d,
                    SeekTarget::Relative(s) => {
                        let step = Duration::from_secs(s.unsigned_abs());
                        let t = if s >= 0 {
                            self.last_pos.saturating_add(step)
                        } else {
                            self.last_pos.saturating_sub(step)
                        };
                        self.duration.map_or(t, |d| t.min(d))
                    }
                };
                self.seek_to(to);
            }
            Command::SetVolume(v) => {
                self.volume = v.min(100);
                self.engine.set_volume(f32::from(self.volume) / 100.0);
                self.emit(Event::Volume(self.volume));
            }
            Command::Browse { req, page } => self.browse(req, page),
            Command::SignIn { provider } => self.sign_in(provider),
            Command::SignOut { provider } => self.sign_out(provider),
            Command::Shutdown => unreachable!("handled in run"),
        }
    }

    fn remove(&mut self, i: usize) {
        if i >= self.queue.len() {
            return;
        }
        self.queue.remove(i);
        self.queue_version += 1;
        match self.current {
            Some(c) if c == i => {
                self.pre = None;
                if i >= self.queue.len() {
                    self.current = None;
                    self.stop();
                    self.emit_queue();
                } else if matches!(self.state, PlayState::Playing | PlayState::Buffering) {
                    self.start(i);
                } else if self.cued {
                    // The successor now sits at `i` and takes over the cue.
                    self.emit_queue();
                } else {
                    // A paused session stops with the cursor just before the successor: `Next`
                    // then plays it.
                    self.current = i.checked_sub(1);
                    if self.state != PlayState::Stopped {
                        self.stop();
                    }
                    self.emit_queue();
                }
            }
            cur => {
                if let Some(c) = cur.filter(|c| i < *c) {
                    self.current = Some(c - 1);
                }
                self.drop_pre();
                self.emit_queue();
                if self.started {
                    self.ensure_pre_resolve();
                }
            }
        }
    }

    fn move_track(&mut self, from: usize, to: usize) {
        let len = self.queue.len();
        if from >= len || to >= len || from == to {
            return;
        }
        let track = self.queue.remove(from);
        self.queue.insert(to, track);
        self.current = self.current.map(|c| {
            if c == from {
                to
            } else if from < c && c <= to {
                c - 1
            } else if to <= c && c < from {
                c + 1
            } else {
                c
            }
        });
        self.queue_version += 1;
        self.drop_pre();
        self.emit_queue();
        if self.started {
            self.ensure_pre_resolve();
        }
    }

    fn search(&mut self, req: u64, query: String) {
        if self.providers.is_empty() {
            self.emit(Event::SearchFailed {
                req,
                error: "no provider configured".into(),
            });
            return;
        }
        let providers = self.providers.clone();
        let tx = self.tx.clone();
        let limit = self.config.search_limit;
        thread::spawn(move || {
            let result = fan_out("search", providers, move |p| p.search(&query, limit));
            let _ = tx.send(Msg::Searched { req, result });
        });
    }

    fn provider(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == *id).cloned()
    }

    fn browse(&mut self, req: u64, page: Page) {
        let targets: Vec<Arc<dyn Provider>> = match &page {
            Page::Collection(c) => self.provider(&c.provider).into_iter().collect(),
            Page::Home => self
                .providers
                .iter()
                .find(|p| p.has_home())
                .cloned()
                .into_iter()
                .collect(),
            Page::Library => {
                let signed_in: Vec<_> = self
                    .providers
                    .iter()
                    .filter(|p| {
                        matches!(
                            self.account_of(&p.id()),
                            Some(AccountState::SignedIn { .. })
                        )
                    })
                    .cloned()
                    .collect();
                if signed_in.is_empty() {
                    let e = ProviderError::SignInRequired("sign in to browse your library".into());
                    self.emit(Event::BrowseFailed {
                        req,
                        error: e.to_string(),
                        sign_in_required: true,
                    });
                    return;
                }
                signed_in
            }
        };
        if targets.is_empty() {
            self.emit(Event::BrowseFailed {
                req,
                error: "no provider for this page".into(),
                sign_in_required: false,
            });
            return;
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = fan_out("browse", targets.clone(), {
                let page = page.clone();
                move |p| p.browse(&page)
            });
            let accounts = targets.iter().map(|p| (p.id(), p.account())).collect();
            let _ = tx.send(Msg::Browsed {
                req,
                page,
                result,
                accounts,
            });
        });
    }

    fn account_of(&self, provider: &ProviderId) -> Option<&AccountState> {
        self.accounts
            .iter()
            .find(|(p, _)| p == provider)
            .map(|(_, s)| s)
    }

    /// Records `state` and announces it if it changed, or always with `force`.
    fn set_account(&mut self, provider: ProviderId, state: AccountState, force: bool) {
        match self.accounts.iter_mut().find(|(p, _)| *p == provider) {
            Some((_, known)) if *known == state && !force => return,
            Some((_, known)) => *known = state.clone(),
            None => self.accounts.push((provider.clone(), state.clone())),
        }
        self.emit(Event::Account { provider, state });
    }

    fn sign_in(&mut self, id: ProviderId) {
        let Some(provider) = self.provider(&id) else {
            self.emit(Event::AccountError {
                provider: id.clone(),
                error: format!("no provider {}", id.0),
            });
            return;
        };
        let current = self.account_of(&id).cloned();
        if self.signing_in.contains(&id) {
            // Before the code arrives there is nothing to repeat; its `Pending` reaches everyone.
            if let Some(state @ AccountState::Pending { .. }) = current {
                self.emit(Event::Account {
                    provider: id,
                    state,
                });
            }
            return;
        }
        if let Some(state @ AccountState::SignedIn { .. }) = current {
            self.emit(Event::Account {
                provider: id,
                state,
            });
            return;
        }
        self.signing_in.insert(id.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let report = tx.clone();
            let reporter = id.clone();
            let result = provider.sign_in(&move |state| {
                let _ = report.send(Msg::Account {
                    provider: reporter.clone(),
                    state,
                });
            });
            let state = provider.account();
            let _ = tx.send(Msg::SignInDone {
                provider: id,
                result,
                state,
            });
        });
    }

    fn sign_out(&mut self, id: ProviderId) {
        let Some(provider) = self.provider(&id) else {
            self.emit(Event::AccountError {
                provider: id.clone(),
                error: format!("no provider {}", id.0),
            });
            return;
        };
        if self.signing_in.contains(&id) {
            self.signed_out_while_signing_in.insert(id.clone());
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = provider.sign_out();
            let state = provider.account();
            let _ = tx.send(Msg::SignOutDone {
                provider: id,
                result,
                state,
            });
        });
    }

    fn pause(&mut self) {
        if matches!(self.state, PlayState::Playing | PlayState::Buffering) {
            self.engine.pause();
            self.set_state(PlayState::Paused);
        }
    }

    fn resume(&mut self) {
        match self.state {
            PlayState::Paused if self.started => {
                self.engine.resume();
                self.set_state(PlayState::Playing);
            }
            // Still loading: `Started` moves on to `Playing`. Before the load, the engine
            // holds only the previous track, which stays paused.
            PlayState::Paused => {
                if self.loaded {
                    self.engine.resume();
                }
                self.set_state(PlayState::Buffering);
            }
            // The cursor is the last track played, or sits just before the next one after
            // the current track was removed; a finished queue starts again from the top.
            PlayState::Stopped if !self.queue.is_empty() => {
                let next = self.current.map_or(0, |c| c + 1);
                let cued = self.current.filter(|c| self.cued && *c < self.queue.len());
                self.start(cued.unwrap_or(if next < self.queue.len() { next } else { 0 }));
            }
            _ => {}
        }
    }

    fn seek_to(&mut self, to: Duration) {
        if self.playing.is_some() {
            let to = self.duration.map_or(to, |d| to.min(d));
            self.engine.seek(to);
            self.last_pos = to;
            // The engine reports position only while playing; a paused seek would otherwise
            // leave clients showing the old time until resume.
            self.emit(Event::Position(to));
        }
    }

    fn stop(&mut self) {
        self.engine.stop();
        self.r#gen += 1;
        self.loaded = false;
        self.started = false;
        self.pre = None;
        self.playing = None;
        self.cued = false;
        self.strikes = 0;
        self.stream_strikes = 0;
        self.last_parse_failure = None;
        self.set_state(PlayState::Stopped);
    }

    /// Starts `idx` because the user picked it, which ends a pause.
    fn play(&mut self, idx: usize) {
        if self.state == PlayState::Paused {
            self.set_state(PlayState::Buffering);
        }
        self.start(idx);
    }

    /// Keeps a pause: a track moved to on its own loads paused.
    fn start(&mut self, idx: usize) {
        let Some(track) = self.queue.get(idx).cloned() else {
            self.stop();
            return;
        };
        self.current = Some(idx);
        self.cued = false;
        self.playing = Some(track.clone());
        self.r#gen += 1;
        self.loaded = false;
        self.started = false;
        self.last_pos = Duration::ZERO;
        self.duration = None;
        self.emit_queue();
        if self.state == PlayState::Stopped {
            self.set_state(PlayState::Buffering);
        }

        let r#gen = self.r#gen;
        match self.pre.take() {
            Some(mut slot) if slot.matches(self.queue_version, idx, &track.id) => {
                match slot.state {
                    PreState::Ready(src) => self.load(r#gen, src),
                    PreState::Pending => {
                        slot.waiting = Some(r#gen);
                        self.pre = Some(slot);
                    }
                }
            }
            _ => self.spawn_resolve(track.id, Purpose::Start { r#gen }),
        }
    }

    /// Loads paused if the user paused while the track resolved.
    fn load(&mut self, r#gen: u64, src: AudioSource) {
        self.loaded = true;
        self.engine
            .load(r#gen, src, self.state == PlayState::Paused);
    }

    /// Discards the pre-resolve slot. A start already waiting on it is resolved afresh.
    fn drop_pre(&mut self) {
        let Some(slot) = self.pre.take() else { return };
        if let (Some(r#gen), Some(track)) = (slot.waiting, self.playing.as_ref())
            && r#gen == self.r#gen
        {
            self.spawn_resolve(track.id.clone(), Purpose::Start { r#gen });
        }
    }

    fn ensure_pre_resolve(&mut self) {
        let Some(idx) = self.current.map(|c| c + 1) else {
            return;
        };
        let Some(track) = self.queue.get(idx) else {
            return;
        };
        if self
            .pre
            .as_ref()
            .is_some_and(|p| p.matches(self.queue_version, idx, &track.id))
        {
            return;
        }
        let track = track.id.clone();
        self.pre = Some(PreSlot {
            version: self.queue_version,
            index: idx,
            track: track.clone(),
            state: PreState::Pending,
            waiting: None,
        });
        let purpose = Purpose::Pre {
            version: self.queue_version,
            index: idx,
            track: track.clone(),
        };
        self.spawn_resolve(track, purpose);
    }

    fn spawn_resolve(&self, track: TrackId, purpose: Purpose) {
        let provider = self
            .providers
            .iter()
            .find(|p| p.id() == track.provider)
            .cloned();
        let tx = self.tx.clone();
        let delay = self.config.resolve_retry_delay;
        thread::spawn(move || {
            let result = match provider {
                Some(p) => resolve_with_retry(p.as_ref(), &track, delay),
                None => Err(ProviderError::Unplayable(format!(
                    "no provider {:?}",
                    track.provider.0
                ))),
            };
            let _ = tx.send(Msg::Resolved { purpose, result });
        });
    }

    fn on_resolved(&mut self, purpose: Purpose, result: Result<AudioSource, ProviderError>) {
        match purpose {
            Purpose::Start { r#gen } => {
                if r#gen != self.r#gen {
                    return;
                }
                match result {
                    Ok(src) => self.load(r#gen, src),
                    Err(e) => self.on_resolve_error(e),
                }
            }
            Purpose::Pre {
                version,
                index,
                track,
            } => {
                let Some(slot) = self.pre.as_mut() else {
                    return;
                };
                if !slot.matches(version, index, &track) || version != self.queue_version {
                    return;
                }
                let waiting = slot.waiting.filter(|g| *g == self.r#gen);
                match (result, waiting) {
                    (Ok(src), Some(r#gen)) => {
                        self.pre = None;
                        self.load(r#gen, src);
                    }
                    (Ok(src), None) => slot.state = PreState::Ready(src),
                    // A failed pre-resolve is never a strike; the track is resolved
                    // again when it actually starts.
                    (Err(_), Some(r#gen)) => {
                        self.pre = None;
                        self.spawn_resolve(track, Purpose::Start { r#gen });
                    }
                    (Err(_), None) => self.pre = None,
                }
            }
        }
    }

    fn on_resolve_error(&mut self, err: ProviderError) {
        let Some(track) = self.playing.clone() else {
            return;
        };
        match err {
            ProviderError::ClientBroken(d) => {
                self.emit(Event::ProviderBroken {
                    provider: track.id.provider,
                    detail: d,
                });
                self.stop();
            }
            ProviderError::Parse(d) => {
                if self
                    .last_parse_failure
                    .as_ref()
                    .is_some_and(|t| *t != track.id)
                {
                    self.emit(Event::ProviderBroken {
                        provider: track.id.provider,
                        detail: format!("repeated parse failures: {d}"),
                    });
                    self.stop();
                } else {
                    self.last_parse_failure = Some(track.id.clone());
                    self.track_failed(format!("parse error: {d}"), false);
                }
            }
            e @ (ProviderError::Unplayable(_)
            | ProviderError::Network(_)
            | ProviderError::SignInRequired(_)) => {
                self.last_parse_failure = None;
                self.track_failed(e.to_string(), false);
            }
        }
    }

    fn on_engine(&mut self, ev: EngineEvent) {
        let r#gen = match &ev {
            EngineEvent::Started { r#gen, .. }
            | EngineEvent::Position { r#gen, .. }
            | EngineEvent::Buffering { r#gen, .. }
            | EngineEvent::Ended { r#gen }
            | EngineEvent::Failed { r#gen, .. } => *r#gen,
        };
        if r#gen != self.r#gen {
            return;
        }
        match ev {
            EngineEvent::Started { duration, .. } => {
                let Some(track) = self.playing.clone() else {
                    return;
                };
                self.started = true;
                self.strikes = 0;
                self.stream_strikes = 0;
                self.last_parse_failure = None;
                self.duration = duration.or(track.duration);
                let duration = self.duration;
                self.emit(Event::NowPlaying { track, duration });
                if self.state != PlayState::Paused {
                    self.set_state(PlayState::Playing);
                }
                self.ensure_pre_resolve();
            }
            EngineEvent::Position { at, .. } => {
                self.last_pos = at;
                self.emit(Event::Position(at));
            }
            EngineEvent::Buffering { on, .. } => {
                if on {
                    if self.state == PlayState::Playing {
                        self.set_state(PlayState::Buffering);
                    }
                } else if self.state == PlayState::Buffering {
                    self.set_state(PlayState::Playing);
                }
            }
            EngineEvent::Ended { .. } => {
                self.started = false;
                self.advance();
            }
            EngineEvent::Failed { kind, detail, .. } => {
                self.started = false;
                self.last_parse_failure = None;
                self.track_failed(detail, kind == FailureKind::Stream);
            }
        }
    }

    fn advance(&mut self) {
        match self.current.map(|c| c + 1) {
            Some(n) if n < self.queue.len() => self.start(n),
            _ => self.stop(),
        }
    }

    fn track_failed(&mut self, error: String, stream: bool) {
        let Some(track) = self.playing.clone() else {
            return;
        };
        self.emit(Event::TrackFailed {
            track: track.clone(),
            error,
        });
        self.strikes += 1;
        if stream {
            self.stream_strikes += 1;
        }
        if self.strikes >= self.config.max_consecutive_failures {
            if self.stream_strikes >= 2 {
                self.emit(Event::ProviderBroken {
                    provider: track.id.provider,
                    detail: format!(
                        "{} consecutive failures, {} stream errors",
                        self.strikes, self.stream_strikes
                    ),
                });
            }
            self.stop();
        } else {
            self.advance();
        }
    }
}
