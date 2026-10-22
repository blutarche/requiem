//! Home's shelves as views. A refresh keeps the views of what did not change as the same
//! objects, so nothing on screen is torn down and made again; a mouse drags a shelf sideways.
//! AppKit views need the main thread, which only a test without the harness runs on.

#[cfg(target_os = "macos")]
fn main() {
    use std::time::Duration;

    let mtm = objc2_foundation::MainThreadMarker::new()
        .expect("a test without the harness runs on the main thread");
    let _app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    // A press waits on events; one that never comes would hang the test.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("shelves: timed out");
        std::process::exit(1);
    });
    mac::a_refresh_keeps_the_views_of_what_did_not_change(mtm);
    mac::a_mouse_drags_a_shelf_sideways(mtm);
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{ClassType, MainThreadOnly, Message, msg_send};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSView,
        NSWindow, NSWindowStyleMask,
    };
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

    fn ptr<T: Message>(v: &Retained<T>) -> *const T {
        Retained::as_ptr(v)
    }

    fn cards(views: &Views, row: usize) -> Vec<(usize, *const NSView)> {
        views[row].1.iter().map(|(i, c)| (*i, ptr(c))).collect()
    }

    fn page_frame() -> NSRect {
        NSRect::new(NSPoint::ZERO, NSSize::new(1000.0, 2000.0))
    }

    pub fn a_refresh_keeps_the_views_of_what_did_not_change(mtm: MainThreadMarker) {
        let shelves = Shelves::new(mtm, page_frame());
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

    fn mouse(ty: NSEventType, at: NSPoint, t: f64, window: &NSWindow) -> Retained<NSEvent> {
        let none: *const AnyObject = std::ptr::null();
        // SAFETY: the arguments have the types of
        // `mouseEventWithType:location:modifierFlags:timestamp:windowNumber:context:eventNumber:clickCount:pressure:`,
        // whose context may be nil.
        let event: Option<Retained<NSEvent>> = unsafe {
            msg_send![
                NSEvent::class(),
                mouseEventWithType: ty,
                location: at,
                modifierFlags: NSEventModifierFlags::empty(),
                timestamp: t,
                windowNumber: window.windowNumber(),
                context: none,
                eventNumber: 0isize,
                clickCount: 1isize,
                pressure: 1.0f32
            ]
        };
        event.expect("a mouse event")
    }

    /// Presses `card`, with the moves `path` from where it was pressed already waiting, the
    /// last of them where it is let go.
    fn press(card: &NSView, window: &NSWindow, path: &[(f64, f64)]) {
        let b = card.bounds();
        let centre = NSPoint::new(b.size.width / 2.0, b.size.height / 2.0);
        let start = card.convertPoint_toView(centre, None);
        let app = NSApplication::sharedApplication(card.mtm());
        let mut t = 100.0;
        for (i, &(dx, dy)) in path.iter().enumerate() {
            t += 0.01;
            let ty = if i + 1 == path.len() {
                NSEventType::LeftMouseUp
            } else {
                NSEventType::LeftMouseDragged
            };
            let to = NSPoint::new(start.x + dx, start.y + dy);
            app.postEvent_atStart(&mouse(ty, to, t, window), false);
        }
        card.mouseDown(&mouse(NSEventType::LeftMouseDown, start, 100.0, window));
    }

    pub fn a_mouse_drags_a_shelf_sideways(mtm: MainThreadMarker) {
        let shelves = Shelves::new(mtm, page_frame());
        // SAFETY: a window released only when the test drops it.
        let window = unsafe {
            let w = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                page_frame(),
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            );
            w.setReleasedWhenClosed(false);
            w
        };
        window.setContentView(Some(&shelves.view));
        shelves.set_shelves(&[shelf("Mixes", 0..20)], false);
        let views = shelves.views();
        let row = &views[0];
        let offset = || row.0.contentView().bounds().origin.x;
        let card = row.1[1].1.clone();
        let between = row.0.documentView().expect("the row's document");

        // A click, and a press that wanders less than the slop, do not move the row.
        press(&card, &window, &[(0.0, 0.0)]);
        press(&card, &window, &[(-3.0, 0.0), (-3.0, 0.0)]);
        assert_eq!(offset(), 0.0);
        // Up and down does nothing.
        press(
            &card,
            &window,
            &[(0.0, -60.0), (0.0, -120.0), (0.0, -120.0)],
        );
        assert_eq!(offset(), 0.0);
        // Dragged left, the row follows the pointer.
        press(
            &card,
            &window,
            &[(-60.0, 0.0), (-150.0, 5.0), (-150.0, 5.0)],
        );
        assert_eq!(offset(), 150.0);
        // And from between the cards too, within the row's start.
        press(
            &between,
            &window,
            &[(100.0, 0.0), (400.0, 0.0), (400.0, 0.0)],
        );
        assert_eq!(offset(), 0.0);
        // A drag in one move, past its slop at once.
        press(&card, &window, &[(-900.0, 0.0), (-900.0, 0.0)]);
        assert_eq!(offset(), 900.0);
        println!("shelves: a mouse drag scrolls a shelf sideways");
        window.close();
    }
}
