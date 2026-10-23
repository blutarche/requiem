//! Remembers Home, Library and the collections opened across restarts, so that they show
//! before the services answer. Only what the user was shown is kept: shelves, ids and artwork
//! URLs, never a credential. Collections have a file of their own, as they are far larger and
//! change apart from the pages.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use requiem_core::{CollectionId, PagePart, PagesSink, SavedCollection, SavedPages};
use serde::{Deserialize, Serialize};

use crate::persist;

const VERSION: u32 = 1;
/// The collections file keeps the most recently opened that fit, well under the size past
/// which `persist::read_capped` refuses a file.
const COLLECTIONS_BUDGET: usize = 8 * 1024 * 1024;

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

#[derive(Serialize)]
struct CollectionOut<'a> {
    id: &'a CollectionId,
    fetched: u64,
    part: &'a PagePart,
}

#[derive(Deserialize)]
struct CollectionIn {
    id: CollectionId,
    fetched: u64,
    part: PagePart,
}

#[derive(Deserialize)]
struct CollectionsIn {
    version: u32,
    /// Least recently opened first.
    collections: Vec<CollectionIn>,
}

/// Home and Library, as the pages file holds them.
#[derive(PartialEq)]
struct Pages {
    home: Vec<PagePart>,
    library: Vec<PagePart>,
}

/// Next to the state file, in the same private directory.
pub fn path(state: &Path) -> PathBuf {
    state.with_file_name("pages.json")
}

/// Next to the pages file.
fn collections_path(pages: &Path) -> PathBuf {
    pages.with_file_name("collections.json")
}

/// Any failure, a missing file included, is a fresh start of what that file held: the pages
/// are fetched again anyway.
pub fn load(path: &Path) -> SavedPages {
    let pages = read(path).unwrap_or_else(|e| {
        debug!("{e}");
        Pages {
            home: Vec::new(),
            library: Vec::new(),
        }
    });
    let collections = read_collections(&collections_path(path)).unwrap_or_else(|e| {
        debug!("{e}");
        Vec::new()
    });
    SavedPages {
        home: pages.home,
        library: pages.library,
        collections,
    }
}

fn read(path: &Path) -> Result<Pages, String> {
    let Some(bytes) = persist::read_capped(path, "pages")? else {
        return Ok(Pages {
            home: Vec::new(),
            library: Vec::new(),
        });
    };
    let parsed: FileIn =
        serde_json::from_slice(&bytes).map_err(|e| format!("pages file unparsable: {e}"))?;
    if parsed.version != VERSION {
        return Err(format!("pages file version {} unsupported", parsed.version));
    }
    Ok(Pages {
        home: parsed.home,
        library: parsed.library,
    })
}

fn read_collections(path: &Path) -> Result<Vec<SavedCollection>, String> {
    let Some(bytes) = persist::read_capped(path, "collections")? else {
        return Ok(Vec::new());
    };
    let parsed: CollectionsIn =
        serde_json::from_slice(&bytes).map_err(|e| format!("collections file unparsable: {e}"))?;
    if parsed.version != VERSION {
        return Err(format!(
            "collections file version {} unsupported",
            parsed.version
        ));
    }
    Ok(parsed
        .collections
        .into_iter()
        .map(|c| SavedCollection {
            id: c.id,
            part: Arc::new(c.part),
            fetched: c.fetched,
        })
        .collect())
}

fn save(path: &Path, pages: &Pages) -> std::io::Result<()> {
    let json = serde_json::to_vec(&FileOut {
        version: VERSION,
        home: &pages.home,
        library: &pages.library,
    })
    .map_err(std::io::Error::other)?;
    persist::write_atomic(path, &json)
}

fn save_collections(path: &Path, collections: &[SavedCollection]) -> std::io::Result<()> {
    persist::write_atomic(path, &collections_json(collections, COLLECTIONS_BUDGET)?)
}

