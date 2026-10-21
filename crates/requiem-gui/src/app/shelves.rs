//! Home: shelves of artwork cards, one vertical scroll of horizontal rows.
//!
//! A page can hold a hundred cards, and a card with its cover costs a quarter-megabyte of
//! decoded artwork, so a card exists only while its place is near the visible area.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSBorderType, NSControl, NSEvent, NSEventPhase, NSFocusRingType,
    NSLineBreakMode, NSMenu, NSMenuItem, NSResponder, NSScrollView, NSTextField,
    NSTrackingAreaOptions, NSView, NSViewBoundsDidChangeNotification,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSEdgeInsets, NSNotification, NSNotificationCenter, NSPoint, NSRect, NSSize,
    NSString,
};
use requiem_core::{Item, Shelf};
use requiem_macos::artwork::Cover;

use super::badge::{self, Badge};
use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{self, FLEX_W, HoverButton, PIN_RIGHT, rect};
use crate::vm::{self, Service};

const PAD: CGFloat = 24.0;
const TOP_PAD: CGFloat = 8.0;
const CARD_W: CGFloat = 128.0;
const CARD_GAP: CGFloat = 16.0;
/// A card's hover fill reaches this far past its content, on every side.
const INSET: CGFloat = 6.0;
const COVER_RADIUS: CGFloat = 8.0;
const SHELF_TITLE_H: CGFloat = 24.0;
const SHELF_TITLE_GAP: CGFloat = 10.0;
const SHELF_GAP: CGFloat = 28.0;
const GRID_COL_W: CGFloat = 300.0;
const GRID_ROWS: usize = 4;
const GRID_ROW_H: CGFloat = 56.0;
const GRID_COVER: CGFloat = 44.0;
/// How far past the visible area cards are kept: a row's height above and below, and half a
/// card to each side.
const VIEW_MARGIN: CGFloat = 120.0;
const SIDE_MARGIN: CGFloat = 72.0;
/// A paging button's side, its chevron's size, and the space between the two buttons.
const PAGE_BUTTON: CGFloat = 28.0;
const PAGE_GLYPH: CGFloat = 16.0;
const PAGE_GAP: CGFloat = 4.0;

define_class!(
    /// A view with its origin at the top left, so rows and cards lay out downwards.
    // SAFETY: NSView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

define_class!(
    /// Hears the scroll views scroll, so cards come and go with the visible area, and the
    /// paging buttons, whose tag is their row's place in `Shelves::rows`.
    // SAFETY: NSObject has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct ScrollWatcher;

    impl ScrollWatcher {
        #[unsafe(method(scrolled:))]
        fn scrolled(&self, _notification: &NSNotification) {
            ui::with(|ui| ui.shelves.sync());
        }

        #[unsafe(method(pageBack:))]
        fn page_back(&self, sender: Option<&NSControl>) {
            if let Some(b) = sender {
                ui::with(|ui| ui.shelves.page(b.tag() as usize, false));
            }
        }

        #[unsafe(method(pageForward:))]
        fn page_forward(&self, sender: Option<&NSControl>) {
            if let Some(b) = sender {
                ui::with(|ui| ui.shelves.page(b.tag() as usize, true));
            }
        }
    }
);

/// Which way a scroll gesture moves: along the shelf, or down the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// Who gets a scroll-wheel event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Shelf,
    Page,
    /// A touch that has not picked a direction yet: both views see it.
    Both,
}

fn dominant(dx: CGFloat, dy: CGFloat) -> Axis {
    if dy.abs() > dx.abs() {
        Axis::Vertical
    } else {
        Axis::Horizontal
    }
}

fn route_of(axis: Axis) -> Route {
    match axis {
        Axis::Vertical => Route::Page,
        Axis::Horizontal => Route::Shelf,
    }
}

/// Routes one scroll event by the larger of its deltas. A trackpad gesture and its momentum
/// keep the axis their first moving event chose, so they cannot flip between page and shelf
/// part-way; `locked` is that axis, and the second value is what to keep for the next event.
/// A mouse wheel has no phases and is decided event by event.
fn scroll_axis(
    phase: NSEventPhase,
    momentum: NSEventPhase,
    dx: CGFloat,
    dy: CGFloat,
    locked: Option<Axis>,
) -> (Route, Option<Axis>) {
    if phase.is_empty() && momentum.is_empty() {
        return (route_of(dominant(dx, dy)), None);
    }
    if phase.contains(NSEventPhase::MayBegin) {
        return (Route::Both, None);
    }
    let held = if phase.contains(NSEventPhase::Began) {
        None
    } else {
        locked
    };
    let axis = held.or_else(|| (dx != 0.0 || dy != 0.0).then(|| dominant(dx, dy)));
    let route = axis.map_or(Route::Both, route_of);
    let over = NSEventPhase::Ended | NSEventPhase::Cancelled;
    let done = phase.intersects(over) || momentum.intersects(over);
    (route, if done { None } else { axis })
}

