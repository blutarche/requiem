//! A scrolling grid of square cover cards for playlists and albums.
//!
//! A page can hold hundreds of collections, and a card with its cover costs a quarter-megabyte
//! of decoded artwork, so a card exists only while its place is near the visible area.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSBorderType, NSEvent, NSFocusRingType, NSResponder, NSScrollView,
    NSTextField, NSTrackingAreaOptions, NSView, NSViewBoundsDidChangeNotification,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSEdgeInsets, NSNotification, NSNotificationCenter, NSPoint, NSRect, NSSize,
};
use requiem_core::{Collection, CollectionKind};
use requiem_macos::artwork::Cover;

use super::badge::{self, Badge};
use super::shelves::spoken;
use super::theme::{self, Weight};
use super::widgets::{self, FLEX_H, FLEX_W, rect};
use crate::vm::{self, Service};

const PAD: CGFloat = 24.0;
const TOP_PAD: CGFloat = 8.0;
/// The side of Home's cards, which the grid's cards grow from to fill the width.
const CARD_W: CGFloat = 128.0;
const GAP: CGFloat = 16.0;
const ROW_GAP: CGFloat = 20.0;
/// A card's hover fill reaches this far past its content, on every side.
const INSET: CGFloat = 6.0;
const COVER_RADIUS: CGFloat = 8.0;

/// The corner radius of `collection`'s cover `side` square: an artist's photo is round.
fn cover_radius(collection: &Collection, side: CGFloat) -> CGFloat {
    if collection.kind == CollectionKind::Artist {
        side / 2.0
    } else {
        COVER_RADIUS
    }
}
/// How far past the visible area cards are kept.
const VIEW_MARGIN: CGFloat = 160.0;
const SELECTED_W: CGFloat = 2.0;

/// The columns that fill a grid `width` wide and the side of the square covers in them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    cols: usize,
    side: CGFloat,
}