/// The file's bytes, keeping of `collections`, least recently opened first, the most recently
/// opened whose entries fit in `budget` bytes.
fn collections_json(collections: &[SavedCollection], budget: usize) -> std::io::Result<Vec<u8>> {
    let mut entries = Vec::new();
    let mut size = 0;
    for c in collections.iter().rev() {
        let entry = serde_json::to_vec(&CollectionOut {
            id: &c.id,
            fetched: c.fetched,
            part: &c.part,
        })
        .map_err(std::io::Error::other)?;
        size += entry.len() + 1;
        if size > budget {
            break;
        }
        entries.push(entry);
    }
    let mut json = format!(r#"{{"version":{VERSION},"collections":["#).into_bytes();
    for (i, entry) in entries.iter().rev().enumerate() {
        if i > 0 {
            json.push(b',');
        }
        json.extend_from_slice(entry);
    }
    json.extend_from_slice(b"]}");
    Ok(json)
}

/// Starts the writer threads, which skip what equals `loaded` and write anything pending once
/// the sink is dropped.
pub fn spawn_writer(path: PathBuf, loaded: SavedPages) -> (PagesSink, [JoinHandle<()>; 2]) {
    let collections = collections_path(&path);
    let (pages_tx, pages_writer) = persist::spawn_saver(
        "requiemd-pages",
        Some(Pages {
            home: loaded.home,
            library: loaded.library,
        }),
        persist::QUIET,
        persist::MAX_DELAY,
        move |p| save(&path, p),
    );
    let (collections_tx, collections_writer) = persist::spawn_saver(
        "requiemd-collections",
        Some(loaded.collections),
        persist::QUIET,
        persist::MAX_DELAY,
        move |c: &Vec<SavedCollection>| save_collections(&collections, c),
    );
    let sink: PagesSink = Arc::new(move |pages: SavedPages| {
        let _ = pages_tx.send(Pages {
            home: pages.home,
            library: pages.library,
        });
        let _ = collections_tx.send(pages.collections);
    });
    (sink, [pages_writer, collections_writer])
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
                        artist_ids: Vec::new(),
                        album: None,
                        duration: None,
                        artwork_url: Some(format!("https://example.test/{id}.jpg")),
                    })
                })
                .collect(),
        }
    }

    fn part(ids: &[&str]) -> PagePart {
        PagePart {
            provider: ProviderId("fake".into()),
            account: Some("Me".into()),
            shelves: vec![shelf(ids)],
        }
    }

    fn collection_id(id: &str) -> CollectionId {
        CollectionId {
            provider: ProviderId("fake".into()),
            id: id.into(),
        }
    }

    fn saved_collection(id: &str, ids: &[&str]) -> SavedCollection {
        SavedCollection {
            id: collection_id(id),
            part: Arc::new(part(ids)),
            fetched: 1_800_000_000,
        }
    }

    fn pages() -> SavedPages {
        SavedPages {
            home: vec![],
            library: vec![part(&["a"])],
            collections: vec![
                saved_collection("x", &["x1"]),
                saved_collection("y", &["y1"]),
            ],
        }
    }

    fn save_all(file: &Path, pages: &SavedPages) {
        save(
            file,
            &Pages {
                home: pages.home.clone(),
                library: pages.library.clone(),
            },
        )
        .unwrap();
        save_collections(&collections_path(file), &pages.collections).unwrap();
    }

    #[test]
    fn the_files_sit_next_to_the_state_file() {
        let pages = path(Path::new("/x/requiem/state.json"));
        assert_eq!(pages, PathBuf::from("/x/requiem/pages.json"));
        assert_eq!(
            collections_path(&pages),
            PathBuf::from("/x/requiem/collections.json")
        );
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new();
        save_all(&dir.file(), &pages());
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

    #[test]
    fn a_bad_collections_file_loses_only_the_collections() {
        let dir = TempDir::new();
        save_all(&dir.file(), &pages());
        fs::write(collections_path(&dir.file()), b"{not json").unwrap();
        assert_eq!(
            load(&dir.file()),
            SavedPages {
                collections: vec![],
                ..pages()
            }
        );
    }

    #[test]
    fn the_collections_file_keeps_the_most_recently_opened_that_fit() {
        let all = vec![
            saved_collection("old", &["o1", "o2", "o3"]),
            saved_collection("mid", &["m1"]),
            saved_collection("new", &["n1"]),
        ];
        let size = |c: &[SavedCollection]| collections_json(c, usize::MAX).unwrap().len();
        // Room for the two newest only.
        let budget = size(&all[1..]) - r#"{"version":1,"collections":[]}"#.len() + 1;
        let json = collections_json(&all, budget).unwrap();
        let parsed: CollectionsIn = serde_json::from_slice(&json).unwrap();
        let ids: Vec<String> = parsed.collections.into_iter().map(|c| c.id.id).collect();
        assert_eq!(ids, ["mid", "new"]);
    }

    fn start(provider: Arc<FakeProvider>, dir: &TempDir) -> (Session, [JoinHandle<()>; 2]) {
        let (sink, writers) = spawn_writer(dir.file(), load(&dir.file()));
        let session = Session::spawn(
            vec![provider as Arc<dyn Provider>],
            FakeEngineHandle::new().factory(),
            SessionConfig {
                pages: load(&dir.file()),
                save_pages: Some(sink),
                ..SessionConfig::default()
            },
        );
        (session, writers)
    }

    fn stop(session: Session, writers: [JoinHandle<()>; 2]) {
        session.shutdown();
        for w in writers {
            w.join().unwrap();
        }
    }

    fn browse(session: &Session, page: Page) -> Event {
        let rx = session.subscribe();
        session.send(Command::Browse { req: 1, page });
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("an answer");
            if matches!(ev, Event::BrowseResults { .. } | Event::BrowseFailed { .. }) {
                return ev;
            }
        }
    }

    fn library(session: &Session) -> Event {
        browse(session, Page::Library)
    }

    #[test]
    fn a_restart_answers_a_collection_with_what_the_last_run_kept() {
        let dir = TempDir::new();
        let me = AccountState::SignedIn {
            name: Some("Me".into()),
        };
        let page = Page::Collection(collection_id("x"));
        let first = FakeProvider::new("fake");
        first.set_account(me.clone());
        first.script_browse(page.clone(), vec![Step::ok(vec![shelf(&["x1"])])]);
        let (session, writers) = start(first, &dir);
        browse(&session, page.clone());
        stop(session, writers);

        // The next run's fetch is held: only the file can answer.
        let second = FakeProvider::new("fake");
        second.set_account(me);
        let gate = Gate::new();
        second.script_browse(
            page.clone(),
            vec![Step::ok(vec![shelf(&["x2"])]).gated(&gate)],
        );
        let (session, writers) = start(second, &dir);
        assert_eq!(
            browse(&session, page.clone()),
            Event::BrowseResults {
                req: 1,
                page,
                shelves: vec![shelf(&["x1"])],
            }
        );
        gate.release();
        stop(session, writers);
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
        let (session, writers) = start(first, &dir);
        library(&session);
        stop(session, writers);

        // The next run's fetch is held: only the file can answer.
        let second = FakeProvider::new("fake");
        second.set_account(me);
        let gate = Gate::new();
        second.script_browse(
            Page::Library,
            vec![Step::ok(vec![shelf(&["b"])]).gated(&gate)],
        );
        let (session, writers) = start(second, &dir);
        assert_eq!(
            library(&session),
            Event::BrowseResults {
                req: 1,
                page: Page::Library,
                shelves: vec![shelf(&["a"])],
            }
        );
        gate.release();
        stop(session, writers);
    }
}