pub struct RowScrollIvars {
    locked: Cell<Option<Axis>>,
}

define_class!(
    /// A shelf's scroll view. It scrolls sideways only; a mostly vertical gesture goes on to
    /// the page behind it, which a plain scroll view would swallow.
    // SAFETY: NSScrollView has no subclassing requirements for `scrollWheel:`; no Drop impl.
    #[unsafe(super(NSScrollView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RowScrollIvars]
    pub struct RowScroll;

    impl RowScroll {
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let iv = self.ivars();
            let (route, locked) = scroll_axis(
                event.phase(),
                event.momentumPhase(),
                event.scrollingDeltaX(),
                event.scrollingDeltaY(),
                iv.locked.get(),
            );
            // Settled before either call: both re-enter through bounds notifications.
            iv.locked.set(locked);
            if route != Route::Page {
                // SAFETY: `scrollWheel:` is NSScrollView's, called on its superclass.
                let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
            }
            if route != Route::Shelf {
                // SAFETY: the next responder is a live responder or nil.
                if let Some(next) = unsafe { self.nextResponder() } {
                    next.scrollWheel(event);
                }
            }
        }
    }
);

impl RowScroll {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RowScrollIvars {
            locked: Cell::new(None),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct CardIvars {
    shelf: usize,
    index: usize,
    /// Whether the card is a track, which plays and queues; the rest open.
    track: bool,
    hovered: Cell<bool>,
    pressed: Cell<bool>,
}

define_class!(
    /// One item: a button that plays a track or opens a playlist or album, with the hover
    /// fill of a table row.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CardIvars]
    pub struct ShelfCard;

    impl ShelfCard {
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

        /// Tracks offer Play and Add to Queue.
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, _event: &NSEvent) -> Option<Retained<NSMenu>> {
            self.ivars().track.then(|| self.track_menu())
        }

        #[unsafe(method(cardPlay:))]
        fn card_play(&self, _sender: Option<&AnyObject>) {
            self.activate();
        }

        #[unsafe(method(cardAdd:))]
        fn card_add(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            ui::with(|ui| ui.card_add(iv.shelf, iv.index));
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, _item: &NSMenuItem) -> bool {
            ui::with(|ui| ui.playback().1.player).unwrap_or(false)
        }
    }
);

impl ShelfCard {
    fn track_menu(&self) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::new());
        for (title, action) in [("Play", sel!(cardPlay:)), ("Add to Queue", sel!(cardAdd:))] {
            // SAFETY: the card implements both actions and outlives its menu's use: the menu
            // is shown, and dropped, within the right-click that asked for it.
            let item = unsafe {
                let i = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::new(),
                );
                i.setTarget(Some(self));
                i
            };
            menu.addItem(&item);
        }
        menu
    }

    /// `badge` is the item's service, on a page that mixes services.
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        shelf: usize,
        index: usize,
        item: &Item,
        compact: bool,
        badge: Option<Service>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CardIvars {
            shelf,
            index,
            track: matches!(item, Item::Track(_)),
            hovered: Cell::new(false),
            pressed: Cell::new(false),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        widgets::round(&this, COVER_RADIUS);
        let (title, subtitle, art) = parts(item);
        if compact {
            this.fill_compact(mtm, frame.size.width, title, &subtitle, art);
        } else {
            this.fill_card(mtm, title, &subtitle, art);
        }
        if badge.is_some() {
            let (cover, room) = if compact {
                let pad = (GRID_ROW_H - GRID_COVER) / 2.0;
                (rect(pad, pad, GRID_COVER, GRID_COVER), pad)
            } else {
                (rect(INSET, INSET, CARD_W, CARD_W), INSET)
            };
            let b = Badge::new(mtm, badge::frame(cover, room, true));
            b.set(badge);
            this.addSubview(&b);
        }
        widgets::as_button(&this, &spoken(title, &subtitle, badge));
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this
    }

    /// A 128 pt cover, a title of up to two lines, and a muted subtitle.
    fn fill_card(&self, mtm: MainThreadMarker, title: &str, subtitle: &str, art: Option<&str>) {
        let cover = widgets::cover(mtm, rect(INSET, INSET, CARD_W, CARD_W), COVER_RADIUS, 32.0);
        cover.set_url(art);
        self.addSubview(&cover);
        let title_font = theme::font(13.0, Weight::Medium);
        let lines = widgets::label(mtm, title, &title_font, &theme::ink());
        lines.setUsesSingleLineMode(false);
        lines.setMaximumNumberOfLines(2);
        lines.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        if let Some(cell) = lines.cell() {
            cell.setTruncatesLastVisibleLine(true);
        }
        let max_h = 2.0 * widgets::line_height(&title_font);
        let fits = lines.cell().map_or(max_h, |c| {
            c.cellSizeForBounds(rect(0.0, 0.0, CARD_W, 1000.0)).height
        });
        let title_h = fits.ceil().min(max_h);
        let y = INSET + CARD_W + 8.0;
        lines.setFrame(rect(INSET, y, CARD_W, title_h));
        self.addSubview(&lines);
        let sub_font = theme::font(12.0, Weight::Regular);
        let sub = widgets::label(mtm, subtitle, &sub_font, &theme::muted());
        sub.setFrame(rect(
            INSET,
            y + title_h + 2.0,
            CARD_W,
            widgets::line_height(&sub_font),
        ));
        self.addSubview(&sub);
    }

    /// A 44 pt cover with the title, and the artists in muted text below it.
    fn fill_compact(
        &self,
        mtm: MainThreadMarker,
        width: CGFloat,
        title: &str,
        subtitle: &str,
        art: Option<&str>,
    ) {
        let pad = (GRID_ROW_H - GRID_COVER) / 2.0;
        let cover = widgets::cover(mtm, rect(pad, pad, GRID_COVER, GRID_COVER), 4.0, 16.0);
        cover.set_url(art);
        self.addSubview(&cover);
        let x = pad + GRID_COVER + 12.0;
        let title_font = theme::font(14.0, Weight::Medium);
        let sub_font = theme::font(12.0, Weight::Regular);
        let (th, sh) = (
            widgets::line_height(&title_font),
            widgets::line_height(&sub_font),
        );
        let y = ((GRID_ROW_H - th - sh) / 2.0).round();
        let w = width - x - 8.0;
        let t = widgets::label(mtm, title, &title_font, &theme::ink());
        t.setFrame(rect(x, y, w, th));
        self.addSubview(&t);
        let s = widgets::label(mtm, subtitle, &sub_font, &theme::muted());
        s.setFrame(rect(x, y + th, w, sh));
        self.addSubview(&s);
    }

    fn set_hovered(&self, hovered: bool) {
        if self.ivars().hovered.replace(hovered) == hovered {
            return;
        }
        let fill = hovered.then(theme::row_hover);
        widgets::fill(self, fill.as_deref());
    }

    fn activate(&self) {
        let iv = self.ivars();
        ui::with(|ui| ui.card_clicked(iv.shelf, iv.index));
    }
}

