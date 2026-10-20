//! Google sign-in through the OAuth device flow, and access tokens for the YouTube Data API.
//!
//! Innertube rejects OAuth tokens, so a signed-in account reaches only what the Data API
//! offers: the user's playlists and liked videos.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use requiem_core::{AccountState, ProviderError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::log::debug;
use crate::transport::ApiHttp;

pub(crate) const SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";
const DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const PERMISSIONS_URL: &str = "https://myaccount.google.com/permissions";
pub(crate) const CHANNELS_URL: &str = "https://www.googleapis.com/youtube/v3/channels";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// RFC 8628 3.5: each `slow_down` adds 5 seconds to the polling interval.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// An access token is refreshed this long before Google says it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const MAX_FILE_BYTES: u64 = 64 * 1024;

const CLIENT_ENV: &str = "REQUIEM_GOOGLE_CLIENT_FILE";
const TOKEN_ENV: &str = "REQUIEM_GOOGLE_TOKEN_FILE";
const CLIENT_NAME: &str = "google-client.json";
const TOKEN_NAME: &str = "google-token.json";

/// A token or secret. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(String);

impl Secret {
    #[cfg(test)]
    pub(crate) fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Where the OAuth client and the refresh token are kept. `None` means no usable location.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GoogleFiles {
    pub client: Option<PathBuf>,
    pub token: Option<PathBuf>,
}

impl GoogleFiles {
    /// `REQUIEM_GOOGLE_CLIENT_FILE` and `REQUIEM_GOOGLE_TOKEN_FILE` override the defaults:
    /// both in `~/Library/Application Support/requiem` on macOS; on Linux the client under
    /// `$XDG_CONFIG_HOME/requiem` and the token beside the player state under
    /// `$XDG_STATE_HOME/requiem`.
    pub(crate) fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let mut files = default_files(
            var("HOME"),
            var("XDG_CONFIG_HOME"),
            var("XDG_STATE_HOME"),
            cfg!(target_os = "macos"),
        );
        for (key, slot) in [
            (CLIENT_ENV, &mut files.client),
            (TOKEN_ENV, &mut files.token),
        ] {
            match var(key) {
                Some(p) if p.is_absolute() => *slot = Some(p),
                Some(_) => eprintln!("[ytmusic] ignoring {key}: not an absolute path"),
                None => {}
            }
        }
        files
    }
}

fn default_files(
    home: Option<PathBuf>,
    xdg_config: Option<PathBuf>,
    xdg_state: Option<PathBuf>,
    macos: bool,
) -> GoogleFiles {
    let home = home.filter(|h| h.is_absolute());
    let xdg = |x: Option<PathBuf>, fallback: &str| match x.filter(|x| x.is_absolute()) {
        Some(x) => Some(x),
        None => home.as_ref().map(|h| h.join(fallback)),
    };
    let (config, state) = if macos {
        let support = home.as_ref().map(|h| h.join("Library/Application Support"));
        (support.clone(), support)
    } else {
        (xdg(xdg_config, ".config"), xdg(xdg_state, ".local/state"))
    };
    GoogleFiles {
        client: config.map(|d| d.join("requiem").join(CLIENT_NAME)),
        token: state.map(|d| d.join("requiem").join(TOKEN_NAME)),
    }
}

/// An OAuth client of type "TVs and Limited Input devices".
#[derive(Clone)]
pub(crate) struct ClientConfig {
    pub client_id: String,
    pub client_secret: Secret,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .finish()
    }
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other(format!("over {MAX_FILE_BYTES} bytes")));
    }
    Ok(bytes)
}

/// Google's downloaded client JSON (`{"installed": {...}}`) or the same keys at the top level.
/// The error is the one-line hint `AccountState::Unavailable` carries.
pub(crate) fn load_client(files: &GoogleFiles) -> Result<ClientConfig, String> {
    let Some(path) = files.client.as_deref() else {
        return Err(format!(
            "no usable HOME: set {CLIENT_ENV} to the Google OAuth client JSON"
        ));
    };
    if files.token.is_none() {
        return Err(format!(
            "no usable HOME: set {TOKEN_ENV} to where the sign-in should be kept"
        ));
    }
    let shown = path.display();
    let bytes = match read_capped(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "no Google OAuth client: save the JSON of a \"TVs and Limited Input devices\" client to {shown}"
            ));
        }
        Err(e) => return Err(format!("cannot read the Google OAuth client {shown}: {e}")),
    };
    parse_client(&bytes).ok_or_else(|| {
        format!("invalid Google OAuth client {shown}: expected client_id and client_secret")
    })
}

/// Google's downloaded client JSON (`{"installed": {...}}`) or the same keys at the top level.
fn parse_client(bytes: &[u8]) -> Option<ClientConfig> {
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let obj = root.get("installed").unwrap_or(&root);
    let field = |k: &str| {
        obj.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(ClientConfig {
        client_id: field("client_id")?,
        client_secret: Secret(field("client_secret")?),
    })
}

#[derive(Serialize, Deserialize)]
struct TokenFile {
    refresh_token: String,
    #[serde(default)]
    name: Option<String>,
}

impl fmt::Debug for TokenFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenFile")
            .field("refresh_token", &"<redacted>")
            .field("name", &self.name)
            .finish()
    }
}

fn load_token(path: &Path) -> Option<TokenFile> {
    // A file left readable by others (an older build, a copy, a restore) is tightened first.
    if let Ok(meta) = fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
        && let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    {
        debug!("cannot tighten the token file's mode: {e}");
    }
    let bytes = match read_capped(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            debug!("token file unreadable: {e}");
            return None;
        }
    };
    match serde_json::from_slice::<TokenFile>(&bytes) {
        Ok(t) if !t.refresh_token.is_empty() => Some(t),
        _ => {
            debug!("token file unparsable; treating as signed out");
            None
        }
    }
}

