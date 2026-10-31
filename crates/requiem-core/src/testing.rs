//! Fakes for driving a `Session` deterministically.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::{
    AccountState, AudioSource, Browsed, ByteSource, Engine, EngineEvent, EngineEventSink,
    FormatHint, Page, PageSink, Provider, ProviderError, ProviderId, SearchResults, Shelf, Track,
    TrackId,
};

const WAIT: Duration = Duration::from_secs(5);

pub struct CursorSource(Cursor<Vec<u8>>);

impl CursorSource {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Cursor::new(bytes))
    }
}

impl Read for CursorSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for CursorSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl ByteSource for CursorSource {
    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}

/// Blocks a scripted provider call until released.
#[derive(Clone, Default)]
pub struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }

    fn wait(&self) {
        let mut open = self.0.0.lock().unwrap();
        while !*open {
            open = self.0.1.wait(open).unwrap();
        }
    }
}

/// One scripted provider call: optionally wait, then return `result`.
pub struct Step<T> {
    pub result: Result<T, ProviderError>,
    pub delay: Duration,
    pub gate: Option<Gate>,
}

impl<T> Step<T> {
    pub fn ok(value: T) -> Self {
        Self {
            result: Ok(value),
            delay: Duration::ZERO,
            gate: None,
        }
    }

    pub fn err(e: ProviderError) -> Self {
        Self {
            result: Err(e),
            delay: Duration::ZERO,
            gate: None,
        }
    }

