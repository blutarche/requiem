use std::collections::HashMap;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, never, select, unbounded};
use requiem_core::{AccountState, Command, Event, PlayState, ProviderId, Session, Track};

use crate::frame;
use crate::proto::{PROTOCOL, Role, ServerMsg, UpdateStatus, VERSION};
use crate::server::{ClientCounts, ServerHooks, UpdateRequest};

pub(crate) const QUEUE_CAP: usize = 256;
/// A client or subscriber with more queued messages than this stops receiving `Position`.
const POSITION_BACKLOG: usize = 64;
/// How long, in total, writers get on stop to send what is queued, `Goodbye` last.
const GOODBYE_FLUSH: Duration = Duration::from_millis(200);

pub(crate) type ClientId = u64;
pub(crate) type Frame = Arc<Vec<u8>>;

pub(crate) enum HubMsg {
    Join {
        id: ClientId,
        role: Role,
        queue: Sender<Frame>,
        stream: Arc<UnixStream>,
        flushed: Receiver<()>,
    },
    Leave(ClientId),
    /// `from` is `None` for in-daemon controls.
    Command {
        from: Option<ClientId>,
        cmd: Command,
    },
    Ping {
        id: ClientId,
        n: u64,
    },
    Subscribe(Sender<Event>),
    Counts(Sender<ClientCounts>),
    Raise(Sender<bool>),
    Update(UpdateStatus),
    UpdateRequest(UpdateRequest),
    Stop,
}

struct Peer {
    id: ClientId,
    role: Role,
    queue: Sender<Frame>,
    stream: Arc<UnixStream>,
    /// Disconnects when the writer thread exits, which it does once `queue` is dropped
    /// and drained.
    flushed: Receiver<()>,
}

impl Peer {
    /// False once the peer has been cut off and must be dropped from the list.
    fn offer(&self, frame: &Frame, position: bool) -> bool {
        if position && self.queue.len() > POSITION_BACKLOG {
            return true;
        }
        if self.queue.try_send(frame.clone()).is_ok() {
            return true;
        }
        let _ = self.stream.shutdown(Shutdown::Both);
        false
    }
}

/// What a client that has seen every event so far knows.
struct Mirror {
    tracks: Vec<Track>,
    current: Option<usize>,
    now: Option<(Track, Option<Duration>)>,
    state: PlayState,
    volume: u8,
    position: Duration,
    /// In the order the providers were first announced.
    accounts: Vec<(ProviderId, AccountState)>,
}

impl Mirror {
    /// Apart from the `Account`s it sends a new subscriber, a `Session` emits nothing until
    /// something changes, so this must match its initial state, and the hub must subscribe
    /// before any command reaches the Session.
    fn new() -> Self {
        Self {
            tracks: Vec::new(),
            current: None,
            now: None,
            state: PlayState::Stopped,
            volume: 100,
            position: Duration::ZERO,
            accounts: Vec::new(),
        }
    }

    fn apply(&mut self, ev: &Event) {
        match ev {
            Event::QueueChanged { tracks, current } => {
                self.tracks.clone_from(tracks);
                self.current = *current;
            }
            Event::NowPlaying { track, duration } => {
                self.now = Some((track.clone(), *duration));
                self.position = Duration::ZERO;
            }
            Event::Position(at) => self.position = *at,
            Event::State(s) => {
                self.state = *s;
                if *s == PlayState::Stopped {
                    self.now = None;
                }
            }
            Event::Volume(v) => self.volume = *v,
            Event::Account { provider, state } => {
                match self.accounts.iter_mut().find(|(p, _)| p == provider) {
                    Some((_, known)) => known.clone_from(state),
                    None => self.accounts.push((provider.clone(), state.clone())),
                }
            }
            Event::SearchResults { .. }
            | Event::SearchFailed { .. }
            | Event::BrowseResults { .. }
            | Event::BrowseFailed { .. }
            | Event::AccountError { .. }
            | Event::TrackFailed { .. }
            | Event::ProviderBroken { .. }
            | Event::PageChanged { .. } => {}
        }
    }

