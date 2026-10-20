//! `SpotifyProvider` against a fake helper: the provider starts this test binary itself as
//! `<exe> --spotify-helper <socket>`, and it serves a stub backend instead of librespot.

use std::env;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use requiem_core::{
    AccountState, AudioSource, Page, Provider, ProviderError, ProviderId, Shelf, Track, TrackId,
};
use requiem_provider_spotify::{
    Backend, HELPER_ARG, Pending, SpotifyOptions, SpotifyProvider, Stream, serve,
};

const LEN: u64 = 1 << 20;
/// Where the "stall" stream stops answering.
const STALL_AT: u64 = 64 * 1024;
const WAIT: Duration = Duration::from_secs(5);

fn pattern(i: u64) -> u8 {
    (i * 13 % 251) as u8
}

struct FakeStream {
    stall: bool,
}

impl Stream for FakeStream {
    fn byte_len(&self) -> u64 {
        LEN
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if self.stall && offset >= STALL_AT {
            thread::sleep(Duration::from_secs(3600));
        }
        let n = (LEN.saturating_sub(offset)).min(buf.len() as u64) as usize;
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = pattern(offset + i as u64);
        }
        Ok(n)
    }
}

/// Searches answer with this process's pid, so the test can tell helpers apart.
#[derive(Default)]
struct Fake {
    signed_out: Mutex<bool>,
    wake: Condvar,
}

impl Backend for Fake {
    fn search(&self, _q: &str, _limit: usize) -> Result<Vec<Track>, ProviderError> {
        Ok(vec![Track {
            id: TrackId {
                provider: ProviderId("spotify".into()),
                id: "pid".into(),
            },
            title: process::id().to_string(),
            artists: Vec::new(),
            album: None,
            duration: None,
            artwork_url: None,
        }])
    }

    fn browse(&self, _page: &Page) -> Result<Vec<Shelf>, ProviderError> {
        Ok(Vec::new())
    }

    fn prepare(&self, track: &str) -> Result<String, ProviderError> {
        Ok(track.to_owned())
    }

    fn attach(&self, stream: &str) -> Result<Box<dyn Stream>, ProviderError> {
        Ok(Box::new(FakeStream {
            stall: stream == "stall",
        }))
    }

    fn sign_in(&self, pending: &mut dyn FnMut(Pending)) -> Result<(), ProviderError> {
        pending(Pending {
            url: "https://spotify.com/pair?code=ABCDEF".into(),
            code: "ABCDEF".into(),
            expires_at_unix: 4_000_000_000,
        });
        let mut out = self.signed_out.lock().unwrap();
        while !*out {
            out = self.wake.wait(out).unwrap();
        }
        Err(ProviderError::SignInRequired(
            "sign-in was cancelled".into(),
        ))
    }

