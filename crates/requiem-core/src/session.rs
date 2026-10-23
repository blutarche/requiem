use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use serde::{Deserialize, Serialize};

use crate::{
    AccountState, AudioSource, CollectionId, Command, Engine, EngineEvent, EngineEventSink,
    EnqueueMode, Event, FailureKind, HOME_MIXES, HOME_PLAYLISTS, Item, Page, PlayState, Provider,
    ProviderError, ProviderId, SeekTarget, Shelf, Track, TrackId,
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

/// One provider's answer to Home, Library or a collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PagePart {
    pub provider: ProviderId,
    /// The name of the account that answered.
    pub account: Option<String>,
    pub shelves: Vec<Shelf>,
}

/// The last answer to a collection.
#[derive(Debug, Clone)]
pub struct SavedCollection {
    pub id: CollectionId,
    /// Shared with the session, so handing the kept collections over copies no tracks.
    pub part: Arc<PagePart>,
    /// When it was fetched, in seconds since the Unix epoch.
    pub fetched: u64,
}

impl PartialEq for SavedCollection {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.fetched == other.fetched
            && (Arc::ptr_eq(&self.part, &other.part) || self.part == other.part)
    }
}

/// The last answers to Home, Library and the collections opened, which a restart answers with
/// at once: only what the user saw, never a credential.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavedPages {
    pub home: Vec<PagePart>,
    pub library: Vec<PagePart>,
    /// Least recently opened first.
    pub collections: Vec<SavedCollection>,
}

