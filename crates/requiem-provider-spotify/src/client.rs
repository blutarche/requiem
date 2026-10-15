//! The daemon's side: [`SpotifyProvider`] talks to the helper over plain blocking sockets and
//! starts it when a call first needs it.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use requiem_core::{
    AccountState, AudioSource, FormatHint, Page, Provider, ProviderError, ProviderId, Shelf, Track,
    TrackId,
};
use serde::de::DeserializeOwned;

use crate::source::HelperSource;
use crate::wire::{
    self, Done, Pending, Prepared, ReplyError, Request, Shelves, SignInLine, Tracks,
};
use crate::{HELPER_ARG, PROVIDER_ID, creds};

pub(crate) const SOCKET_NAME: &str = "spotify.sock";
/// The smaller `sun_path` (macOS, 104) less its NUL, so the path also fits on Linux.
pub(crate) const MAX_SOCKET_PATH: usize = 103;
pub(crate) const HELPER_READY_WAIT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(20);
/// Search, browse, prepare and sign-out. A key fetch alone may take ~20 s under the key
/// policy; a sign-in waits on the user and has no limit.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

pub struct SpotifyOptions {
    /// This program, started as `<exe> --spotify-helper <socket>`.
    pub exe: Option<PathBuf>,
    /// The daemon's 0700 socket directory; the helper listens in it.
    pub socket_dir: Option<PathBuf>,
    /// The sign-in file, usually [`crate::credentials_file`]. The helper is told the same path.
    pub credentials: Option<PathBuf>,
}

enum Launch {
    Spawn {
        exe: PathBuf,
        socket: PathBuf,
    },
    /// A helper someone else started, for the live test.
    #[cfg(test)]
    Running(PathBuf),
}

impl Launch {
    fn socket(&self) -> &Path {
        match self {
            Self::Spawn { socket, .. } => socket,
            #[cfg(test)]
            Self::Running(socket) => socket,
        }
    }
}

