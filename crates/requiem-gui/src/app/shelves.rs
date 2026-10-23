//! Home: shelves of artwork cards, one vertical scroll of horizontal rows.
//!
//! A page can hold a hundred cards, and a card with its cover costs a quarter-megabyte of
//! decoded artwork, so a card exists only while its place is near the visible area.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSBorderType, NSControl, NSCursor, NSEvent, NSEventMask,
    NSEventPhase, NSEventType, NSFocusRingType, NSLineBreakMode, NSMenu, NSMenuItem, NSResponder,
    NSScrollView, NSTextField, NSTrackingAreaOptions, NSView, NSViewBoundsDidChangeNotification,
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
/// How far a press moves sideways before it drags its shelf rather than clicking.
const DRAG_SLOP: CGFloat = 4.0;
/// A fling's speed falls by a factor of e every this many seconds, so it carries on for its
/// release speed times this, in points.
const FLING_DECAY: f64 = 0.325;
/// Released slower than this, in points a second, a drag just stops.
const FLING_MIN: CGFloat = 100.0;
/// A fling ends when it slows below this, in points a second.
const FLING_STOP: CGFloat = 10.0;
/// Only the pointer's last moves before release, over this many seconds, set a fling's speed.
const FLING_WINDOW: f64 = 0.1;
const FLING_TICK: Duration = Duration::from_millis(16);

/// The page whose shelves a `Shelves` holds, which cards and scrolling find it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Home,
    /// The All view of search results.
    Search,
}

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

pub struct WatcherIvars {
    surface: Surface,
}