impl Layout {
    /// As many columns as fit cards of Home's size; the cards then grow to fill the width.
    fn of(width: CGFloat) -> Layout {
        let room = (width - 2.0 * PAD).max(CARD_W / 2.0);
        let cols = (((room + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        let side = ((room - (cols - 1) as CGFloat * GAP) / cols as CGFloat).floor();
        Layout { cols, side }
    }

    fn title_h() -> CGFloat {
        widgets::line_height(&theme::font(13.0, Weight::Medium))
    }

    fn sub_h() -> CGFloat {
        widgets::line_height(&theme::font(12.0, Weight::Regular))
    }

    /// A card's frame height: cover, title, subtitle and the hover inset.
    fn card_h(self) -> CGFloat {
        2.0 * INSET + self.side + 8.0 + Self::title_h() + 2.0 + Self::sub_h()
    }

    /// From the top of one row of cards to the next.
    fn pitch(self) -> CGFloat {
        self.card_h() - 2.0 * INSET + ROW_GAP
    }

    /// Where card `i`'s frame goes in the document.
    fn frame(self, i: usize) -> NSRect {
        let (row, col) = (i / self.cols, i % self.cols);
        rect(
            PAD - INSET + col as CGFloat * (self.side + GAP),
            TOP_PAD - INSET + row as CGFloat * self.pitch(),
            self.side + 2.0 * INSET,
            self.card_h(),
        )
    }

    /// The document's height for `n` cards.
    fn height(self, n: usize) -> CGFloat {
        if n == 0 {
            return 0.0;
        }
        let rows = n.div_ceil(self.cols) as CGFloat;
        TOP_PAD + rows * self.pitch() - ROW_GAP + PAD
    }

    /// The cards, of `n`, whose rows reach the vertical span `lo..hi` of the document, with
    /// a margin.
    fn visible(self, n: usize, lo: CGFloat, hi: CGFloat) -> Range<usize> {
        let first = ((lo - VIEW_MARGIN - TOP_PAD) / self.pitch())
            .floor()
            .max(0.0) as usize;
        let last = (((hi + VIEW_MARGIN - TOP_PAD) / self.pitch())
            .floor()
            .max(-1.0)
            + 1.0) as usize;
        (first * self.cols).min(n)..(last * self.cols).min(n)
    }
}

define_class!(
    /// The grid's document: its origin at the top left, so rows lay out downwards, and it
    /// takes the keyboard.
    // SAFETY: NSView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct GridDoc;

    impl GridDoc {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }
    }
);

pub struct WatcherIvars {
    grid: Weak<Inner>,
}

define_class!(
    /// Hears the grid scroll, so cards come and go with the visible area.
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = WatcherIvars]
    pub struct GridWatcher;

    impl GridWatcher {
        #[unsafe(method(scrolled:))]
        fn scrolled(&self, _notification: &NSNotification) {
            if let Some(grid) = self.ivars().grid.upgrade() {
                grid.sync();
            }
        }
    }
);

struct CardParts {
    cover: Retained<Cover>,
    title: Retained<NSTextField>,
    sub: Retained<NSTextField>,
    badge: Option<Retained<Badge>>,
}

pub struct CardIvars {
    collection: Collection,
    open: Rc<dyn Fn(Collection)>,
    hovered: Cell<bool>,
    pressed: Cell<bool>,
    parts: OnceCell<CardParts>,
}

define_class!(
    /// One playlist or album: a button that opens it, with the hover fill of a shelf card.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CardIvars]
    pub struct GridCard;

    impl GridCard {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.set_hovered(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().pressed.set(false);
            self.set_hovered(false);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.ivars().pressed.set(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let was = self.ivars().pressed.replace(false);
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            let b = self.bounds();
            let inside = at.x >= 0.0 && at.x < b.size.width && at.y >= 0.0 && at.y < b.size.height;
            if was && inside {
                self.activate();
            }
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Clicks on the cover or a label land on the card itself.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let f = self.frame();
            let inside = point.x >= f.origin.x
                && point.x < f.origin.x + f.size.width
                && point.y >= f.origin.y
                && point.y < f.origin.y + f.size.height;
            inside.then(|| Retained::into_super(objc2::Message::retain(self)))
        }

        /// Also called when an ancestor is hidden or the card leaves the page; a hidden view
        /// gets no `mouseExited:`.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            self.set_hovered(false);
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            self.activate();
            true
        }

        /// A plain view reports itself disabled, which VoiceOver reads as dimmed.
        #[unsafe(method(isAccessibilityEnabled))]
        fn accessibility_enabled(&self) -> bool {
            true
        }
    }
);

impl GridCard {
    /// `badge` is the collection's service, in a grid that mixes services.
    fn new(
        mtm: MainThreadMarker,
        layout: Layout,
        index: usize,
        collection: &Collection,
        open: &Rc<dyn Fn(Collection)>,
        badge: Option<Service>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CardIvars {
            collection: collection.clone(),
            open: open.clone(),
            hovered: Cell::new(false),
            pressed: Cell::new(false),
            parts: OnceCell::new(),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: layout.frame(index)] };
        widgets::round(&this, COVER_RADIUS);
        let side = layout.side;
        let cover = widgets::cover(
            mtm,
            rect(INSET, INSET, side, side),
            cover_radius(collection, side),
            32.0,
        );
        cover.set_url(collection.artwork_url.as_deref());
        let title = widgets::label(
            mtm,
            &collection.title,
            &theme::font(13.0, Weight::Medium),
            &theme::ink(),
        );
        let subtitle = collection.subtitle.as_deref().unwrap_or_default();
        let sub = widgets::label(
            mtm,
            subtitle,
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
        );
        this.addSubview(&cover);
        this.addSubview(&title);
        this.addSubview(&sub);
        let mark = badge.map(|service| {
            let b = Badge::new(
                mtm,
                badge::frame(rect(INSET, INSET, side, side), INSET, true),
            );
            b.set(Some(service));
            this.addSubview(&b);
            b
        });
        let _ = this.ivars().parts.set(CardParts {
            cover,
            title,
            sub,
            badge: mark,
        });
        this.place(layout);
        widgets::as_button(&this, &spoken(&collection.title, subtitle, badge));
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this
    }

