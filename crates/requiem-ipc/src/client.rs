use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Child, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use requiem_core::{Command, Event};

use crate::frame::{self, Deadline};
use crate::path::{DIR_ENV, LOG_NAME, OLD_LOG_NAME, check_sun_path, socket_dir, socket_path_in};
use crate::proto::{ClientMsg, PROTOCOL, Role, ServerMsg, VERSION};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// A daemon that stops reading must not block a sender forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SPAWN_POLL: Duration = Duration::from_millis(25);
/// How long a spawned daemon that exited gets before nothing listening counts against it.
const EXIT_GRACE: Duration = Duration::from_millis(500);
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// The last message is always exactly one `Goodbye` or `Disconnected`.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Event(Event),
    Raise,
    /// The daemon quit on purpose, as opposed to the connection failing.
    Goodbye,
    /// The connection failed or was closed without a `Goodbye`; the reason is for display.
    Disconnected(String),
}

pub struct Client {
    stream: Arc<UnixStream>,
    /// Keeps frames from concurrent `send`s from interleaving.
    write: Mutex<()>,
}

impl Client {
    /// Fails with `ErrorKind::Unsupported` if the daemon speaks another protocol version.
    pub fn connect(path: &Path, role: Role) -> io::Result<(Client, Receiver<Incoming>)> {
        check_sun_path(path)?;
        Self::handshake(UnixStream::connect(path)?, role)
    }

    /// Connect; if nothing listens, spawn `daemon_exe` detached and retry until `timeout`.
    /// `daemon_exe` must be absolute.
    pub fn connect_or_spawn(
        daemon_exe: &Path,
        role: Role,
        timeout: Duration,
    ) -> io::Result<(Client, Receiver<Incoming>)> {
        if !daemon_exe.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "daemon executable must be absolute, got {}",
                    daemon_exe.display()
                ),
            ));
        }
        let dir = socket_dir()?;
        let path = socket_path_in(&dir)?;
        match Self::connect(&path, role) {
            Err(e) if nobody_listening(&e) => {}
            done => return done,
        }
        let log = dir.join(LOG_NAME);
        let deadline = Instant::now() + timeout;
        let mut child = spawn_daemon(daemon_exe, &dir)?;
        let mut respawned = false;
        let mut exited = None;
        loop {
            thread::sleep(SPAWN_POLL);
            match Self::connect(&path, role) {
                Err(e) if nobody_listening(&e) => {}
                done => {
                    reap(child);
                    return done;
                }
            }
            let now = Instant::now();
            if exited.is_none() {
                exited = child.try_wait().ok().flatten().map(|status| (status, now));
            }
            // Status 0 means another daemon holds the lock. It may still be starting, or be
            // stopping with its socket already gone, so wait, then spawn once more.
            if let Some((status, at)) = exited
                && now.duration_since(at) >= EXIT_GRACE
            {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "{} exited with {status}; see {}",
                        daemon_exe.display(),
                        log.display()
                    )));
                }
                if !respawned {
                    respawned = true;
                    exited = None;
                    child = spawn_daemon(daemon_exe, &dir)?;
                }
            }
            if now >= deadline {
                reap(child);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "daemon not listening after {timeout:?}; see {}",
                        log.display()
                    ),
                ));
            }
        }
    }

    /// A failed or timed-out write closes the connection; the receiver then gets
    /// `Disconnected`.
    pub fn send(&self, cmd: Command) -> io::Result<()> {
        let frame = frame::encode(&ClientMsg::Command(cmd))?;
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let written = (&mut &*self.stream).write_all(&frame);
        if written.is_err() {
            // Part of the frame may be on the wire, so the stream can't carry another.
            let _ = self.stream.shutdown(Shutdown::Both);
        }
        written
    }

    pub(crate) fn handshake(
        stream: UnixStream,
        role: Role,
    ) -> io::Result<(Client, Receiver<Incoming>)> {
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let hello = ClientMsg::Hello {
            protocol: PROTOCOL,
            role,
            version: VERSION.into(),
        };
        frame::write(&mut &stream, &hello)?;
        match frame::read(&mut Deadline::new(&stream, HANDSHAKE_TIMEOUT))? {
            Some(ServerMsg::Welcome {
                protocol: PROTOCOL, ..
            }) => {}
            Some(ServerMsg::Incompatible { supported }) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "daemon speaks IPC protocol {supported}; this client speaks {PROTOCOL}"
                    ),
                ));
            }
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected handshake reply: {other:?}"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed the connection during the handshake",
                ));
            }
        }
        // A daemon that is stopping may already have sent `Goodbye` and closed.
        frame::set_read_timeout(&stream, None)?;
        let stream = Arc::new(stream);
        let (tx, rx) = unbounded();
        let reader = stream.clone();
        thread::Builder::new()
            .name("requiem-ipc-client".into())
            .spawn(move || read_loop(&reader, &tx))?;
        let client = Client {
            stream,
            write: Mutex::new(()),
        };
        Ok((client, rx))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn read_loop(stream: &UnixStream, tx: &Sender<Incoming>) {
    let last = loop {
        let incoming = match frame::read(&mut &*stream) {
            Ok(Some(ServerMsg::Event(ev))) => Incoming::Event(ev),
            Ok(Some(ServerMsg::Raise)) => Incoming::Raise,
            Ok(Some(ServerMsg::Pong(_))) => continue,
            Ok(Some(ServerMsg::Goodbye)) => break Some(Incoming::Goodbye),
            Ok(Some(other)) => {
                break Some(Incoming::Disconnected(format!(
                    "unexpected message from daemon: {other:?}"
                )));
            }
            Ok(None) => {
                break Some(Incoming::Disconnected(
                    "daemon closed the connection".to_owned(),
                ));
            }
            Err(e) => break Some(Incoming::Disconnected(e.to_string())),
        };
        if tx.send(incoming).is_err() {
            break None;
        }
    };
    let _ = stream.shutdown(Shutdown::Both);
    if let Some(last) = last {
        let _ = tx.send(last);
    }
}

fn nobody_listening(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Appended to, not truncated: a spawner that loses the race must not cut the log the
/// running daemon writes to. Racing rotations are harmless.
fn open_log(dir: &Path) -> io::Result<File> {
    let path = dir.join(LOG_NAME);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = fs::rename(&path, dir.join(OLD_LOG_NAME));
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

fn spawn_daemon(exe: &Path, dir: &Path) -> io::Result<Child> {
    let log = open_log(dir)?;
    let mut cmd = process::Command::new(exe);
    // The daemon must listen where this client polls, whatever it would resolve itself.
    cmd.env(DIR_ENV, dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // A new session takes the daemon out of the caller's process group, so it outlives it.
    // SAFETY: setsid is async-signal-safe, so it may run between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Waits on the daemon from a thread so it isn't left a zombie when it exits.
fn reap(mut child: Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    let child = Arc::new(Mutex::new(child));
    let waiter = child.clone();
    let spawned = thread::Builder::new()
        .name("requiem-ipc-reaper".into())
        .spawn(move || wait_on(&waiter));
    if spawned.is_err() {
        // Blocks until the daemon exits.
        wait_on(&child);
    }
}

fn wait_on(child: &Mutex<Child>) {
    let _ = child.lock().unwrap_or_else(PoisonError::into_inner).wait();
}