/// A card's title, subtitle and artwork. A track's subtitle is its artists.
fn parts(item: &Item) -> (&str, String, Option<&str>) {
    match item {
        Item::Track(t) => (&t.title, vm::artists(t), t.artwork_url.as_deref()),
        Item::Collection(c) => (
            &c.title,
            c.subtitle.clone().unwrap_or_default(),
            c.artwork_url.as_deref(),
        ),
    }
}

/// What VoiceOver reads for a card: "title, subtitle", then the service its badge shows.
fn spoken(title: &str, subtitle: &str, badge: Option<Service>) -> String {
    [Some(title), Some(subtitle), badge.map(Service::name)]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a shelf is a grid of tracks, YouTube Music's compact "Quick picks" layout.
fn is_grid(shelf: &Shelf) -> bool {
    !shelf.items.is_empty() && shelf.items.iter().all(|i| matches!(i, Item::Track(_)))
}

/// The items whose cards cover the horizontal span `lo..hi` of a row's document, as a range
/// of item indexes.
fn visible_items(n: usize, grid: bool, lo: CGFloat, hi: CGFloat) -> std::ops::Range<usize> {
    let (first, pitch, per) = if grid {
        (PAD, GRID_COL_W + CARD_GAP, GRID_ROWS)
    } else {
        (PAD - INSET, CARD_W + CARD_GAP, 1)
    };
    let cols = n.div_ceil(per);
    let first_col = ((lo - SIDE_MARGIN - first) / pitch).floor().max(0.0) as usize;
    let last_col = (((hi + SIDE_MARGIN - first) / pitch).floor().max(-1.0) + 1.0) as usize;
    let last_col = last_col.min(cols);
    if first_col >= last_col {
        return 0..0;
    }
    (first_col * per).min(n)..(last_col * per).min(n)
}

/// Whether a row's back and forward buttons can act at scroll offset `offset`; `None`, which
/// hides them, when the row's `content` fits its `visible` width.
fn page_buttons(content: CGFloat, visible: CGFloat, offset: CGFloat) -> Option<(bool, bool)> {
    let max = content - visible;
    // Half a point of slack: a trackpad leaves fractional offsets.
    (max > 0.5).then_some((offset > 0.5, offset < max - 0.5))
}

/// The offset a page back or forward from `offset`: the visible width rounded down to whole
/// columns of `pitch`, at least one, kept within the row.
fn page_offset(
    content: CGFloat,
    visible: CGFloat,
    pitch: CGFloat,
    offset: CGFloat,
    forward: bool,
) -> CGFloat {
    let step = (visible / pitch).floor().max(1.0) * pitch;
    let to = if forward {
        offset + step
    } else {
        offset - step
    };
    to.clamp(0.0, (content - visible).max(0.0))
}

struct ShelfRow {
    /// The shelf's index in the page's list, which `Intent`s name.
    shelf: usize,
    title: Retained<NSTextField>,
    scroll: Retained<RowScroll>,
    doc: Retained<FlippedView>,
    back: Retained<HoverButton>,
    forward: Retained<HoverButton>,
    items: Vec<Item>,
    grid: bool,
    /// Whether the page mixes services, so its cards show their service's badge.
    mixed: bool,
    /// Top and bottom of the row, title included, in the page's document.
    top: CGFloat,
    bottom: CGFloat,
    cards: RefCell<HashMap<usize, Retained<ShelfCard>>>,
}

impl ShelfRow {
    fn clear(&self) {
        for (_, card) in self.cards.borrow_mut().drain() {
            card.removeFromSuperview();
        }
    }

    /// One column: a card, or a grid column, and the gap after it.
    fn pitch(&self) -> CGFloat {
        if self.grid {
            GRID_COL_W + CARD_GAP
        } else {
            CARD_W + CARD_GAP
        }
    }

    fn update_paging(&self) {
        let clip = self.scroll.contentView().bounds();
        let state = page_buttons(self.doc.frame().size.width, clip.size.width, clip.origin.x);
        let (back, forward) = state.unwrap_or_default();
        for (button, enabled) in [(&self.back, back), (&self.forward, forward)] {
            button.set_shown(state.is_some());
            button.set_enabled(enabled);
        }
    }

    /// Keeps the cards of `visible_items` and drops the rest.
    fn fill(&self, mtm: MainThreadMarker, lo: CGFloat, hi: CGFloat) {
        let want = visible_items(self.items.len(), self.grid, lo, hi);
        let mut cards = self.cards.borrow_mut();
        cards.retain(|i, card| {
            let keep = want.contains(i);
            if !keep {
                card.removeFromSuperview();
            }
            keep
        });
        for i in want {
            if cards.contains_key(&i) {
                continue;
            }
            let frame = if self.grid {
                let (col, row) = (i / GRID_ROWS, i % GRID_ROWS);
                rect(
                    PAD + col as CGFloat * (GRID_COL_W + CARD_GAP),
                    row as CGFloat * GRID_ROW_H,
                    GRID_COL_W,
                    GRID_ROW_H,
                )
            } else {
                rect(
                    PAD - INSET + i as CGFloat * (CARD_W + CARD_GAP),
                    0.0,
                    CARD_W + 2.0 * INSET,
                    card_h(),
                )
            };
            let item = &self.items[i];
            let badge = vm::badge(vm::item_provider(item), self.mixed);
            let card = ShelfCard::new(mtm, frame, self.shelf, i, item, self.grid, badge);
            self.doc.addSubview(&card);
            cards.insert(i, card);
        }
    }
}

/// A card's height: cover, up to two title lines, one subtitle line, and the hover inset.
fn card_h() -> CGFloat {
    2.0 * INSET
        + CARD_W
        + 8.0
        + 2.0 * widgets::line_height(&theme::font(13.0, Weight::Medium))
        + 2.0
        + widgets::line_height(&theme::font(12.0, Weight::Regular))
}

pub struct Shelves {
    mtm: MainThreadMarker,
    pub view: Retained<NSScrollView>,
    doc: Retained<FlippedView>,
    rows: RefCell<Vec<ShelfRow>>,
    watcher: Retained<ScrollWatcher>,
}

impl Shelves {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Shelves {
        let view = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        view.setAutoresizingMask(widgets::FLEX_W | widgets::FLEX_H);
        view.setHasVerticalScroller(true);
        view.setDrawsBackground(false);
        view.setBorderType(NSBorderType::NoBorder);
        view.contentView().setDrawsBackground(false);
        view.setFocusRingType(NSFocusRingType::None);
        let doc: Retained<FlippedView> = {
            // SAFETY: `initWithFrame:` is NSView's designated initializer.
            let v: Retained<FlippedView> = unsafe {
                msg_send![FlippedView::alloc(mtm), initWithFrame: rect(0.0, 0.0, frame.size.width, 0.0)]
            };
            v.setAutoresizingMask(FLEX_W);
            v
        };
        view.setDocumentView(Some(&doc));
        // SAFETY: NSObject's `init`.
        let watcher: Retained<ScrollWatcher> =
            unsafe { msg_send![ScrollWatcher::alloc(mtm), init] };
        Shelves {
            mtm,
            view,
            doc,
            rows: RefCell::new(Vec::new()),
            watcher,
        }
    }

    /// Replaces the page's shelves; shelves with nothing in them are left out.
    pub fn set_shelves(&self, shelves: &[Shelf]) {
        let centre = NSNotificationCenter::defaultCenter();
        // SAFETY: the watcher is an object; removing observers it never added is allowed.
        unsafe { centre.removeObserver(&self.watcher) };
        for row in self.rows.borrow_mut().drain(..) {
            row.clear();
            row.title.removeFromSuperview();
            row.back.removeFromSuperview();
            row.forward.removeFromSuperview();
            row.scroll.removeFromSuperview();
        }
        let width = self.view.contentSize().width;
        let title_font = theme::font(17.0, Weight::SemiBold);
        let mut y = TOP_PAD;
        let mut rows = Vec::new();
        let mixed = vm::mixed(
            shelves
                .iter()
                .flat_map(|s| s.items.iter().map(vm::item_provider)),
        );
        for (si, shelf) in shelves.iter().enumerate() {
            if shelf.items.is_empty() {
                continue;
            }
            let grid = is_grid(shelf);
            let top = y;
            let title = widgets::label(self.mtm, &shelf.title, &title_font, &theme::ink());
            let buttons_w = 2.0 * PAGE_BUTTON + PAGE_GAP;
            title.setFrame(rect(
                PAD,
                y,
                width - 2.0 * PAD - buttons_w - PAGE_GAP,
                SHELF_TITLE_H,
            ));
            title.setAutoresizingMask(FLEX_W);
            self.doc.addSubview(&title);
            let button_y = y + (SHELF_TITLE_H - PAGE_BUTTON) / 2.0;
            let back = self.page_button(
                Icon::ChevronLeft,
                rect(width - PAD - buttons_w, button_y, PAGE_BUTTON, PAGE_BUTTON),
                rows.len(),
                sel!(pageBack:),
                &format!("Show previous in {}", shelf.title),
            );
            let forward = self.page_button(
                Icon::ChevronRight,
                rect(
                    width - PAD - PAGE_BUTTON,
                    button_y,
                    PAGE_BUTTON,
                    PAGE_BUTTON,
                ),
                rows.len(),
                sel!(pageForward:),
                &format!("Show more in {}", shelf.title),
            );
            y += SHELF_TITLE_H + SHELF_TITLE_GAP;
            let (row_h, content_w) = if grid {
                let cols = shelf.items.len().div_ceil(GRID_ROWS) as CGFloat;
                (
                    GRID_ROWS as CGFloat * GRID_ROW_H,
                    2.0 * PAD + cols * GRID_COL_W + (cols - 1.0) * CARD_GAP,
                )
            } else {
                let n = shelf.items.len() as CGFloat;
                (card_h(), 2.0 * PAD + n * CARD_W + (n - 1.0) * CARD_GAP)
            };
            let scroll = RowScroll::new(self.mtm, rect(0.0, y, width, row_h));
            scroll.setAutoresizingMask(FLEX_W);
            scroll.setHasHorizontalScroller(true);
            scroll.setHasVerticalScroller(false);
            scroll.setAutohidesScrollers(true);
            scroll.setDrawsBackground(false);
            scroll.setBorderType(NSBorderType::NoBorder);
            scroll.contentView().setDrawsBackground(false);
            scroll.setFocusRingType(NSFocusRingType::None);
            // SAFETY: `initWithFrame:` is NSView's designated initializer.
            let doc: Retained<FlippedView> = unsafe {
                msg_send![FlippedView::alloc(self.mtm), initWithFrame: rect(0.0, 0.0, content_w, row_h)]
            };
            scroll.setDocumentView(Some(&doc));
            self.doc.addSubview(&scroll);
            let clip = scroll.contentView();
            clip.setPostsBoundsChangedNotifications(true);
            observe(&self.watcher, &clip);
            y += row_h;
            rows.push(ShelfRow {
                shelf: si,
                title,
                scroll,
                doc,
                back,
                forward,
                items: shelf.items.clone(),
                grid,
                mixed,
                top,
                bottom: y,
                cards: RefCell::new(HashMap::new()),
            });
            y += SHELF_GAP;
        }
        self.doc
            .setFrameSize(NSSize::new(width, (y - SHELF_GAP + TOP_PAD).max(0.0)));
        let outer = self.view.contentView();
        outer.setPostsBoundsChangedNotifications(true);
        observe(&self.watcher, &outer);
        *self.rows.borrow_mut() = rows;
        self.view.contentView().scrollToPoint(NSPoint::ZERO);
        self.view.reflectScrolledClipView(&self.view.contentView());
        self.sync();
    }

    /// Creates the cards near the visible area and drops those that left it.
    pub fn sync(&self) {
        let vis = self.view.documentVisibleRect();
        for row in self.rows.borrow().iter() {
            let near = row.bottom > vis.origin.y - VIEW_MARGIN
                && row.top < vis.origin.y + vis.size.height + VIEW_MARGIN;
            if near {
                let inner = row.scroll.documentVisibleRect();
                row.fill(self.mtm, inner.origin.x, inner.origin.x + inner.size.width);
            } else {
                row.clear();
            }
            row.update_paging();
        }
    }

    /// Updates the paging buttons for the rows' widths, after the window resized.
    pub fn update_paging(&self) {
        for row in self.rows.borrow().iter() {
            row.update_paging();
        }
    }

    /// The place in the page of the first row of cards, not a grid.
    pub fn first_card_row(&self) -> Option<usize> {
        self.rows.borrow().iter().position(|r| !r.grid)
    }

    /// Scrolls row `row` a page back or forward.
    pub fn page(&self, row: usize, forward: bool) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(row) else { return };
        let clip = row.scroll.contentView();
        let b = clip.bounds();
        let x = page_offset(
            row.doc.frame().size.width,
            b.size.width,
            row.pitch(),
            b.origin.x,
            forward,
        );
        let to = NSPoint::new(x, b.origin.y);
        let duration = if widgets::reduce_motion() {
            Duration::ZERO
        } else {
            widgets::STANDARD
        };
        let scroll = row.scroll.clone();
        widgets::animate(
            &clip,
            duration,
            widgets::EASE_OUT,
            || clip.animator().setBoundsOrigin(to),
            move || scroll.reflectScrolledClipView(&scroll.contentView()),
        );
    }

    fn page_button(
        &self,
        icon: Icon,
        frame: NSRect,
        row: usize,
        action: Sel,
        label: &str,
    ) -> Retained<HoverButton> {
        let b = HoverButton::new(
            self.mtm,
            frame,
            icon,
            PAGE_GLYPH,
            &theme::muted(),
            None,
            &theme::bg_3(),
            theme::RADIUS,
        );
        b.setAutoresizingMask(PIN_RIGHT);
        b.setTag(row as isize);
        b.set_action(self.watcher.as_ref(), action);
        widgets::accessibility_label(&b, label);
        b.set_shown(false);
        self.doc.addSubview(&b);
        b
    }

    /// Scrolls the page down to its last shelf.
    pub fn scroll_to_end(&self) {
        let clip = self.view.contentView();
        let size = clip.bounds().size;
        let bottom = self.doc.frame().size.height;
        let end = clip.constrainBoundsRect(rect(0.0, bottom, size.width, size.height));
        clip.scrollToPoint(end.origin);
        self.view.reflectScrolledClipView(&clip);
    }

    /// Item `index` of the page's shelf `shelf`.
    pub fn item(&self, shelf: usize, index: usize) -> Option<Item> {
        let rows = self.rows.borrow();
        let row = rows.iter().find(|r| r.shelf == shelf)?;
        row.items.get(index).cloned()
    }

    /// A card's cover has not arrived.
    pub fn covers_loading(&self) -> bool {
        self.rows.borrow().iter().any(|row| {
            row.cards.borrow().values().any(|card| {
                card.subviews()
                    .iter()
                    .any(|v| v.downcast_ref::<Cover>().is_some_and(Cover::is_loading))
            })
        })
    }

    /// Leaves `inset` below the last shelf, so it can scroll clear of an overlay.
    pub fn set_bottom_inset(&self, inset: CGFloat) {
        self.view.setAutomaticallyAdjustsContentInsets(false);
        self.view.setContentInsets(NSEdgeInsets {
            top: 0.0,
            left: 0.0,
            bottom: inset,
            right: 0.0,
        });
    }
}

