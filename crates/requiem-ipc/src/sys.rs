use std::io;
use std::os::unix::net::UnixStream;

pub(crate) fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Effective uid of the process at the other end of a connected Unix socket.
#[cfg(target_os = "macos")]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: the fd is a live socket and both out-pointers are valid.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(peer_cred(stream)?.uid)
}

#[cfg(target_os = "linux")]
fn peer_cred(stream: &UnixStream) -> io::Result<libc::ucred> {
    use std::mem;
    use std::os::fd::AsRawFd;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is a live socket and `cred`/`len` describe a valid ucred buffer.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred)
}

/// Pid of the process at the other end of a connected Unix socket.
#[cfg(target_os = "macos")]
pub(crate) fn peer_pid(stream: &UnixStream) -> io::Result<i32> {
    use std::mem;
    use std::os::fd::AsRawFd;

    let mut pid: libc::pid_t = 0;
    let mut len = mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the fd is a live socket and `pid`/`len` describe a valid pid_t buffer.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_pid(stream: &UnixStream) -> io::Result<i32> {
    Ok(peer_cred(stream)?.pid)
}
