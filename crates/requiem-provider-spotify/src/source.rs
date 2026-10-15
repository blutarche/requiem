//! The daemon's end of a stream: a `ByteSource` that attaches to the helper on first use.

use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use requiem_core::{ByteSource, ProviderError};

use crate::wire::{self, Attached, Chunk, MAX_READ, ReplyError, Request};

/// librespot's own read gives up after 8 s.
pub(crate) const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct HelperSource {
    socket: PathBuf,
    stream: String,
    /// A mutex because `byte_len(&self)` may have to attach.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conn: Option<Conn>,
    len: u64,
    pos: u64,
    /// Set once the connection fails: the helper has dropped the stream, so it stays failed.
    failed: Option<String>,
}

struct Conn {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl HelperSource {
    /// Opens nothing until the first read, seek from the end, or `byte_len`.
    pub(crate) fn new(socket: PathBuf, stream: String) -> Self {
        Self {
            socket,
            stream,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn attach(&self, st: &mut State) -> io::Result<()> {
        if st.conn.is_some() {
            return Ok(());
        }
        if let Some(e) = &st.failed {
            return Err(io::Error::other(e.clone()));
        }
        let result = self.open();
        match result {
            Ok((conn, len)) => {
                st.conn = Some(conn);
                st.len = len;
                Ok(())
            }
            Err(e) => {
                st.failed = Some(e.to_string());
                Err(e)
            }
        }
    }

    fn open(&self) -> io::Result<(Conn, u64)> {
        let conn = UnixStream::connect(&self.socket)?;
        conn.set_read_timeout(Some(STREAM_READ_TIMEOUT))?;
        conn.set_write_timeout(Some(STREAM_READ_TIMEOUT))?;
        let mut w = conn.try_clone()?;
        wire::write_line(
            &mut w,
            &Request::Attach {
                stream: self.stream.clone(),
            },
        )?;
        let mut r = BufReader::new(conn);
        let a: Attached = wire::read_reply(&mut r).map_err(into_io)?;
        Ok((Conn { r, w }, a.len))
    }

    fn read_chunk(conn: &mut Conn, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        wire::write_line(
            &mut conn.w,
            &Request::Read {
                offset,
                len: buf.len(),
            },
        )?;
        let c: Chunk = wire::read_reply(&mut conn.r).map_err(into_io)?;
        if c.ok > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the Spotify helper sent more than asked for",
            ));
        }
        conn.r.read_exact(&mut buf[..c.ok])?;
        Ok(c.ok)
    }
}

fn into_io(e: ReplyError) -> io::Error {
    match e {
        ReplyError::Io(e) => e,
        ReplyError::Provider(ProviderError::Network(m)) => io::Error::other(m),
        ReplyError::Provider(e) => io::Error::other(e.to_string()),
    }
}

impl Read for HelperSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut st = self.state();
        self.attach(&mut st)?;
        let st = &mut *st;
        if buf.is_empty() || st.pos >= st.len {
            return Ok(0);
        }
        let n = buf.len().min(MAX_READ).min((st.len - st.pos) as usize);
        let conn = st.conn.as_mut().expect("attached above");
        match Self::read_chunk(conn, st.pos, &mut buf[..n]) {
            Ok(m) => {
                st.pos += m as u64;
                Ok(m)
            }
            Err(e) => {
                // The connection may be mid-reply, so it can't be used again.
                st.conn = None;
                st.failed = Some(e.to_string());
                Err(e)
            }
        }
    }
}

impl Seek for HelperSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut st = self.state();
        let target = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => st.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                self.attach(&mut st)?;
                st.len.checked_add_signed(d)
            }
        };
        st.pos = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of stream")
        })?;
        Ok(st.pos)
    }
}

impl ByteSource for HelperSource {
    fn byte_len(&self) -> Option<u64> {
        let mut st = self.state();
        self.attach(&mut st).ok().map(|()| st.len)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::server::tests::{FakeBackend, pattern};
    use crate::server::{Backend, accept_loop, bind};
    use crate::testing::TempDir;

    fn serve(tmp: &TempDir) -> PathBuf {
        let path = tmp.path().join("spotify.sock");
        let listener = bind(&path).unwrap();
        let backend: Arc<dyn Backend> = Arc::new(FakeBackend::default());
        thread::spawn(move || accept_loop(&listener, &backend));
        path
    }

    #[test]
    fn nothing_connects_before_the_first_use() {
        let tmp = TempDir::new();
        let path = tmp.path().join("spotify.sock");
        let mut s = HelperSource::new(path.clone(), "s-short".into());
        assert_eq!(s.seek(SeekFrom::Start(500)).unwrap(), 500);
        assert_eq!(s.seek(SeekFrom::Current(-100)).unwrap(), 400);
        // No helper is listening, so only now does anything fail.
        assert!(s.read(&mut [0; 4]).is_err());
        assert!(s.byte_len().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn reads_seeks_and_ends_like_a_file() {
        let tmp = TempDir::new();
        let path = serve(&tmp);
        let mut s = HelperSource::new(path, "s-short".into());
        assert_eq!(s.byte_len(), Some(100_000));
        let mut buf = vec![0; 3 * MAX_READ];
        let n = s.read(&mut buf).unwrap();
        assert_eq!(n, MAX_READ);
        assert!(
            buf[..n]
                .iter()
                .enumerate()
                .all(|(i, b)| *b == pattern(i as u64))
        );
        assert_eq!(s.seek(SeekFrom::End(-10)).unwrap(), 99_990);
        let mut tail = Vec::new();
        // The fake fails from 70_000 on; seek back to the end of the good part.
        s.seek(SeekFrom::Start(69_990)).unwrap();
        let mut ten = [0; 10];
        s.read_exact(&mut ten).unwrap();
        tail.extend_from_slice(&ten);
        assert_eq!(tail, (69_990..70_000).map(pattern).collect::<Vec<_>>());
        s.seek(SeekFrom::Start(200_000)).unwrap();
        assert_eq!(s.read(&mut ten).unwrap(), 0);
        assert!(s.seek(SeekFrom::Current(-300_000)).is_err());
    }

    #[test]
    fn a_failed_read_fails_every_later_read() {
        let tmp = TempDir::new();
        let path = serve(&tmp);
        let mut s = HelperSource::new(path, "s-short".into());
        s.seek(SeekFrom::Start(80_000)).unwrap();
        let e = s.read(&mut [0; 10]).unwrap_err();
        assert!(e.to_string().contains("stalled"), "{e}");
        s.seek(SeekFrom::Start(0)).unwrap();
        assert!(s.read(&mut [0; 10]).is_err());

        let mut gone = HelperSource::new(tmp.path().join("spotify.sock"), "s-gone".into());
        let e = gone.read(&mut [0; 10]).unwrap_err();
        assert!(e.to_string().contains("expired"), "{e}");
        assert!(fs::metadata(tmp.path().join("spotify.sock")).is_ok());
    }
}
