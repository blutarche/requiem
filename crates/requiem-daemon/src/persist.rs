//! Remembers the queue, the track Play resumes and the volume across restarts.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use requiem_core::{PersistState, Track};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// A state is written once no newer one has arrived for this long...
const QUIET: Duration = Duration::from_millis(500);
/// ...or this long after the first unwritten one.
const MAX_DELAY: Duration = Duration::from_secs(2);

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    volume: u8,
    resume: Option<usize>,
    queue: &'a [Track],
}

#[derive(Deserialize)]
struct FileIn {
    version: u32,
    volume: u8,
    resume: Option<usize>,
    queue: Vec<Track>,
}

/// `None` turns persistence off.
pub fn state_path() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if let Some(p) = var("REQUIEM_STATE_FILE") {
        if p.is_absolute() {
            return Some(p);
        }
        eprintln!("requiemd: ignoring REQUIEM_STATE_FILE: not an absolute path");
    }
    let path = default_path(
        var("HOME"),
        var("XDG_STATE_HOME"),
        cfg!(target_os = "macos"),
    );
    if path.is_none() {
        eprintln!("requiemd: no usable HOME; the queue and volume will not be remembered");
    }
    path
}

fn default_path(home: Option<PathBuf>, xdg_state: Option<PathBuf>, macos: bool) -> Option<PathBuf> {
    let home = home.filter(|h| h.is_absolute());
    let base = if macos {
        home?.join("Library/Application Support")
    } else {
        match xdg_state.filter(|x| x.is_absolute()) {
            Some(x) => x,
            None => home?.join(".local/state"),
        }
    };
    Some(base.join("requiem/state.json"))
}

/// A missing file is a quiet fresh start; any other failure is a logged one. The file is left
/// alone either way: the next save replaces it.
pub fn load(path: &Path) -> Option<PersistState> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            debug!("state file unreadable: {e}");
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(e) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
        debug!("state file unreadable: {e}");
        return None;
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        debug!("state file over {MAX_FILE_BYTES} bytes, ignoring");
        return None;
    }
    let parsed: FileIn = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            debug!("state file unparsable: {e}");
            return None;
        }
    };
    if parsed.version != VERSION {
        debug!("state file version {} unsupported", parsed.version);
        return None;
    }
    let resume = parsed.resume.filter(|r| *r < parsed.queue.len());
    Some(PersistState {
        tracks: parsed.queue,
        resume,
        volume: parsed.volume.min(100),
    })
}

fn save(path: &Path, state: &PersistState) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("state path has no parent"))?;
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let json = serde_json::to_vec(&FileOut {
        version: VERSION,
        volume: state.volume,
        resume: state.resume,
        queue: &state.tracks,
    })
    .map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(&json))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Starts the writer thread. It skips states equal to the last one written, which `loaded`
/// seeds, and writes any pending state when `Sender`s are all dropped.
pub fn spawn_writer(
    path: PathBuf,
    loaded: Option<PersistState>,
) -> (Sender<PersistState>, JoinHandle<()>) {
    spawn_writer_with(path, loaded, QUIET, MAX_DELAY)
}

fn spawn_writer_with(
    path: PathBuf,
    loaded: Option<PersistState>,
    quiet: Duration,
    max_delay: Duration,
) -> (Sender<PersistState>, JoinHandle<()>) {
    let (tx, rx) = unbounded();
    let handle = thread::Builder::new()
        .name("requiemd-persist".into())
        .spawn(move || write_loop(&rx, &path, loaded, quiet, max_delay))
        .expect("spawn persist thread");
    (tx, handle)
}

