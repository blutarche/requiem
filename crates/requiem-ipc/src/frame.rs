//! A frame is a u32 big-endian length followed by that many bytes of JSON.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) const MAX_FRAME: usize = 4 * 1024 * 1024;

/// Header and body in one buffer, so a frame goes out in a single `write_all`.
pub(crate) fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; 4];
    serde_json::to_writer(&mut buf, msg).map_err(io::Error::other)?;
    let len = buf.len() - 4;
    if len > MAX_FRAME {
        return Err(too_big(len));
    }
    buf[..4].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(buf)
}

pub(crate) fn write<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)
}

/// `Ok(None)` when the stream ends cleanly between frames.
pub(crate) fn read_body<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0; 4];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(too_big(len));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

pub(crate) fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub(crate) fn read<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    read_body(r)?.map(|b| decode(&b)).transpose()
}

fn too_big(len: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "frame of {len} bytes exceeds the {} MiB limit",
            MAX_FRAME >> 20
        ),
    )
}