    pub fn delayed(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    pub fn gated(mut self, g: &Gate) -> Self {
        self.gate = Some(g.clone());
        self
    }

    fn map<U>(self, f: impl FnOnce(T) -> U) -> Step<U> {
        Step {
            result: self.result.map(f),
            delay: self.delay,
            gate: self.gate,
        }
    }

    fn run(self) -> Result<T, ProviderError> {
        if let Some(g) = &self.gate {
            g.wait();
        }
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        self.result
    }
}

/// One scripted sign-in: report `pending` if any, wait for `gate` if any, then end with
/// `outcome`. A failed sign-in leaves the account `SignedOut`.
pub struct SignInScript {
    pub pending: Option<AccountState>,
    pub gate: Option<Gate>,
    pub outcome: Result<AccountState, ProviderError>,
}

struct ProviderState {
    resolve_scripts: HashMap<String, VecDeque<Step<()>>>,
    search_scripts: HashMap<String, VecDeque<Step<SearchResults>>>,
    browse_scripts: HashMap<Page, VecDeque<Step<Browsed>>>,
    radio_scripts: HashMap<String, VecDeque<Step<Vec<Track>>>>,
    sign_in_scripts: VecDeque<SignInScript>,
    sign_out_gate: Option<Gate>,
    sign_out_error: Option<ProviderError>,
    import_gate: Option<Gate>,
    import_error: Option<ProviderError>,
    imported: Vec<String>,
    resolve_calls: HashMap<String, usize>,
    search_calls: usize,
    browse_calls: Vec<Page>,
    radio_calls: usize,
    sign_in_calls: usize,
    sign_out_calls: usize,
    account: AccountState,
    has_home: bool,
    fetch_ahead: bool,
    changed: Option<PageSink>,
}

impl Default for ProviderState {
    fn default() -> Self {
        Self {
            resolve_scripts: HashMap::new(),
            search_scripts: HashMap::new(),
            browse_scripts: HashMap::new(),
            radio_scripts: HashMap::new(),
            sign_in_scripts: VecDeque::new(),
            sign_out_gate: None,
            sign_out_error: None,
            import_gate: None,
            import_error: None,
            imported: Vec::new(),
            resolve_calls: HashMap::new(),
            search_calls: 0,
            browse_calls: Vec::new(),
            radio_calls: 0,
            sign_in_calls: 0,
            sign_out_calls: 0,
            account: AccountState::SignedOut,
            has_home: false,
            fetch_ahead: true,
            changed: None,
        }
    }
}

/// Unscripted resolves succeed with a source whose bytes are the track id; unscripted browses
/// return no shelves and unscripted radios no tracks; the account starts `SignedOut`.
pub struct FakeProvider {
    id: ProviderId,
    state: Mutex<ProviderState>,
    called: Condvar,
}

impl FakeProvider {
    pub fn new(id: &str) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId(id.into()),
            state: Mutex::default(),
            called: Condvar::new(),
        })
    }

    /// Steps are consumed one per call; once exhausted, calls succeed.
    pub fn script_resolve(&self, track_id: &str, steps: Vec<Step<()>>) {
        self.state
            .lock()
            .unwrap()
            .resolve_scripts
            .insert(track_id.into(), steps.into());
    }

    /// Searches that find only tracks.
    pub fn script_search(&self, query: &str, steps: Vec<Step<Vec<Track>>>) {
        let steps = steps
            .into_iter()
            .map(|s| {
                s.map(|tracks| SearchResults {
                    tracks,
                    ..SearchResults::default()
                })
            })
            .collect();
        self.script_search_results(query, steps);
    }

    pub fn script_search_results(&self, query: &str, steps: Vec<Step<SearchResults>>) {
        self.state
            .lock()
            .unwrap()
            .search_scripts
            .insert(query.into(), steps.into());
    }

    /// Browses that answer with shelves alone.
    pub fn script_browse(&self, page: Page, steps: Vec<Step<Vec<Shelf>>>) {
        let steps = steps
            .into_iter()
            .map(|s| {
                s.map(|shelves| Browsed {
                    about: None,
                    shelves,
                })
            })
            .collect();
        self.script_browsed(page, steps);
    }

    pub fn script_browsed(&self, page: Page, steps: Vec<Step<Browsed>>) {
        self.state
            .lock()
            .unwrap()
            .browse_scripts
            .insert(page, steps.into());
    }

    /// Radios of the track `seed_id`, one step per call.
    pub fn script_radio(&self, seed_id: &str, steps: Vec<Step<Vec<Track>>>) {
        self.state
            .lock()
            .unwrap()
            .radio_scripts
            .insert(seed_id.into(), steps.into());
    }

    /// Scripts are consumed one per `sign_in`; unscripted sign-ins fail.
    pub fn script_sign_in(&self, script: SignInScript) {
        self.state.lock().unwrap().sign_in_scripts.push_back(script);
    }

    /// The next `sign_out` waits for `gate` before it signs out.
    pub fn gate_sign_out(&self, gate: Gate) {
        self.state.lock().unwrap().sign_out_gate = Some(gate);
    }

    /// The next `sign_out` signs out, then fails with `e`, like a revoke the server didn't
    /// confirm.
    pub fn fail_sign_out(&self, e: ProviderError) {
        self.state.lock().unwrap().sign_out_error = Some(e);
    }

    /// The next `import_client` waits for `gate` before it imports.
    pub fn gate_import(&self, gate: Gate) {
        self.state.lock().unwrap().import_gate = Some(gate);
    }

    /// The next `import_client` fails with `e` and changes nothing.
    pub fn fail_import(&self, e: ProviderError) {
        self.state.lock().unwrap().import_error = Some(e);
    }

    /// The JSON of every `import_client` call, in order.
    pub fn imported(&self) -> Vec<String> {
        self.state.lock().unwrap().imported.clone()
    }

    pub fn set_account(&self, state: AccountState) {
        self.state.lock().unwrap().account = state;
    }

    pub fn set_has_home(&self, has_home: bool) {
        self.state.lock().unwrap().has_home = has_home;
    }

    pub fn set_fetch_ahead(&self, fetch_ahead: bool) {
        self.state.lock().unwrap().fetch_ahead = fetch_ahead;
    }

    /// Tells the session that `page` has newer content, as a provider may.
    pub fn say_changed(&self, page: Page) {
        let sink = self.state.lock().unwrap().changed.clone();
        sink.expect("no session has asked to hear of changes")(page);
    }

    pub fn browse_calls(&self) -> Vec<Page> {
        self.state.lock().unwrap().browse_calls.clone()
    }

    pub fn sign_in_calls(&self) -> usize {
        self.state.lock().unwrap().sign_in_calls
    }

    pub fn sign_out_calls(&self) -> usize {
        self.state.lock().unwrap().sign_out_calls
    }

    pub fn resolve_calls(&self, track_id: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .resolve_calls
            .get(track_id)
            .copied()
            .unwrap_or(0)
    }

    pub fn search_calls(&self) -> usize {
        self.state.lock().unwrap().search_calls
    }

    /// Counts calls on entry, so it also fires for calls still blocked in a step.
    pub fn wait_resolve_calls(&self, track_id: &str, n: usize) {
        let st = self.state.lock().unwrap();
        let (_st, res) = self
            .called
            .wait_timeout_while(st, WAIT, |s| {
                s.resolve_calls.get(track_id).copied().unwrap_or(0) < n
            })
            .unwrap();
        assert!(
            !res.timed_out(),
            "timed out waiting for {n} resolve calls of {track_id}"
        );
    }

    pub fn wait_search_calls(&self, n: usize) {
        let st = self.state.lock().unwrap();
        let (_st, res) = self
            .called
            .wait_timeout_while(st, WAIT, |s| s.search_calls < n)
            .unwrap();
        assert!(!res.timed_out(), "timed out waiting for {n} search calls");
    }

    /// Counts calls on entry, so it also fires for a browse still held in a step.
    pub fn wait_browse_calls(&self, n: usize) {
        let st = self.state.lock().unwrap();
        let (_st, res) = self
            .called
            .wait_timeout_while(st, WAIT, |s| s.browse_calls.len() < n)
            .unwrap();
        assert!(!res.timed_out(), "timed out waiting for {n} browse calls");
    }

    /// Counts calls on entry, so it also fires for a radio still held in a step.
    pub fn wait_radio_calls(&self, n: usize) {
        let st = self.state.lock().unwrap();
        let (_st, res) = self
            .called
            .wait_timeout_while(st, WAIT, |s| s.radio_calls < n)
            .unwrap();
        assert!(!res.timed_out(), "timed out waiting for {n} radio calls");
    }

    /// Counts calls on entry, so it also fires for a sign-out still held by its gate.
    pub fn wait_sign_out_calls(&self, n: usize) {
        let st = self.state.lock().unwrap();
        let (_st, res) = self
            .called
            .wait_timeout_while(st, WAIT, |s| s.sign_out_calls < n)
            .unwrap();
        assert!(!res.timed_out(), "timed out waiting for {n} sign-out calls");
    }

    /// Counts calls on entry, so it also fires for an import still held by its gate.
    pub fn wait_import_calls(&self, n: usize) {
        let st = self.state.lock().unwrap();
        let (_st, res) = self
            .called
            .wait_timeout_while(st, WAIT, |s| s.imported.len() < n)
            .unwrap();
        assert!(!res.timed_out(), "timed out waiting for {n} import calls");
    }
}