    /// The CLI treats the first `Volume` as the end of the snapshot, so it must stay last.
    fn snapshot(&self) -> Vec<Event> {
        let mut evs = vec![Event::QueueChanged {
            tracks: self.tracks.clone(),
            current: self.current,
        }];
        if let Some((track, duration)) = &self.now {
            evs.push(Event::NowPlaying {
                track: track.clone(),
                duration: *duration,
            });
        }
        evs.push(Event::State(self.state));
        if self.now.is_some() {
            evs.push(Event::Position(self.position));
        }
        evs.extend(
            self.accounts
                .iter()
                .map(|(provider, state)| Event::Account {
                    provider: provider.clone(),
                    state: state.clone(),
                }),
        );
        evs.push(Event::Volume(self.volume));
        evs
    }
}

struct Hub {
    session: Session,
    hooks: ServerHooks,
    mirror: Mirror,
    update: UpdateStatus,
    peers: Vec<Peer>,
    subs: Vec<Sender<Event>>,
    /// Global search or browse id sent to the Session -> (requester, the requester's own id).
    requests: HashMap<u64, (Option<ClientId>, u64)>,
    next_request: u64,
}

pub(crate) fn spawn(
    session: Session,
    hooks: ServerHooks,
    update: UpdateStatus,
) -> io::Result<(Sender<HubMsg>, JoinHandle<()>)> {
    let events = session.subscribe();
    let (tx, rx) = unbounded();
    let mut hub = Hub::new(session, hooks, update);
    // The Session queued its `Account`s before `subscribe` returned. Applying them here, before
    // any client can join, puts them in every snapshot.
    for ev in events.try_iter() {
        hub.mirror.apply(&ev);
    }
    let handle = thread::Builder::new()
        .name("requiem-ipc-hub".into())
        .spawn(move || hub.run(&rx, events))?;
    Ok((tx, handle))
}

fn encode(msg: &ServerMsg) -> Option<Frame> {
    frame::encode(msg).ok().map(Arc::new)
}

impl Hub {
    fn new(session: Session, hooks: ServerHooks, update: UpdateStatus) -> Self {
        Self {
            session,
            hooks,
            mirror: Mirror::new(),
            update,
            peers: Vec::new(),
            subs: Vec::new(),
            requests: HashMap::new(),
            next_request: 0,
        }
    }

    fn run(mut self, rx: &Receiver<HubMsg>, events: Receiver<Event>) {
        let mut events = Some(events);
        let gone = never();
        loop {
            select! {
                recv(events.as_ref().unwrap_or(&gone)) -> ev => match ev {
                    Ok(ev) => self.on_event(ev),
                    Err(_) => events = None,
                },
                recv(rx) -> msg => match msg {
                    Ok(HubMsg::Stop) | Err(_) => break,
                    Ok(msg) => self.on_msg(msg),
                },
            }
        }
        self.say_goodbye();
        self.session.shutdown();
    }

    /// A client whose queue is full, or that doesn't read its backlog by the deadline, is
    /// closed without its `Goodbye`.
    fn say_goodbye(&mut self) {
        let deadline = Instant::now() + GOODBYE_FLUSH;
        let goodbye = encode(&ServerMsg::Goodbye);
        let mut closing = Vec::with_capacity(self.peers.len());
        for p in self.peers.drain(..) {
            if let Some(frame) = &goodbye {
                p.offer(frame, false);
            }
            let Peer {
                queue,
                stream,
                flushed,
                ..
            } = p;
            drop(queue);
            closing.push((stream, flushed));
        }
        for (stream, flushed) in closing {
            let _ = flushed.recv_deadline(deadline);
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::SearchResults { req, tracks } => {
                self.route(req, |req| Event::SearchResults { req, tracks });
            }
            Event::SearchFailed { req, error } => {
                self.route(req, |req| Event::SearchFailed { req, error });
            }
            Event::BrowseResults { req, page, shelves } => {
                self.route(req, |req| Event::BrowseResults { req, page, shelves });
            }
            Event::BrowseFailed {
                req,
                error,
                sign_in_required,
            } => {
                self.route(req, |req| Event::BrowseFailed {
                    req,
                    error,
                    sign_in_required,
                });
            }
            ev => {
                self.mirror.apply(&ev);
                self.broadcast(ev);
            }
        }
    }

    fn broadcast(&mut self, ev: Event) {
        let position = matches!(ev, Event::Position(_));
        self.subs
            .retain(|s| (position && s.len() > POSITION_BACKLOG) || s.send(ev.clone()).is_ok());
        // Over the frame limit (a queue of many thousands of tracks), peers miss this event
        // rather than get a frame they would reject and disconnect on.
        let Some(frame) = encode(&ServerMsg::Event(ev)) else {
            return;
        };
        self.peers.retain(|p| p.offer(&frame, position));
    }

