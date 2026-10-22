//! Remembers Home and Library across restarts, so that they show before the services answer.
//! Only what the user was shown is kept: shelves, ids and artwork URLs, never a credential.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use requiem_core::{PagePart, SavedPages};
use serde::{Deserialize, Serialize};

use crate::persist;

const VERSION: u32 = 1;

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    home: &'a [PagePart],
    library: &'a [PagePart],
}

#[derive(Deserialize)]
struct FileIn {
    version: u32,
    home: Vec<PagePart>,
    library: Vec<PagePart>,
}

/// Next to the state file, in the same private directory.
pub fn path(state: &Path) -> PathBuf {
    state.with_file_name("pages.json")
}

/// Any failure, a missing file included, is a fresh start: the pages are fetched again anyway.
pub fn load(path: &Path) -> SavedPages {
    read(path).unwrap_or_else(|e| {
        debug!("{e}");
        SavedPages::default()
    })
}

fn read(path: &Path) -> Result<SavedPages, String> {
    let Some(bytes) = persist::read_capped(path, "pages")? else {
        return Ok(SavedPages::default());
    };
    let parsed: FileIn =
        serde_json::from_slice(&bytes).map_err(|e| format!("pages file unparsable: {e}"))?;
    if parsed.version != VERSION {
        return Err(format!("pages file version {} unsupported", parsed.version));
    }
    Ok(SavedPages {
        home: parsed.home,
        library: parsed.library,
    })
}

fn save(path: &Path, pages: &SavedPages) -> std::io::Result<()> {
    let json = serde_json::to_vec(&FileOut {
        version: VERSION,
        home: &pages.home,
        library: &pages.library,
    })
    .map_err(std::io::Error::other)?;
    persist::write_atomic(path, &json)
}

/// Starts the writer thread, which skips pages equal to `loaded` and writes any pending ones
/// when `Sender`s are all dropped.
pub fn spawn_writer(path: PathBuf, loaded: SavedPages) -> (Sender<SavedPages>, JoinHandle<()>) {
    persist::spawn_saver(
        "requiemd-pages",
        Some(loaded),
        persist::QUIET,
        persist::MAX_DELAY,
        move |p| save(&path, p),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use requiem_core::testing::{FakeEngineHandle, FakeProvider, Gate, Step};
    use requiem_core::{
        AccountState, Command, Event, Item, Page, Provider, ProviderId, Session, SessionConfig,
        Shelf, Track, TrackId,
    };

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "requiemd-pages-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            path(&self.0.join("state.json"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn shelf(ids: &[&str]) -> Shelf {
        Shelf {
            title: "Playlists".into(),
            items: ids
                .iter()
                .map(|id| {
                    Item::Track(Track {
                        id: TrackId {
                            provider: ProviderId("fake".into()),
                            id: (*id).into(),
                        },
                        title: (*id).into(),
                        artists: vec![],
                        album: None,
                        duration: None,
                        artwork_url: Some(format!("https://example.test/{id}.jpg")),
                    })
                })
                .collect(),
        }
    }

    fn pages() -> SavedPages {
        SavedPages {
            home: vec![],
            library: vec![PagePart {
                provider: ProviderId("fake".into()),
                account: Some("Me".into()),
                shelves: vec![shelf(&["a"])],
            }],
        }
    }

    #[test]
    fn the_file_sits_next_to_the_state_file() {
        assert_eq!(
            path(Path::new("/x/requiem/state.json")),
            PathBuf::from("/x/requiem/pages.json")
        );
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new();
        save(&dir.file(), &pages()).unwrap();
        assert_eq!(load(&dir.file()), pages());
    }

    #[test]
    fn missing_corrupt_or_newer_files_are_a_fresh_start() {
        let dir = TempDir::new();
        assert_eq!(load(&dir.file()), SavedPages::default());
        fs::write(dir.file(), b"{not json").unwrap();
        assert_eq!(load(&dir.file()), SavedPages::default());
        fs::write(
            dir.file(),
            r#"{"version":2,"home":[],"library":[]}"#.as_bytes(),
        )
        .unwrap();
        assert_eq!(load(&dir.file()), SavedPages::default());
    }

    fn start(provider: Arc<FakeProvider>, dir: &TempDir) -> (Session, JoinHandle<()>) {
        let (tx, writer) = spawn_writer(dir.file(), load(&dir.file()));
        let session = Session::spawn(
            vec![provider as Arc<dyn Provider>],
            FakeEngineHandle::new().factory(),
            SessionConfig {
                pages: load(&dir.file()),
                save_pages: Some(Arc::new(move |p| {
                    let _ = tx.send(p);
                })),
                ..SessionConfig::default()
            },
        );
        (session, writer)
    }

    fn library(session: &Session) -> Event {
        let rx = session.subscribe();
        session.send(Command::Browse {
            req: 1,
            page: Page::Library,
        });
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("an answer");
            if matches!(ev, Event::BrowseResults { .. } | Event::BrowseFailed { .. }) {
                return ev;
            }
        }
    }

    #[test]
    fn a_restart_answers_library_with_what_the_last_run_kept() {
        let dir = TempDir::new();
        let me = AccountState::SignedIn {
            name: Some("Me".into()),
        };
        let first = FakeProvider::new("fake");
        first.set_account(me.clone());
        first.script_browse(Page::Library, vec![Step::ok(vec![shelf(&["a"])])]);
        let (session, writer) = start(first, &dir);
        library(&session);
        session.shutdown();
        writer.join().unwrap();

        // The next run's fetch is held: only the file can answer.
        let second = FakeProvider::new("fake");
        second.set_account(me);
        let gate = Gate::new();
        second.script_browse(
            Page::Library,
            vec![Step::ok(vec![shelf(&["b"])]).gated(&gate)],
        );
        let (session, writer) = start(second, &dir);
        assert_eq!(
            library(&session),
            Event::BrowseResults {
                req: 1,
                page: Page::Library,
                shelves: vec![shelf(&["a"])],
            }
        );
        gate.release();
        session.shutdown();
        writer.join().unwrap();
    }
}