impl Provider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn search(&self, query: &str, _limit: usize) -> Result<SearchResults, ProviderError> {
        let step = {
            let mut st = self.state.lock().unwrap();
            st.search_calls += 1;
            self.called.notify_all();
            st.search_scripts
                .get_mut(query)
                .and_then(VecDeque::pop_front)
        };
        match step {
            Some(s) => s.run(),
            None => Ok(SearchResults::default()),
        }
    }

    fn browse(&self, page: &Page) -> Result<Browsed, ProviderError> {
        let step = {
            let mut st = self.state.lock().unwrap();
            st.browse_calls.push(page.clone());
            self.called.notify_all();
            st.browse_scripts
                .get_mut(page)
                .and_then(VecDeque::pop_front)
        };
        match step {
            Some(s) => s.run(),
            None => Ok(Browsed::default()),
        }
    }

    fn radio(&self, seed: &TrackId) -> Result<Vec<Track>, ProviderError> {
        let step = {
            let mut st = self.state.lock().unwrap();
            st.radio_calls += 1;
            self.called.notify_all();
            st.radio_scripts
                .get_mut(&seed.id)
                .and_then(VecDeque::pop_front)
        };
        match step {
            Some(s) => s.run(),
            None => Ok(Vec::new()),
        }
    }

    fn has_home(&self) -> bool {
        self.state.lock().unwrap().has_home
    }

    fn fetch_ahead(&self) -> bool {
        self.state.lock().unwrap().fetch_ahead
    }

    fn on_page_changed(&self, changed: PageSink) {
        self.state.lock().unwrap().changed = Some(changed);
    }

    fn account(&self) -> AccountState {
        self.state.lock().unwrap().account.clone()
    }

    fn sign_in(&self, on_state: &dyn Fn(AccountState)) -> Result<(), ProviderError> {
        let script = {
            let mut st = self.state.lock().unwrap();
            st.sign_in_calls += 1;
            self.called.notify_all();
            st.sign_in_scripts.pop_front()
        };
        let Some(script) = script else {
            return Err(ProviderError::Unplayable("no sign-in scripted".into()));
        };
        if let Some(pending) = script.pending {
            self.set_account(pending.clone());
            on_state(pending);
        }
        if let Some(g) = &script.gate {
            g.wait();
        }
        match script.outcome {
            Ok(state) => {
                self.set_account(state.clone());
                on_state(state);
                Ok(())
            }
            // Like a real provider, a failure ends a pending sign-in and leaves any other
            // state as it was.
            Err(e) => {
                let mut st = self.state.lock().unwrap();
                if matches!(st.account, AccountState::Pending { .. }) {
                    st.account = AccountState::SignedOut;
                }
                Err(e)
            }
        }
    }

    fn sign_out(&self) -> Result<(), ProviderError> {
        let (gate, error) = {
            let mut st = self.state.lock().unwrap();
            st.sign_out_calls += 1;
            self.called.notify_all();
            (st.sign_out_gate.take(), st.sign_out_error.take())
        };
        if let Some(g) = gate {
            g.wait();
        }
        self.set_account(AccountState::SignedOut);
        error.map_or(Ok(()), Err)
    }

    /// Like a provider that was missing only its client, a successful import makes an
    /// `Unavailable` account `SignedOut`.
    fn import_client(&self, json: &str) -> Result<(), ProviderError> {
        let gate = {
            let mut st = self.state.lock().unwrap();
            st.imported.push(json.to_owned());
            self.called.notify_all();
            st.import_gate.take()
        };
        if let Some(g) = gate {
            g.wait();
        }
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.import_error.take() {
            return Err(e);
        }
        if matches!(st.account, AccountState::Unavailable(_)) {
            st.account = AccountState::SignedOut;
        }
        Ok(())
    }

    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError> {
        let step = {
            let mut st = self.state.lock().unwrap();
            *st.resolve_calls.entry(track.id.clone()).or_default() += 1;
            self.called.notify_all();
            st.resolve_scripts
                .get_mut(&track.id)
                .and_then(VecDeque::pop_front)
        };
        if let Some(s) = step {
            s.run()?;
        }
        Ok(AudioSource::Encoded {
            bytes: Box::new(CursorSource::new(track.id.clone().into_bytes())),
            hint: FormatHint::WebmOpus,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineCall {
    /// `payload` is the full content of the loaded source.
    Load {
        r#gen: u64,
        payload: Vec<u8>,
        paused: bool,
    },
    Pause,
    Resume,
    Seek(Duration),
    SetVolume(f32),
    Stop,
}

impl EngineCall {
    pub fn load_payload(&self) -> Option<String> {
        match self {
            Self::Load { payload, .. } => Some(String::from_utf8_lossy(payload).into_owned()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct EngineShared {
    calls: Mutex<Vec<EngineCall>>,
    recorded: Condvar,
    sink: Mutex<Option<EngineEventSink>>,
}

/// Test-side view of a `FakeEngine` living inside a `Session`.
#[derive(Clone, Default)]
pub struct FakeEngineHandle(Arc<EngineShared>);

impl FakeEngineHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pass to `Session::spawn`.
    pub fn factory(&self) -> impl FnOnce(EngineEventSink) -> Box<dyn Engine> + Send + 'static {
        let shared = self.0.clone();
        move |sink| {
            *shared.sink.lock().unwrap() = Some(sink);
            Box::new(FakeEngine(shared))
        }
    }

    pub fn calls(&self) -> Vec<EngineCall> {
        self.0.calls.lock().unwrap().clone()
    }

    /// Waits until `pred` holds over the recorded calls, then returns them.
    pub fn wait_calls(&self, pred: impl Fn(&[EngineCall]) -> bool) -> Vec<EngineCall> {
        let calls = self.0.calls.lock().unwrap();
        let (calls, res) = self
            .0
            .recorded
            .wait_timeout_while(calls, WAIT, |c| !pred(c))
            .unwrap();
        assert!(
            !res.timed_out(),
            "timed out waiting on engine calls: {calls:?}"
        );
        calls.clone()
    }

    /// Waits for the nth (1-based) load and returns its `(r#gen, payload)`.
    pub fn wait_load(&self, n: usize) -> (u64, String) {
        let calls = self.wait_calls(|c| loads(c) >= n);
        match calls
            .iter()
            .filter(|c| c.load_payload().is_some())
            .nth(n - 1)
        {
            Some(EngineCall::Load { r#gen, payload, .. }) => {
                (*r#gen, String::from_utf8_lossy(payload).into_owned())
            }
            _ => unreachable!(),
        }
    }

    pub fn emit(&self, ev: EngineEvent) {
        let sink = self
            .0
            .sink
            .lock()
            .unwrap()
            .clone()
            .expect("engine not constructed yet");
        sink(ev);
    }
}

fn loads(calls: &[EngineCall]) -> usize {
    calls
        .iter()
        .filter(|c| matches!(c, EngineCall::Load { .. }))
        .count()
}

struct FakeEngine(Arc<EngineShared>);

impl FakeEngine {
    fn record(&self, call: EngineCall) {
        self.0.calls.lock().unwrap().push(call);
        self.0.recorded.notify_all();
    }
}

impl Engine for FakeEngine {
    fn load(&mut self, r#gen: u64, src: AudioSource, paused: bool) {
        let AudioSource::Encoded { mut bytes, .. } = src;
        let mut payload = Vec::new();
        bytes.read_to_end(&mut payload).expect("read fake source");
        self.record(EngineCall::Load {
            r#gen,
            payload,
            paused,
        });
    }

    fn pause(&mut self) {
        self.record(EngineCall::Pause);
    }

    fn resume(&mut self) {
        self.record(EngineCall::Resume);
    }

    fn seek(&mut self, to: Duration) {
        self.record(EngineCall::Seek(to));
    }

    fn set_volume(&mut self, v: f32) {
        self.record(EngineCall::SetVolume(v));
    }

    fn stop(&mut self) {
        self.record(EngineCall::Stop);
    }
}
