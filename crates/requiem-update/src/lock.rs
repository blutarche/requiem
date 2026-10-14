use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// An exclusive `flock` on a folder, held until dropped.
#[derive(Debug)]
pub struct DirLock {
    _file: File,
}

impl DirLock {
    /// `None` when another process holds it.
    pub fn try_new(dir: &Path) -> io::Result<Option<DirLock>> {
        let file = File::open(dir)?;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(DirLock { _file: file }));
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(e)
        }
    }

    /// Retries until `wait` has passed, so a stuck holder can't block the caller forever.
    pub fn wait(dir: &Path, wait: Duration) -> io::Result<DirLock> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(lock) = DirLock::try_new(dir)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} stayed locked for {} s", dir.display(), wait.as_secs()),
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, eventually};

    #[test]
    fn excludes_other_holders_until_dropped() {
        let dir = TempDir::new();
        let held = DirLock::try_new(dir.path()).unwrap().unwrap();
        assert!(DirLock::try_new(dir.path()).unwrap().is_none());
        let err = DirLock::wait(dir.path(), Duration::from_millis(250)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(held);
        assert!(eventually(|| DirLock::try_new(dir.path())
            .unwrap()
            .is_some()));
    }
}
