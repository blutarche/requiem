//! The helper protocol. Each connection carries one JSON request line, answered by JSON lines;
//! after an `attach` it carries any number of `read` lines, each answered by a header line and
//! that many bytes, until the daemon closes it. A `watch` is answered by a `changed` line each
//! time a page has newer content than the helper last gave, until either side hangs up.

use std::io::{self, BufRead, Read, Write};

use requiem_core::{Page, ProviderError, Shelf, Track};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::logger::redact;

/// Longest line either side accepts; a large playlist's shelf is a few MiB.
pub(crate) const MAX_LINE: usize = 16 * 1024 * 1024;
/// Largest `read` either side asks for or serves; symphonia reads in 32 KiB blocks.
pub(crate) const MAX_READ: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    Search { q: String, limit: usize },
    Browse { page: Page },
    Prepare { track: String },
    Attach { stream: String },
    Read { offset: u64, len: usize },
    SignIn,
    SignOut,
    Watch,
}

/// A page has newer content than the helper last gave for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Changed {
    pub changed: Page,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Tracks {
    pub tracks: Vec<Track>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Shelves {
    pub shelves: Vec<Shelf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Prepared {
    pub stream: String,
}

/// The file's size is known only once the CDN answers, so it comes with `attach`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Attached {
    pub ok: bool,
    pub len: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Done {
    pub ok: bool,
}

/// Exactly `ok` bytes follow; fewer than asked for only at the end of the stream.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Chunk {
    pub ok: usize,
}

/// A device-code sign-in waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub url: String,
    pub code: String,
    pub expires_at_unix: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignInLine {
    Pending(Pending),
    SignedIn {},
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrLine {
    err: WireError,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireError {
    kind: Kind,
    msg: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Network,
    Unplayable,
    ClientBroken,
    Parse,
    SignInRequired,
}

/// Why a reply was not the one asked for.
#[derive(Debug)]
pub(crate) enum ReplyError {
    Io(io::Error),
    Provider(ProviderError),
}

impl From<io::Error> for ReplyError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub(crate) fn write_line<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(msg).map_err(io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf)
}

pub(crate) fn write_err<W: Write>(w: &mut W, e: &ProviderError) -> io::Result<()> {
    let (kind, msg) = match e {
        ProviderError::Network(m) => (Kind::Network, m),
        ProviderError::Unplayable(m) => (Kind::Unplayable, m),
        ProviderError::ClientBroken(m) => (Kind::ClientBroken, m),
        ProviderError::Parse(m) => (Kind::Parse, m),
        ProviderError::SignInRequired(m) => (Kind::SignInRequired, m),
    };
    write_line(
        w,
        &ErrLine {
            err: WireError {
                kind,
                msg: redact(msg),
            },
        },
    )
}

/// One line without its newline; `Ok(None)` when the stream ends before a line starts.
pub(crate) fn read_line<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = r.take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(if buf.len() > MAX_LINE {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a line over {} MiB", MAX_LINE >> 20),
            )
        } else {
            io::ErrorKind::UnexpectedEof.into()
        });
    }
    buf.pop();
    Ok(Some(buf))
}

