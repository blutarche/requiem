use std::fs::OpenOptions;
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

use crate::frame;
use crate::path::{DIR_ENV, LOG_NAME, check_sun_path, socket_dir, socket_path_in};
use crate::proto::{ClientMsg, PROTOCOL, Role, ServerMsg, VERSION};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Event(Event),
    Raise,
    /// Always the last message; the reason is for display.
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
    pub fn connect_or_spawn(
        daemon_exe: &Path,
        role: Role,
        timeout: Duration,
    ) -> io::Result<(Client, Receiver<Incoming>)> {
        let dir = socket_dir()?;
        let path = socket_path_in(&dir)?;
        match Self::connect(&path, role) {
            Err(e) if nobody_listening(&e) => {}
            done => return done,
        }
        let log = dir.join(LOG_NAME);
        let mut child = spawn_daemon(daemon_exe, &dir)?;
        let deadline = Instant::now() + timeout;
        loop {
            thread::sleep(SPAWN_POLL);
            match Self::connect(&path, role) {
                Err(e) if nobody_listening(&e) => {}
                done => {
                    reap(child);
                    return done;
                }
            }
            // Exit status 0 is a daemon that lost the single-instance race to another
            // spawner's; keep polling for the winner.
            if let Some(status) = child.try_wait()?
                && !status.success()
            {
                return Err(io::Error::other(format!(
                    "{} exited with {status}; see {}",
                    daemon_exe.display(),
                    log.display()
                )));
            }
            if Instant::now() >= deadline {
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

    pub fn send(&self, cmd: Command) -> io::Result<()> {
        let frame = frame::encode(&ClientMsg::Command(cmd))?;
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        (&mut &*self.stream).write_all(&frame)
    }

    pub(crate) fn handshake(
        stream: UnixStream,
        role: Role,
    ) -> io::Result<(Client, Receiver<Incoming>)> {
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let hello = ClientMsg::Hello {
            protocol: PROTOCOL,
            role,
            version: VERSION.into(),
        };
        frame::write(&mut &stream, &hello)?;
        match frame::read(&mut &stream)? {
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
        stream.set_read_timeout(None)?;
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
    let reason = loop {
        let incoming = match frame::read(&mut &*stream) {
            Ok(Some(ServerMsg::Event(ev))) => Incoming::Event(ev),
            Ok(Some(ServerMsg::Raise)) => Incoming::Raise,
            Ok(Some(ServerMsg::Pong(_))) => continue,
            Ok(Some(other)) => break Some(format!("unexpected message from daemon: {other:?}")),
            Ok(None) => break Some("daemon closed the connection".to_owned()),
            Err(e) => break Some(e.to_string()),
        };
        if tx.send(incoming).is_err() {
            break None;
        }
    };
    let _ = stream.shutdown(Shutdown::Both);
    if let Some(reason) = reason {
        let _ = tx.send(Incoming::Disconnected(reason));
    }
}

fn nobody_listening(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn spawn_daemon(exe: &Path, dir: &Path) -> io::Result<Child> {
    let log = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(LOG_NAME))?;
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

/// Waits on a running daemon from a thread so it isn't left a zombie when it exits.
fn reap(mut child: Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = thread::Builder::new()
            .name("requiem-ipc-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}