define_class!(
    /// Hears the scroll views scroll, so cards come and go with the visible area, and the
    /// paging buttons, whose tag is their row's place in `Shelves::rows`.
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = WatcherIvars]
    pub struct ScrollWatcher;

    impl ScrollWatcher {
        #[unsafe(method(scrolled:))]
        fn scrolled(&self, _notification: &NSNotification) {
            let surface = self.ivars().surface;
            ui::with(|ui| ui.shelves_of(surface).sync());
        }

        #[unsafe(method(pageBack:))]
        fn page_back(&self, sender: Option<&NSControl>) {
            let surface = self.ivars().surface;
            if let Some(b) = sender {
                ui::with(|ui| ui.shelves_of(surface).page(b.tag() as usize, false));
            }
        }

        #[unsafe(method(pageForward:))]
        fn page_forward(&self, sender: Option<&NSControl>) {
            let surface = self.ivars().surface;
            if let Some(b) = sender {
                ui::with(|ui| ui.shelves_of(surface).page(b.tag() as usize, true));
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
    // The touch's own `Ended` comes before its momentum, so only the momentum's end lets go;
    // a lock left behind by a gesture without momentum is reset by the next `Began`.
    let done = momentum.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled);
    (route, if done { None } else { axis })
}

/// Whether a press that has moved `dx` sideways is a drag. Up and down count for nothing.
fn is_drag(dx: CGFloat) -> bool {
    dx.abs() > DRAG_SLOP
}

/// A row's offset after a drag of `dx` from a press at offset `start`: the cards follow the
/// pointer, within the row's `max` offset.
fn drag_offset(start: CGFloat, dx: CGFloat, max: CGFloat) -> CGFloat {
    (start - dx).clamp(0.0, max.max(0.0))
}

/// The pointer's sideways speed, in points a second, over `moves`: its times and x positions
/// within `FLING_WINDOW` of the release, the release last.
fn release_speed(moves: &[(f64, CGFloat)]) -> CGFloat {
    match (moves.first(), moves.last()) {
        (Some(&(t0, x0)), Some(&(t1, x1))) if t1 > t0 => (x1 - x0) / (t1 - t0),
        _ => 0.0,
    }
}

/// A fling `dt` seconds on from `offset` at `speed`: the new offset within `0..=max`, and the
/// speed it goes on at, or `None` once it has slowed to a stop or reached an end.
fn fling_step(
    offset: CGFloat,
    speed: CGFloat,
    dt: f64,
    max: CGFloat,
) -> (CGFloat, Option<CGFloat>) {
    let decay = (-dt / FLING_DECAY).exp();
    let to = offset + speed * FLING_DECAY * (1.0 - decay);
    let at = to.clamp(0.0, max.max(0.0));
    let speed = speed * decay;
    let going = at == to && speed.abs() >= FLING_STOP;
    (at, going.then_some(speed))
}

pub struct RowScrollIvars {
    locked: Cell<Option<Axis>>,
    /// Counts flings; a fling whose number is no longer this one has been stopped.
    fling: Cell<u64>,
}

/// What a press on a shelf turned out to be.
enum Press {
    /// Let go without dragging: the release.
    Click(Retained<NSEvent>),
    Drag,
}

define_class!(
    /// A shelf's scroll view. It scrolls sideways only; a mostly vertical gesture goes on to
    /// the page behind it, which a plain scroll view would swallow. A mouse, with no sideways
    /// wheel, drags it instead.
    // SAFETY: NSScrollView has no subclassing requirements for `scrollWheel:` or
    // `mouseDown:`; no Drop impl.
    #[unsafe(super(NSScrollView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RowScrollIvars]
    pub struct RowScroll;

    impl RowScroll {
        /// A press between cards, which the row's document passes on.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.track(event);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.stop_fling();
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
            fling: Cell::new(0),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn max_offset(&self) -> CGFloat {
        let content = self.documentView().map_or(0.0, |d| d.frame().size.width);
        (content - self.contentSize().width).max(0.0)
    }

    /// Scrolls to `x`; the bounds change brings the cards and paging buttons along.
    fn scroll_to(&self, x: CGFloat) {
        let clip = self.contentView();
        clip.scrollToPoint(NSPoint::new(x, clip.bounds().origin.y));
        self.reflectScrolledClipView(&clip);
    }

    fn stop_fling(&self) {
        let fling = &self.ivars().fling;
        fling.set(fling.get().wrapping_add(1));
    }

    /// Follows a press until it is let go. Moved sideways past `DRAG_SLOP`, it drags the
    /// row, and flings it on at the speed it was let go at.
    fn track(&self, down: &NSEvent) -> Press {
        self.stop_fling();
        let Some(window) = self.window() else {
            return Press::Drag;
        };
        let start_x = down.locationInWindow().x;
        let start = self.contentView().bounds().origin.x;
        let mut dragging = false;
        let mut moves = vec![(down.timestamp(), start_x)];
        // A loop of its own, as a button tracks its press: the card pressed can scroll out
        // of the row and be dropped while the drag goes on.
        loop {
            let mask = NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp;
            let Some(event) = window.nextEventMatchingMask(mask) else {
                continue;
            };
            let (t, x) = (event.timestamp(), event.locationInWindow().x);
            moves.retain(|&(s, _)| t - s <= FLING_WINDOW);
            moves.push((t, x));
            if event.r#type() == NSEventType::LeftMouseUp {
                if !dragging {
                    return Press::Click(event);
                }
                NSCursor::pop_class();
                self.fling(-release_speed(&moves));
                return Press::Drag;
            }
            if !dragging && is_drag(x - start_x) {
                dragging = true;
                NSCursor::closedHandCursor().push();
            }
            if dragging {
                self.scroll_to(drag_offset(start, x - start_x, self.max_offset()));
            }
        }
    }

    fn fling(&self, speed: CGFloat) {
        if speed.abs() < FLING_MIN || widgets::reduce_motion() {
            return;
        }
        self.fling_on(speed, Instant::now(), self.ivars().fling.get());
    }

    fn fling_on(&self, speed: CGFloat, last: Instant, fling: u64) {
        let Ok(when) = DispatchTime::try_from(FLING_TICK) else {
            return;
        };
        let this = MainThreadBound::new(self.retain(), self.mtm());
        let _ = DispatchQueue::main().after(when, move || {
            this.get_on_main(|this| {
                if this.ivars().fling.get() != fling || this.window().is_none() {
                    return;
                }
                let now = Instant::now();
                let at = this.contentView().bounds().origin.x;
                let dt = now.duration_since(last).as_secs_f64();
                let (to, next) = fling_step(at, speed, dt, this.max_offset());
                this.scroll_to(to);
                if let Some(speed) = next {
                    this.fling_on(speed, now, fling);
                }
            });
        });
    }
}

pub struct CardIvars {
    surface: Surface,
    /// The shelf's index in the page's list, which moves when a refresh adds or drops a
    /// shelf before it.
    shelf: Cell<usize>,
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

        /// A press that drags sideways scrolls the row and is no click.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            // A drag can scroll the card out of its row, which then lets go of it.
            let this = self.retain();
            this.ivars().pressed.set(true);
            let row = this
                .enclosingScrollView()
                .and_then(|s| s.downcast::<RowScroll>().ok());
            match row.map(|row| row.track(event)) {
                Some(Press::Click(up)) => this.release(&up),
                Some(Press::Drag) => this.ivars().pressed.set(false),
                None => {}
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.release(event);
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
            ui::with(|ui| ui.card_add(iv.surface, iv.shelf.get(), iv.index));
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
    #[allow(clippy::too_many_arguments)]
    fn new(
        mtm: MainThreadMarker,
        surface: Surface,
        frame: NSRect,
        shelf: usize,
        index: usize,
        item: &Item,
        compact: bool,
        badge: Option<Service>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CardIvars {
            surface,
            shelf: Cell::new(shelf),
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

    /// Activates the card if it was pressed and is let go over it.
    fn release(&self, up: &NSEvent) {
        let was = self.ivars().pressed.replace(false);
        let at = self.convertPoint_fromView(up.locationInWindow(), None);
        let b = self.bounds();
        let inside = at.x >= 0.0 && at.x < b.size.width && at.y >= 0.0 && at.y < b.size.height;
        if was && inside {
            self.activate();
        }
    }

    fn activate(&self) {
        let iv = self.ivars();
        ui::with(|ui| ui.card_clicked(iv.surface, iv.shelf.get(), iv.index));
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
pub(super) fn spoken(title: &str, subtitle: &str, badge: Option<Service>) -> String {
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

/// Each title with how many shelves of that title came before it, which tells shelves of the
/// same title apart across a refresh.
fn title_keys<'a>(titles: impl Iterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    titles
        .map(|t| {
            let n = seen.entry(t).or_default();
            *n += 1;
            (t.to_owned(), *n - 1)
        })
        .collect()
}

/// What a page's new row is made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Old row `n`, whose items have not changed.
    Same(usize),
    /// Old row `n`, with different items.
    Changed(usize),
    /// Nothing before: a new row.
    New,
}

/// Matches a page's new rows to its old ones by `title_keys`, given each row's title and
/// items. Returns each new row's fate in order, and the old rows no new row continues.
fn row_fates<T: PartialEq>(old: &[(&str, &[T])], new: &[(&str, &[T])]) -> (Vec<Fate>, Vec<usize>) {
    let before: HashMap<(String, usize), usize> = title_keys(old.iter().map(|r| r.0))
        .into_iter()
        .enumerate()
        .map(|(i, key)| (key, i))
        .collect();
    let mut kept = vec![false; old.len()];
    let fates = title_keys(new.iter().map(|r| r.0))
        .into_iter()
        .zip(new)
        .map(|(key, (_, items))| match before.get(&key) {
            Some(&i) => {
                kept[i] = true;
                if old[i].1 == *items {
                    Fate::Same(i)
                } else {
                    Fate::Changed(i)
                }
            }
            None => Fate::New,
        })
        .collect();
    let gone = (0..old.len()).filter(|&i| !kept[i]).collect();
    (fates, gone)
}

/// Whether a changed row keeps its card for item `i`: the item there is the same as before.
fn keeps_card<T: PartialEq>(old: &[T], new: &[T], i: usize) -> bool {
    old.get(i).is_some_and(|o| new.get(i) == Some(o))
}

/// A row's height without its title, and its document's width.
fn row_size(items: usize, grid: bool) -> (CGFloat, CGFloat) {
    if grid {
        let cols = items.div_ceil(GRID_ROWS) as CGFloat;
        (
            GRID_ROWS as CGFloat * GRID_ROW_H,
            2.0 * PAD + cols * GRID_COL_W + (cols - 1.0) * CARD_GAP,
        )
    } else {
        let n = items as CGFloat;
        (card_h(), 2.0 * PAD + n * CARD_W + (n - 1.0) * CARD_GAP)
    }
}

struct ShelfRow {
    surface: Surface,
    /// The shelf's index in the page's list, which `Intent`s name.
    shelf: usize,
    /// The shelf's title, which a refreshed page finds it again by.
    name: String,
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

    fn remove(&self) {
        self.clear();
        self.title.removeFromSuperview();
        self.back.removeFromSuperview();
        self.forward.removeFromSuperview();
        self.scroll.removeFromSuperview();
    }

    /// Takes on a refreshed shelf of the same title in place: cards whose item is unchanged
    /// stay, the rest go for `fill` to make again, and the document fits the new items.
    fn update(&mut self, shelf: usize, new: &Shelf, mixed: bool, changed: bool) {
        let grid = is_grid(new);
        // A new layout or badge changes every card.
        let restyle = grid != self.grid || mixed != self.mixed;
        self.cards.borrow_mut().retain(|&i, card| {
            let keep = !restyle && (!changed || keeps_card(&self.items, &new.items, i));
            if keep {
                card.ivars().shelf.set(shelf);
            } else {
                card.removeFromSuperview();
            }
            keep
        });
        if changed {
            self.items = new.items.clone();
        }
        (self.shelf, self.grid, self.mixed) = (shelf, grid, mixed);
        let (row_h, content_w) = row_size(self.items.len(), grid);
        let size = NSSize::new(content_w, row_h);
        if self.doc.frame().size != size {
            self.doc.setFrameSize(size);
        }
    }

    /// Moves the row so its title starts at `top`, if it is not there or its height changed.
    fn place(&mut self, top: CGFloat) {
        let row_h = self.doc.frame().size.height;
        let bottom = top + SHELF_TITLE_H + SHELF_TITLE_GAP + row_h;
        if top == self.top && bottom == self.bottom {
            return;
        }
        let dy = top - self.top;
        for v in [
            &*self.title as &NSView,
            &self.back as &NSView,
            &self.forward as &NSView,
        ] {
            let mut f = v.frame();
            f.origin.y += dy;
            v.setFrame(f);
        }
        let mut f = self.scroll.frame();
        f.origin.y = top + SHELF_TITLE_H + SHELF_TITLE_GAP;
        f.size.height = row_h;
        self.scroll.setFrame(f);
        (self.top, self.bottom) = (top, bottom);
    }

    /// Pulls the row back within its document, after its items shrank.
    fn clamp_scroll(&self) {
        let clip = self.scroll.contentView();
        let at = clip.bounds().origin;
        let room = (self.doc.frame().size.width - self.scroll.contentSize().width).max(0.0);
        if at.x > room {
            clip.scrollToPoint(NSPoint::new(room, at.y));
            self.scroll.reflectScrolledClipView(&clip);
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
            let card = ShelfCard::new(
                mtm,
                self.surface,
                frame,
                self.shelf,
                i,
                item,
                self.grid,
                badge,
            );
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

/// A row's scroll view, and its cards by item index.
#[doc(hidden)]
pub type RowViews = (Retained<NSScrollView>, Vec<(usize, Retained<NSView>)>);

/// What sits above a page's first shelf: a view across the page's width, and its height.
struct Lead {
    view: Retained<NSView>,
    height: CGFloat,
}

pub struct Shelves {
    mtm: MainThreadMarker,
    surface: Surface,
    pub view: Retained<NSScrollView>,
    doc: Retained<FlippedView>,
    rows: RefCell<Vec<ShelfRow>>,
    lead: RefCell<Option<Lead>>,
    watcher: Retained<ScrollWatcher>,
}

impl Shelves {
    /// Home's shelves in `frame`.
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Shelves {
        Self::on(mtm, Surface::Home, frame)
    }

    /// The shelves of `surface`'s page in `frame`.
    pub fn on(mtm: MainThreadMarker, surface: Surface, frame: NSRect) -> Shelves {
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
        let watcher = ScrollWatcher::alloc(mtm).set_ivars(WatcherIvars { surface });
        // SAFETY: NSObject's `init`.
        let watcher: Retained<ScrollWatcher> = unsafe { msg_send![super(watcher), init] };
        Shelves {
            mtm,
            surface,
            view,
            doc,
            rows: RefCell::new(Vec::new()),
            lead: RefCell::new(None),
            watcher,
        }
    }

    /// Puts `view`, `height` high and as wide as the page, above the first shelf, in place of
    /// the one before; `None` takes it away. It lays out at the next `set_shelves`.
    pub fn set_lead(&self, view: Option<(Retained<NSView>, CGFloat)>) {
        let old = self
            .lead
            .replace(view.map(|(view, height)| Lead { view, height }));
        if let Some(old) = old {
            old.view.removeFromSuperview();
        }
    }

    /// The lead's view, if there is one.
    pub fn lead(&self) -> Option<Retained<NSView>> {
        self.lead.borrow().as_ref().map(|l| l.view.clone())
    }

    /// Replaces the page's shelves; shelves with nothing in them are left out. With
    /// `keep_scroll`, each shelf found again by its title, and by which of the shelves of that
    /// title it is, keeps its views: unchanged, it is left alone, and changed, it keeps its
    /// scroll and the cards of the items it still has in the same place. The page stays
    /// scrolled where it was, as far as the new shelves reach. Otherwise every row is made
    /// anew and the page starts at its top.
    pub fn set_shelves(&self, shelves: &[Shelf], keep_scroll: bool) {
        let shown: Vec<(usize, &Shelf)> = shelves
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.items.is_empty())
            .collect();
        let mixed = vm::mixed(
            shelves
                .iter()
                .flat_map(|s| s.items.iter().map(vm::item_provider)),
        );
        let page_y = self.view.documentVisibleRect().origin.y;
        let centre = NSNotificationCenter::defaultCenter();
        // Resizing and moving rows below posts bounds changes; `sync` waits for the end.
        // SAFETY: the watcher is an object; removing observers it never added is allowed.
        unsafe { centre.removeObserver(&self.watcher) };
        let mut old: Vec<Option<ShelfRow>> = std::mem::take(&mut *self.rows.borrow_mut())
            .into_iter()
            .map(Some)
            .collect();
        let (fates, gone) = if keep_scroll {
            let before: Vec<(&str, &[Item])> = old
                .iter()
                .flatten()
                .map(|r| (r.name.as_str(), r.items.as_slice()))
                .collect();
            let after: Vec<(&str, &[Item])> = shown
                .iter()
                .map(|(_, s)| (s.title.as_str(), s.items.as_slice()))
                .collect();
            row_fates(&before, &after)
        } else {
            (vec![Fate::New; shown.len()], (0..old.len()).collect())
        };
        for i in gone {
            if let Some(row) = old[i].take() {
                row.remove();
            }
        }
        let width = self.view.contentSize().width;
        let mut y = TOP_PAD;
        if let Some(lead) = &*self.lead.borrow() {
            lead.view.setFrame(rect(0.0, y, width, lead.height));
            lead.view.setAutoresizingMask(FLEX_W);
            self.doc.addSubview(&lead.view);
            y += lead.height + SHELF_GAP;
        }
        let mut rows = Vec::with_capacity(shown.len());
        for (&(si, shelf), fate) in shown.iter().zip(fates) {
            let row = match fate {
                Fate::Same(i) | Fate::Changed(i) => {
                    let mut row = old[i].take().expect("an old row is continued at most once");
                    row.update(si, shelf, mixed, matches!(fate, Fate::Changed(_)));
                    row.place(y);
                    row
                }
                Fate::New => self.new_row(si, shelf, mixed, width, y),
            };
            row.back.setTag(rows.len() as isize);
            row.forward.setTag(rows.len() as isize);
            y = row.bottom + SHELF_GAP;
            rows.push(row);
        }
        let height = (y - SHELF_GAP + TOP_PAD).max(0.0);
        if self.doc.frame().size != NSSize::new(width, height) {
            self.doc.setFrameSize(NSSize::new(width, height));
        }
        for row in &rows {
            let clip = row.scroll.contentView();
            clip.setPostsBoundsChangedNotifications(true);
            observe(&self.watcher, &clip);
        }
        let outer = self.view.contentView();
        outer.setPostsBoundsChangedNotifications(true);
        observe(&self.watcher, &outer);
        *self.rows.borrow_mut() = rows;
        for row in self.rows.borrow().iter() {
            row.clamp_scroll();
        }
        let room = (self.doc.frame().size.height - self.view.contentSize().height).max(0.0);
        let top = if keep_scroll {
            page_y.min(room).max(0.0)
        } else {
            0.0
        };
        if top != outer.bounds().origin.y {
            outer.scrollToPoint(NSPoint::new(0.0, top));
            self.view.reflectScrolledClipView(&outer);
        }
        self.sync();
    }

    /// A row for `shelf`, the page's shelf `si`, with its title at `top`.
    fn new_row(
        &self,
        si: usize,
        shelf: &Shelf,
        mixed: bool,
        width: CGFloat,
        top: CGFloat,
    ) -> ShelfRow {
        let grid = is_grid(shelf);
        let title_font = theme::font(17.0, Weight::SemiBold);
        let title = widgets::label(self.mtm, &shelf.title, &title_font, &theme::ink());
        let buttons_w = 2.0 * PAGE_BUTTON + PAGE_GAP;
        title.setFrame(rect(
            PAD,
            top,
            width - 2.0 * PAD - buttons_w - PAGE_GAP,
            SHELF_TITLE_H,
        ));
        title.setAutoresizingMask(FLEX_W);
        self.doc.addSubview(&title);
        let button_y = top + (SHELF_TITLE_H - PAGE_BUTTON) / 2.0;
        let back = self.page_button(
            Icon::ChevronLeft,
            rect(width - PAD - buttons_w, button_y, PAGE_BUTTON, PAGE_BUTTON),
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
            sel!(pageForward:),
            &format!("Show more in {}", shelf.title),
        );
        let y = top + SHELF_TITLE_H + SHELF_TITLE_GAP;
        let (row_h, content_w) = row_size(shelf.items.len(), grid);
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
        ShelfRow {
            surface: self.surface,
            shelf: si,
            name: shelf.title.clone(),
            title,
            scroll,
            doc,
            back,
            forward,
            items: shelf.items.clone(),
            grid,
            mixed,
            top,
            bottom: y + row_h,
            cards: RefCell::new(HashMap::new()),
        }
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

    /// Each row's scroll view, and its cards with their items' indexes, in order: what a test
    /// checks a refresh kept.
    #[doc(hidden)]
    pub fn views(&self) -> Vec<RowViews> {
        self.rows
            .borrow()
            .iter()
            .map(|row| {
                let mut cards: Vec<(usize, Retained<NSView>)> = row
                    .cards
                    .borrow()
                    .iter()
                    .map(|(&i, card)| (i, Retained::into_super(card.clone())))
                    .collect();
                cards.sort_by_key(|&(i, _)| i);
                (Retained::into_super(row.scroll.clone()), cards)
            })
            .collect()
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
        row.scroll.stop_fling();
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
    fn shelves_of_the_same_title_keep_apart() {
        let key = |t: &str, n| (t.to_owned(), n);
        assert_eq!(
            title_keys(["Mixes", "New", "Mixes"].into_iter()),
            [key("Mixes", 0), key("New", 0), key("Mixes", 1)]
        );
    }

    #[test]
    fn a_refresh_keeps_rows_by_title_and_tells_which_changed() {
        use Fate::*;
        let (a, b, c): (&[u8], &[u8], &[u8]) = (&[1, 2], &[3], &[4, 5, 6]);
        let old = [("Mixes", a), ("New", b)];
        // Same.
        assert_eq!(row_fates(&old, &old), (vec![Same(0), Same(1)], vec![]));
        // Changed: same title, different items.
        assert_eq!(
            row_fates(&old, &[("Mixes", a), ("New", c)]),
            (vec![Same(0), Changed(1)], vec![])
        );
        // Added, in the middle.
        assert_eq!(
            row_fates(&old, &[("Mixes", a), ("Hot", c), ("New", b)]),
            (vec![Same(0), New, Same(1)], vec![])
        );
        // Removed.
        assert_eq!(row_fates(&old, &[("New", b)]), (vec![Same(1)], vec![0]));
        assert_eq!(row_fates(&old, &[]), (vec![], vec![0, 1]));
        assert_eq!(row_fates::<u8>(&[], &[("New", b)]), (vec![New], vec![]));
        // Reordered.
        assert_eq!(
            row_fates(&old, &[("New", b), ("Mixes", a)]),
            (vec![Same(1), Same(0)], vec![])
        );
    }

    #[test]
    fn shelves_of_the_same_title_match_in_their_order() {
        use Fate::*;
        let (a, b, c): (&[u8], &[u8], &[u8]) = (&[1], &[2], &[3]);
        let old = [("Mixes", a), ("New", c), ("Mixes", b)];
        assert_eq!(
            row_fates(&old, &[("Mixes", a), ("Mixes", c)]),
            (vec![Same(0), Changed(2)], vec![1])
        );
        // Losing the first of two leaves the second's items to the first's place.
        assert_eq!(
            row_fates(&old, &[("New", c), ("Mixes", b)]),
            (vec![Same(1), Changed(0)], vec![2])
        );
        // A third of the title is new.
        assert_eq!(
            row_fates(
                &old,
                &[("Mixes", a), ("New", c), ("Mixes", b), ("Mixes", a)]
            ),
            (vec![Same(0), Same(1), Same(2), New], vec![])
        );
    }

    #[test]
    fn a_changed_row_keeps_the_cards_of_items_in_the_same_place() {
        let old = [1, 2, 3, 4];
        let new = [1, 9, 3];
        let kept: Vec<bool> = (0..5).map(|i| keeps_card(&old, &new, i)).collect();
        assert_eq!(kept, [true, false, true, false, false]);
        // Growing keeps the old ones; the new places have no card to keep.
        assert!(keeps_card(&[1], &[1, 2], 0));
        assert!(!keeps_card(&[1], &[1, 2], 1));
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
    fn the_lock_holds_through_momentum_and_clears_when_it_ends() {
        let (none, changed, began) = (
            NSEventPhase::None,
            NSEventPhase::Changed,
            NSEventPhase::Began,
        );
        for end in [NSEventPhase::Ended, NSEventPhase::Cancelled] {
            // Lifting the fingers keeps the lock for the momentum that follows.
            let mut lock = Some(Axis::Vertical);
            assert_eq!(gesture(end, none, 0.0, 0.0, &mut lock), Route::Page);
            assert_eq!(lock, Some(Axis::Vertical));
            assert_eq!(gesture(none, began, 6.0, -1.0, &mut lock), Route::Page);
            assert_eq!(gesture(none, changed, 5.0, 0.0, &mut lock), Route::Page);
            assert_eq!(gesture(none, end, 0.0, 0.0, &mut lock), Route::Page);
            assert_eq!(lock, None);
        }
        // Momentum with no lock picks its axis from its own first delta.
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
    fn a_press_drags_only_past_four_points_sideways() {
        assert!(!is_drag(0.0));
        assert!(!is_drag(4.0) && !is_drag(-4.0));
        assert!(is_drag(4.5) && is_drag(-4.5));
    }

    #[test]
    fn a_drag_moves_the_row_with_the_pointer_within_its_ends() {
        // Dragging right shows what was to the left.
        assert_eq!(drag_offset(300.0, 50.0, 1000.0), 250.0);
        assert_eq!(drag_offset(300.0, -50.0, 1000.0), 350.0);
        assert_eq!(drag_offset(30.0, 50.0, 1000.0), 0.0);
        assert_eq!(drag_offset(980.0, -50.0, 1000.0), 1000.0);
        // A row that fits does not move.
        assert_eq!(drag_offset(0.0, -50.0, -20.0), 0.0);
    }

    #[test]
    fn the_release_speed_is_the_last_moves_over_their_time() {
        assert_eq!(release_speed(&[]), 0.0);
        assert_eq!(release_speed(&[(1.0, 50.0)]), 0.0);
        assert_eq!(release_speed(&[(1.0, 0.0), (1.0625, 50.0)]), 800.0);
        assert_eq!(
            release_speed(&[(1.0, 100.0), (1.05, 70.0), (1.125, 0.0)]),
            -800.0
        );
    }

    #[test]
    fn a_fling_slows_down_and_stops() {
        // In one decay time it goes 1 - 1/e of its whole reach, at 1/e of the speed.
        let (at, next) = fling_step(0.0, 1000.0, FLING_DECAY, 10_000.0);
        let e = std::f64::consts::E;
        assert!(
            (at - 1000.0 * FLING_DECAY * (1.0 - 1.0 / e)).abs() < 1e-6,
            "{at}"
        );
        assert!((next.unwrap() - 1000.0 / e).abs() < 1e-6);
        // Step by step, it covers about speed × decay time, and stops.
        let (mut at, mut speed, mut steps) = (0.0, 1000.0, 0);
        while let (to, Some(s)) = fling_step(at, speed, 0.016, 10_000.0) {
            (at, speed, steps) = (to, s, steps + 1);
            assert!(steps < 1000, "never stopped");
        }
        assert!(at > 300.0 && at < 1000.0 * FLING_DECAY, "{at}");
        // Backwards too.
        let (at, next) = fling_step(500.0, -1000.0, 0.016, 10_000.0);
        assert!(at < 500.0 && next.unwrap() < 0.0);
    }

    #[test]
    fn a_fling_stops_at_the_row_ends() {
        assert_eq!(fling_step(990.0, 5000.0, 0.1, 1000.0), (1000.0, None));
        assert_eq!(fling_step(5.0, -5000.0, 0.1, 1000.0), (0.0, None));
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