    /// Fits the card's parts to covers `layout.side` square.
    fn place(&self, layout: Layout) {
        let Some(p) = self.ivars().parts.get() else {
            return;
        };
        let cover = rect(INSET, INSET, layout.side, layout.side);
        p.cover.setFrame(cover);
        widgets::round(
            &p.cover,
            cover_radius(&self.ivars().collection, layout.side),
        );
        let y = INSET + layout.side + 8.0;
        p.title
            .setFrame(rect(INSET, y, layout.side, Layout::title_h()));
        p.sub.setFrame(rect(
            INSET,
            y + Layout::title_h() + 2.0,
            layout.side,
            Layout::sub_h(),
        ));
        if let Some(b) = &p.badge {
            b.setFrame(badge::frame(cover, INSET, true));
        }
    }

    fn set_hovered(&self, hovered: bool) {
        if self.ivars().hovered.replace(hovered) == hovered {
            return;
        }
        let fill = hovered.then(theme::row_hover);
        widgets::fill(self, fill.as_deref());
    }

    /// The keyboard's choice: an accent ring, and selected to VoiceOver.
    fn set_selected(&self, selected: bool) {
        self.setAccessibilitySelected(selected);
        widgets::border(
            self,
            &theme::accent(),
            if selected { SELECTED_W } else { 0.0 },
        );
    }

    fn activate(&self) {
        let iv = self.ivars();
        (iv.open)(iv.collection.clone());
    }

    fn is_loading(&self) -> bool {
        self.ivars()
            .parts
            .get()
            .is_some_and(|p| p.cover.is_loading())
    }
}

struct State {
    items: Vec<Collection>,
    /// Whether the grid mixes services, so its cards show their service's badge.
    mixed: bool,
    layout: Layout,
    cards: HashMap<usize, Retained<GridCard>>,
}

struct Inner {
    mtm: MainThreadMarker,
    view: Retained<NSScrollView>,
    doc: Retained<GridDoc>,
    open: Rc<dyn Fn(Collection)>,
    state: RefCell<State>,
    selected: Cell<Option<usize>>,
}

impl Inner {
    fn width(&self) -> CGFloat {
        self.view.contentSize().width
    }

    /// Creates the cards near the visible area and drops those that left it.
    fn sync(&self) {
        let Ok(mut st) = self.state.try_borrow_mut() else {
            return;
        };
        let vis = self.view.documentVisibleRect();
        let want = st
            .layout
            .visible(st.items.len(), vis.origin.y, vis.origin.y + vis.size.height);
        st.cards.retain(|i, card| {
            let keep = want.contains(i);
            if !keep {
                card.removeFromSuperview();
            }
            keep
        });
        for i in want {
            if st.cards.contains_key(&i) {
                continue;
            }
            let c = &st.items[i];
            let badge = vm::badge(&c.id.provider, st.mixed);
            let card = GridCard::new(self.mtm, st.layout, i, c, &self.open, badge);
            card.set_selected(self.selected.get() == Some(i));
            self.doc.addSubview(&card);
            st.cards.insert(i, card);
        }
    }

    /// Fits the document and the cards to the scroll view's width.
    fn relayout(&self) {
        let layout = Layout::of(self.width());
        let height = {
            let mut st = self.state.borrow_mut();
            st.layout = layout;
            for (i, card) in &st.cards {
                card.setFrame(layout.frame(*i));
                card.place(layout);
            }
            layout.height(st.items.len())
        };
        self.doc.setFrameSize(NSSize::new(self.width(), height));
        self.sync();
    }
}

pub struct CoverGrid {
    pub view: Retained<NSScrollView>,
    inner: Rc<Inner>,
    watcher: Retained<GridWatcher>,
}

