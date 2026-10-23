//! The helper's end of the socket, over any [`Backend`]: librespot in `requiemd`, fakes in
//! tests. Each connection gets its own thread, because an attached stream's reads block.

use std::fs::{self, Permissions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use requiem_core::{Browsed, Page, ProviderError, Track};
use serde::Serialize;

use crate::logger::redact;
use crate::sys;
use crate::wire::{
    self, Attached, Changed, Chunk, Done, MAX_READ, Pending, Prepared, Request, Shelves,
    SignInLine, Tracks,
};

/// What the helper serves. Every method blocks.
pub trait Backend: Send + Sync + 'static {
    fn search(&self, q: &str, limit: usize) -> Result<Vec<Track>, ProviderError>;
    fn browse(&self, page: &Page) -> Result<Browsed, ProviderError>;
    /// Gets ready to stream `track` without opening a CDN connection; returns a stream id.
    fn prepare(&self, track: &str) -> Result<String, ProviderError>;
    /// Opens a stream `prepare` returned. Each id opens once.
    fn attach(&self, stream: &str) -> Result<Box<dyn Stream>, ProviderError>;
    /// Calls `pending` once the user has a code to enter, then waits until the sign-in ends.
    fn sign_in(&self, pending: &mut dyn FnMut(Pending)) -> Result<(), ProviderError>;
    /// Also ends a pending sign-in.
    fn sign_out(&self) -> Result<(), ProviderError>;
    /// Calls `changed` with each page that gets newer content than the backend last gave for
    /// it, until `changed` returns false. A backend whose pages never change returns at once.
    fn watch(&self, _changed: &mut dyn FnMut(Page) -> bool) {}
}

