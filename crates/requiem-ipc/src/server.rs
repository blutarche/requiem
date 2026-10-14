use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use requiem_core::{Command, Event, Session};

use crate::frame::{self, Deadline};
use crate::hub::{self, ClientId, Frame, HubMsg, QUEUE_CAP};
use crate::path::{LOCK_NAME, socket_dir, socket_path_in};
use crate::proto::{ClientMsg, HelloProbe, PROTOCOL, Role, ServerMsg, UpdateStatus};
use crate::sys;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

pub struct ServerHooks {
    /// Called on the hub thread when a client sends `Command::Shutdown`. It must not call
    /// back into the `Server`; the daemon leaves its main loop and calls `Server::shutdown`.
    pub on_shutdown: Box<dyn Fn() + Send + Sync>,
    /// Called on the hub thread for a client's update request, under the same rule.
    pub on_update: Box<dyn Fn(UpdateRequest) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateRequest {
    Check,
    Install,
    SetAuto(bool),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCounts {
    pub gui: usize,
    pub cli: usize,
}

#[derive(Debug)]
pub enum ServeError {
    /// Another daemon holds the single-instance lock.
    AlreadyRunning,
    Io(io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => f.write_str("a requiem daemon is already running"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl Error for ServeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyRunning => None,
            Self::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for ServeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct Server {
    hub: Sender<HubMsg>,
    hub_thread: Option<JoinHandle<()>>,
    accept_thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    path: PathBuf,
    /// Released only after the socket file is removed. `None` once handed to the caller.
    lock: Option<File>,
}

impl Server {
    /// Takes the single-instance lock, binds the socket and starts the accept and hub threads.
    /// Fails with `AlreadyRunning` if another daemon holds the lock. Every client gets
    /// `update` after `Welcome` until `set_update` replaces it.
    pub fn start(
        session: Session,
        hooks: ServerHooks,
        update: UpdateStatus,
    ) -> Result<Server, ServeError> {
        Self::start_in(&socket_dir()?, session, hooks, update)
    }

    /// `dir` must already be created and validated.
    pub(crate) fn start_in(
        dir: &Path,
        session: Session,
        hooks: ServerHooks,
        update: UpdateStatus,
    ) -> Result<Server, ServeError> {
        let path = socket_path_in(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(LOCK_NAME))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(ServeError::AlreadyRunning),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        // Under the lock, a socket file here can only be left over from a dead daemon.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        let started = fs::set_permissions(&path, Permissions::from_mode(0o600))
            .and_then(|()| hub::spawn(session, hooks, update));
        let (hub, hub_thread) = match started {
            Ok(h) => h,
            Err(e) => {
                // Still under the lock, so this can't be another daemon's socket.
                let _ = fs::remove_file(&path);
                return Err(e.into());
            }
        };
        let mut server = Server {
            hub: hub.clone(),
            hub_thread: Some(hub_thread),
            accept_thread: None,
            stopping: Arc::new(AtomicBool::new(false)),
            path,
            lock: Some(lock),
        };
        let stopping = server.stopping.clone();
        // On failure, dropping `server` removes the socket before releasing the lock.
        server.accept_thread = Some(
            thread::Builder::new()
                .name("requiem-ipc-accept".into())
                .spawn(move || accept_loop(&listener, &hub, &stopping))?,
        );
        Ok(server)
    }

    /// In-daemon controls (menu bar, media keys); same path as client commands.
    pub fn send(&self, cmd: Command) {
        let _ = self.hub.send(HubMsg::Command { from: None, cmd });
    }

    /// Starts with the same snapshot a connecting client gets. Results of searches sent
    /// through `send` arrive here; client searches never do.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = unbounded();
        let _ = self.hub.send(HubMsg::Subscribe(tx));
        rx
    }

    pub fn clients(&self) -> ClientCounts {
        self.ask(HubMsg::Counts).unwrap_or_default()
    }

    /// True if a GUI client was told to raise its window.
    pub fn raise_gui(&self) -> bool {
        self.ask(HubMsg::Raise).unwrap_or(false)
    }

    pub fn set_update(&self, status: UpdateStatus) {
        let _ = self.hub.send(HubMsg::Update(status));
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    /// Stops like `shutdown` but keeps the single-instance lock, which is released when the
    /// returned file is dropped. Meanwhile a daemon that starts exits with `AlreadyRunning`.
    pub fn shutdown_keeping_lock(mut self) -> File {
        self.stop();
        self.lock
            .take()
            .expect("only taken here, which consumes the server")
    }

    fn ask<T>(&self, msg: impl FnOnce(Sender<T>) -> HubMsg) -> Option<T> {
        let (tx, rx) = bounded(1);
        self.hub.send(msg(tx)).ok()?;
        rx.recv().ok()
    }

    fn stop(&mut self) {
        let Some(hub_thread) = self.hub_thread.take() else {
            return;
        };
        self.stopping.store(true, Ordering::Release);
        // Connecting wakes the blocked accept so the thread sees the flag. If the path no
        // longer reaches this listener, the wake fails and the thread is left detached.
        if let Some(accept) = self.accept_thread.take()
            && UnixStream::connect(&self.path).is_ok()
        {
            let _ = accept.join();
        }
        let _ = fs::remove_file(&self.path);
        let _ = self.hub.send(HubMsg::Stop);
        let _ = hub_thread.join();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(listener: &UnixListener, hub: &Sender<HubMsg>, stopping: &AtomicBool) {
    let mut next_id: ClientId = 0;
    for conn in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
            return;
        }
        match conn {
            Ok(stream) => {
                next_id += 1;
                admit(next_id, stream, hub);
            }
            // Typically fd exhaustion; back off instead of spinning.
            Err(_) => thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

/// No cap on connections: each one costs two threads until it closes.
pub(crate) fn admit(id: ClientId, stream: UnixStream, hub: &Sender<HubMsg>) {
    let euid = sys::euid();
    if !sys::peer_uid(&stream).is_ok_and(|uid| uid == euid) {
        return;
    }
    let hub = hub.clone();
    let _ = thread::Builder::new()
        .name("requiem-ipc-conn".into())
        .spawn(move || serve(id, stream, &hub));
}

/// Runs on the connection's own thread, so a silent peer never stalls accept.
fn serve(id: ClientId, stream: UnixStream, hub: &Sender<HubMsg>) {
    let Some(role) = handshake(&stream) else {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };
    let stream = Arc::new(stream);
    let (queue, outbound) = bounded(QUEUE_CAP);
    let (flushed_tx, flushed) = bounded::<()>(0);
    let writer = {
        let stream = stream.clone();
        thread::Builder::new()
            .name("requiem-ipc-writer".into())
            .spawn(move || {
                write_loop(&stream, &outbound);
                drop(flushed_tx);
            })
    };
    let join = HubMsg::Join {
        id,
        role,
        queue,
        stream: stream.clone(),
        flushed,
    };
    if writer.is_ok() && hub.send(join).is_ok() {
        forward(id, &stream, hub);
    }
    let _ = hub.send(HubMsg::Leave(id));
    let _ = stream.shutdown(Shutdown::Both);
}

fn forward(id: ClientId, stream: &UnixStream, hub: &Sender<HubMsg>) {
    loop {
        let msg = match frame::read(&mut &*stream) {
            Ok(Some(ClientMsg::Command(cmd))) => HubMsg::Command {
                from: Some(id),
                cmd,
            },
            Ok(Some(ClientMsg::Ping(n))) => HubMsg::Ping { id, n },
            Ok(Some(ClientMsg::CheckForUpdates)) => HubMsg::UpdateRequest(UpdateRequest::Check),
            Ok(Some(ClientMsg::InstallUpdate)) => HubMsg::UpdateRequest(UpdateRequest::Install),
            Ok(Some(ClientMsg::SetAutoUpdate(on))) => {
                HubMsg::UpdateRequest(UpdateRequest::SetAuto(on))
            }
            // A second Hello, a bad or oversize frame, or the end of the stream.
            Ok(Some(ClientMsg::Hello { .. }) | None) | Err(_) => return,
        };
        if hub.send(msg).is_err() {
            return;
        }
    }
}

/// Reads the client's `Hello` and answers a protocol mismatch with `Incompatible`.
/// `Welcome` comes from the hub, ahead of the snapshot.
fn handshake(stream: &UnixStream) -> Option<Role> {
    let body = frame::read_body(&mut Deadline::new(stream, HANDSHAKE_TIMEOUT)).ok()??;
    let HelloProbe::Hello { protocol } = frame::decode(&body).ok()?;
    if protocol != PROTOCOL {
        let reply = ServerMsg::Incompatible {
            supported: PROTOCOL,
        };
        let _ = frame::write(&mut &*stream, &reply);
        return None;
    }
    let ClientMsg::Hello { role, .. } = frame::decode(&body).ok()? else {
        return None;
    };
    stream.set_read_timeout(None).ok()?;
    Some(role)
}

fn write_loop(stream: &UnixStream, outbound: &Receiver<Frame>) {
    for frame in outbound {
        if (&mut &*stream).write_all(&frame).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}