impl CoverGrid {
    /// A grid in `frame` of the collections `set` gives it. `open` is called with the
    /// collection of a card that is clicked, or that is selected when the user presses Return.
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        open: impl Fn(Collection) + 'static,
    ) -> CoverGrid {
        let view = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        view.setAutoresizingMask(FLEX_W | FLEX_H);
        view.setHasVerticalScroller(true);
        view.setDrawsBackground(false);
        view.setBorderType(NSBorderType::NoBorder);
        view.contentView().setDrawsBackground(false);
        view.setFocusRingType(NSFocusRingType::None);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let doc: Retained<GridDoc> = unsafe {
            msg_send![GridDoc::alloc(mtm), initWithFrame: rect(0.0, 0.0, frame.size.width, 0.0)]
        };
        doc.setAutoresizingMask(FLEX_W);
        doc.setFocusRingType(NSFocusRingType::None);
        view.setDocumentView(Some(&doc));
        let inner = Rc::new(Inner {
            mtm,
            view: view.clone(),
            doc,
            open: Rc::new(open),
            state: RefCell::new(State {
                items: Vec::new(),
                mixed: false,
                layout: Layout::of(frame.size.width),
                cards: HashMap::new(),
            }),
            selected: Cell::new(None),
        });
        let watcher = GridWatcher::alloc(mtm).set_ivars(WatcherIvars {
            grid: Rc::downgrade(&inner),
        });
        // SAFETY: NSObject's `init`.
        let watcher: Retained<GridWatcher> = unsafe { msg_send![super(watcher), init] };
        let clip = view.contentView();
        clip.setPostsBoundsChangedNotifications(true);
        // SAFETY: the watcher implements the selector; it is kept as long as the grid, and
        // removed from the centre when the grid is dropped.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &watcher,
                sel!(scrolled:),
                Some(NSViewBoundsDidChangeNotification),
                Some(&clip),
            );
        }
        CoverGrid {
            view,
            inner,
            watcher,
        }
    }

    /// Lists `collections`, with their services' badges when `mixed`. With `keep_scroll`, the
    /// grid stays scrolled where it was, as far as the new list reaches; otherwise it starts at
    /// its top. Nothing is selected.
    pub fn set(&self, collections: &[Collection], mixed: bool, keep_scroll: bool) {
        let g = &self.inner;
        let y = g.view.documentVisibleRect().origin.y;
        self.select(None);
        {
            let mut st = g.state.borrow_mut();
            for (_, card) in st.cards.drain() {
                card.removeFromSuperview();
            }
            st.items = collections.to_vec();
            st.mixed = mixed;
        }
        g.relayout();
        let room = g.doc.frame().size.height - g.view.contentSize().height;
        let top = NSPoint::new(
            0.0,
            if keep_scroll {
                y.min(room).max(0.0)
            } else {
                0.0
            },
        );
        g.view.contentView().scrollToPoint(top);
        g.view.reflectScrolledClipView(&g.view.contentView());
        g.sync();
    }

    /// Fits the grid to its width, after the window resized.
    pub fn relayout(&self) {
        self.inner.relayout();
    }

    /// A card's cover has not arrived.
    pub fn covers_loading(&self) -> bool {
        self.inner
            .state
            .borrow()
            .cards
            .values()
            .any(|card| card.is_loading())
    }

    /// Leaves `inset` below the last row, so it can scroll clear of an overlay.
    pub fn set_bottom_inset(&self, inset: CGFloat) {
        self.view.setAutomaticallyAdjustsContentInsets(false);
        self.view.setContentInsets(NSEdgeInsets {
            top: 0.0,
            left: 0.0,
            bottom: inset,
            right: 0.0,
        });
    }

    /// The view that takes the keyboard: the arrow keys move the selection.
    pub fn responder(&self) -> &NSView {
        &self.inner.doc
    }

    /// The card selected with the keyboard, as an index into the last list set.
    pub fn selected(&self) -> Option<usize> {
        self.inner.selected.get()
    }

    /// Selects card `index`, scrolled into view; `None` clears the selection.
    pub fn select(&self, index: Option<usize>) {
        let g = &self.inner;
        let old = g.selected.replace(index);
        let card = |st: &State, i: Option<usize>| i.and_then(|i| st.cards.get(&i).cloned());
        if let Some(card) = card(&g.state.borrow(), old) {
            card.set_selected(false);
        }
        let Some(i) = index else { return };
        let frame = g.state.borrow().layout.frame(i);
        g.doc.scrollRectToVisible(frame);
        g.sync();
        if let Some(card) = card(&g.state.borrow(), Some(i)) {
            card.set_selected(true);
        }
    }

    /// Moves the selection `cols` columns and `rows` rows, from the first card when nothing is
    /// selected. A move off the list stays where it was, except that down from a row above
    /// the last goes to the last card.
    pub fn move_selection(&self, cols: isize, rows: isize) {
        let (n, per) = {
            let st = self.inner.state.borrow();
            (st.items.len(), st.layout.cols as isize)
        };
        if n == 0 {
            return;
        }
        let next = match self.inner.selected.get() {
            None => 0,
            Some(i) => {
                let to = i as isize + cols + rows * per;
                if (0..n as isize).contains(&to) {
                    to as usize
                } else if rows > 0 && (i as isize) / per < (n as isize - 1) / per {
                    n - 1
                } else {
                    i
                }
            }
        };
        self.select(Some(next));
    }

    /// The collection selected with the keyboard.
    pub fn selected_collection(&self) -> Option<Collection> {
        let st = self.inner.state.borrow();
        self.inner
            .selected
            .get()
            .and_then(|i| st.items.get(i))
            .cloned()
    }

    /// Opens the selected card, as a click does.
    pub fn open_selected(&self) {
        if let Some(c) = self.selected_collection() {
            (self.inner.open)(c);
        }
    }
}