    /// Search and browse results go only to the requester, under its own id. In-daemon
    /// requests report to subscribers.
    fn route(&mut self, global: u64, make: impl FnOnce(u64) -> Event) {
        let Some((from, req)) = self.requests.remove(&global) else {
            return;
        };
        let ev = make(req);
        match from {
            Some(id) => self.offer_to(id, &ServerMsg::Event(ev)),
            None => self.subs.retain(|s| s.send(ev.clone()).is_ok()),
        }
    }

    fn offer_to(&mut self, id: ClientId, msg: &ServerMsg) {
        let Some(i) = self.peers.iter().position(|p| p.id == id) else {
            return;
        };
        let Some(frame) = encode(msg) else { return };
        if !self.peers[i].offer(&frame, false) {
            self.peers.remove(i);
        }
    }

    fn on_msg(&mut self, msg: HubMsg) {
        match msg {
            HubMsg::Join {
                id,
                role,
                queue,
                stream,
                flushed,
            } => self.join(Peer {
                id,
                role,
                queue,
                stream,
                flushed,
            }),
            // Every connection ends with a Leave, including peers the hub cut off.
            HubMsg::Leave(id) => {
                self.peers.retain(|p| p.id != id);
                self.requests.retain(|_, (from, _)| *from != Some(id));
            }
            HubMsg::Command { from, cmd } => self.on_command(from, cmd),
            HubMsg::Ping { id, n } => self.offer_to(id, &ServerMsg::Pong(n)),
            HubMsg::Subscribe(tx) => {
                for ev in self.mirror.snapshot() {
                    let _ = tx.send(ev);
                }
                self.subs.push(tx);
            }
            HubMsg::Counts(reply) => {
                let gui = self.peers.iter().filter(|p| p.role == Role::Gui).count();
                let _ = reply.send(ClientCounts {
                    gui,
                    cli: self.peers.len() - gui,
                });
            }
            HubMsg::Raise(reply) => {
                let mut raised = false;
                if let Some(frame) = encode(&ServerMsg::Raise) {
                    self.peers.retain(|p| {
                        if p.role != Role::Gui {
                            return true;
                        }
                        let kept = p.offer(&frame, false);
                        raised |= kept;
                        kept
                    });
                }
                let _ = reply.send(raised);
            }
            HubMsg::Update(status) => {
                if let Some(frame) = encode(&ServerMsg::Update(status.clone())) {
                    self.peers.retain(|p| p.offer(&frame, false));
                }
                self.update = status;
            }
            HubMsg::UpdateRequest(req) => (self.hooks.on_update)(req),
            HubMsg::Stop => unreachable!("handled in run"),
        }
    }

    /// Welcome, the update status and the snapshot are queued before the peer joins the
    /// broadcast list, all on this thread, so no live event can come between them or be
    /// missed.
    fn join(&mut self, peer: Peer) {
        let welcome = ServerMsg::Welcome {
            protocol: PROTOCOL,
            version: VERSION.into(),
        };
        let update = ServerMsg::Update(self.update.clone());
        let snapshot = self.mirror.snapshot().into_iter().map(ServerMsg::Event);
        for msg in [welcome, update].into_iter().chain(snapshot) {
            // An oversize event is skipped here too; see `broadcast`.
            if let Some(frame) = encode(&msg)
                && !peer.offer(&frame, false)
            {
                return;
            }
        }
        self.peers.push(peer);
    }

    fn on_command(&mut self, from: Option<ClientId>, cmd: Command) {
        match cmd {
            // Not forwarded: the daemon quits its main loop, then `Server::shutdown` stops
            // the Session.
            Command::Shutdown => (self.hooks.on_shutdown)(),
            Command::Search { req, query } => {
                let req = self.track_request(from, req);
                self.session.send(Command::Search { req, query });
            }
            Command::Browse { req, page } => {
                let req = self.track_request(from, req);
                self.session.send(Command::Browse { req, page });
            }
            cmd => self.session.send(cmd),
        }
    }