/// An attached stream's playable bytes.
pub trait Stream: Send {
    fn byte_len(&self) -> u64;
    /// Fills `buf` from `offset`; fewer bytes only at the end of the stream.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

/// Serves `backend` on `socket` and exits when stdin closes, so the helper dies with the daemon
/// that holds the other end.
pub fn serve(socket: &Path, backend: Arc<dyn Backend>) -> ExitCode {
    let watcher = thread::Builder::new()
        .name("spotify-stdin".into())
        .spawn(|| {
            let mut sink = [0u8; 64];
            let mut stdin = io::stdin().lock();
            loop {
                match stdin.read(&mut sink) {
                    Ok(0) => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                    Ok(_) => {}
                }
            }
            // Leaves the socket file: a newer helper may already own the path.
            std::process::exit(0);
        });
    if let Err(e) = watcher {
        eprintln!("[spotify] helper: {e}");
        return ExitCode::from(1);
    }
    match bind(socket) {
        Ok(listener) => {
            accept_loop(&listener, &backend);
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("[spotify] cannot listen on {}: {e}", socket.display());
            ExitCode::from(1)
        }
    }
}

/// Replaces a stale socket file; the socket is 0600 inside the daemon's 0700 directory.
pub(crate) fn bind(socket: &Path) -> io::Result<UnixListener> {
    match fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Returns only if the listener fails for good.
pub(crate) fn accept_loop(listener: &UnixListener, backend: &Arc<dyn Backend>) {
    let me = sys::euid();
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("[spotify] accept: {e}");
                // Out of descriptors, say: let connections finish rather than spin.
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        match sys::peer_uid(&conn) {
            Ok(uid) if uid == me => {}
            Ok(uid) => {
                eprintln!("[spotify] refused a connection from uid {uid}");
                continue;
            }
            Err(e) => {
                eprintln!("[spotify] refused a connection: {e}");
                continue;
            }
        }
        let backend = backend.clone();
        let spawned = thread::Builder::new()
            .name("spotify-conn".into())
            .spawn(move || {
                let _ = handle(conn, &*backend);
            });
        if let Err(e) = spawned {
            eprintln!("[spotify] {e}");
        }
    }
}

fn handle(conn: UnixStream, backend: &dyn Backend) -> io::Result<()> {
    let mut w = BufWriter::new(conn.try_clone()?);
    let mut r = BufReader::new(conn);
    // The daemon's readiness check connects and hangs up without a word.
    let Some(line) = wire::read_line(&mut r)? else {
        return Ok(());
    };
    let Ok(req) = wire::parse::<Request>(&line) else {
        return reply::<Done>(&mut w, Err(ProviderError::Parse("unknown request".into())));
    };
    match req {
        Request::Search { q, limit } => reply(
            &mut w,
            backend.search(&q, limit).map(|tracks| Tracks { tracks }),
        ),
        Request::Browse { page } => reply(
            &mut w,
            backend
                .browse(&page)
                .map(|Browsed { about, shelves }| Shelves { shelves, about }),
        ),
        Request::Prepare { track } => reply(
            &mut w,
            backend.prepare(&track).map(|stream| Prepared { stream }),
        ),
        Request::SignOut => reply(&mut w, backend.sign_out().map(|()| Done { ok: true })),
        Request::SignIn => {
            let result = backend.sign_in(&mut |p| {
                // A daemon that hung up learns nothing more; its sign-out cancels the flow.
                let _ = wire::write_line(&mut w, &SignInLine::Pending(p)).and_then(|()| w.flush());
            });
            reply(&mut w, result.map(|()| SignInLine::SignedIn {}))
        }
        Request::Watch => {
            // Ends once a line can't be written: the daemon hung up.
            backend.watch(&mut |changed| {
                wire::write_line(&mut w, &Changed { changed })
                    .and_then(|()| w.flush())
                    .is_ok()
            });
            Ok(())
        }
        Request::Attach { stream } => match backend.attach(&stream) {
            Ok(s) => serve_stream(&mut r, &mut w, s),
            Err(e) => reply::<Done>(&mut w, Err(e)),
        },
        Request::Read { .. } => reply::<Done>(
            &mut w,
            Err(ProviderError::Parse("read before attach".into())),
        ),
    }
}

fn reply<T: Serialize>(
    w: &mut BufWriter<UnixStream>,
    result: Result<T, ProviderError>,
) -> io::Result<()> {
    match result {
        Ok(msg) => wire::write_line(w, &msg)?,
        Err(e) => wire::write_err(w, &e)?,
    }
    w.flush()
}

/// Answers `read` lines until the daemon closes the connection, which drops the stream.
fn serve_stream(
    r: &mut BufReader<UnixStream>,
    w: &mut BufWriter<UnixStream>,
    mut s: Box<dyn Stream>,
) -> io::Result<()> {
    wire::write_line(
        w,
        &Attached {
            ok: true,
            len: s.byte_len(),
        },
    )?;
    w.flush()?;
    let mut buf = vec![0u8; MAX_READ];
    while let Some(line) = wire::read_line(r)? {
        let Ok(Request::Read { offset, len }) = wire::parse::<Request>(&line) else {
            return reply::<Done>(
                w,
                Err(ProviderError::Parse(
                    "an attached stream takes only reads".into(),
                )),
            );
        };
        let want = len.min(MAX_READ);
        match s.read_at(offset, &mut buf[..want]) {
            Ok(n) => {
                wire::write_line(w, &Chunk { ok: n })?;
                w.write_all(&buf[..n])?;
            }
            Err(e) => wire::write_err(
                w,
                &ProviderError::Network(format!("Spotify stream: {}", redact(&e.to_string()))),
            )?,
        }
        w.flush()?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::BufRead;
    use std::sync::Mutex;

    use requiem_core::Shelf;

    use super::*;
    use crate::testing::TempDir;
    use crate::wire::ReplyError;

    /// Serves `pattern(i)` for i in 0..len; reads from `fail_from` on fail.
    pub(crate) struct FakeStream {
        pub len: u64,
        pub fail_from: Option<u64>,
    }

    pub(crate) fn pattern(i: u64) -> u8 {
        (i * 31 % 251) as u8
    }

    impl Stream for FakeStream {
        fn byte_len(&self) -> u64 {
            self.len
        }

        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_from.is_some_and(|f| offset >= f) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "stalled"));
            }
            let end = (offset + buf.len() as u64).min(self.len);
            let n = end.saturating_sub(offset) as usize;
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = pattern(offset + i as u64);
            }
            Ok(n)
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeBackend {
        pub signed_out: Mutex<bool>,
    }

    impl Backend for FakeBackend {
        fn search(&self, q: &str, _limit: usize) -> Result<Vec<Track>, ProviderError> {
            match q {
                "fail" => Err(ProviderError::SignInRequired("sign in".into())),
                _ => Ok(Vec::new()),
            }
        }

        fn browse(&self, _page: &Page) -> Result<Browsed, ProviderError> {
            Ok(Browsed {
                about: None,
                shelves: vec![Shelf {
                    title: "Spotify".into(),
                    items: Vec::new(),
                }],
            })
        }

        fn prepare(&self, track: &str) -> Result<String, ProviderError> {
            Ok(format!("s-{track}"))
        }

        fn attach(&self, stream: &str) -> Result<Box<dyn Stream>, ProviderError> {
            match stream {
                "s-short" => Ok(Box::new(FakeStream {
                    len: 100_000,
                    fail_from: Some(70_000),
                })),
                _ => Err(ProviderError::Network("expired".into())),
            }
        }

        fn sign_in(&self, pending: &mut dyn FnMut(Pending)) -> Result<(), ProviderError> {
            pending(Pending {
                url: "https://spotify.com/pair?code=ABC".into(),
                code: "ABC".into(),
                expires_at_unix: 99,
            });
            Ok(())
        }

        fn sign_out(&self) -> Result<(), ProviderError> {
            *self.signed_out.lock().unwrap() = true;
            Ok(())
        }

        fn watch(&self, changed: &mut dyn FnMut(Page) -> bool) {
            for page in [Page::Home, Page::Library] {
                if !changed(page) {
                    return;
                }
            }
        }
    }

    fn start(tmp: &TempDir) -> (std::path::PathBuf, Arc<FakeBackend>) {
        let path = tmp.path().join("spotify.sock");
        // A stale file from a dead helper is replaced.
        fs::write(&path, "stale").unwrap();
        let listener = bind(&path).unwrap();
        let fake = Arc::new(FakeBackend::default());
        let backend: Arc<dyn Backend> = fake.clone();
        thread::spawn(move || accept_loop(&listener, &backend));
        (path, fake)
    }

    fn request(path: &Path, req: &Request) -> (BufReader<UnixStream>, UnixStream) {
        let conn = UnixStream::connect(path).unwrap();
        let mut w = conn.try_clone().unwrap();
        wire::write_line(&mut w, req).unwrap();
        (BufReader::new(conn), w)
    }

    #[test]
    fn the_socket_is_0600_and_answers_each_op() {
        let tmp = TempDir::new();
        let (path, fake) = start(&tmp);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let (mut r, _w) = request(
            &path,
            &Request::Search {
                q: "fail".into(),
                limit: 3,
            },
        );
        assert!(matches!(
            wire::read_reply::<_, Tracks>(&mut r),
            Err(ReplyError::Provider(ProviderError::SignInRequired(_)))
        ));
        let (mut r, _w) = request(
            &path,
            &Request::Prepare {
                track: "short".into(),
            },
        );
        let p: Prepared = wire::read_reply(&mut r).unwrap();
        assert_eq!(p.stream, "s-short");
        let (mut r, _w) = request(&path, &Request::SignIn);
        let line: SignInLine = wire::read_reply(&mut r).unwrap();
        assert!(matches!(line, SignInLine::Pending(p) if p.code == "ABC"));
        let line: SignInLine = wire::read_reply(&mut r).unwrap();
        assert!(matches!(line, SignInLine::SignedIn {}));
        let (mut r, _w) = request(&path, &Request::SignOut);
        let d: Done = wire::read_reply(&mut r).unwrap();
        assert!(d.ok);
        assert!(*fake.signed_out.lock().unwrap());
        let (mut r, _w) = request(&path, &Request::Watch);
        for page in [Page::Home, Page::Library] {
            let c: Changed = wire::read_reply(&mut r).unwrap();
            assert_eq!(c.changed, page);
        }

        // A connection that says nothing is a readiness check, and a bad line gets an error.
        drop(UnixStream::connect(&path).unwrap());
        let conn = UnixStream::connect(&path).unwrap();
        (&conn).write_all(b"{\"op\":\"dance\"}\n").unwrap();
        let mut r = BufReader::new(conn);
        assert!(matches!(
            wire::read_reply::<_, Done>(&mut r),
            Err(ReplyError::Provider(ProviderError::Parse(_)))
        ));
    }

    #[test]
    fn an_attached_stream_reads_short_at_the_end_and_reports_errors() {
        let tmp = TempDir::new();
        let (path, _) = start(&tmp);
        let (mut r, mut w) = request(
            &path,
            &Request::Attach {
                stream: "s-short".into(),
            },
        );
        let a: Attached = wire::read_reply(&mut r).unwrap();
        assert_eq!(a.len, 100_000);
        let mut read = |offset: u64, len: usize| -> Result<Vec<u8>, ReplyError> {
            wire::write_line(&mut w, &Request::Read { offset, len }).unwrap();
            let c: Chunk = wire::read_reply(&mut r)?;
            let mut buf = vec![0; c.ok];
            r.read_exact(&mut buf).unwrap();
            Ok(buf)
        };
        let got = read(10, 1000).unwrap();
        assert_eq!(got.len(), 1000);
        assert!(
            got.iter()
                .enumerate()
                .all(|(i, b)| *b == pattern(10 + i as u64))
        );
        // Asking for more than MAX_READ gets MAX_READ.
        assert_eq!(read(0, 10 * MAX_READ).unwrap().len(), MAX_READ);
        assert!(matches!(
            read(69_000, 100),
            Ok(b) if b.len() == 100
        ));
        assert!(matches!(
            read(70_000, 100),
            Err(ReplyError::Provider(ProviderError::Network(_)))
        ));
        // The connection still serves after an error.
        assert_eq!(
            read(5, 5).unwrap(),
            (5..10).map(pattern).collect::<Vec<_>>()
        );

        // Unknown streams fail, and an attached connection takes only reads.
        let (mut r, _w) = request(
            &path,
            &Request::Attach {
                stream: "s-gone".into(),
            },
        );
        assert!(matches!(
            wire::read_reply::<_, Attached>(&mut r),
            Err(ReplyError::Provider(ProviderError::Network(_)))
        ));
        let (mut r, mut w) = request(
            &path,
            &Request::Attach {
                stream: "s-short".into(),
            },
        );
        let _: Attached = wire::read_reply(&mut r).unwrap();
        wire::write_line(&mut w, &Request::SignOut).unwrap();
        assert!(matches!(
            wire::read_reply::<_, Chunk>(&mut r),
            Err(ReplyError::Provider(ProviderError::Parse(_)))
        ));
        let mut rest = String::new();
        assert_eq!(r.read_line(&mut rest).unwrap(), 0);
    }
}