impl Drop for CoverGrid {
    fn drop(&mut self) {
        // SAFETY: removing an observer that was added is allowed.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.watcher) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fill_the_width_with_cards_a_little_larger_than_homes() {
        // 860 pt, the window at its default: room for 5 of 128 pt, grown to fill the width.
        let l = Layout::of(860.0);
        assert_eq!(l.cols, 5);
        assert_eq!(l.side, 149.0);
        let used = 2.0 * PAD + l.cols as CGFloat * l.side + (l.cols - 1) as CGFloat * GAP;
        assert!((860.0 - used).abs() < l.cols as CGFloat);
        for width in [300.0, 520.0, 860.0, 1400.0, 2400.0] {
            let l = Layout::of(width);
            assert!(l.cols >= 1);
            assert!((CARD_W..CARD_W + GAP + CARD_W).contains(&l.side) || l.cols == 1);
        }
    }

    #[test]
    fn a_narrow_window_still_has_a_column() {
        assert_eq!(Layout::of(100.0).cols, 1);
    }

    #[test]
    fn cards_sit_in_rows_of_the_columns_and_the_document_ends_after_the_last_row() {
        let l = Layout::of(860.0);
        let (a, b, c) = (l.frame(0), l.frame(1), l.frame(5));
        assert_eq!(a.origin.y, b.origin.y);
        assert!(b.origin.x > a.origin.x + l.side);
        assert_eq!(c.origin.x, a.origin.x);
        assert_eq!(c.origin.y, a.origin.y + l.pitch());
        assert_eq!(l.height(0), 0.0);
        assert_eq!(l.height(5), l.height(1));
        assert_eq!(l.height(6), l.height(1) + l.pitch());
    }

    #[test]
    fn only_the_rows_near_the_visible_span_have_cards() {
        let l = Layout::of(860.0);
        assert_eq!(l.visible(0, 0.0, 500.0), 0..0);
        assert_eq!(l.visible(3, 0.0, 500.0), 0..3);
        let n = 200;
        let top = l.visible(n, 0.0, 500.0);
        assert_eq!(top.start, 0);
        assert!(top.end >= l.cols && top.end < 40);
        let down = l.visible(n, 4000.0, 4500.0);
        assert!(down.start > 0 && down.start.is_multiple_of(l.cols));
        assert!(down.contains(&((4200.0 / l.pitch()) as usize * l.cols)));
        // Past the end, nothing.
        assert_eq!(l.visible(n, 1.0e6, 1.0e6 + 500.0), n..n);
    }
}