fn write_loop(
    rx: &Receiver<PersistState>,
    path: &Path,
    mut written: Option<PersistState>,
    quiet: Duration,
    max_delay: Duration,
) {
    // The newest unwritten state, when the first one arrived, and when the latest did.
    let mut pending: Option<(PersistState, Instant, Instant)> = None;
    let mut flush = |pending: &mut Option<(PersistState, Instant, Instant)>| {
        let Some((state, ..)) = pending.take() else {
            return;
        };
        if written.as_ref() == Some(&state) {
            return;
        }
        match save(path, &state) {
            Ok(()) => written = Some(state),
            Err(e) => debug!("saving state failed: {e}"),
        }
    };
    loop {
        let msg = match &pending {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some((_, first, last)) => {
                let due = (*last + quiet).min(*first + max_delay);
                rx.recv_timeout(due.saturating_duration_since(Instant::now()))
            }
        };
        match msg {
            Ok(state) => {
                let now = Instant::now();
                let first = pending.as_ref().map_or(now, |p| p.1);
                pending = Some((state, first, now));
            }
            Err(RecvTimeoutError::Timeout) => flush(&mut pending),
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut pending);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    use requiem_core::{ProviderId, TrackId};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "requiemd-persist-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("state.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn track(id: &str) -> Track {
        Track {
            id: TrackId {
                provider: ProviderId("yt".into()),
                id: id.into(),
            },
            title: id.into(),
            artists: vec!["artist".into()],
            album: Some("album".into()),
            duration: Some(Duration::from_millis(201_500)),
            artwork_url: None,
        }
    }

    fn state(ids: &[&str], resume: Option<usize>, volume: u8) -> PersistState {
        PersistState {
            tracks: ids.iter().map(|i| track(i)).collect(),
            resume,
            volume,
        }
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new();
        let s = state(&["a", "b"], Some(1), 40);
        save(&dir.file(), &s).unwrap();
        assert_eq!(load(&dir.file()), Some(s));
    }

    #[test]
    fn file_shape() {
        let dir = TempDir::new();
        save(&dir.file(), &state(&["a"], None, 7)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["volume"], 7);
        assert!(v["resume"].is_null());
        assert_eq!(v["queue"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let dir = TempDir::new();
        let json = serde_json::json!({
            "version": 1, "volume": 250, "resume": 5, "queue": [track("a")],
        });
        fs::write(dir.file(), json.to_string()).unwrap();
        let s = load(&dir.file()).unwrap();
        assert_eq!((s.resume, s.volume), (None, 100));
    }

    #[test]
    fn missing_file_is_a_fresh_start() {
        let dir = TempDir::new();
        assert_eq!(load(&dir.file()), None);
    }

    #[test]
    fn corrupt_json_is_a_fresh_start_and_left_alone() {
        let dir = TempDir::new();
        fs::write(dir.file(), b"{not json").unwrap();
        assert_eq!(load(&dir.file()), None);
        assert_eq!(fs::read(dir.file()).unwrap(), b"{not json");
    }

    #[test]
    fn wrong_version_is_a_fresh_start() {
        let dir = TempDir::new();
        let json = serde_json::json!({
            "version": 2, "volume": 40, "resume": null, "queue": [],
        });
        fs::write(dir.file(), json.to_string()).unwrap();
        assert_eq!(load(&dir.file()), None);
    }

    #[test]
    fn oversize_file_is_a_fresh_start() {
        let dir = TempDir::new();
        let mut json = br#"{"version":1,"volume":1,"resume":null,"queue":[],"pad":""#.to_vec();
        json.resize(MAX_FILE_BYTES as usize + 1, b'x');
        fs::write(dir.file(), json).unwrap();
        assert_eq!(load(&dir.file()), None);
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = TempDir::new();
        save(&dir.file(), &state(&["a"], Some(0), 1)).unwrap();
        save(&dir.file(), &state(&["b"], Some(0), 2)).unwrap();
        let names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
    }

    #[test]
    fn save_creates_the_parent_dir_private() {
        let dir = TempDir::new();
        let path = dir.0.join("sub/requiem/state.json");
        save(&path, &state(&["a"], Some(0), 1)).unwrap();
        let mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn default_paths() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            default_path(p("/Users/u"), p("/x"), true),
            p("/Users/u/Library/Application Support/requiem/state.json")
        );
        assert_eq!(
            default_path(p("/home/u"), p("/xdg"), false),
            p("/xdg/requiem/state.json")
        );
        assert_eq!(
            default_path(p("/home/u"), p("rel"), false),
            p("/home/u/.local/state/requiem/state.json")
        );
        assert_eq!(default_path(None, p("/xdg"), true), None);
        assert_eq!(default_path(p("rel"), None, false), None);
    }

    #[test]
    fn writer_skips_the_loaded_state_and_flushes_on_close() {
        let dir = TempDir::new();
        let loaded = state(&["a"], Some(0), 50);
        let (tx, handle) = spawn_writer_with(
            dir.file(),
            Some(loaded.clone()),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        tx.send(loaded).unwrap();
        tx.send(state(&["a", "b"], Some(1), 50)).unwrap();
        tx.send(state(&["a", "b", "c"], Some(2), 50)).unwrap();
        drop(tx);
        handle.join().unwrap();
        assert_eq!(
            load(&dir.file()),
            Some(state(&["a", "b", "c"], Some(2), 50))
        );
    }

    #[test]
    fn writer_does_not_write_a_state_equal_to_the_loaded_one() {
        let dir = TempDir::new();
        let loaded = state(&["a"], Some(0), 50);
        let (tx, handle) = spawn_writer(dir.file(), Some(loaded.clone()));
        tx.send(loaded).unwrap();
        drop(tx);
        handle.join().unwrap();
        assert!(!dir.file().exists());
    }

    #[test]
    fn writer_dedupes_identical_states() {
        let dir = TempDir::new();
        let s = state(&["a"], Some(0), 50);
        let (tx, handle) = spawn_writer_with(
            dir.file(),
            None,
            Duration::from_millis(20),
            Duration::from_secs(60),
        );
        tx.send(s.clone()).unwrap();
        wait_for(|| dir.file().exists());
        fs::remove_file(dir.file()).unwrap();
        tx.send(s).unwrap();
        thread::sleep(Duration::from_millis(200));
        drop(tx);
        handle.join().unwrap();
        assert!(!dir.file().exists());
    }

    #[test]
    fn writer_writes_within_the_max_delay_despite_a_steady_stream() {
        let dir = TempDir::new();
        let (tx, handle) = spawn_writer_with(
            dir.file(),
            None,
            Duration::from_secs(60),
            Duration::from_millis(100),
        );
        tx.send(state(&["a"], Some(0), 1)).unwrap();
        wait_for(|| dir.file().exists());
        drop(tx);
        handle.join().unwrap();
    }

    fn wait_for(pred: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pred() {
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