    /// Returns the global id to send to the Session in place of the requester's `req`.
    fn track_request(&mut self, from: Option<ClientId>, req: u64) -> u64 {
        self.next_request += 1;
        self.requests.insert(self.next_request, (from, req));
        self.next_request
    }
}

#[cfg(test)]
mod tests {
    use requiem_core::SessionConfig;
    use requiem_core::testing::{FakeEngineHandle, FakeProvider};

    use super::*;
    use crate::proto::UpdateState;

    fn hooks() -> ServerHooks {
        ServerHooks {
            on_shutdown: Box::new(|| {}),
            on_update: Box::new(|_| {}),
        }
    }

    fn idle() -> UpdateStatus {
        UpdateStatus {
            current: "test".into(),
            auto: true,
            state: UpdateState::Idle,
        }
    }

    #[test]
    fn snapshot_ends_with_volume() {
        let mut mirror = Mirror::new();
        assert_eq!(mirror.snapshot().last(), Some(&Event::Volume(100)));

        let track = Track {
            id: requiem_core::TrackId {
                provider: requiem_core::ProviderId("fake".into()),
                id: "a".into(),
            },
            title: "a".into(),
            artists: Vec::new(),
            album: None,
            duration: None,
            artwork_url: None,
        };
        mirror.apply(&Event::NowPlaying {
            track,
            duration: None,
        });
        mirror.apply(&Event::State(PlayState::Playing));
        mirror.apply(&Event::Position(Duration::from_secs(3)));
        mirror.apply(&Event::Volume(5));
        let snap = mirror.snapshot();
        assert!(snap.contains(&Event::Position(Duration::from_secs(3))));
        assert_eq!(snap.last(), Some(&Event::Volume(5)));
    }

    #[test]
    fn snapshot_carries_the_latest_account_before_volume() {
        let mut mirror = Mirror::new();
        let fake = || ProviderId("fake".into());
        mirror.apply(&Event::Account {
            provider: fake(),
            state: AccountState::SignedOut,
        });
        mirror.apply(&Event::Account {
            provider: ProviderId("other".into()),
            state: AccountState::Unavailable("no client".into()),
        });
        let signed_in = AccountState::SignedIn {
            name: Some("Me".into()),
        };
        mirror.apply(&Event::Account {
            provider: fake(),
            state: signed_in.clone(),
        });
        let snap = mirror.snapshot();
        let n = snap.len();
        assert_eq!(
            snap[n - 3..],
            [
                Event::Account {
                    provider: fake(),
                    state: signed_in,
                },
                Event::Account {
                    provider: ProviderId("other".into()),
                    state: AccountState::Unavailable("no client".into()),
                },
                Event::Volume(100),
            ]
        );
    }

    #[test]
    fn hub_starts_with_the_sessions_accounts() {
        let provider = FakeProvider::new("fake");
        provider.set_account(AccountState::Unavailable("set up a client".into()));
        let session = Session::spawn(
            vec![provider],
            FakeEngineHandle::new().factory(),
            SessionConfig::default(),
        );
        let (tx, handle) = spawn(session, hooks(), idle()).unwrap();
        let (sub_tx, sub) = unbounded();
        tx.send(HubMsg::Subscribe(sub_tx)).unwrap();
        let snap: Vec<Event> = sub
            .iter()
            .take_while(|e| !matches!(e, Event::Volume(_)))
            .collect();
        assert!(snap.contains(&Event::Account {
            provider: ProviderId("fake".into()),
            state: AccountState::Unavailable("set up a client".into()),
        }));
        tx.send(HubMsg::Stop).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn leaving_client_forgets_its_requests() {
        let session = Session::spawn(
            vec![FakeProvider::new("fake")],
            FakeEngineHandle::new().factory(),
            SessionConfig::default(),
        );
        let mut hub = Hub::new(session, hooks(), idle());
        for from in [Some(1), Some(2), Some(1), None] {
            hub.on_command(
                from,
                Command::Search {
                    req: 5,
                    query: "q".into(),
                },
            );
            hub.on_command(
                from,
                Command::Browse {
                    req: 6,
                    page: requiem_core::Page::Home,
                },
            );
        }
        hub.on_msg(HubMsg::Leave(1));
        let mut left: Vec<_> = hub.requests.values().copied().collect();
        left.sort();
        assert_eq!(left, [(None, 5), (None, 6), (Some(2), 5), (Some(2), 6)]);
    }
}
