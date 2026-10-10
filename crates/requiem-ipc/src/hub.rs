use std::collections::HashMap;
use std::io;
use std::iter;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, never, select, unbounded};
use requiem_core::{Command, Event, PlayState, Session, Track};

use crate::frame;
use crate::proto::{PROTOCOL, Role, ServerMsg, VERSION};
use crate::server::{ClientCounts, ServerHooks};

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
}

impl Mirror {
    /// A `Session` emits nothing until something changes, so this must match its initial
    /// state, and the hub must subscribe before any command reaches the Session.
    fn new() -> Self {
        Self {
            tracks: Vec::new(),
            current: None,
            now: None,
            state: PlayState::Stopped,
            volume: 100,
            position: Duration::ZERO,
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
            Event::SearchResults { .. }
            | Event::SearchFailed { .. }
            | Event::TrackFailed { .. }
            | Event::ProviderBroken { .. } => {}
        }
    }

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
        evs.push(Event::Volume(self.volume));
        if self.now.is_some() {
            evs.push(Event::Position(self.position));
        }
        evs
    }
}

struct Hub {
    session: Session,
    hooks: ServerHooks,
    mirror: Mirror,
    peers: Vec<Peer>,
    subs: Vec<Sender<Event>>,
    /// Global search id sent to the Session -> (requester, the requester's own id).
    searches: HashMap<u64, (Option<ClientId>, u64)>,
    next_search: u64,
}

pub(crate) fn spawn(
    session: Session,
    hooks: ServerHooks,
) -> io::Result<(Sender<HubMsg>, JoinHandle<()>)> {
    let events = session.subscribe();
    let (tx, rx) = unbounded();
    let hub = Hub::new(session, hooks);
    let handle = thread::Builder::new()
        .name("requiem-ipc-hub".into())
        .spawn(move || hub.run(&rx, events))?;
    Ok((tx, handle))
}

fn encode(msg: &ServerMsg) -> Option<Frame> {
    frame::encode(msg).ok().map(Arc::new)
}

impl Hub {
    fn new(session: Session, hooks: ServerHooks) -> Self {
        Self {
            session,
            hooks,
            mirror: Mirror::new(),
            peers: Vec::new(),
            subs: Vec::new(),
            searches: HashMap::new(),
            next_search: 0,
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

    /// Search results go only to the requester, under its own id. In-daemon searches
    /// report to subscribers.
    fn route(&mut self, global: u64, make: impl FnOnce(u64) -> Event) {
        let Some((from, req)) = self.searches.remove(&global) else {
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
                self.searches.retain(|_, (from, _)| *from != Some(id));
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
            HubMsg::Stop => unreachable!("handled in run"),
        }
    }

    /// Welcome and the snapshot are queued before the peer joins the broadcast list, all on
    /// this thread, so no live event can come between them or be missed.
    fn join(&mut self, peer: Peer) {
        let welcome = ServerMsg::Welcome {
            protocol: PROTOCOL,
            version: VERSION.into(),
        };
        let snapshot = self.mirror.snapshot().into_iter().map(ServerMsg::Event);
        for msg in iter::once(welcome).chain(snapshot) {
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
                self.next_search += 1;
                self.searches.insert(self.next_search, (from, req));
                self.session.send(Command::Search {
                    req: self.next_search,
                    query,
                });
            }
            cmd => self.session.send(cmd),
        }
    }
}

#[cfg(test)]
mod tests {
    use requiem_core::SessionConfig;
    use requiem_core::testing::{FakeEngineHandle, FakeProvider};

    use super::*;

    #[test]
    fn leaving_client_forgets_its_searches() {
        let session = Session::spawn(
            vec![FakeProvider::new("fake")],
            FakeEngineHandle::new().factory(),
            SessionConfig::default(),
        );
        let hooks = ServerHooks {
            on_shutdown: Box::new(|| {}),
        };
        let mut hub = Hub::new(session, hooks);
        for from in [Some(1), Some(2), Some(1), None] {
            hub.on_command(
                from,
                Command::Search {
                    req: 5,
                    query: "q".into(),
                },
            );
        }
        hub.on_msg(HubMsg::Leave(1));
        let mut left: Vec<_> = hub.searches.values().copied().collect();
        left.sort();
        assert_eq!(left, [(None, 5), (Some(2), 5)]);
    }
}