/// Called on the session thread; must not block.
pub type PagesSink = Arc<dyn Fn(SavedPages) + Send + Sync>;

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
    /// What the last run kept of Home, Library and the collections. Parts of accounts not
    /// signed in at the start are dropped.
    pub pages: SavedPages,
    /// Called with the pages kept whenever they change.
    pub save_pages: Option<PagesSink>,
    /// At most this many collections are kept; the least recently opened goes first.
    pub collection_cap: usize,
    /// When Home or the Library is shown, this many of its collections are fetched ahead of an
    /// open: of each of Home's mixes and playlists, and of the Library's first.
    pub prefetch_per_page: usize,
    /// How many collections are fetched ahead at once.
    pub prefetch_parallel: usize,
    /// A collection fetched this recently is not fetched ahead again.
    pub prefetch_fresh: Duration,
    /// After a provider fails for now, as when it is rate limited, its collections are not
    /// fetched ahead for this long.
    pub prefetch_pause: Duration,
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
            .field("pages", &self.pages)
            .field("save_pages", &self.save_pages.is_some())
            .field("collection_cap", &self.collection_cap)
            .field("prefetch_per_page", &self.prefetch_per_page)
            .field("prefetch_parallel", &self.prefetch_parallel)
            .field("prefetch_fresh", &self.prefetch_fresh)
            .field("prefetch_pause", &self.prefetch_pause)
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
            pages: SavedPages::default(),
            save_pages: None,
            collection_cap: 200,
            prefetch_per_page: 12,
            prefetch_parallel: 2,
            prefetch_fresh: Duration::from_secs(10 * 60),
            prefetch_pause: Duration::from_secs(10 * 60),
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
    /// One provider's answer to fetch `run` of a kept page, with its state after it.
    PageAnswered {
        page: Page,
        run: u64,
        provider: ProviderId,
        result: Result<Vec<Shelf>, ProviderError>,
        account: AccountState,
    },
    /// A provider says `page` has newer content than it gave.
    ProviderChanged(Page),
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
    ClientImported {
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

/// A fetch of a kept page from every signed-in provider that answers it.
struct Run {
    id: u64,
    /// Browses with nothing kept to answer them; the first answer that leaves something to
    /// show does, or else the last.
    waiting: Vec<u64>,
    /// The providers asked, with their states when asked.
    asked: Vec<(ProviderId, AccountState)>,
    left: usize,
    hard: Option<ProviderError>,
    sign_in: Option<ProviderError>,
}

/// What is kept of Home, Library or a collection, and its fetch.
#[derive(Default)]
struct Kept {
    /// Only of accounts that are signed in now.
    parts: Vec<Arc<PagePart>>,
    run: Option<Run>,
    /// Fetch again once `run` ends: something changed after it started.
    again: bool,
}

/// A collection kept.
#[derive(Default)]
struct Cached {
    kept: Kept,
    /// When the user last opened it, as `Actor::last_open` counts; 0 if never.
    opened: u64,
    /// When its part was fetched, in seconds since the Unix epoch.
    fetched: u64,
}

fn collection_ids(items: &[Item]) -> impl Iterator<Item = CollectionId> + '_ {
    items.iter().filter_map(|i| match i {
        Item::Collection(c) => Some(c.id.clone()),
        Item::Track(_) => None,
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Whether `shelves` hold anything to show.
fn showable(shelves: &[Shelf]) -> bool {
    shelves.iter().any(|s| !s.items.is_empty())
}

/// What a browse of a kept page gets with no provider signed in that answers it.
fn page_sign_in(page: &Page) -> ProviderError {
    ProviderError::SignInRequired(
        match page {
            Page::Home => "sign in to see your music",
            Page::Library => "sign in to browse your library",
            Page::Collection(_) => "sign in to see its tracks",
        }
        .into(),
    )
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

    home: Kept,
    library: Kept,
    collections: HashMap<CollectionId, Cached>,
    /// Counts the user's opens of collections.
    last_open: u64,
    last_run: u64,
    /// Collections to fetch ahead, in turn.
    prefetch: VecDeque<CollectionId>,
    /// Collections being fetched ahead.
    prefetching: HashSet<CollectionId>,
    /// When each provider last failed for now.
    failed_at: HashMap<ProviderId, Instant>,
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

/// Runs `call` on every provider in parallel and waits for all. Returns the answer of each
/// provider that succeeded, in provider order; a sign-in failure is skipped silently and any
/// other is logged. It fails only when none succeeded, with the first error that isn't a
/// sign-in error, else a sign-in error.
fn fan_out<T: Send + 'static>(
    what: &'static str,
    providers: Vec<Arc<dyn Provider>>,
    call: impl Fn(&dyn Provider) -> Result<Vec<T>, ProviderError> + Send + Sync + 'static,
) -> Result<Vec<Vec<T>>, ProviderError> {
    let call = Arc::new(call);
    let workers: Vec<_> = providers
        .into_iter()
        .map(|p| {
            let call = call.clone();
            thread::spawn(move || (p.id(), call(p.as_ref())))
        })
        .collect();
    let mut answers = Vec::new();
    let mut hard = None;
    let mut sign_in = None;
    for worker in workers {
        let Ok((id, result)) = worker.join() else {
            eprintln!("requiem: {what} worker panicked");
            continue;
        };
        match result {
            Ok(items) => answers.push(items),
            Err(e @ ProviderError::SignInRequired(_)) => {
                sign_in.get_or_insert(e);
            }
            Err(e) => {
                eprintln!("requiem: {what} on {} failed: {e}", id.0);
                hard.get_or_insert(e);
            }
        }
    }
    if !answers.is_empty() {
        return Ok(answers);
    }
    Err(hard
        .or(sign_in)
        .unwrap_or_else(|| ProviderError::Unplayable("no provider answered".into())))
}

/// The first of each list, then the second of each, and so on; a list that runs out drops
/// out.
fn interleave<T>(lists: Vec<Vec<T>>) -> Vec<T> {
    let mut lists: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut items = Vec::new();
    loop {
        let before = items.len();
        for list in &mut lists {
            items.extend(list.next());
        }
        if items.len() == before {
            return items;
        }
    }
}

/// Providers' shelves taken in turn (each one's first, then each one's second, ...), in
/// provider order; a shelf whose title already appeared joins that shelf, its items taken in
/// turn with the others', instead of taking a slot. `answers` are each provider's shelves, in
/// provider order.
fn merge_home(answers: Vec<Vec<Shelf>>) -> Vec<Shelf> {
    let answers = answers
        .into_iter()
        .enumerate()
        .map(|(provider, shelves)| shelves.into_iter().map(|s| (provider, s)).collect())
        .collect();
    type FromProvider = (usize, Vec<Item>);
    let mut titles: Vec<(String, Vec<FromProvider>)> = Vec::new();
    for (provider, shelf) in interleave(answers) {
        match titles.iter_mut().find(|(t, _)| *t == shelf.title) {
            Some((_, lists)) => lists.push((provider, shelf.items)),
            None => titles.push((shelf.title, vec![(provider, shelf.items)])),
        }
    }
    titles
        .into_iter()
        .map(|(title, mut lists)| {
            lists.sort_by_key(|(provider, _)| *provider);
            Shelf {
                title,
                items: interleave(lists.into_iter().map(|(_, items)| items).collect()),
            }
        })
        .collect()
}

/// With items from two or more providers, one "Library" shelf of their items taken in turn,
/// in provider order; else each provider's shelves as they are. `answers` are each
/// provider's shelves, in provider order.
fn merge_library(answers: Vec<Vec<Shelf>>) -> Vec<Shelf> {
    let stocked = answers
        .iter()
        .filter(|shelves| shelves.iter().any(|s| !s.items.is_empty()))
        .count();
    if stocked < 2 {
        return answers.into_iter().flatten().collect();
    }
    let lists = answers
        .into_iter()
        .map(|shelves| shelves.into_iter().flat_map(|s| s.items).collect())
        .collect();
    vec![Shelf {
        title: "Library".into(),
        items: interleave(lists),
    }]
}

/// `answers` are each provider's shelves, in provider order.
fn merge(page: &Page, answers: Vec<Vec<Shelf>>) -> Vec<Shelf> {
    match page {
        Page::Home => merge_home(answers),
        Page::Library => merge_library(answers),
        Page::Collection(_) => answers.into_iter().flatten().collect(),
    }
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
        mut config: SessionConfig,
        tx: Sender<Msg>,
    ) -> Self {
        let accounts: Vec<(ProviderId, AccountState)> =
            providers.iter().map(|p| (p.id(), p.account())).collect();
        for p in &providers {
            let tx = tx.clone();
            p.on_page_changed(Arc::new(move |page| {
                let _ = tx.send(Msg::ProviderChanged(page));
            }));
        }
        // Parts of accounts signed out since are dropped now: a later sign-in may be another
        // account that the name can't tell apart.
        let holds = |part: &PagePart| {
            accounts.iter().any(|(p, s)| {
                *p == part.provider
                    && matches!(s, AccountState::SignedIn { name } if *name == part.account)
            })
        };
        let kept = |mut parts: Vec<PagePart>| {
            parts.retain(&holds);
            Kept {
                parts: parts.into_iter().map(Arc::new).collect(),
                ..Kept::default()
            }
        };
        let saved = std::mem::take(&mut config.pages);
        let total = saved.home.len() + saved.library.len() + saved.collections.len();
        let home = kept(saved.home);
        let library = kept(saved.library);
        let mut collections = HashMap::new();
        let mut last_open = 0;
        for c in saved.collections {
            if c.id.provider != c.part.provider || !holds(&c.part) {
                continue;
            }
            last_open += 1;
            collections.insert(
                c.id,
                Cached {
                    kept: Kept {
                        parts: vec![c.part],
                        ..Kept::default()
                    },
                    opened: last_open,
                    fetched: c.fetched,
                },
            );
        }
        let mut actor = Self {
            home,
            library,
            collections,
            last_open,
            last_run: 0,
            prefetch: VecDeque::new(),
            prefetching: HashSet::new(),
            failed_at: HashMap::new(),
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
        };
        actor.evict();
        let left = actor.home.parts.len() + actor.library.parts.len() + actor.collections.len();
        if left != total {
            actor.save_pages();
        }
        actor
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
                Msg::PageAnswered {
                    page,
                    run,
                    provider,
                    result,
                    account,
                } => self.page_answered(page, run, provider, result, account),
                Msg::ProviderChanged(page) => self.provider_changed(page),
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
                Msg::ClientImported {
                    provider,
                    result,
                    state,
                } => {
                    // As after a sign-in: the error first, then the state, announced even when
                    // unchanged, so a client waiting on its import hears it. Only the state
                    // tells a refused import from one that stood but could not finish.
                    if let Err(e) = result {
                        self.emit(Event::AccountError {
                            provider: provider.clone(),
                            error: sign_in_error(&e),
                        });
                    }
                    self.set_account(provider, state, true);
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
            Command::ImportSignInClient { provider, json } => self.import_client(provider, json.0),
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
            let result =
                fan_out("search", providers, move |p| p.search(&query, limit)).map(interleave);
            let _ = tx.send(Msg::Searched { req, result });
        });
    }

    fn provider(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == *id).cloned()
    }

    fn browse(&mut self, req: u64, page: Page) {
        let targets: Vec<Arc<dyn Provider>> = match &page {
            // Only a signed-in account's answer is kept, as it can be told from another's.
            Page::Collection(c) if self.page_targets(&page).is_empty() => {
                self.provider(&c.provider).into_iter().collect()
            }
            Page::Collection(c) => {
                self.last_open += 1;
                self.collections.entry(c.clone()).or_default().opened = self.last_open;
                self.answer_kept(req, page);
                return;
            }
            Page::Home | Page::Library => {
                if self.page_targets(&page).is_empty() {
                    self.emit(Event::BrowseFailed {
                        req,
                        error: page_sign_in(&page).to_string(),
                        sign_in_required: true,
                    });
                    return;
                }
                self.answer_kept(req, page);
                return;
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
            let result = result.map(|answers| merge(&page, answers));
            let accounts = targets.iter().map(|p| (p.id(), p.account())).collect();
            let _ = tx.send(Msg::Browsed {
                req,
                page,
                result,
                accounts,
            });
        });
    }

    /// Answers a browse of a kept page with what is kept of it and fetches it again, or waits
    /// on the fetch when nothing is kept.
    fn answer_kept(&mut self, req: u64, page: Page) {
        // Clients never browse on hearing of a change, which carries the shelves, so fetching
        // on every browse cannot loop.
        match self.served(&page) {
            Some(shelves) => {
                self.emit(Event::BrowseResults {
                    req,
                    page: page.clone(),
                    shelves: shelves.clone(),
                });
                self.shown(&page, &shelves);
                self.fetch_page(page, None);
            }
            None => self.fetch_page(page, Some(req)),
        }
        if self.evict() {
            self.save_pages();
        }
    }

    /// The signed-in providers that answer `page`, in provider order.
    fn page_targets(&self, page: &Page) -> Vec<Arc<dyn Provider>> {
        self.providers
            .iter()
            .filter(|p| match page {
                Page::Home => p.has_home(),
                Page::Library => true,
                Page::Collection(c) => p.id() == c.provider,
            })
            .filter(|p| {
                matches!(
                    self.account_of(&p.id()),
                    Some(AccountState::SignedIn { .. })
                )
            })
            .cloned()
            .collect()
    }

    fn kept(&self, page: &Page) -> Option<&Kept> {
        match page {
            Page::Home => Some(&self.home),
            Page::Library => Some(&self.library),
            Page::Collection(c) => self.collections.get(c).map(|c| &c.kept),
        }
    }

    fn kept_mut(&mut self, page: &Page) -> Option<&mut Kept> {
        match page {
            Page::Home => Some(&mut self.home),
            Page::Library => Some(&mut self.library),
            Page::Collection(c) => self.collections.get_mut(c).map(|c| &mut c.kept),
        }
    }

    /// The kept parts of `page` merged, as all of them landing at once would be; `None` when
    /// none is kept.
    fn served(&self, page: &Page) -> Option<Vec<Shelf>> {
        let kept = self.kept(page)?;
        let answers: Vec<Vec<Shelf>> = self
            .page_targets(page)
            .iter()
            .filter_map(|p| {
                let id = p.id();
                let part = kept.parts.iter().find(|part| part.provider == id)?;
                Some(part.shelves.clone())
            })
            .collect();
        (!answers.is_empty()).then(|| merge(page, answers))
    }

    fn save_pages(&self) {
        let Some(sink) = &self.config.save_pages else {
            return;
        };
        let mut collections: Vec<(u64, SavedCollection)> = self
            .collections
            .iter()
            .filter_map(|(id, c)| {
                let saved = SavedCollection {
                    id: id.clone(),
                    part: c.kept.parts.first()?.clone(),
                    fetched: c.fetched,
                };
                Some((c.opened, saved))
            })
            .collect();
        // By id after the open, so that the same collections always come in the same order.
        collections.sort_by(|(a, x), (b, y)| {
            (a, &x.id.provider.0, &x.id.id).cmp(&(b, &y.id.provider.0, &y.id.id))
        });
        let parts = |kept: &Kept| kept.parts.iter().map(|p| (**p).clone()).collect();
        sink(SavedPages {
            home: parts(&self.home),
            library: parts(&self.library),
            collections: collections.into_iter().map(|(_, c)| c).collect(),
        });
    }

    /// Drops the least recently opened collections beyond the cap, except any being fetched,
    /// and says whether one held anything.
    fn evict(&mut self) -> bool {
        let mut dropped = false;
        while self.collections.len() > self.config.collection_cap {
            let oldest = self
                .collections
                .iter()
                .filter(|(_, c)| c.kept.run.is_none())
                .min_by_key(|(_, c)| c.opened)
                .map(|(id, _)| id.clone());
            let Some(id) = oldest else {
                break;
            };
            let gone = self.collections.remove(&id).expect("the id was just found");
            dropped |= !gone.kept.parts.is_empty();
        }
        dropped
    }

    /// Plans fetching ahead the collections `page` shows, when it is Home or the Library: the
    /// ones an open is likeliest to follow.
    fn shown(&mut self, page: &Page, shelves: &[Shelf]) {
        let n = self.config.prefetch_per_page;
        let ids: Vec<CollectionId> = match page {
            Page::Home => shelves
                .iter()
                .filter(|s| s.title == HOME_MIXES || s.title == HOME_PLAYLISTS)
                .flat_map(|s| collection_ids(&s.items).take(n))
                .collect(),
            Page::Library => shelves
                .iter()
                .flat_map(|s| collection_ids(&s.items))
                .take(n)
                .collect(),
            Page::Collection(_) => return,
        };
        for id in ids {
            if !self.prefetch.contains(&id) {
                self.prefetch.push_back(id);
            }
        }
        self.pump_prefetch();
    }

    /// Starts fetching ahead the next collections planned, as many at once as the config
    /// allows, while no collection the user opened is being fetched: those go first.
    fn pump_prefetch(&mut self) {
        let fresh = self.config.prefetch_fresh.as_secs();
        let now = now_unix();
        while self.prefetching.len() < self.config.prefetch_parallel {
            let user_waits = self
                .collections
                .iter()
                .any(|(id, c)| c.kept.run.is_some() && !self.prefetching.contains(id));
            if user_waits {
                return;
            }
            let Some(id) = self.prefetch.pop_front() else {
                return;
            };
            let page = Page::Collection(id.clone());
            let cheap = self.provider(&id.provider).is_some_and(|p| p.fetch_ahead());
            let paused = self
                .failed_at
                .get(&id.provider)
                .is_some_and(|t| t.elapsed() < self.config.prefetch_pause);
            if !cheap || paused || self.page_targets(&page).is_empty() {
                continue;
            }
            let skip = self.collections.get(&id).is_some_and(|c| {
                c.kept.run.is_some()
                    || (!c.kept.parts.is_empty() && now.saturating_sub(c.fetched) < fresh)
            });
            if skip {
                continue;
            }
            self.collections.entry(id.clone()).or_default();
            self.prefetching.insert(id);
            self.fetch_page(page, None);
            if self.evict() {
                self.save_pages();
            }
        }
    }

    /// Starts fetching `page`, a kept one, from its providers, unless a fetch runs: then
    /// `req` waits on that one, which is followed by another if the providers changed since
    /// it started.
    fn fetch_page(&mut self, page: Page, req: Option<u64>) {
        let targets = self.page_targets(&page);
        let asked: Vec<(ProviderId, AccountState)> = targets
            .iter()
            .map(|p| {
                let id = p.id();
                let state = self.account_of(&id).cloned();
                (id, state.unwrap_or(AccountState::SignedOut))
            })
            .collect();
        let id = self.last_run + 1;
        let Some(kept) = self.kept_mut(&page) else {
            return;
        };
        if let Some(run) = &mut kept.run {
            run.waiting.extend(req);
            if run.asked != asked {
                kept.again = true;
            }
            return;
        }
        if targets.is_empty() {
            return;
        }
        kept.run = Some(Run {
            id,
            waiting: req.into_iter().collect(),
            asked,
            left: targets.len(),
            hard: None,
            sign_in: None,
        });
        self.last_run = id;
        for p in targets {
            let tx = self.tx.clone();
            let page = page.clone();
            thread::spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| p.browse(&page)))
                    .unwrap_or_else(|_| {
                        eprintln!("requiem: browse worker panicked");
                        Err(ProviderError::Unplayable("the browse failed".into()))
                    });
                let _ = tx.send(Msg::PageAnswered {
                    page,
                    run: id,
                    provider: p.id(),
                    result,
                    account: p.account(),
                });
            });
        }
    }

    /// Takes one provider's answer into what is kept of `page`. The first that leaves
    /// something to show, or else the last, answers the browses waiting; any later change is a
    /// `PageChanged`.
    fn page_answered(
        &mut self,
        page: Page,
        run: u64,
        provider: ProviderId,
        result: Result<Vec<Shelf>, ProviderError>,
        account: AccountState,
    ) {
        let before = self.served(&page);
        let current = self.account_of(&provider).cloned();
        let Some(kept) = self.kept_mut(&page) else {
            self.set_account(provider, account, false);
            return;
        };
        let Some(r) = kept.run.as_mut().filter(|r| r.id == run) else {
            self.set_account(provider, account, false);
            return;
        };
        r.left -= 1;
        let asked = r
            .asked
            .iter()
            .find(|(p, _)| *p == provider)
            .map(|(_, s)| s.clone());
        let mut changed = false;
        let mut fetched = false;
        let failed_for_now = matches!(result, Err(ProviderError::Network(_)));
        // The account changed while it answered: neither its answer nor its part from before
        // is the current account's.
        let same = matches!(&asked, Some(AccountState::SignedIn { name })
            if account == AccountState::SignedIn { name: name.clone() })
            && current == asked;
        if !same {
            let n = kept.parts.len();
            kept.parts.retain(|p| p.provider != provider);
            changed = kept.parts.len() != n;
        }
        match result {
            Ok(shelves) => {
                if let Some(AccountState::SignedIn { name }) = asked
                    && same
                {
                    let part = PagePart {
                        provider: provider.clone(),
                        account: name,
                        shelves,
                    };
                    fetched = true;
                    match kept.parts.iter_mut().find(|p| p.provider == provider) {
                        Some(old) if **old == part => {}
                        Some(old) => {
                            *old = Arc::new(part);
                            changed = true;
                        }
                        None => {
                            kept.parts.push(Arc::new(part));
                            changed = true;
                        }
                    }
                }
            }
            Err(e @ ProviderError::SignInRequired(_)) => {
                r.sign_in.get_or_insert(e);
                let n = kept.parts.len();
                kept.parts.retain(|p| p.provider != provider);
                changed |= kept.parts.len() != n;
            }
            // The part kept from before stays: stale beats none.
            Err(e) => {
                eprintln!("requiem: browse on {} failed: {e}", provider.0);
                r.hard.get_or_insert(e);
            }
        }
        let done = r.left == 0;
        // Saved only with a change of content: a fetch time lost to a restart costs no more
        // than one early fetch ahead.
        if fetched
            && let Page::Collection(id) = &page
            && let Some(c) = self.collections.get_mut(id)
        {
            c.fetched = now_unix();
        }
        if changed {
            self.save_pages();
        }
        let after = self.served(&page);
        let signed_out = self.page_targets(&page).is_empty();
        let Some(kept) = self.kept_mut(&page) else {
            return;
        };
        let Some(r) = kept.run.as_mut() else {
            return;
        };
        // Parts with nothing in them answer only once every provider is in.
        let answered = if after.as_deref().is_some_and(showable) || (done && after.is_some()) {
            std::mem::take(&mut r.waiting)
        } else {
            Vec::new()
        };
        let unanswered = !r.waiting.is_empty();
        let mut failed = None;
        if done {
            let r = kept.run.take().expect("the run is current");
            if !r.waiting.is_empty() {
                // As a browse now would be answered, when its providers signed out meanwhile.
                let e = if signed_out {
                    page_sign_in(&page)
                } else {
                    r.hard
                        .or(r.sign_in)
                        .unwrap_or_else(|| ProviderError::Unplayable("no provider answered".into()))
                };
                failed = Some((r.waiting, e));
            }
        }
        let again = done && std::mem::take(&mut kept.again);
        let browsed = matches!(page, Page::Home | Page::Library);
        let mut shown = None;
        // A collection first fetched ahead of an open was never shown, so nothing changed.
        let unseen = before.is_none() && matches!(page, Page::Collection(_));
        if answered.is_empty() {
            // Browses still waiting are answered with the change once there is one to show.
            if !unanswered && !unseen && after != before {
                let shelves = after.unwrap_or_default();
                shown = browsed.then(|| shelves.clone());
                self.emit(Event::PageChanged {
                    page: page.clone(),
                    shelves,
                });
            }
        } else if let Some(shelves) = after {
            for req in answered {
                self.emit(Event::BrowseResults {
                    req,
                    page: page.clone(),
                    shelves: shelves.clone(),
                });
            }
            shown = browsed.then_some(shelves);
        }
        if let Some((waiting, e)) = failed {
            for req in waiting {
                self.emit(Event::BrowseFailed {
                    req,
                    sign_in_required: matches!(e, ProviderError::SignInRequired(_)),
                    error: e.to_string(),
                });
            }
        }
        if failed_for_now {
            self.failed_at.insert(provider.clone(), Instant::now());
        }
        // After the answer, as it may have signed the provider out.
        self.set_account(provider, account, false);
        if again {
            self.fetch_page(page.clone(), None);
        }
        if done && let Page::Collection(id) = &page {
            self.prefetching.remove(id);
            // As after a failed first fetch.
            let empty = self
                .collections
                .get(id)
                .is_some_and(|c| c.kept.run.is_none() && c.kept.parts.is_empty());
            if empty {
                self.collections.remove(id);
            }
            if self.evict() {
                self.save_pages();
            }
        }
        match shown {
            Some(shelves) => self.shown(&page, &shelves),
            None => self.pump_prefetch(),
        }
    }

    /// A provider's newer content is fetched for a page that has been answered.
    fn provider_changed(&mut self, page: Page) {
        let Some(kept) = self.kept_mut(&page) else {
            return;
        };
        if kept.run.is_some() {
            kept.again = true;
        } else if !kept.parts.is_empty() {
            self.fetch_page(page, None);
        }
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
        // What the account that was signed in answered is never shown for another, or for none.
        let mut dropped = false;
        let collections = self.collections.values_mut().map(|c| &mut c.kept);
        for kept in [&mut self.home, &mut self.library]
            .into_iter()
            .chain(collections)
        {
            let n = kept.parts.len();
            kept.parts.retain(|p| {
                p.provider != provider
                    || matches!(&state, AccountState::SignedIn { name } if *name == p.account)
            });
            dropped |= kept.parts.len() != n;
        }
        self.collections
            .retain(|_, c| c.kept.run.is_some() || !c.kept.parts.is_empty());
        if dropped {
            self.save_pages();
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

    fn import_client(&mut self, id: ProviderId, json: String) {
        let Some(provider) = self.provider(&id) else {
            self.emit(Event::AccountError {
                provider: id.clone(),
                error: format!("no provider {}", id.0),
            });
            return;
        };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = provider.import_client(&json);
            let state = provider.account();
            let _ = tx.send(Msg::ClientImported {
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