/// Parses the token file as `load_token` does, without changing it. A missing file passes.
/// Errors never quote the file, which holds a secret.
pub(crate) fn check_token(path: &Path) -> Result<(), String> {
    let bytes = match read_capped(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    match serde_json::from_slice::<TokenFile>(&bytes) {
        Ok(t) if !t.refresh_token.is_empty() => Ok(()),
        _ => Err(format!("{}: not a Google sign-in file", path.display())),
    }
}

/// Plain files rather than the Keychain: the app is ad-hoc signed and rebuilt often, and
/// Keychain ACLs bind to the code signature, so every rebuild would prompt on a Mac nobody
/// watches (Secret Service unlock prompts are as hostile to a headless Linux daemon).
/// TODO: move the refresh token to the Keychain once the app has a stable Developer ID.
fn save_token(path: &Path, token: &TokenFile) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("token path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // `create` leaves an existing directory as it was.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let json = serde_json::to_vec(token).map_err(io::Error::other)?;
    write_private(path, &json)
}

/// Replaces `path` through a temp file and a rename, so a reader never sees half of it. Only
/// the user can read it, from its creation on.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // A leftover from a crash would make `create_new` fail.
    let _ = fs::remove_file(&tmp);
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Unlike the token's, the client's directory may be one the user named through
/// `REQUIEM_GOOGLE_CLIENT_FILE`, so only a directory created here is made private.
fn save_client(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("client path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    write_private(path, bytes)
}

fn remove_token(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => debug!("cannot delete the token file: {e}"),
    }
}

/// Cancels a device-flow poll: each `start` or `cancel` ends the flows started before it.
#[derive(Default)]
pub(crate) struct Flow {
    r#gen: Mutex<u64>,
    cv: Condvar,
}

impl Flow {
    fn lock(&self) -> MutexGuard<'_, u64> {
        self.r#gen.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn start(&self) -> u64 {
        let mut g = self.lock();
        *g += 1;
        self.cv.notify_all();
        *g
    }

    fn cancel(&self) {
        self.start();
    }

    pub(crate) fn is_current(&self, r#gen: u64) -> bool {
        *self.lock() == r#gen
    }
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn unix(&self) -> u64;
    /// Waits `d`, or less if `flow` moves past `gen`; false if it did.
    fn sleep(&self, d: Duration, flow: &Flow, r#gen: u64) -> bool;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn sleep(&self, d: Duration, flow: &Flow, r#gen: u64) -> bool {
        let g = flow.lock();
        let (g, _) = flow
            .cv
            .wait_timeout_while(g, d, |g| *g == r#gen)
            .unwrap_or_else(|e| e.into_inner());
        *g == r#gen
    }
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_uri")]
    verification_url: String,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

pub(crate) struct DeviceCode {
    device_code: Secret,
    pub user_code: String,
    pub verification_url: String,
    pub expires_at_unix: u64,
    pub interval: Duration,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
}

pub(crate) struct Tokens {
    pub access: Secret,
    pub expires_in: Duration,
    pub refresh: Secret,
}

fn token_response(body: &[u8]) -> TokenResponse {
    serde_json::from_slice(body).unwrap_or_default()
}

fn client_rejected(code: &str) -> ProviderError {
    ProviderError::ClientBroken(format!(
        "Google rejected the OAuth client ({code}); it must be a \"TVs and Limited Input devices\" client"
    ))
}

/// Maps an OAuth `error` code. Google's descriptions are left out: only the code is known
/// to carry nothing from the request.
fn oauth_error(status: u16, code: Option<&str>) -> ProviderError {
    match code {
        Some(c @ ("invalid_client" | "unauthorized_client")) => client_rejected(c),
        Some("access_denied") => ProviderError::SignInRequired("access was denied".into()),
        Some("expired_token") => {
            ProviderError::SignInRequired("the code expired before it was entered".into())
        }
        Some(c) => ProviderError::ClientBroken(format!("Google sign-in failed: {c}")),
        None if status == 429 || status >= 500 => {
            ProviderError::Network(format!("Google sign-in: http {status}"))
        }
        None => ProviderError::ClientBroken(format!("Google sign-in: http {status}")),
    }
}

pub(crate) fn request_code(
    http: &dyn ApiHttp,
    client: &ClientConfig,
    clock: &dyn Clock,
) -> Result<DeviceCode, ProviderError> {
    let resp = http.post_form(
        DEVICE_CODE_URL,
        &[("client_id", &client.client_id), ("scope", SCOPE)],
    )?;
    if resp.status != 200 {
        let code = token_response(&resp.body).error;
        return Err(oauth_error(resp.status, code.as_deref()));
    }
    let r: DeviceCodeResponse = serde_json::from_slice(&resp.body)
        .map_err(|e| ProviderError::Parse(format!("device code response: {e}")))?;
    Ok(DeviceCode {
        device_code: Secret(r.device_code),
        user_code: r.user_code,
        verification_url: r.verification_url,
        expires_at_unix: clock.unix().saturating_add(r.expires_in),
        interval: r.interval.map_or(DEFAULT_INTERVAL, Duration::from_secs),
    })
}

/// Polls the token endpoint at the code's interval until the user answers, the code
/// expires, or `flow` moves past `gen`. A failed request is retried at the next interval.
pub(crate) fn poll_tokens(
    http: &dyn ApiHttp,
    client: &ClientConfig,
    code: &DeviceCode,
    clock: &dyn Clock,
    flow: &Flow,
    r#gen: u64,
) -> Result<Tokens, ProviderError> {
    let cancelled = || ProviderError::SignInRequired("sign-in was cancelled".into());
    let mut interval = code.interval;
    loop {
        if !clock.sleep(interval, flow, r#gen) {
            return Err(cancelled());
        }
        if clock.unix() >= code.expires_at_unix {
            return Err(oauth_error(400, Some("expired_token")));
        }
        let resp = match http.post_form(
            TOKEN_URL,
            &[
                ("client_id", &client.client_id),
                ("client_secret", client.client_secret.expose()),
                ("device_code", code.device_code.expose()),
                ("grant_type", DEVICE_GRANT),
            ],
        ) {
            Ok(r) => r,
            Err(e) => {
                debug!("device token poll failed: {e}");
                continue;
            }
        };
        let body = token_response(&resp.body);
        if resp.status == 200
            && let (Some(access), Some(refresh)) = (body.access_token, body.refresh_token)
        {
            return Ok(Tokens {
                access: Secret(access),
                expires_in: Duration::from_secs(body.expires_in.unwrap_or(0)),
                refresh: Secret(refresh),
            });
        }
        match body.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += SLOW_DOWN_STEP,
            None if resp.status == 429 || resp.status >= 500 => {}
            code => return Err(oauth_error(resp.status, code)),
        }
    }
}

/// The name and liked-videos playlist from `channels.list?mine=true`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelInfo {
    pub name: Option<String>,
    pub likes: Option<String>,
}

pub(crate) fn parse_channel(body: &[u8]) -> Result<ChannelInfo, ProviderError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| ProviderError::Parse(format!("channels json: {e}")))?;
    let Some(first) = v.get("items").and_then(|i| i.get(0)) else {
        // A Google account without a YouTube channel.
        return Ok(ChannelInfo::default());
    };
    let text = |p: &str| {
        first
            .pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Ok(ChannelInfo {
        name: text("/snippet/title"),
        likes: text("/contentDetails/relatedPlaylists/likes"),
    })
}

/// Turns a Data API error status into a `ProviderError`, keeping Google's reason and
/// message, which describe the call and carry nothing from the request.
pub(crate) fn api_error(status: u16, body: &[u8]) -> ProviderError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let reason = v
        .pointer("/error/errors/0/reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = match (reason, message) {
        ("", "") => format!("YouTube Data API: http {status}"),
        ("", m) | (m, "") => format!("YouTube Data API: {m}"),
        (r, m) => format!("YouTube Data API: {r}: {m}"),
    };
    match status {
        404 => ProviderError::Unplayable(detail),
        429 | 500.. => ProviderError::Network(detail),
        _ => ProviderError::ClientBroken(detail),
    }
}

#[derive(Default)]
struct State {
    refresh: Option<Secret>,
    name: Option<String>,
    access: Option<(Secret, Instant)>,
    pending: Option<(String, String, u64)>,
    likes: Option<String>,
    /// Bumped by every sign-out and completed sign-in, so a token refresh that started
    /// before one cannot store its result after it.
    auth_gen: u64,
}