fn observe(watcher: &ScrollWatcher, clip: &NSView) {
    // SAFETY: the watcher implements the selector and lives as long as the page, in the
    // installed `Ui`, until exit; the observation ends with `removeObserver`.
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            watcher,
            sel!(scrolled:),
            Some(NSViewBoundsDidChangeNotification),
            Some(clip),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gesture(
        phase: NSEventPhase,
        momentum: NSEventPhase,
        dx: CGFloat,
        dy: CGFloat,
        locked: &mut Option<Axis>,
    ) -> Route {
        let (route, next) = scroll_axis(phase, momentum, dx, dy, *locked);
        *locked = next;
        route
    }

    #[test]
    fn a_mouse_wheel_goes_by_its_larger_delta() {
        let none = NSEventPhase::None;
        assert_eq!(scroll_axis(none, none, 0.0, -3.0, None).0, Route::Page);
        assert_eq!(scroll_axis(none, none, 3.0, 0.0, None).0, Route::Shelf);
        assert_eq!(scroll_axis(none, none, 2.0, -3.0, None).0, Route::Page);
        assert_eq!(scroll_axis(none, none, -4.0, 3.0, None).0, Route::Shelf);
        // A stale lock does not hold a wheel.
        let stale = Some(Axis::Horizontal);
        assert_eq!(
            scroll_axis(none, none, 0.0, 3.0, stale),
            (Route::Page, None)
        );
    }

    #[test]
    fn a_gesture_keeps_the_axis_it_started_on() {
        let (none, began, changed) = (
            NSEventPhase::None,
            NSEventPhase::Began,
            NSEventPhase::Changed,
        );
        let mut lock = None;
        assert_eq!(gesture(began, none, 1.0, -5.0, &mut lock), Route::Page);
        assert_eq!(lock, Some(Axis::Vertical));
        // One event with a larger dx stays on the page, as does the momentum after.
        assert_eq!(gesture(changed, none, 9.0, -2.0, &mut lock), Route::Page);
        assert_eq!(gesture(none, changed, 8.0, -1.0, &mut lock), Route::Page);
        assert_eq!(lock, Some(Axis::Vertical));

        let mut lock = None;
        assert_eq!(gesture(began, none, 5.0, -1.0, &mut lock), Route::Shelf);
        assert_eq!(gesture(changed, none, 1.0, -9.0, &mut lock), Route::Shelf);
    }

    #[test]
    fn the_lock_clears_when_a_gesture_or_its_momentum_ends() {
        let (none, changed, began) = (
            NSEventPhase::None,
            NSEventPhase::Changed,
            NSEventPhase::Began,
        );
        for end in [NSEventPhase::Ended, NSEventPhase::Cancelled] {
            let mut lock = Some(Axis::Vertical);
            assert_eq!(gesture(end, none, 0.0, 0.0, &mut lock), Route::Page);
            assert_eq!(lock, None);
            let mut lock = Some(Axis::Vertical);
            assert_eq!(gesture(none, end, 0.0, 0.0, &mut lock), Route::Page);
            assert_eq!(lock, None);
        }
        // Momentum after a cleared gesture picks its axis from its own first delta.
        let mut lock = None;
        assert_eq!(gesture(none, changed, 0.0, -4.0, &mut lock), Route::Page);
        assert_eq!(lock, Some(Axis::Vertical));
        // A new gesture starts afresh even over an old lock.
        let mut lock = Some(Axis::Vertical);
        assert_eq!(gesture(began, none, 4.0, 0.0, &mut lock), Route::Shelf);
        assert_eq!(lock, Some(Axis::Horizontal));
    }

    #[test]
    fn a_touch_with_no_movement_reaches_both_and_locks_nothing() {
        let none = NSEventPhase::None;
        let mut lock = None;
        let may = NSEventPhase::MayBegin;
        assert_eq!(gesture(may, none, 0.0, 0.0, &mut lock), Route::Both);
        assert_eq!(lock, None);
        let began = NSEventPhase::Began;
        assert_eq!(gesture(began, none, 0.0, 0.0, &mut lock), Route::Both);
        assert_eq!(lock, None);
        let changed = NSEventPhase::Changed;
        assert_eq!(gesture(changed, none, 0.0, -2.0, &mut lock), Route::Page);
        assert_eq!(lock, Some(Axis::Vertical));
    }

    #[test]
    fn cards_exist_only_near_the_visible_span() {
        // Cards 144 pt apart, the first at x = 18: with 72 pt of margin, a 0..800 span
        // reaches 872 and keeps the cards starting before it.
        let r = visible_items(100, false, 0.0, 800.0);
        assert_eq!(r.start, 0);
        assert!(r.end >= 6 && r.end <= 8, "{r:?}");
        // Scrolled far right, the left cards are gone.
        let r = visible_items(100, false, 4000.0, 4800.0);
        assert!(r.start > 20 && r.end < 40 && r.start < r.end, "{r:?}");
        // Short rows take what they have; none past the end.
        assert_eq!(visible_items(3, false, 0.0, 800.0), 0..3);
        assert_eq!(visible_items(0, false, 0.0, 800.0), 0..0);
        assert_eq!(visible_items(3, false, 5000.0, 5800.0), 0..0);
    }

    #[test]
    fn grid_columns_hold_four_rows() {
        // 316 pt per column: an 860 pt span shows three columns and a margin into the 4th.
        let r = visible_items(40, true, 0.0, 860.0);
        assert_eq!(r.start, 0);
        assert_eq!(r.end % GRID_ROWS, 0);
        assert!(r.end >= 12 && r.end <= 16, "{r:?}");
        assert_eq!(visible_items(6, true, 0.0, 860.0), 0..6);
    }

    #[test]
    fn paging_buttons_show_only_for_a_row_wider_than_it_shows() {
        assert_eq!(page_buttons(800.0, 860.0, 0.0), None);
        assert_eq!(page_buttons(860.0, 860.0, 0.0), None);
        // 8 cards: 1184 pt of content, 324 pt past an 860 pt view.
        assert_eq!(page_buttons(1184.0, 860.0, 0.0), Some((false, true)));
        assert_eq!(page_buttons(1184.0, 860.0, 144.0), Some((true, true)));
        assert_eq!(page_buttons(1184.0, 860.0, 324.0), Some((true, false)));
        assert_eq!(page_buttons(1184.0, 860.0, 323.8), Some((true, false)));
    }

    #[test]
    fn a_page_is_the_visible_width_in_whole_columns_within_the_row() {
        let pitch = CARD_W + CARD_GAP;
        // At the start, back stays put; forward moves five 144 pt columns.
        assert_eq!(page_offset(2000.0, 860.0, pitch, 0.0, false), 0.0);
        assert_eq!(page_offset(2000.0, 860.0, pitch, 0.0, true), 720.0);
        // The last page is partial: it stops at the row's end, 1140 pt in.
        assert_eq!(page_offset(2000.0, 860.0, pitch, 720.0, true), 1140.0);
        assert_eq!(page_offset(2000.0, 860.0, pitch, 1140.0, true), 1140.0);
        assert_eq!(page_offset(2000.0, 860.0, pitch, 1140.0, false), 420.0);
        assert_eq!(page_offset(1184.0, 860.0, pitch, 0.0, true), 324.0);
        // Narrower than a column, a page is still one column.
        assert_eq!(page_offset(2000.0, 100.0, pitch, 0.0, true), 144.0);
        assert_eq!(page_offset(2000.0, 100.0, pitch, 144.0, false), 0.0);
        // A grid steps by its own columns: two of 316 pt fit in 860.
        let grid = GRID_COL_W + CARD_GAP;
        assert_eq!(page_offset(2000.0, 860.0, grid, 0.0, true), 632.0);
    }

    #[test]
    fn a_shelf_of_tracks_is_a_grid_and_a_mixed_one_is_cards() {
        use requiem_core::{ProviderId, Track, TrackId};
        let track = Track {
            id: TrackId {
                provider: ProviderId("t".into()),
                id: "1".into(),
            },
            title: "T".into(),
            artists: vec![],
            album: None,
            duration: None,
            artwork_url: None,
        };
        let tracks = Shelf {
            title: "Quick picks".into(),
            items: vec![Item::Track(track.clone())],
        };
        assert!(is_grid(&tracks));
        assert!(!is_grid(&Shelf {
            title: String::new(),
            items: Vec::new()
        }));
        assert_eq!(spoken("Title", "Sub", None), "Title, Sub");
        assert_eq!(spoken("Title", "", None), "Title");
        let spotify = Some(Service::Spotify);
        assert_eq!(spoken("Title", "Sub", spotify), "Title, Sub, Spotify");
        assert_eq!(spoken("Title", "", spotify), "Title, Spotify");
    }
}