pub(crate) fn parse<T: DeserializeOwned>(line: &[u8]) -> io::Result<T> {
    serde_json::from_slice(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The next line as `T`, or the error the helper sent instead.
pub(crate) fn read_reply<R: BufRead, T: DeserializeOwned>(r: &mut R) -> Result<T, ReplyError> {
    let line = read_line(r)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    if let Ok(ErrLine { err }) = serde_json::from_slice::<ErrLine>(&line) {
        return Err(ReplyError::Provider(match err.kind {
            Kind::Network => ProviderError::Network(err.msg),
            Kind::Unplayable => ProviderError::Unplayable(err.msg),
            Kind::ClientBroken => ProviderError::ClientBroken(err.msg),
            Kind::Parse => ProviderError::Parse(err.msg),
            Kind::SignInRequired => ProviderError::SignInRequired(err.msg),
        }));
    }
    Ok(parse(&line)?)
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use requiem_core::{CollectionId, ProviderId};

    use super::*;

    /// Hands out at most `step` bytes per read, as a socket may.
    struct Trickle {
        data: Cursor<Vec<u8>>,
        step: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.step);
            self.data.read(&mut buf[..n])
        }
    }

    fn trickle(data: Vec<u8>, step: usize) -> BufReader<Trickle> {
        // A one-byte buffer makes every line cross many short reads.
        BufReader::with_capacity(
            1,
            Trickle {
                data: Cursor::new(data),
                step,
            },
        )
    }

    #[test]
    fn requests_have_the_documented_shape() {
        let json = |r: &Request| serde_json::to_string(r).unwrap();
        assert_eq!(
            json(&Request::Search {
                q: "a b".into(),
                limit: 5
            }),
            r#"{"op":"search","q":"a b","limit":5}"#
        );
        assert_eq!(
            json(&Request::Read { offset: 7, len: 9 }),
            r#"{"op":"read","offset":7,"len":9}"#
        );
        assert_eq!(json(&Request::SignIn), r#"{"op":"sign_in"}"#);
        assert_eq!(json(&Request::SignOut), r#"{"op":"sign_out"}"#);
        assert_eq!(json(&Request::Watch), r#"{"op":"watch"}"#);
        let changed = serde_json::to_string(&Changed {
            changed: Page::Library,
        })
        .unwrap();
        assert_eq!(changed, r#"{"changed":"Library"}"#);
        let browse = Request::Browse {
            page: Page::Collection(CollectionId {
                provider: ProviderId("spotify".into()),
                id: "collection".into(),
            }),
        };
        assert_eq!(parse::<Request>(json(&browse).as_bytes()).unwrap(), browse);
        let line = serde_json::to_string(&SignInLine::SignedIn {}).unwrap();
        assert_eq!(line, r#"{"signed_in":{}}"#);
    }

    #[test]
    fn lines_survive_short_reads() {
        let mut out = Vec::new();
        for i in 0..3 {
            write_line(
                &mut out,
                &Request::Prepare {
                    track: format!("t{i}"),
                },
            )
            .unwrap();
        }
        for step in [1, 2, 7] {
            let mut r = trickle(out.clone(), step);
            for i in 0..3 {
                let line = read_line(&mut r).unwrap().unwrap();
                assert_eq!(
                    parse::<Request>(&line).unwrap(),
                    Request::Prepare {
                        track: format!("t{i}")
                    }
                );
            }
            assert!(read_line(&mut r).unwrap().is_none());
        }
    }

    #[test]
    fn a_line_cut_off_at_eof_is_an_error_not_a_message() {
        let mut r = trickle(br#"{"ok":3}"#.to_vec(), 2);
        let e = read_line(&mut r).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        let mut r = trickle(Vec::new(), 2);
        assert!(read_line(&mut r).unwrap().is_none());
    }

    #[test]
    fn an_overlong_line_is_refused() {
        let mut data = vec![b'x'; MAX_LINE + 10];
        data.push(b'\n');
        let mut r = BufReader::new(Cursor::new(data));
        let e = read_line(&mut r).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_chunk_header_then_its_bytes_then_a_short_chunk_at_eof() {
        let mut out = Vec::new();
        write_line(&mut out, &Chunk { ok: 4 }).unwrap();
        out.extend_from_slice(b"abcd");
        write_line(&mut out, &Chunk { ok: 1 }).unwrap();
        out.extend_from_slice(b"e");
        write_line(&mut out, &Chunk { ok: 0 }).unwrap();
        let mut r = trickle(out, 3);
        let mut got = Vec::new();
        loop {
            let c: Chunk = match read_reply(&mut r) {
                Ok(c) => c,
                Err(e) => panic!("{e:?}"),
            };
            if c.ok == 0 {
                break;
            }
            let mut buf = vec![0; c.ok];
            r.read_exact(&mut buf).unwrap();
            got.extend(buf);
        }
        assert_eq!(got, b"abcde");
        // The helper hung up between chunks.
        assert!(matches!(
            read_reply::<_, Chunk>(&mut r),
            Err(ReplyError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn errors_map_one_to_one() {
        for e in [
            ProviderError::Network("n".into()),
            ProviderError::Unplayable("u".into()),
            ProviderError::ClientBroken("c".into()),
            ProviderError::Parse("p".into()),
            ProviderError::SignInRequired("s".into()),
        ] {
            let mut out = Vec::new();
            write_err(&mut out, &e).unwrap();
            let mut r = BufReader::new(Cursor::new(out));
            match read_reply::<_, Tracks>(&mut r) {
                Err(ReplyError::Provider(got)) => assert_eq!(got, e),
                other => panic!("{other:?}"),
            }
        }
        let mut r = BufReader::new(Cursor::new(
            b"{\"err\":{\"kind\":\"sign_in_required\",\"msg\":\"m\"}}\n".to_vec(),
        ));
        assert!(matches!(
            read_reply::<_, Tracks>(&mut r),
            Err(ReplyError::Provider(ProviderError::SignInRequired(m))) if m == "m"
        ));
    }

    #[test]
    fn urls_in_errors_lose_their_queries() {
        let mut out = Vec::new();
        let e = ProviderError::Network(
            "Fetching https://audio.example/a/b?__token__=exp=1~hmac=SECRET failed".into(),
        );
        write_err(&mut out, &e).unwrap();
        assert!(!String::from_utf8_lossy(&out).contains("SECRET"));
        let mut r = BufReader::new(Cursor::new(out));
        match read_reply::<_, Tracks>(&mut r) {
            Err(ReplyError::Provider(ProviderError::Network(m))) => {
                assert_eq!(m, "Fetching https://audio.example/a/b?<redacted> failed");
            }
            other => panic!("{other:?}"),
        }
    }
}