    fn sign_out(&self) -> Result<(), ProviderError> {
        *self.signed_out.lock().unwrap() = true;
        self.wake.notify_all();
        Ok(())
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().collect();
    if args.get(1).is_some_and(|a| a == HELPER_ARG) {
        return serve(Path::new(&args[2]), Arc::new(Fake::default()));
    }
    let tmp = env::temp_dir().join(format!("rqsh{}", process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir(&tmp).unwrap();
    let result = panic::catch_unwind(|| run(&tmp));
    let _ = fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        panic::resume_unwind(e);
    }
    println!("test helper_spawn_respawn_and_sign_out ... ok");
    ExitCode::SUCCESS
}

fn helper_pid(p: &SpotifyProvider) -> i32 {
    p.search("who", 1).unwrap()[0].title.parse().unwrap()
}

fn resolve(p: &SpotifyProvider, id: &str) -> AudioSource {
    p.resolve(&TrackId {
        provider: ProviderId("spotify".into()),
        id: id.into(),
    })
    .unwrap()
}

/// Reaps `pid` once it exits; false if it is still running after `WAIT`.
fn exits(pid: i32) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let mut status = 0;
        // SAFETY: `status` is a valid out-pointer; `pid` is this process's child.
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn run(tmp: &Path) {
    let sockets = tmp.join("s");
    fs::DirBuilder::new().mode(0o700).create(&sockets).unwrap();
    let creds: PathBuf = tmp.join("spotify-credentials.json");
    let p = SpotifyProvider::new(SpotifyOptions {
        exe: Some(env::current_exe().unwrap()),
        socket_dir: Some(sockets.clone()),
        credentials: Some(creds.clone()),
    });

    // Signed out: nothing starts.
    assert_eq!(p.account(), AccountState::SignedOut);
    assert!(matches!(
        p.search("x", 1),
        Err(ProviderError::SignInRequired(_))
    ));
    assert!(matches!(
        p.browse(&Page::Home),
        Err(ProviderError::SignInRequired(_))
    ));
    assert!(!sockets.join("spotify.sock").exists());

    fs::write(
        &creds,
        r#"{"username":"someone","auth_type":1,"auth_data":"U0VDUkVU"}"#,
    )
    .unwrap();
    assert_eq!(p.account(), AccountState::SignedIn { name: None });
    let first = helper_pid(&p);
    assert_ne!(first, process::id() as i32);
    let socket = sockets.join("spotify.sock");
    let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // The running helper is reused.
    assert_eq!(helper_pid(&p), first);

    // A stream reads, seeks and ends like the file it stands for.
    let AudioSource::Encoded { mut bytes, .. } = resolve(&p, "data");
    assert_eq!(bytes.byte_len(), Some(LEN));
    let mut all = Vec::new();
    bytes.read_to_end(&mut all).unwrap();
    assert_eq!(all.len() as u64, LEN);
    assert!(all.iter().enumerate().all(|(i, b)| *b == pattern(i as u64)));
    bytes.seek(SeekFrom::End(-3)).unwrap();
    let mut tail = [0; 8];
    assert_eq!(bytes.read(&mut tail).unwrap(), 3);

    // Killing the helper fails a blocked read at once, and the next call starts a new one.
    let AudioSource::Encoded { mut bytes, .. } = resolve(&p, "stall");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = vec![0; STALL_AT as usize];
        bytes.read_exact(&mut buf).unwrap();
        let started = Instant::now();
        let r = bytes.read(&mut buf);
        let _ = tx.send((r.map_err(|e| e.kind()), started.elapsed()));
    });
    thread::sleep(Duration::from_millis(300));
    assert!(rx.try_recv().is_err(), "the stalled read returned early");
    // SAFETY: plain kill(2) of our own child.
    assert_eq!(unsafe { libc::kill(first, libc::SIGKILL) }, 0);
    let (read, took) = rx.recv_timeout(WAIT).expect("read still blocked");
    assert!(read.is_err(), "{read:?}");
    assert!(took < Duration::from_secs(15), "{took:?}");
    let second = helper_pid(&p);
    assert_ne!(second, first);
    // The provider reaped the dead one when it replaced it.
    // SAFETY: as in `exits`.
    assert_eq!(unsafe { libc::waitpid(first, &mut 0, libc::WNOHANG) }, -1);

    // Sign-out ends a pending sign-in in the helper and deletes the file.
    let p = Arc::new(p);
    let (states_tx, states) = mpsc::channel();
    let signing = {
        let p = p.clone();
        thread::spawn(move || {
            p.sign_in(&move |s| {
                let _ = states_tx.send(s);
            })
        })
    };
    let pending = states.recv_timeout(WAIT).unwrap();
    assert!(
        matches!(&pending, AccountState::Pending { user_code, .. } if user_code == "ABCDEF"),
        "{pending:?}"
    );
    assert_eq!(p.account(), pending);
    // As a helper killed mid sign-in leaves it; the fake helper never removes it.
    let scratch = tmp.join(".spotify-sign-in");
    fs::create_dir(&scratch).unwrap();
    fs::write(scratch.join("credentials.json"), "{}").unwrap();
    p.sign_out().unwrap();
    assert!(!scratch.exists());
    let ended = signing.join().unwrap();
    assert!(
        matches!(&ended, Err(ProviderError::SignInRequired(m)) if m.contains("cancelled")),
        "{ended:?}"
    );
    assert!(!creds.exists());
    assert_eq!(p.account(), AccountState::SignedOut);

    // The helper exits when the daemon's end of its stdin closes.
    drop(Arc::into_inner(p).unwrap());
    assert!(exits(second), "the helper outlived the provider");
}