pub struct SpotifyProvider {
    /// `Err` says why there can be no helper.
    launch: Result<Launch, String>,
    credentials: Option<PathBuf>,
    helper: Mutex<Option<Child>>,
    pending: Mutex<Option<Pending>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn helper_gone(e: &io::Error) -> ProviderError {
    ProviderError::Network(format!("Spotify helper: {e}"))
}

impl SpotifyProvider {
    /// Starts nothing; the helper starts on the first call that needs it.
    pub fn new(opts: SpotifyOptions) -> Self {
        let launch = match (opts.exe, opts.socket_dir) {
            (None, _) => Err("cannot locate requiemd to start the Spotify helper".to_owned()),
            (_, None) => Err("no socket directory for the Spotify helper".to_owned()),
            (Some(exe), Some(dir)) => {
                let socket = dir.join(SOCKET_NAME);
                if socket.as_os_str().len() > MAX_SOCKET_PATH {
                    Err(format!(
                        "the Spotify helper's socket path {} is over {MAX_SOCKET_PATH} bytes",
                        socket.display()
                    ))
                } else {
                    Ok(Launch::Spawn { exe, socket })
                }
            }
        };
        Self {
            launch,
            credentials: opts.credentials,
            helper: Mutex::new(None),
            pending: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_running_helper(socket: PathBuf, credentials: Option<PathBuf>) -> Self {
        Self {
            launch: Ok(Launch::Running(socket)),
            credentials,
            helper: Mutex::new(None),
            pending: Mutex::new(None),
        }
    }

    fn launch(&self) -> Result<&Launch, ProviderError> {
        self.launch
            .as_ref()
            .map_err(|e| ProviderError::SignInRequired(e.clone()))
    }

    /// Calls other than sign-in go to the helper only once there is a sign-in to use.
    fn signed_in(&self) -> Result<&Launch, ProviderError> {
        let launch = self.launch()?;
        match self.account() {
            AccountState::SignedIn { .. } => Ok(launch),
            AccountState::Unavailable(m) => Err(ProviderError::SignInRequired(m)),
            _ => Err(ProviderError::SignInRequired("sign in to Spotify".into())),
        }
    }

    /// A connection to a live helper, starting one if there is none, it died, or it stopped
    /// answering. Starts at most one per call.
    fn connect(&self, launch: &Launch) -> Result<UnixStream, ProviderError> {
        let (exe, socket) = match launch {
            Launch::Spawn { exe, socket } => (exe, socket),
            #[cfg(test)]
            Launch::Running(socket) => {
                return UnixStream::connect(socket).map_err(|e| helper_gone(&e));
            }
        };
        let mut helper = locked(&self.helper);
        if let Some(child) = helper.as_mut()
            && matches!(child.try_wait(), Ok(None))
            && let Ok(conn) = UnixStream::connect(socket)
        {
            return Ok(conn);
        }
        if let Some(mut old) = helper.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        let mut cmd = Command::new(exe);
        cmd.arg(HELPER_ARG)
            .arg(socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        match &self.credentials {
            Some(p) => cmd.env(creds::ENV, p),
            None => cmd.env_remove(creds::ENV),
        };
        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Network(format!("cannot start the Spotify helper: {e}")))?;
        let conn = wait_ready(&mut child, socket);
        // Kept even when it failed to answer: the next call kills and replaces it.
        *helper = Some(child);
        conn
    }

    fn open(&self, req: &Request, timeout: Option<Duration>) -> Result<UnixStream, ProviderError> {
        let launch = match req {
            Request::SignIn => self.launch()?,
            _ => self.signed_in()?,
        };
        let conn = self.connect(launch)?;
        conn.set_read_timeout(timeout)
            .and_then(|()| conn.set_write_timeout(timeout))
            .map_err(|e| helper_gone(&e))?;
        wire::write_line(&mut &conn, req).map_err(|e| helper_gone(&e))?;
        Ok(conn)
    }

    fn call<T: DeserializeOwned>(&self, req: &Request) -> Result<T, ProviderError> {
        let conn = self.open(req, Some(CALL_TIMEOUT))?;
        read_reply(&mut BufReader::new(conn))
    }

    fn set_pending(&self, p: Option<Pending>) {
        *locked(&self.pending) = p;
    }
}

fn read_reply<T: DeserializeOwned>(r: &mut BufReader<UnixStream>) -> Result<T, ProviderError> {
    wire::read_reply(r).map_err(|e| match e {
        ReplyError::Provider(e) => e,
        ReplyError::Io(e) => helper_gone(&e),
    })
}

fn wait_ready(child: &mut Child, socket: &Path) -> Result<UnixStream, ProviderError> {
    let deadline = Instant::now() + HELPER_READY_WAIT;
    loop {
        if let Ok(conn) = UnixStream::connect(socket) {
            return Ok(conn);
        }
        if !matches!(child.try_wait(), Ok(None)) || Instant::now() >= deadline {
            return Err(ProviderError::Network(
                "Spotify helper did not start".into(),
            ));
        }
        thread::sleep(READY_POLL);
    }
}

impl Provider for SpotifyProvider {
    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.into())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError> {
        self.call::<Tracks>(&Request::Search {
            q: query.to_owned(),
            limit,
        })
        .map(|t| t.tracks)
    }

    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError> {
        let p: Prepared = self.call(&Request::Prepare {
            track: track.id.clone(),
        })?;
        let socket = self.launch()?.socket().to_owned();
        Ok(AudioSource::Encoded {
            bytes: Box::new(HelperSource::new(socket, p.stream)),
            hint: FormatHint::OggVorbis,
        })
    }

    fn browse(&self, page: &Page) -> Result<Vec<Shelf>, ProviderError> {
        if *page == Page::Home {
            return Ok(Vec::new());
        }
        self.call::<Shelves>(&Request::Browse { page: page.clone() })
            .map(|s| s.shelves)
    }

    fn account(&self) -> AccountState {
        if let Err(e) = &self.launch {
            return AccountState::Unavailable(e.clone());
        }
        let Some(path) = &self.credentials else {
            return AccountState::Unavailable(format!(
                "no place to keep a Spotify sign-in: set HOME or {}",
                creds::ENV
            ));
        };
        if let Some(p) = &*locked(&self.pending) {
            return AccountState::Pending {
                verification_url: p.url.clone(),
                user_code: p.code.clone(),
                expires_at_unix: p.expires_at_unix,
            };
        }
        match creds::load(path) {
            Ok(Some(_)) => AccountState::SignedIn { name: None },
            _ => AccountState::SignedOut,
        }
    }

    fn sign_in(&self, on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        if let AccountState::Unavailable(m) = self.account() {
            return Err(ProviderError::SignInRequired(m));
        }
        let conn = self.open(&Request::SignIn, None)?;
        let mut r = BufReader::new(conn);
        let result = loop {
            match read_reply::<SignInLine>(&mut r) {
                Ok(SignInLine::Pending(p)) => {
                    self.set_pending(Some(p));
                    on_state(self.account());
                }
                Ok(SignInLine::SignedIn {}) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.set_pending(None);
        result?;
        on_state(self.account());
        Ok(())
    }

    fn sign_out(&self) -> Result<(), ProviderError> {
        self.set_pending(None);
        // A live helper drops its session and any pending sign-in, then deletes the file.
        let alive = locked(&self.helper)
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        let remote = if alive {
            self.launch()
                .and_then(|l| self.connect(l))
                .and_then(|conn| {
                    conn.set_read_timeout(Some(CALL_TIMEOUT))
                        .map_err(|e| helper_gone(&e))?;
                    wire::write_line(&mut &conn, &Request::SignOut).map_err(|e| helper_gone(&e))?;
                    read_reply::<Done>(&mut BufReader::new(conn)).map(drop)
                })
        } else {
            Ok(())
        };
        let local = match &self.credentials {
            Some(path) => creds::remove(path).map_err(|e| {
                ProviderError::ClientBroken(format!("cannot delete {}: {e}", path.display()))
            }),
            None => Ok(()),
        };
        local.and(remote)
    }
}
