//! Home's shelves refreshed in place: views of what did not change are the same objects
//! after a refresh, so nothing on screen is torn down and made again. AppKit views need the
//! main thread, which only a test without the harness runs on.

#[cfg(target_os = "macos")]
fn main() {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSApplication, NSView};
    use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
    use requiem_core::{Collection, CollectionId, CollectionKind, Item, ProviderId, Shelf};
    use requiem_gui::app::shelves::{RowViews, Shelves};

    type Views = Vec<RowViews>;

    fn item(n: usize) -> Item {
        Item::Collection(Collection {
            id: CollectionId {
                provider: ProviderId("ytmusic".into()),
                id: n.to_string(),
            },
            kind: CollectionKind::Playlist,
            title: format!("Mix {n}"),
            subtitle: None,
            artwork_url: None,
        })
    }

    fn shelf(title: &str, items: impl IntoIterator<Item = usize>) -> Shelf {
        Shelf {
            title: title.into(),
            items: items.into_iter().map(item).collect(),
        }
    }

    fn ptr<T: objc2::Message>(v: &Retained<T>) -> *const T {
        Retained::as_ptr(v)
    }

    fn cards(views: &Views, row: usize) -> Vec<(usize, *const NSView)> {
        views[row].1.iter().map(|(i, c)| (*i, ptr(c))).collect()
    }

    let mtm = MainThreadMarker::new().expect("a test without the harness runs on the main thread");
    let _app = NSApplication::sharedApplication(mtm);
    let shelves = Shelves::new(mtm, NSRect::new(NSPoint::ZERO, NSSize::new(1000.0, 2000.0)));
    let page = vec![
        shelf("Mixes", 0..12),
        shelf("New", 100..112),
        shelf("Mixes", 200..212),
    ];
    shelves.set_shelves(&page, false);
    let before = shelves.views();
    assert_eq!(before.len(), 3);
    for (row, (_, c)) in before.iter().enumerate() {
        assert!(c.len() > 2, "row {row} made {} cards", c.len());
    }
    // The user scrolls the second row a little.
    let clip = before[1].0.contentView();
    clip.scrollToPoint(NSPoint::new(150.0, 0.0));
    before[1].0.reflectScrolledClipView(&clip);

    // A refresh changes the second row's second item, and adds a shelf before the rest.
    let mut fresh = page.clone();
    fresh[1].items[1] = item(999);
    fresh.insert(0, shelf("Hot", 300..305));
    shelves.set_shelves(&fresh, true);
    let after = shelves.views();
    assert_eq!(after.len(), 4);
    // The rows of unchanged shelves keep every view.
    for (old, new) in [(0, 1), (2, 3)] {
        assert_eq!(
            ptr(&before[old].0),
            ptr(&after[new].0),
            "row {old} was made again"
        );
        assert_eq!(
            cards(&before, old),
            cards(&after, new),
            "row {old}'s cards were made again"
        );
    }
    // The changed row keeps its scroll view, its place, and its other cards.
    assert_eq!(ptr(&before[1].0), ptr(&after[2].0));
    assert_eq!(after[2].0.contentView().bounds().origin.x, 150.0);
    let (old, new) = (cards(&before, 1), cards(&after, 2));
    let at = |cards: &[(usize, *const NSView)], i| cards.iter().find(|c| c.0 == i).map(|c| c.1);
    assert!(at(&new, 1).is_some(), "the changed item has no card");
    assert_ne!(
        at(&old, 1),
        at(&new, 1),
        "the changed item kept its old card"
    );
    for &(i, card) in &old {
        if i != 1 && at(&new, i).is_some() {
            assert_eq!(at(&new, i), Some(card), "card {i} was made again");
        }
    }

    // The same shelves again change nothing.
    shelves.set_shelves(&fresh, true);
    let again = shelves.views();
    for row in 0..4 {
        assert_eq!(ptr(&after[row].0), ptr(&again[row].0));
        assert_eq!(cards(&after, row), cards(&again, row));
    }

    // Another page, or another filter, starts over.
    shelves.set_shelves(&fresh, false);
    let other = shelves.views();
    assert_ne!(ptr(&again[1].0), ptr(&other[1].0));
    println!("shelves: unchanged rows and cards kept across a refresh");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