/// The signed-in Google account, if any.
pub(crate) struct Google {
    http: Arc<dyn ApiHttp>,
    clock: Arc<dyn Clock>,
    files: GoogleFiles,
    /// The error is the `Unavailable` hint.
    client: Mutex<Result<ClientConfig, String>>,
    state: Mutex<State>,
    /// Held across a token refresh, so concurrent requests refresh once.
    refreshing: Mutex<()>,
    flow: Flow,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Google {
    /// Reads the client and token files; makes no network calls.
    pub(crate) fn new(http: Arc<dyn ApiHttp>, clock: Arc<dyn Clock>, files: GoogleFiles) -> Self {
        let client = load_client(&files);
        let token = files.token.as_deref().and_then(load_token);
        let state = State {
            name: token.as_ref().and_then(|t| t.name.clone()),
            refresh: token.map(|t| Secret(t.refresh_token)),
            ..State::default()
        };
        Self {
            http,
            clock,
            files,
            client: Mutex::new(client),
            state: Mutex::new(state),
            refreshing: Mutex::new(()),
            flow: Flow::default(),
        }
    }

    pub(crate) fn account(&self) -> AccountState {
        if let Err(hint) = &*locked(&self.client) {
            return AccountState::Unavailable(hint.clone());
        }
        let st = locked(&self.state);
        if let Some((url, code, expires)) = &st.pending {
            return AccountState::Pending {
                verification_url: url.clone(),
                user_code: code.clone(),
                expires_at_unix: *expires,
            };
        }
        match &st.refresh {
            Some(_) => AccountState::SignedIn {
                name: st.name.clone(),
            },
            None => AccountState::SignedOut,
        }
    }

    fn client(&self) -> Result<ClientConfig, ProviderError> {
        locked(&self.client)
            .clone()
            .map_err(ProviderError::SignInRequired)
    }

    /// Keeps the user's own client, as just its id and secret, where `load_client` reads it,
    /// and uses it from now on. A sign-in through another client, or through a client this
    /// can no longer read, is revoked, best effort, and forgotten: Google ties a refresh token
    /// to the client it was issued to. Errors never quote `json`.
    pub(crate) fn import_client(&self, json: &str) -> Result<(), ProviderError> {
        if json.len() as u64 > MAX_FILE_BYTES {
            return Err(ProviderError::SignInRequired(format!(
                "the Google OAuth client file is over {} KiB",
                MAX_FILE_BYTES / 1024
            )));
        }
        let client = parse_client(json.as_bytes()).ok_or_else(|| {
            ProviderError::SignInRequired(
                "not a Google OAuth client: expected client_id and client_secret".into(),
            )
        })?;
        let Some(path) = self.files.client.as_deref() else {
            return Err(ProviderError::SignInRequired(format!(
                "no usable HOME: set {CLIENT_ENV} to where the Google OAuth client should be kept"
            )));
        };
        let body = serde_json::json!({
            "client_id": client.client_id,
            "client_secret": client.client_secret.expose(),
        })
        .to_string();
        // Under the client lock, so a sign-in either starts after this with the new client or
        // started before it and is cancelled here.
        let (result, dropped) = {
            let mut slot = locked(&self.client);
            let changed = match &*slot {
                Ok(c) => c.client_id != client.client_id,
                Err(_) => locked(&self.state).refresh.is_some(),
            };
            save_client(path, body.as_bytes()).map_err(|e| {
                ProviderError::SignInRequired(format!(
                    "cannot save the Google OAuth client to {}: {e}",
                    path.display()
                ))
            })?;
            let dropped = changed.then(|| {
                self.flow.cancel();
                self.take_grant()
            });
            *slot = load_client(&self.files);
            let result = slot
                .as_ref()
                .map(|_| ())
                .map_err(|hint| ProviderError::SignInRequired(hint.clone()));
            (result, dropped)
        };
        if let Some((refresh, auth_gen)) = dropped {
            // Outside the client lock: `account` waits on it, and this waits on Google. The
            // import stands whether or not Google confirms.
            if let Some(refresh) = refresh {
                self.revoke(refresh.expose());
            }
            let st = locked(&self.state);
            // Unless a sign-in through the new client has kept its own token since.
            if st.auth_gen == auth_gen
                && let Some(path) = &self.files.token
            {
                remove_token(path);
            }
        }
        result
    }

    pub(crate) fn sign_in(&self, on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        // Under the client lock, so an import either comes first or cancels this flow.
        let (client, r#gen) = {
            let mut slot = locked(&self.client);
            // The client file may have been saved since the player started.
            *slot = load_client(&self.files);
            let client = slot.clone().map_err(ProviderError::SignInRequired)?;
            (client, self.flow.start())
        };
        let code = request_code(&*self.http, &client, &*self.clock)?;
        {
            let mut st = locked(&self.state);
            // A sign-out or import while the code was requested ends this flow.
            if !self.flow.is_current(r#gen) {
                return Err(ProviderError::SignInRequired(
                    "sign-in was cancelled".into(),
                ));
            }
            st.pending = Some((
                code.verification_url.clone(),
                code.user_code.clone(),
                code.expires_at_unix,
            ));
        }
        on_state(self.account());
        let result = poll_tokens(&*self.http, &client, &code, &*self.clock, &self.flow, r#gen)
            .and_then(|tokens| self.finish_sign_in(tokens, r#gen));
        if result.is_err() {
            let mut st = locked(&self.state);
            if self.flow.is_current(r#gen) {
                st.pending = None;
            }
        }
        result?;
        on_state(self.account());
        Ok(())
    }

    fn finish_sign_in(&self, tokens: Tokens, r#gen: u64) -> Result<(), ProviderError> {
        let auth = format!("Bearer {}", tokens.access.expose());
        let info = self
            .http
            .get(
                CHANNELS_URL,
                &[("part", "snippet,contentDetails"), ("mine", "true")],
                &[("Authorization", &auth)],
            )
            .ok()
            .filter(|r| r.status == 200)
            .and_then(|r| parse_channel(&r.body).ok())
            .unwrap_or_default();
        let file = TokenFile {
            refresh_token: tokens.refresh.expose().to_owned(),
            name: info.name.clone(),
        };
        let path = self.files.token.as_deref().ok_or_else(|| {
            ProviderError::SignInRequired(format!("set {TOKEN_ENV} to keep the sign-in"))
        })?;
        let mut st = locked(&self.state);
        // A sign-out or import while the token request was in flight wins.
        if !self.flow.is_current(r#gen) {
            drop(st);
            // Google already issued the grant; nothing else holds it.
            self.revoke(tokens.refresh.expose());
            return Err(ProviderError::SignInRequired(
                "sign-in was cancelled".into(),
            ));
        }
        save_token(path, &file).map_err(|e| {
            ProviderError::ClientBroken(format!(
                "cannot keep the sign-in in {}: {e}",
                path.display()
            ))
        })?;
        st.auth_gen += 1;
        st.pending = None;
        st.refresh = Some(tokens.refresh);
        st.name = info.name;
        st.likes = info.likes;
        st.access = Some((
            tokens.access,
            self.clock.now() + tokens.expires_in.saturating_sub(EXPIRY_MARGIN),
        ));
        Ok(())
    }

    /// Whether Google confirmed the revoke. Logs the cause, never the token.
    fn revoke(&self, token: &str) -> bool {
        match self.http.post_form(REVOKE_URL, &[("token", token)]) {
            Ok(resp) if resp.status == 200 => true,
            Ok(resp) => {
                debug!("revoke: http {}", resp.status);
                false
            }
            Err(e) => {
                debug!("revoke failed: {e}");
                false
            }
        }
    }

    /// Always signs out locally. An `Err` means Google did not confirm the revoke.
    pub(crate) fn sign_out(&self) -> Result<(), ProviderError> {
        self.flow.cancel();
        let refresh = {
            let mut st = locked(&self.state);
            let refresh = st.refresh.take();
            *st = State {
                auth_gen: st.auth_gen + 1,
                ..State::default()
            };
            refresh
        };
        let revoked = refresh.is_none_or(|r| self.revoke(r.expose()));
        if let Some(path) = &self.files.token {
            remove_token(path);
        }
        if revoked {
            Ok(())
        } else {
            Err(ProviderError::ClientBroken(format!(
                "signed out on this computer, but Google did not confirm; remove requiem's access at {PERMISSIONS_URL}"
            )))
        }
    }

    /// Forgets a grant Google no longer honours.
    fn forget(&self) -> ProviderError {
        self.drop_grant();
        ProviderError::SignInRequired("the Google sign-in expired or was revoked".into())
    }

    /// Forgets the grant here, without telling Google.
    fn drop_grant(&self) {
        self.take_grant();
        if let Some(path) = &self.files.token {
            remove_token(path);
        }
    }

    /// Forgets the grant in memory, leaving its file. Returns its refresh token and the
    /// `auth_gen` that forgetting it started.
    fn take_grant(&self) -> (Option<Secret>, u64) {
        let mut st = locked(&self.state);
        let refresh = st.refresh.take();
        *st = State {
            auth_gen: st.auth_gen + 1,
            ..State::default()
        };
        (refresh, st.auth_gen)
    }

    fn access_token(&self) -> Result<Secret, ProviderError> {
        let _one = locked(&self.refreshing);
        let (refresh, auth_gen) = {
            let st = locked(&self.state);
            if let Some((token, until)) = &st.access
                && self.clock.now() < *until
            {
                return Ok(token.clone());
            }
            (st.refresh.clone(), st.auth_gen)
        };
        let Some(refresh) = refresh else {
            return Err(ProviderError::SignInRequired(
                "sign in to Google to see your library".into(),
            ));
        };
        let client = self.client()?;
        let resp = self.http.post_form(
            TOKEN_URL,
            &[
                ("client_id", &client.client_id),
                ("client_secret", client.client_secret.expose()),
                ("refresh_token", refresh.expose()),
                ("grant_type", "refresh_token"),
            ],
        )?;
        let body = token_response(&resp.body);
        let stale =
            || ProviderError::SignInRequired("sign in to Google to see your library".into());
        // A sign-out or sign-in while the request was in flight makes its answer stale.
        if locked(&self.state).auth_gen != auth_gen {
            return Err(stale());
        }
        match (resp.status, body.access_token, body.error.as_deref()) {
            (200, Some(access), _) => {
                let token = Secret(access);
                let life = Duration::from_secs(body.expires_in.unwrap_or(0));
                let mut st = locked(&self.state);
                // Rechecked under the lock that stores it: a sign-out can land after the check above.
                if st.auth_gen != auth_gen {
                    return Err(stale());
                }
                st.access = Some((
                    token.clone(),
                    self.clock.now() + life.saturating_sub(EXPIRY_MARGIN),
                ));
                Ok(token)
            }
            (_, _, Some("invalid_grant")) => Err(self.forget()),
            (status, _, code) => Err(oauth_error(status, code)),
        }
    }

    fn drop_access(&self, token: &Secret) {
        let mut st = locked(&self.state);
        if st.access.as_ref().is_some_and(|(t, _)| t == token) {
            st.access = None;
        }
    }

    /// An authorized Data API GET. A 401 refreshes the access token once and retries.
    pub(crate) fn api_get(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<u8>, ProviderError> {
        let mut retried = false;
        loop {
            let token = self.access_token()?;
            let auth = format!("Bearer {}", token.expose());
            let resp = self.http.get(url, query, &[("Authorization", &auth)])?;
            match resp.status {
                200 => return Ok(resp.body),
                401 if !retried => {
                    retried = true;
                    self.drop_access(&token);
                }
                status => return Err(api_error(status, &resp.body)),
            }
        }
    }

    /// The liked-videos playlist id, looked up once.
    pub(crate) fn likes(&self) -> Result<String, ProviderError> {
        if let Some(l) = locked(&self.state).likes.clone() {
            return Ok(l);
        }
        let body = self.api_get(
            CHANNELS_URL,
            &[("part", "contentDetails"), ("mine", "true")],
        )?;
        let likes = parse_channel(&body)?.likes.ok_or_else(|| {
            ProviderError::Unplayable("this Google account has no liked-videos playlist".into())
        })?;
        locked(&self.state).likes = Some(likes.clone());
        Ok(likes)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::testing::{FakeApi, TempDir};

    #[derive(Default)]
    struct FakeClock {
        elapsed: StdMutex<Duration>,
        slept: StdMutex<Vec<Duration>>,
    }

    const BASE_UNIX: u64 = 1_800_000_000;

    impl FakeClock {
        fn advance(&self, d: Duration) {
            *self.elapsed.lock().unwrap() += d;
        }

        fn slept(&self) -> Vec<u64> {
            self.slept
                .lock()
                .unwrap()
                .iter()
                .map(Duration::as_secs)
                .collect()
        }
    }

    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *START.get_or_init(Instant::now) + *self.elapsed.lock().unwrap()
        }

        fn unix(&self) -> u64 {
            BASE_UNIX + self.elapsed.lock().unwrap().as_secs()
        }

        fn sleep(&self, d: Duration, flow: &Flow, r#gen: u64) -> bool {
            self.advance(d);
            self.slept.lock().unwrap().push(d);
            flow.is_current(r#gen)
        }
    }

    fn client() -> ClientConfig {
        ClientConfig {
            client_id: "cid".into(),
            client_secret: Secret::new("csecret"),
        }
    }

    const CODE: &str = r#"{"device_code":"DEVCODE","user_code":"GQVQ-JKEC",
        "verification_url":"https://www.google.com/device","expires_in":1800,"interval":5}"#;
    const PENDING: &str =
        r#"{"error":"authorization_pending","error_description":"Precondition Required"}"#;
    const SLOW: &str = r#"{"error":"slow_down","error_description":"Forbidden"}"#;
    const DENIED: &str = r#"{"error":"access_denied","error_description":"Forbidden"}"#;
    const EXPIRED: &str = r#"{"error":"expired_token","error_description":"Bad Request"}"#;
    const TOKENS: &str = r#"{"access_token":"AT1","expires_in":3599,
        "refresh_token":"RT1","scope":"https://www.googleapis.com/auth/youtube.readonly","token_type":"Bearer"}"#;
    const REFRESHED: &str = r#"{"access_token":"AT2","expires_in":3599,"token_type":"Bearer"}"#;
    const CHANNEL: &str = include_str!("../tests/fixtures/data_channels.json");
    const LIKES: &str = "LLxxxxxxxxxxxxxxxxxxxxxx";

    fn device_code(api: &FakeApi, clock: &FakeClock) -> DeviceCode {
        api.script(DEVICE_CODE_URL, 200, CODE);
        request_code(api, &client(), clock).unwrap()
    }

    #[test]
    fn device_flow_waits_out_pending_and_slows_down() {
        let api = FakeApi::default();
        let clock = FakeClock::default();
        let code = device_code(&api, &clock);
        assert_eq!(code.user_code, "GQVQ-JKEC");
        assert_eq!(code.verification_url, "https://www.google.com/device");
        assert_eq!(code.expires_at_unix, BASE_UNIX + 1800);
        api.script(TOKEN_URL, 428, PENDING);
        api.script(TOKEN_URL, 403, SLOW);
        api.script(TOKEN_URL, 428, PENDING);
        api.script(TOKEN_URL, 200, TOKENS);
        let flow = Flow::default();
        let r#gen = flow.start();
        let tokens = poll_tokens(&api, &client(), &code, &clock, &flow, r#gen).unwrap();
        assert_eq!(tokens.access.expose(), "AT1");
        assert_eq!(tokens.refresh.expose(), "RT1");
        assert_eq!(clock.slept(), [5, 5, 10, 10]);
        let forms = api.forms(TOKEN_URL);
        assert_eq!(forms.len(), 4);
        assert!(forms[0].contains(&("device_code".into(), "DEVCODE".into())));
        assert!(forms[0].contains(&("grant_type".into(), DEVICE_GRANT.into())));
        assert!(forms[0].contains(&("client_secret".into(), "csecret".into())));
        let first = &api.forms(DEVICE_CODE_URL)[0];
        assert!(first.contains(&("scope".into(), SCOPE.into())));
    }

    #[test]
    fn device_flow_denied() {
        let api = FakeApi::default();
        let clock = FakeClock::default();
        let code = device_code(&api, &clock);
        api.script(TOKEN_URL, 428, PENDING);
        api.script(TOKEN_URL, 403, DENIED);
        let flow = Flow::default();
        let r#gen = flow.start();
        let err = poll_tokens(&api, &client(), &code, &clock, &flow, r#gen)
            .err()
            .unwrap();
        assert_eq!(
            err,
            ProviderError::SignInRequired("access was denied".into())
        );
    }

    #[test]
    fn device_flow_expired_by_google_or_by_the_clock() {
        let api = FakeApi::default();
        let clock = FakeClock::default();
        let code = device_code(&api, &clock);
        api.script(TOKEN_URL, 400, EXPIRED);
        let flow = Flow::default();
        let r#gen = flow.start();
        let expired =
            ProviderError::SignInRequired("the code expired before it was entered".into());
        let err = poll_tokens(&api, &client(), &code, &clock, &flow, r#gen)
            .err()
            .unwrap();
        assert_eq!(err, expired);

        // No answer at all: the poll stops at the code's expiry.
        let api = FakeApi::default();
        let clock = FakeClock::default();
        let code = device_code(&api, &clock);
        for _ in 0..400 {
            api.script(TOKEN_URL, 428, PENDING);
        }
        let err = poll_tokens(&api, &client(), &code, &clock, &flow, r#gen)
            .err()
            .unwrap();
        assert_eq!(err, expired);
        assert_eq!(api.forms(TOKEN_URL).len(), 1800 / 5 - 1);
    }

    #[test]
    fn wrong_client_type_is_named() {
        let api = FakeApi::default();
        api.script(
            DEVICE_CODE_URL,
            401,
            r#"{"error":"invalid_client","error_description":"Unauthorized"}"#,
        );
        let err = request_code(&api, &client(), &FakeClock::default())
            .err()
            .unwrap();
        assert!(
            matches!(&err, ProviderError::ClientBroken(m) if m.contains("TVs and Limited Input")),
            "{err:?}"
        );
    }

    struct Rig {
        api: Arc<FakeApi>,
        clock: Arc<FakeClock>,
        tmp: TempDir,
    }

    impl Rig {
        fn new() -> Self {
            let tmp = TempDir::new();
            fs::write(
                tmp.path().join("client.json"),
                r#"{"installed":{"client_id":"cid","client_secret":"csecret"}}"#,
            )
            .unwrap();
            Self {
                api: Arc::new(FakeApi::default()),
                clock: Arc::new(FakeClock::default()),
                tmp,
            }
        }

        fn files(&self) -> GoogleFiles {
            GoogleFiles {
                client: Some(self.tmp.path().join("client.json")),
                token: Some(self.tmp.path().join("state/requiem/google-token.json")),
            }
        }

        fn token_path(&self) -> PathBuf {
            self.files().token.unwrap()
        }

        fn google(&self) -> Google {
            Google::new(self.api.clone(), self.clock.clone(), self.files())
        }

        fn signed_in(&self) -> Google {
            save_token(
                &self.token_path(),
                &TokenFile {
                    refresh_token: "RT1".into(),
                    name: Some("Me".into()),
                },
            )
            .unwrap();
            self.google()
        }
    }

    #[test]
    fn sign_in_reports_pending_then_signed_in_and_keeps_the_token() {
        let rig = Rig::new();
        let g = rig.google();
        assert_eq!(g.account(), AccountState::SignedOut);
        rig.api.script(DEVICE_CODE_URL, 200, CODE);
        rig.api.script(TOKEN_URL, 428, PENDING);
        rig.api.script(TOKEN_URL, 200, TOKENS);
        rig.api.script(CHANNELS_URL, 200, CHANNEL);
        let seen = StdMutex::new(Vec::new());
        g.sign_in(&|s| seen.lock().unwrap().push(s)).unwrap();
        let signed_in = AccountState::SignedIn {
            name: Some("Requiem Tester".into()),
        };
        assert_eq!(
            *seen.lock().unwrap(),
            [
                AccountState::Pending {
                    verification_url: "https://www.google.com/device".into(),
                    user_code: "GQVQ-JKEC".into(),
                    expires_at_unix: BASE_UNIX + 1800,
                },
                signed_in.clone(),
            ]
        );
        assert_eq!(g.account(), signed_in);
        let saved = load_token(&rig.token_path()).unwrap();
        assert_eq!(saved.refresh_token, "RT1");
        assert_eq!(
            rig.api.headers(CHANNELS_URL)[0],
            [("Authorization".into(), "Bearer AT1".into())]
        );
        // A restarted player is still signed in, without asking Google.
        let calls = rig.api.calls();
        assert_eq!(rig.google().account(), signed_in);
        assert_eq!(rig.api.calls(), calls);
        assert_eq!(g.likes().unwrap(), LIKES);
    }

    #[test]
    fn token_file_is_private_from_creation() {
        let rig = Rig::new();
        let path = rig.token_path();
        save_token(
            &path,
            &TokenFile {
                refresh_token: "RT-SECRET".into(),
                name: None,
            },
        )
        .unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o700);
        assert!(!path.with_extension("json.tmp").exists());
        // Rewrites keep the mode.
        save_token(
            &path,
            &TokenFile {
                refresh_token: "RT-OTHER".into(),
                name: None,
            },
        )
        .unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn save_tightens_an_existing_directory_and_load_an_existing_file() {
        let rig = Rig::new();
        let path = rig.token_path();
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        let token = TokenFile {
            refresh_token: "RT1".into(),
            name: None,
        };
        save_token(&path, &token).unwrap();
        assert_eq!(mode(dir), 0o700);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_token(&path).unwrap().refresh_token, "RT1");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn check_token_reads_without_changing_or_quoting_the_file() {
        let rig = Rig::new();
        let path = rig.token_path();
        assert_eq!(check_token(&path), Ok(()));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"refresh_token":"RT-SECRET"}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(check_token(&path), Ok(()));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        for bad in [
            r#"{"refresh_token":""}"#,
            r#"{"refresh_token":7,"x":"RT-SECRET"}"#,
        ] {
            fs::write(&path, bad).unwrap();
            let e = check_token(&path).unwrap_err();
            assert!(e.contains("not a Google sign-in file"), "{e}");
            assert!(!e.contains("RT-SECRET"), "{e}");
        }
    }

    #[test]
    fn debug_never_shows_secrets() {
        let t = TokenFile {
            refresh_token: "RT-SECRET".into(),
            name: Some("Me".into()),
        };
        let s = format!("{t:?} {:?} {:?}", Secret::new("AT-SECRET"), client());
        assert!(!s.contains("RT-SECRET") && !s.contains("AT-SECRET"), "{s}");
        assert!(!s.contains("csecret"), "{s}");
        assert!(s.contains("Me") && s.contains("cid"), "{s}");
    }

    #[test]
    fn refreshes_an_expired_access_token() {
        let rig = Rig::new();
        let g = rig.signed_in();
        rig.api.script(TOKEN_URL, 200, REFRESHED);
        rig.api.script("https://example.test/a", 200, "{}");
        rig.api.script("https://example.test/a", 200, "{}");
        g.api_get("https://example.test/a", &[]).unwrap();
        g.api_get("https://example.test/a", &[]).unwrap();
        assert_eq!(rig.api.forms(TOKEN_URL).len(), 1);
        let refresh = &rig.api.forms(TOKEN_URL)[0];
        assert!(refresh.contains(&("refresh_token".into(), "RT1".into())));
        assert!(refresh.contains(&("grant_type".into(), "refresh_token".into())));

        rig.clock.advance(Duration::from_secs(3599));
        rig.api
            .script(TOKEN_URL, 200, TOKENS.replace("AT1", "AT3").as_str());
        rig.api.script("https://example.test/a", 200, "{}");
        g.api_get("https://example.test/a", &[]).unwrap();
        assert_eq!(rig.api.forms(TOKEN_URL).len(), 2);
        let auth: Vec<String> = rig
            .api
            .headers("https://example.test/a")
            .iter()
            .map(|h| h[0].1.clone())
            .collect();
        assert_eq!(auth, ["Bearer AT2", "Bearer AT2", "Bearer AT3"]);
    }

    #[test]
    fn a_401_refreshes_once_and_retries() {
        let rig = Rig::new();
        let g = rig.signed_in();
        rig.api.script(TOKEN_URL, 200, REFRESHED);
        rig.api.script("https://example.test/a", 401, "{}");
        rig.api
            .script(TOKEN_URL, 200, TOKENS.replace("AT1", "AT3").as_str());
        rig.api.script("https://example.test/a", 200, r#"{"ok":1}"#);
        assert_eq!(
            g.api_get("https://example.test/a", &[]).unwrap(),
            br#"{"ok":1}"#
        );
        assert_eq!(rig.api.forms(TOKEN_URL).len(), 2);

        // A second 401 in a row is an error, not a loop.
        rig.api.script("https://example.test/a", 401, "{}");
        rig.api.script(TOKEN_URL, 200, REFRESHED);
        rig.api.script("https://example.test/a", 401, "{}");
        assert!(matches!(
            g.api_get("https://example.test/a", &[]),
            Err(ProviderError::ClientBroken(_))
        ));
    }

    #[test]
    fn invalid_grant_signs_out_and_deletes_the_token() {
        let rig = Rig::new();
        let g = rig.signed_in();
        assert!(matches!(g.account(), AccountState::SignedIn { .. }));
        rig.api.script(
            TOKEN_URL,
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        );
        let err = g.api_get("https://example.test/a", &[]).unwrap_err();
        assert!(matches!(err, ProviderError::SignInRequired(_)), "{err:?}");
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(!rig.token_path().exists());
    }

    #[test]
    fn signed_out_requests_need_sign_in() {
        let rig = Rig::new();
        let err = rig
            .google()
            .api_get("https://example.test/a", &[])
            .unwrap_err();
        assert!(matches!(err, ProviderError::SignInRequired(_)), "{err:?}");
        assert!(rig.api.calls().is_empty());
    }

    #[test]
    fn sign_out_revokes_and_deletes_the_token() {
        let rig = Rig::new();
        let g = rig.signed_in();
        rig.api.script(REVOKE_URL, 200, "{}");
        g.sign_out().unwrap();
        assert_eq!(
            rig.api.forms(REVOKE_URL),
            [vec![("token".into(), "RT1".into())]]
        );
        assert!(!rig.token_path().exists());
        assert_eq!(g.account(), AccountState::SignedOut);
    }

    #[test]
    fn a_failed_revoke_still_signs_out_and_says_so() {
        let rig = Rig::new();
        let g = rig.signed_in();
        rig.api.script(REVOKE_URL, 500, "{}");
        let err = g.sign_out().unwrap_err();
        assert!(
            matches!(&err, ProviderError::ClientBroken(m)
                if m.starts_with("signed out on this computer, but Google did not confirm")
                    && m.ends_with("https://myaccount.google.com/permissions")),
            "{err:?}"
        );
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(!rig.token_path().exists());
    }

    /// Wraps the fake and holds the token endpoint's reply until released.
    struct Gated {
        inner: Arc<FakeApi>,
        entered: std::sync::mpsc::Sender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }

    impl ApiHttp for Gated {
        fn get(
            &self,
            url: &str,
            query: &[(&str, &str)],
            headers: &[(&str, &str)],
        ) -> Result<crate::transport::HttpResponse, ProviderError> {
            self.inner.get(url, query, headers)
        }

        fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<crate::transport::HttpResponse, ProviderError> {
            let r = self.inner.post_form(url, form);
            if url == TOKEN_URL {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            r
        }
    }

    #[test]
    fn a_refresh_in_flight_during_sign_out_is_not_kept() {
        let rig = Rig::new();
        rig.signed_in();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        let http = Arc::new(Gated {
            inner: rig.api.clone(),
            entered: entered_tx,
            release: StdMutex::new(release),
        });
        let g = Arc::new(Google::new(http, rig.clock.clone(), rig.files()));
        rig.api.script(TOKEN_URL, 200, REFRESHED);
        rig.api.script(REVOKE_URL, 200, "{}");
        let refresh = {
            let g = g.clone();
            std::thread::spawn(move || g.access_token())
        };
        entered.recv().unwrap();
        g.sign_out().unwrap();
        release_tx.send(()).unwrap();
        let err = refresh.join().unwrap().unwrap_err();
        assert!(matches!(err, ProviderError::SignInRequired(_)), "{err:?}");
        assert!(locked(&g.state).access.is_none());
        assert_eq!(g.account(), AccountState::SignedOut);
    }

    #[test]
    fn a_cancelled_sign_in_revokes_the_grant_it_was_issued() {
        let rig = Rig::new();
        let g = rig.google();
        rig.api.script(CHANNELS_URL, 200, CHANNEL);
        rig.api.script(REVOKE_URL, 200, "{}");
        let tokens = Tokens {
            access: Secret::new("AT1"),
            refresh: Secret::new("RT1"),
            expires_in: Duration::from_secs(3600),
        };
        let r#gen = g.flow.start();
        g.flow.cancel();
        let err = g.finish_sign_in(tokens, r#gen).unwrap_err();
        assert_eq!(
            err,
            ProviderError::SignInRequired("sign-in was cancelled".into())
        );
        assert_eq!(
            rig.api.forms(REVOKE_URL),
            [vec![("token".into(), "RT1".into())]]
        );
        assert!(!rig.token_path().exists());
        assert_eq!(g.account(), AccountState::SignedOut);
    }

    #[test]
    fn sign_out_cancels_a_pending_sign_in() {
        let rig = Rig::new();
        let g = rig.google();
        rig.api.script(DEVICE_CODE_URL, 200, CODE);
        // The callback runs while the flow is pending, before its first poll.
        let err = g
            .sign_in(&|s| {
                if matches!(s, AccountState::Pending { .. }) {
                    g.sign_out().unwrap();
                }
            })
            .unwrap_err();
        assert_eq!(
            err,
            ProviderError::SignInRequired("sign-in was cancelled".into())
        );
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(rig.api.forms(TOKEN_URL).is_empty());
    }

    #[test]
    fn a_cancel_wakes_the_poll_sleep() {
        let flow = Arc::new(Flow::default());
        let r#gen = flow.start();
        let canceller = {
            let flow = flow.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                flow.cancel();
            })
        };
        let t0 = Instant::now();
        assert!(!SystemClock.sleep(Duration::from_secs(30), &flow, r#gen));
        assert!(t0.elapsed() < Duration::from_secs(10));
        canceller.join().unwrap();
        // An unchanged flow sleeps the whole interval.
        let r#gen = flow.start();
        assert!(SystemClock.sleep(Duration::from_millis(20), &flow, r#gen));
    }

    fn write_client(dir: &Path, body: &str) -> GoogleFiles {
        let path = dir.join("google-client.json");
        fs::write(&path, body).unwrap();
        GoogleFiles {
            client: Some(path),
            token: Some(dir.join("google-token.json")),
        }
    }

    #[test]
    fn client_json_in_googles_shape_or_flat() {
        let tmp = TempDir::new();
        let installed = r#"{"installed":{"client_id":"id1.apps.googleusercontent.com",
            "project_id":"p","auth_uri":"https://accounts.google.com/o/oauth2/auth",
            "token_uri":"https://oauth2.googleapis.com/token","client_secret":"s1"}}"#;
        let c = load_client(&write_client(tmp.path(), installed)).unwrap();
        assert_eq!(c.client_id, "id1.apps.googleusercontent.com");
        assert_eq!(c.client_secret.expose(), "s1");
        let flat = r#"{"client_id":"id2","client_secret":"s2"}"#;
        let c = load_client(&write_client(tmp.path(), flat)).unwrap();
        assert_eq!(
            (c.client_id.as_str(), c.client_secret.expose()),
            ("id2", "s2")
        );
    }

    #[test]
    fn missing_or_invalid_client_is_unavailable_naming_the_path() {
        let tmp = TempDir::new();
        let files = GoogleFiles {
            client: Some(tmp.path().join("nope.json")),
            token: Some(tmp.path().join("t.json")),
        };
        let g = Google::new(
            Arc::new(FakeApi::default()),
            Arc::new(FakeClock::default()),
            files.clone(),
        );
        let path = tmp.path().join("nope.json");
        match g.account() {
            AccountState::Unavailable(h) => {
                assert!(h.contains(&*path.to_string_lossy()), "{h}");
                assert!(h.contains("TVs and Limited Input devices"), "{h}");
            }
            other => panic!("{other:?}"),
        }
        // `sign_in` explains the same way.
        let err = g.sign_in(&|_| {}).unwrap_err();
        assert!(
            matches!(&err, ProviderError::SignInRequired(m) if m.contains(&*path.to_string_lossy()))
        );

        for bad in [
            "not json",
            r#"{"installed":{"client_id":"LEAKME"}}"#,
            r#"{"client_secret":"LEAKME"}"#,
            r#"{"web":{}}"#,
        ] {
            let files = write_client(tmp.path(), bad);
            let hint = load_client(&files).unwrap_err();
            assert!(hint.starts_with("invalid Google OAuth client"), "{hint}");
            assert!(
                hint.contains(&*files.client.unwrap().to_string_lossy()),
                "{hint}"
            );
            assert!(!hint.contains("LEAKME"), "{hint}");
        }
    }

    #[test]
    fn a_client_saved_after_start_is_picked_up_by_sign_in() {
        let tmp = TempDir::new();
        let files = GoogleFiles {
            client: Some(tmp.path().join("google-client.json")),
            token: Some(tmp.path().join("google-token.json")),
        };
        let api = Arc::new(FakeApi::default());
        let g = Google::new(api.clone(), Arc::new(FakeClock::default()), files.clone());
        assert!(matches!(g.account(), AccountState::Unavailable(_)));
        write_client(tmp.path(), r#"{"client_id":"id","client_secret":"s"}"#);
        api.script(DEVICE_CODE_URL, 400, r#"{"error":"invalid_scope"}"#);
        let err = g.sign_in(&|_| {}).unwrap_err();
        assert_eq!(
            err,
            ProviderError::ClientBroken("Google sign-in failed: invalid_scope".into())
        );
        assert_eq!(g.account(), AccountState::SignedOut);
    }

    fn google_at(client: Option<PathBuf>, tmp: &TempDir) -> Google {
        let files = GoogleFiles {
            client,
            token: Some(tmp.path().join("google-token.json")),
        };
        Google::new(
            Arc::new(FakeApi::default()),
            Arc::new(FakeClock::default()),
            files,
        )
    }

    #[test]
    fn an_imported_client_is_kept_private_and_used() {
        let tmp = TempDir::new();
        let path = tmp.path().join("new").join("google-client.json");
        let g = google_at(Some(path.clone()), &tmp);
        assert!(matches!(g.account(), AccountState::Unavailable(_)));
        let installed = r#"{"installed":{"client_id":"id1.apps.googleusercontent.com",
            "project_id":"p","client_secret":"s1"}}"#;
        g.import_client(installed).unwrap();
        assert_eq!(g.account(), AccountState::SignedOut);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(!path.with_extension("json.tmp").exists());
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({"client_id": "id1.apps.googleusercontent.com", "client_secret": "s1"})
        );

        g.import_client(r#"{"client_id":"id2","client_secret":"s2"}"#)
            .unwrap();
        let c = g.client().unwrap();
        assert_eq!(
            (c.client_id.as_str(), c.client_secret.expose()),
            ("id2", "s2")
        );
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn an_import_of_another_client_forgets_the_sign_in() {
        let rig = Rig::new();
        let g = rig.signed_in();
        let signed_in = AccountState::SignedIn {
            name: Some("Me".into()),
        };
        assert_eq!(g.account(), signed_in);
        // A new secret for the same client keeps the sign-in.
        g.import_client(r#"{"client_id":"cid","client_secret":"rotated"}"#)
            .unwrap();
        assert_eq!(g.account(), signed_in);
        assert!(rig.token_path().exists());
        assert_eq!(g.client().unwrap().client_secret.expose(), "rotated");

        assert!(rig.api.calls().is_empty(), "{:?}", rig.api.calls());

        rig.api.script(REVOKE_URL, 200, "{}");
        g.import_client(r#"{"client_id":"other","client_secret":"s"}"#)
            .unwrap();
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(!rig.token_path().exists());
        assert_eq!(rig.google().account(), AccountState::SignedOut);
        assert_eq!(
            rig.api.forms(REVOKE_URL),
            [vec![("token".into(), "RT1".into())]]
        );
        assert_eq!(rig.api.calls(), [REVOKE_URL]);
    }

    /// Wraps the fake and runs `hook` as each request to `url` arrives.
    struct Hooked {
        inner: Arc<FakeApi>,
        url: &'static str,
        hook: Box<dyn Fn() + Send + Sync>,
    }

    impl ApiHttp for Hooked {
        fn get(
            &self,
            url: &str,
            query: &[(&str, &str)],
            headers: &[(&str, &str)],
        ) -> Result<crate::transport::HttpResponse, ProviderError> {
            self.inner.get(url, query, headers)
        }

        fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<crate::transport::HttpResponse, ProviderError> {
            if url == self.url {
                (self.hook)();
            }
            self.inner.post_form(url, form)
        }
    }

    #[test]
    fn an_import_over_an_unreadable_client_forgets_the_sign_in() {
        let rig = Rig::new();
        rig.signed_in();
        fs::write(rig.tmp.path().join("client.json"), "{").unwrap();
        let g = rig.google();
        assert!(matches!(g.account(), AccountState::Unavailable(_)));
        rig.api.script(REVOKE_URL, 200, "{}");
        g.import_client(r#"{"client_id":"cid","client_secret":"csecret"}"#)
            .unwrap();
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(!rig.token_path().exists());
        assert_eq!(
            rig.api.forms(REVOKE_URL),
            [vec![("token".into(), "RT1".into())]]
        );
    }

    #[test]
    fn an_import_revokes_the_old_sign_in_before_deleting_it_and_stands_if_that_fails() {
        let rig = Rig::new();
        rig.signed_in();
        let kept = Arc::new(StdMutex::new(Vec::new()));
        let http = Arc::new(Hooked {
            inner: rig.api.clone(),
            url: REVOKE_URL,
            hook: Box::new({
                let (kept, token) = (kept.clone(), rig.token_path());
                move || kept.lock().unwrap().push(token.exists())
            }),
        });
        let g = Google::new(http, rig.clock.clone(), rig.files());
        // Unscripted, so the revoke fails.
        g.import_client(r#"{"client_id":"other","client_secret":"s"}"#)
            .unwrap();
        assert_eq!(*kept.lock().unwrap(), [true]);
        assert_eq!(
            rig.api.forms(REVOKE_URL),
            [vec![("token".into(), "RT1".into())]]
        );
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(!rig.token_path().exists());
        assert_eq!(g.client().unwrap().client_id, "other");
    }

    #[test]
    fn a_sign_in_through_the_new_client_during_the_revoke_is_kept() {
        let rig = Rig::new();
        rig.signed_in();
        let google = Arc::new(std::sync::OnceLock::<std::sync::Weak<Google>>::new());
        let http = Arc::new(Hooked {
            inner: rig.api.clone(),
            url: REVOKE_URL,
            hook: Box::new({
                let google = google.clone();
                move || {
                    let g = google.get().unwrap().upgrade().unwrap();
                    let tokens = Tokens {
                        access: Secret::new("AT2"),
                        refresh: Secret::new("RT2"),
                        expires_in: Duration::from_secs(3600),
                    };
                    g.finish_sign_in(tokens, g.flow.start()).unwrap();
                }
            }),
        });
        let g = Arc::new(Google::new(http, rig.clock.clone(), rig.files()));
        google.set(Arc::downgrade(&g)).unwrap();
        rig.api.script(REVOKE_URL, 200, "{}");
        g.import_client(r#"{"client_id":"other","client_secret":"s"}"#)
            .unwrap();
        assert!(matches!(g.account(), AccountState::SignedIn { .. }));
        assert_eq!(load_token(&rig.token_path()).unwrap().refresh_token, "RT2");
    }

    #[test]
    fn an_import_while_a_sign_in_asks_for_its_code_cancels_it() {
        let rig = Rig::new();
        let google = Arc::new(std::sync::OnceLock::<std::sync::Weak<Google>>::new());
        let http = Arc::new(Hooked {
            inner: rig.api.clone(),
            url: DEVICE_CODE_URL,
            hook: Box::new({
                let google = google.clone();
                move || {
                    let g = google.get().unwrap().upgrade().unwrap();
                    g.import_client(r#"{"client_id":"other","client_secret":"s"}"#)
                        .unwrap();
                }
            }),
        });
        let g = Arc::new(Google::new(http, rig.clock.clone(), rig.files()));
        google.set(Arc::downgrade(&g)).unwrap();
        rig.api.script(DEVICE_CODE_URL, 200, CODE);
        let seen = StdMutex::new(Vec::new());
        let err = g.sign_in(&|s| seen.lock().unwrap().push(s)).unwrap_err();
        assert_eq!(
            err,
            ProviderError::SignInRequired("sign-in was cancelled".into())
        );
        // The old client's code never shows.
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
        assert_eq!(g.account(), AccountState::SignedOut);
        assert!(rig.api.forms(TOKEN_URL).is_empty());
    }

    #[test]
    fn an_import_leaves_an_existing_directory_as_it_was() {
        let tmp = TempDir::new();
        let dir = tmp.path().join("shared");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("my-client.json");
        let g = google_at(Some(path.clone()), &tmp);
        g.import_client(r#"{"client_id":"id","client_secret":"s"}"#)
            .unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o755);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_bad_import_changes_nothing_and_quotes_nothing() {
        let tmp = TempDir::new();
        let path = tmp.path().join("google-client.json");
        let g = google_at(Some(path.clone()), &tmp);
        let good = r#"{"client_id":"id","client_secret":"s"}"#;
        g.import_client(good).unwrap();
        let big = format!(
            r#"{{"client_id":"LEAKME","client_secret":"LEAKME","pad":"{}"}}"#,
            "x".repeat(64 * 1024)
        );
        for (bad, why) in [
            ("not json LEAKME", "not a Google OAuth client"),
            (
                r#"{"installed":{"client_id":"LEAKME"}}"#,
                "not a Google OAuth client",
            ),
            (r#"{"client_secret":"LEAKME"}"#, "not a Google OAuth client"),
            (
                r#"{"web":{"client_id":"LEAKME"}}"#,
                "not a Google OAuth client",
            ),
            (&big, "over 64 KiB"),
        ] {
            let ProviderError::SignInRequired(m) = g.import_client(bad).unwrap_err() else {
                panic!("{bad:.40}");
            };
            assert!(m.contains(why), "{m}");
            assert!(!m.contains("LEAKME"), "{m}");
            assert_eq!(fs::read_to_string(&path).unwrap(), good);
            assert_eq!(g.account(), AccountState::SignedOut);
        }
        // The cap is the same as for reading the file.
        let pad = 64 * 1024 - r#"{"client_id":"id","client_secret":"s","pad":""}"#.len();
        let max = format!(
            r#"{{"client_id":"id","client_secret":"s","pad":"{}"}}"#,
            "x".repeat(pad)
        );
        assert_eq!(max.len(), 64 * 1024);
        g.import_client(&max).unwrap();
    }

    #[test]
    fn an_import_with_nowhere_to_keep_it_says_so() {
        let tmp = TempDir::new();
        let client = r#"{"client_id":"LEAKME","client_secret":"LEAKME"}"#;
        let g = google_at(None, &tmp);
        let ProviderError::SignInRequired(m) = g.import_client(client).unwrap_err() else {
            panic!();
        };
        assert!(m.contains(CLIENT_ENV), "{m}");

        let file = tmp.path().join("file");
        fs::write(&file, "").unwrap();
        let g = google_at(Some(file.join("google-client.json")), &tmp);
        let ProviderError::SignInRequired(m) = g.import_client(client).unwrap_err() else {
            panic!();
        };
        assert!(m.starts_with("cannot save the Google OAuth client"), "{m}");
        assert!(!m.contains("LEAKME"), "{m}");
        assert!(matches!(g.account(), AccountState::Unavailable(_)));
    }

    #[test]
    fn default_locations() {
        let home = Some(PathBuf::from("/home/u"));
        let mac = default_files(home.clone(), None, None, true);
        assert_eq!(
            mac.client.unwrap(),
            Path::new("/home/u/Library/Application Support/requiem/google-client.json")
        );
        assert_eq!(
            mac.token.unwrap(),
            Path::new("/home/u/Library/Application Support/requiem/google-token.json")
        );
        let linux = default_files(home.clone(), None, None, false);
        assert_eq!(
            linux.client.unwrap(),
            Path::new("/home/u/.config/requiem/google-client.json")
        );
        assert_eq!(
            linux.token.unwrap(),
            Path::new("/home/u/.local/state/requiem/google-token.json")
        );
        let xdg = default_files(home, Some("/cfg".into()), Some("/st".into()), false);
        assert_eq!(
            xdg.client.unwrap(),
            Path::new("/cfg/requiem/google-client.json")
        );
        assert_eq!(
            xdg.token.unwrap(),
            Path::new("/st/requiem/google-token.json")
        );
        assert_eq!(
            default_files(None, None, None, true),
            GoogleFiles::default()
        );
    }

    #[test]
    fn data_api_errors_keep_googles_reason() {
        let body = br#"{"error":{"code":403,"message":"The request cannot be completed because you have exceeded your quota.","errors":[{"message":"x","domain":"youtube.quota","reason":"quotaExceeded"}]}}"#;
        assert_eq!(
            api_error(403, body),
            ProviderError::ClientBroken(
                "YouTube Data API: quotaExceeded: The request cannot be completed because you have exceeded your quota.".into()
            )
        );
        assert!(matches!(
            api_error(404, b"{}"),
            ProviderError::Unplayable(_)
        ));
        assert!(matches!(api_error(503, b""), ProviderError::Network(_)));
    }
}
