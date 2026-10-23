//! The results and queue tables: view-based `NSTableView`s with themed rows, cells and
//! headers.

use std::cell::{Cell, OnceCell, RefCell};
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityCellRole, NSApplication, NSAttributedStringNSStringDrawing,
    NSBezierPath, NSBorderType, NSColor, NSControlTextEditingDelegate, NSEvent, NSEventType,
    NSFocusRingType, NSMenu, NSMenuDelegate, NSMenuItem, NSResponder, NSScrollView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableRowView, NSTableView, NSTableViewColumnAutoresizingStyle,
    NSTableViewDataSource, NSTableViewDelegate, NSTableViewSelectionHighlightStyle,
    NSTableViewStyle, NSTextAlignment, NSTextField, NSTrackingAreaOptions,
    NSUserInterfaceItemIdentification, NSView, NSViewFrameDidChangeNotification,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSEdgeInsets, NSIndexSet, NSInteger, NSNotification,
    NSNotificationCenter, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use requiem_core::Track;
use requiem_macos::artwork::Cover;

use super::badge::{self, Badge};
use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{self, FLEX_H, FLEX_W, HoverButton, PIN_BOTTOM, PIN_LEFT, PIN_TOP, rect};
use crate::vm::{self, Intent, List, QueueEdit, Service};

pub const ROW_H: CGFloat = 36.0;
/// The title's artwork, before the title.
const THUMB: CGFloat = 32.0;
const THUMB_GAP: CGFloat = 10.0;
/// A played queue row's cover opacity.
const PLAYED_COVER: CGFloat = 0.6;
const FOCUS_RING: CGFloat = 2.0;
/// How far the table's focus ring sits outside its rows.
const RING_OUTSET: CGFloat = 4.0;
const HEADER_H: CGFloat = 32.0;
const PAD: CGFloat = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Results,
    Queue,
    /// The tracks of an open playlist or album, listed like results.
    Collection,
}

impl Kind {
    /// Whether rows have the "+" column.
    fn adds(self) -> bool {
        matches!(self, Kind::Results | Kind::Collection)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Title,
    Artists,
    Album,
    Duration,
    Add,
}

impl Col {
    fn id(self) -> &'static str {
        match self {
            Col::Title => "title",
            Col::Artists => "artists",
            Col::Album => "album",
            Col::Duration => "duration",
            Col::Add => "add",
        }
    }

    fn from_id(id: &str) -> Option<Col> {
        [
            Col::Title,
            Col::Artists,
            Col::Album,
            Col::Duration,
            Col::Add,
        ]
        .into_iter()
        .find(|c| c.id() == id)
    }
}

/// One row's text, formatted once per list change, and its artwork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowText {
    title: String,
    artists: String,
    album: String,
    duration: String,
    artwork: Option<String>,
    /// The service badge on the artwork, in a list that mixes services.
    badge: Option<Service>,
}

pub fn rows(tracks: &[Track]) -> Vec<RowText> {
    let mixed = vm::mixed(tracks.iter().map(|t| &t.id.provider));
    tracks
        .iter()
        .map(|t| RowText {
            title: t.title.clone(),
            artists: vm::artists(t),
            album: t.album.clone().unwrap_or_default(),
            duration: t.duration.map(vm::fmt_time).unwrap_or_default(),
            artwork: t.artwork_url.clone(),
            badge: vm::badge(&t.id.provider, mixed),
        })
        .collect()
}

/// Where the row `was`, selected at `at` before `rows` replaced the list, is now: at `at`
/// if it is still there, else the nearest row with the same text, else none.
fn reselect(rows: &[RowText], was: &RowText, at: usize) -> Option<usize> {
    if rows.get(at) == Some(was) {
        return Some(at);
    }
    (0..rows.len())
        .filter(|&i| rows[i] == *was)
        .min_by_key(|&i| i.abs_diff(at))
}

pub struct ControllerIvars {
    kind: Kind,
    rows: RefCell<Vec<RowText>>,
    current: Cell<Option<usize>>,
    /// Whether the rows' "+" buttons take clicks.
    enabled: Cell<bool>,
    table: OnceCell<Weak<NSTableView>>,
    /// The row outlined while its context menu is open.
    menu_row: Cell<Option<NSInteger>>,
}

define_class!(
    /// Data source, delegate and click target of one table.
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ControllerIvars]
    pub struct Controller;

    // SAFETY: no requirements.
    unsafe impl NSObjectProtocol for Controller {}

    // SAFETY: the implemented method has the protocol's signature.
    unsafe impl NSTableViewDataSource for Controller {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().rows.borrow().len() as NSInteger
        }
    }

    // SAFETY: no required methods.
    unsafe impl NSControlTextEditingDelegate for Controller {}

    // SAFETY: the implemented methods have the protocol's signatures.
    unsafe impl NSMenuDelegate for Controller {
        /// Outlines the clicked row, as AppKit's own tables do; the rows draw their own
        /// backgrounds, so its outline is theirs too.
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            let row = self.table().map(|t| t.clickedRow()).filter(|&r| r >= 0);
            self.outline(row);
        }

        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            self.outline(None);
        }
    }

    // SAFETY: the implemented methods have the protocol's signatures.
    unsafe impl NSTableViewDelegate for Controller {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for(
            &self,
            table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            self.view_for_cell(table, column, row)
        }

        #[unsafe(method_id(tableView:rowViewForRow:))]
        fn row_view_for(&self, table: &NSTableView, _row: NSInteger) -> Option<Retained<NSTableRowView>> {
            let id = NSString::from_str("row");
            // SAFETY: views under this identifier are only ever `RowView`s.
            let reused = unsafe { table.makeViewWithIdentifier_owner(&id, Some(self)) }
                .and_then(|v| v.downcast::<RowView>().ok());
            let rv = reused.unwrap_or_else(|| RowView::new(self.mtm(), &id));
            Some(Retained::into_super(rv))
        }

        #[unsafe(method(tableView:didAddRowView:forRow:))]
        fn did_add_row_view(&self, table: &NSTableView, row_view: &NSTableRowView, row: NSInteger) {
            if let Some(rv) = row_view.downcast_ref::<RowView>() {
                let iv = rv.ivars();
                iv.odd.set(row % 2 == 1);
                iv.current.set(usize::try_from(row).ok() == self.ivars().current.get());
                iv.selected.set(table.selectedRow() == row);
                iv.focused.set(table.downcast_ref::<KeyTable>().is_some_and(KeyTable::focused));
                rv.paint();
            }
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_did_change(&self, _notification: &NSNotification) {
            if let Some(table) = self.table() {
                let selected = table.selectedRow();
                for_visible_rows(&table, |row, rv| {
                    rv.ivars().selected.set(row == selected);
                    rv.paint();
                });
                if let Some(t) = table.downcast_ref::<KeyTable>() {
                    t.show_focus();
                }
            }
        }
    }

    impl Controller {
        #[unsafe(method(rowDoubleClicked:))]
        fn row_double_clicked(&self, _sender: Option<&AnyObject>) {
            if let Some(row) = self.clicked_row() {
                self.play(row);
            }
        }

        #[unsafe(method(contextPlay:))]
        fn context_play(&self, _sender: Option<&AnyObject>) {
            if let Some(row) = self.clicked_row() {
                self.play(row);
            }
        }

        #[unsafe(method(contextAdd:))]
        fn context_add(&self, _sender: Option<&AnyObject>) {
            if let (Some(row), Some(list)) = (self.clicked_row(), self.list()) {
                ui::with(|ui| ui.append_row(list, row));
            }
        }

        #[unsafe(method(contextRemove:))]
        fn context_remove(&self, _sender: Option<&AnyObject>) {
            self.edit_clicked(QueueEdit::Remove);
        }

        #[unsafe(method(contextMoveUp:))]
        fn context_move_up(&self, _sender: Option<&AnyObject>) {
            self.edit_clicked(QueueEdit::MoveUp);
        }

        #[unsafe(method(contextMoveDown:))]
        fn context_move_down(&self, _sender: Option<&AnyObject>) {
            self.edit_clicked(QueueEdit::MoveDown);
        }

        #[unsafe(method(contextClear:))]
        fn context_clear(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.edit_queue(QueueEdit::Clear));
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            self.validate(item)
        }

        #[unsafe(method(addClicked:))]
        fn add_clicked(&self, sender: Option<&NSView>) {
            let (Some(table), Some(sender)) = (self.table(), sender) else { return };
            let Ok(index) = usize::try_from(table.rowForView(sender)) else { return };
            if let Some(list) = self.list() {
                ui::with(|ui| ui.append_row(list, index));
            }
        }
    }
);

impl Controller {
    fn view_for_cell(
        &self,
        table: &NSTableView,
        column: Option<&NSTableColumn>,
        row: NSInteger,
    ) -> Option<Retained<NSView>> {
        let col = Col::from_id(&column?.identifier().to_string())?;
        self.cell(table, col, usize::try_from(row).ok()?)
    }

    fn table(&self) -> Option<Retained<NSTableView>> {
        self.ivars().table.get()?.load()
    }

    /// The track list the rows show, when they are tracks to append.
    fn list(&self) -> Option<List> {
        match self.ivars().kind {
            Kind::Results => Some(List::Results),
            Kind::Collection => Some(List::Collection),
            Kind::Queue => None,
        }
    }

    fn outline(&self, row: Option<NSInteger>) {
        let Some(table) = self.table() else { return };
        let old = self.ivars().menu_row.replace(row);
        for (r, on) in [(old, false), (row, true)] {
            if let Some(rv) = r
                .and_then(|r| table.rowViewAtRow_makeIfNecessary(r, false))
                .and_then(|rv| rv.downcast::<RowView>().ok())
            {
                rv.ivars().outlined.set(on);
                rv.paint();
            }
        }
    }

    /// The row a double-click or the context menu is for: the clicked row, not the selection.
    fn clicked_row(&self) -> Option<usize> {
        let table = self.table()?;
        usize::try_from(table.clickedRow()).ok()
    }

    fn play(&self, row: usize) {
        match self.ivars().kind {
            Kind::Results => ui::with(|ui| {
                ui.intent(Intent::PlayFrom {
                    list: List::Results,
                    from: row,
                })
            }),
            Kind::Collection => ui::with(|ui| {
                ui.intent(Intent::PlayFrom {
                    list: List::Collection,
                    from: row,
                })
            }),
            Kind::Queue => ui::with(|ui| ui.play_queue(row)),
        };
    }

    fn edit_clicked(&self, edit: fn(usize) -> QueueEdit) {
        if let Some(row) = self.clicked_row() {
            ui::with(|ui| ui.edit_queue(edit(row)));
        }
    }

    /// Enables the context menu's items for the clicked row; all of them need a player.
    fn validate(&self, item: &NSMenuItem) -> bool {
        let row = self.clicked_row();
        let rows = self.ivars().rows.borrow().len();
        let action = item.action();
        ui::with(|ui| {
            let edit = |f: fn(usize) -> QueueEdit| row.is_some_and(|r| ui.can_edit(f(r)));
            if action == Some(sel!(contextRemove:)) {
                edit(QueueEdit::Remove)
            } else if action == Some(sel!(contextMoveUp:)) {
                edit(QueueEdit::MoveUp)
            } else if action == Some(sel!(contextMoveDown:)) {
                edit(QueueEdit::MoveDown)
            } else if action == Some(sel!(contextClear:)) {
                ui.can_edit(QueueEdit::Clear)
            } else {
                row.is_some_and(|r| r < rows) && ui.playback().1.player
            }
        })
        .unwrap_or(false)
    }

    pub fn set_rows(&self, rows: Vec<RowText>, current: Option<usize>) {
        *self.ivars().rows.borrow_mut() = rows;
        self.ivars().current.set(current);
    }

    fn cell(&self, table: &NSTableView, col: Col, row: usize) -> Option<Retained<NSView>> {
        let mtm = self.mtm();
        let id = NSString::from_str(col.id());
        if col == Col::Add {
            // SAFETY: views under the "add" identifier are only ever `AddCell`s.
            let reused = unsafe { table.makeViewWithIdentifier_owner(&id, Some(self)) }
                .and_then(|v| v.downcast::<AddCell>().ok());
            let target = self.ivars().kind.adds().then_some(self);
            let cell = reused.unwrap_or_else(|| AddCell::new(mtm, &id, target));
            // A reused cell may come from a hovered or just-added row.
            cell.set_added(false);
            if let Some(button) = cell.ivars().button.get() {
                button.set_shown(false);
                button.set_enabled(self.ivars().enabled.get());
            }
            return Some(Retained::into_super(cell));
        }
        let rows = self.ivars().rows.borrow();
        let r = rows.get(row)?;
        // SAFETY: views under these identifiers are only ever `TextCell`s.
        let reused = unsafe { table.makeViewWithIdentifier_owner(&id, Some(self)) }
            .and_then(|v| v.downcast::<TextCell>().ok());
        let cell = reused.unwrap_or_else(|| TextCell::new(mtm, &id, col));
        let text = match col {
            Col::Title => &r.title,
            Col::Artists => &r.artists,
            Col::Album => &r.album,
            Col::Duration => &r.duration,
            Col::Add => unreachable!(),
        };
        let current = self.ivars().current.get() == Some(row);
        let played =
            self.ivars().kind == Kind::Queue && vm::played(row, self.ivars().current.get());
        cell.set_played(played);
        let iv = cell.ivars();
        if let Some(l) = iv.label.get() {
            widgets::set_text(l, text);
            if col == Col::Title {
                let spoken = spoken_title(text, current, played, r.badge);
                cell.setAccessibilityLabel(Some(&NSString::from_str(&spoken)));
            }
        }
        if let Some(bar) = iv.bar.get() {
            bar.setHidden(!current);
        }
        if let Some(cover) = iv.cover.get() {
            cover.set_url(r.artwork.as_deref());
        }
        if let Some(b) = iv.badge.get() {
            b.set(r.badge);
        }
        Some(Retained::into_super(cell))
    }
}

/// What VoiceOver reads for a title cell. The current and played rows, and the service a
/// badge shows, are otherwise marked only in paint.
fn spoken_title(title: &str, current: bool, played: bool, badge: Option<Service>) -> String {
    let mut spoken = if current {
        format!("Now playing, {title}")
    } else if played {
        format!("{title}, played")
    } else {
        title.to_owned()
    };
    if let Some(s) = badge {
        spoken.push_str(", ");
        spoken.push_str(s.name());
    }
    spoken
}

pub struct KeyTableIvars {
    focused: Cell<bool>,
    /// Focus came by the keyboard, or a key was pressed in the table since; only then does
    /// the ring show.
    by_key: Cell<bool>,
    ring: OnceCell<Retained<FocusRing>>,
}

/// Whether focus that arrives while handling an event of `kind` came by the keyboard: a key
/// press did it, no mouse did, and with neither in hand, as after a deferred change, a key
/// pressed `recently` did.
fn focus_by_key(kind: Option<NSEventType>, recently: bool) -> bool {
    match kind {
        Some(NSEventType::KeyDown) => true,
        Some(
            NSEventType::LeftMouseDown
            | NSEventType::LeftMouseUp
            | NSEventType::RightMouseDown
            | NSEventType::RightMouseUp
            | NSEventType::OtherMouseDown
            | NSEventType::OtherMouseUp,
        ) => false,
        _ => recently,
    }
}

/// Whether the table's ring shows: it has focus, which came by the keyboard, and no selected
/// row shows it instead.
fn ring_shown(focused: bool, by_key: bool, selected: bool) -> bool {
    focused && by_key && !selected
}

pub struct PassScrollIvars {
    pass: Cell<bool>,
}

define_class!(
    /// The rows' scroll view. Set to pass, it leaves the wheel to the page it sits in, as a
    /// table that shows every row does inside a page that scrolls.
    // SAFETY: NSScrollView has no subclassing requirements for `scrollWheel:`; no Drop impl.
    #[unsafe(super(NSScrollView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PassScrollIvars]
    pub struct PassScroll;

    impl PassScroll {
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            if self.ivars().pass.get() {
                // SAFETY: the next responder is a live responder or nil.
                if let Some(next) = unsafe { self.nextResponder() } {
                    next.scrollWheel(event);
                }
            } else {
                // SAFETY: `scrollWheel:` is NSScrollView's, called on its superclass.
                let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
            }
        }
    }
);

impl PassScroll {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PassScrollIvars {
            pass: Cell::new(false),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

define_class!(
    /// An outline over the rows that lets clicks through to them.
    // SAFETY: NSView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct FocusRing;

    impl FocusRing {
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> {
            None
        }
    }
);

define_class!(
    /// A table that tells its rows when it holds keyboard focus. The selected row turns from
    /// neutral to accent; with no row selected, a ring over the rows shows focus.
    // SAFETY: NSTableView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSTableView, objc2_app_kit::NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = KeyTableIvars]
    pub struct KeyTable;

    impl KeyTable {
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: forwards to the superclass implementation.
            let ok: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if ok {
                let kind = NSApplication::sharedApplication(self.mtm())
                    .currentEvent()
                    .map(|e| e.r#type());
                self.ivars()
                    .by_key
                    .set(focus_by_key(kind, widgets::key_quiet()));
                self.set_focused(true);
            }
            ok
        }

        /// A key in a table the mouse gave focus to shows the ring from then on.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.ivars().by_key.replace(true) {
                self.show_focus();
            }
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), keyDown: event] };
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: forwards to the superclass implementation.
            let ok: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if ok {
                self.set_focused(false);
            }
            ok
        }
    }
);

impl KeyTable {
    fn new_table(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSTableView> {
        let this = Self::alloc(mtm).set_ivars(KeyTableIvars {
            focused: Cell::new(false),
            by_key: Cell::new(false),
            ring: OnceCell::new(),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        Retained::into_super(this)
    }

    fn focused(&self) -> bool {
        self.ivars().focused.get()
    }

    fn set_focused(&self, focused: bool) {
        if self.ivars().focused.replace(focused) == focused {
            return;
        }
        for_visible_rows(self, |_, rv| {
            rv.ivars().focused.set(focused);
            rv.paint();
        });
        self.show_focus();
    }

    /// The ring, while the table has focus that came by the keyboard and no selected row to
    /// show it.
    fn show_focus(&self) {
        if let Some(ring) = self.ivars().ring.get() {
            let by_key = self.ivars().by_key.get();
            ring.setHidden(!ring_shown(self.focused(), by_key, self.selectedRow() >= 0));
        }
    }
}

fn for_visible_rows(table: &NSTableView, mut f: impl FnMut(NSInteger, &RowView)) {
    let range = table.rowsInRect(table.visibleRect());
    for row in range.location..range.location + range.length {
        let row = row as NSInteger;
        if let Some(rv) = table.rowViewAtRow_makeIfNecessary(row, false)
            && let Some(rv) = rv.downcast_ref::<RowView>()
        {
            f(row, rv);
        }
    }
}

pub struct RowIvars {
    odd: Cell<bool>,
    hovered: Cell<bool>,
    selected: Cell<bool>,
    current: Cell<bool>,
    /// The table holds keyboard focus.
    focused: Cell<bool>,
    /// The row's context menu is open.
    outlined: Cell<bool>,
}

define_class!(
    /// Zebra stripes, hover, keyboard selection and the queue's current row, all as the row's
    /// background colour.
    // SAFETY: NSTableRowView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSTableRowView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RowIvars]
    pub struct RowView;

    impl RowView {
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.set_hovered(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.set_hovered(false);
        }

        /// Background is a layer property; returning true keeps AppKit from giving every row
        /// a bitmap backing store.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            self.paint();
        }

        #[unsafe(method(prepareForReuse))]
        fn prepare_for_reuse(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), prepareForReuse] };
            // The pointer may have left while the row was off screen, with no `mouseExited:`.
            if self.ivars().outlined.replace(false) {
                self.paint();
            }
            self.set_hovered(false);
        }
    }
);

impl RowView {
    fn new(mtm: MainThreadMarker, id: &NSString) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RowIvars {
            odd: Cell::new(false),
            hovered: Cell::new(false),
            selected: Cell::new(false),
            current: Cell::new(false),
            focused: Cell::new(false),
            outlined: Cell::new(false),
        });
        // SAFETY: `initWithFrame:` is the designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        this.setIdentifier(Some(id));
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this
    }

    fn set_hovered(&self, hovered: bool) {
        self.ivars().hovered.set(hovered);
        self.paint();
        for v in self.subviews().iter() {
            if let Some(add) = v.downcast_ref::<AddCell>()
                && let Some(b) = add.ivars().button.get()
            {
                b.set_shown(hovered || add.ivars().added.get());
            }
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let selected = iv.selected.get();
        let base = if selected && !iv.focused.get() {
            // Selection without keyboard focus is neutral, as in AppKit.
            theme::LINE_2
        } else if iv.hovered.get() {
            theme::ROW_HOVER
        } else if iv.odd.get() || iv.current.get() {
            // The current row's tint over the darker zebra stripe would barely show.
            theme::BG_2
        } else {
            theme::BG
        };
        let accent = match (selected && iv.focused.get(), iv.current.get()) {
            // Selection over the current row's accent-soft tint, so the row stays marked.
            (true, true) => 1.0 - (1.0 - theme::SELECTED) * (1.0 - theme::SOFT),
            (true, false) => theme::SELECTED,
            (false, true) => theme::SOFT,
            (false, false) => 0.0,
        };
        widgets::fill(self, Some(&theme::mix(base, theme::ACCENT, accent)));
        let outline = if iv.outlined.get() { FOCUS_RING } else { 0.0 };
        widgets::border(self, &theme::accent(), outline);
        for v in self.subviews().iter() {
            if let Some(cell) = v.downcast_ref::<TextCell>() {
                cell.set_row_selected(selected);
            } else if let Some(cell) = v.downcast_ref::<AddCell>() {
                cell.set_row_selected(selected);
            }
        }
    }
}

pub struct TextCellIvars {
    label: OnceCell<Retained<NSTextField>>,
    bar: OnceCell<Retained<NSView>>,
    /// The title's artwork.
    cover: OnceCell<Retained<Cover>>,
    badge: OnceCell<Retained<Badge>>,
    /// Muted text, raised to ink-2 on a selected row.
    secondary: bool,
    row_selected: Cell<bool>,
    /// A queue row before the current one: dimmer text and cover.
    played: Cell<bool>,
}

define_class!(
    /// A cell: one truncating label, the row's 1 px bottom rule, and for the title column the
    /// artwork and the queue's 2 px current-row marker.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TextCellIvars]
    pub struct TextCell;
);

impl TextCell {
    fn new(mtm: MainThreadMarker, id: &NSString, col: Col) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TextCellIvars {
            label: OnceCell::new(),
            bar: OnceCell::new(),
            cover: OnceCell::new(),
            badge: OnceCell::new(),
            secondary: col != Col::Title,
            row_selected: Cell::new(false),
            played: Cell::new(false),
        });
        let w = 200.0;
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, w, ROW_H)] };
        this.setIdentifier(Some(id));
        this.setWantsLayer(true);
        let (font, color) = match col {
            Col::Title => (theme::font(14.0, Weight::Regular), theme::ink()),
            Col::Duration => (theme::tabular(14.0, Weight::Regular), theme::muted()),
            _ => (theme::font(14.0, Weight::Regular), theme::muted()),
        };
        let x = if col == Col::Title {
            PAD + THUMB + THUMB_GAP
        } else {
            PAD
        };
        let label = widgets::placed_label(
            mtm,
            "",
            &font,
            &color,
            x,
            ROW_H / 2.0,
            w - x - PAD,
            FLEX_W | PIN_TOP | PIN_BOTTOM,
        );
        if col == Col::Duration {
            label.setAlignment(NSTextAlignment::Right);
        }
        // A truncated cell shows its full text on hover.
        label.setAllowsExpansionToolTips(true);
        this.addSubview(&label);
        add_rule(mtm, &this, w);
        if col == Col::Title {
            let at = rect(PAD, ((ROW_H - THUMB) / 2.0).round(), THUMB, THUMB);
            let cover = widgets::cover(mtm, at, 4.0, 14.0);
            cover.setAutoresizingMask(PIN_LEFT);
            this.addSubview(&cover);
            let _ = this.ivars().cover.set(cover);
            // Clear of the row's rule.
            let badge = Badge::new(mtm, badge::frame(at, at.origin.y - 1.0, false));
            badge.setAutoresizingMask(PIN_LEFT);
            this.addSubview(&badge);
            let _ = this.ivars().badge.set(badge);
            let bar = widgets::panel(
                mtm,
                rect(0.0, 0.0, 2.0, ROW_H),
                FLEX_H | PIN_LEFT,
                &theme::accent(),
            );
            bar.setHidden(true);
            this.addSubview(&bar);
            let _ = this.ivars().bar.set(bar);
            // VoiceOver reads a label's text, never its accessibility label, so the cell speaks
            // for the title and can name the current row.
            widgets::hide_from_accessibility(&label);
            this.setAccessibilityElement(true);
            // SAFETY: the role static is an immutable framework constant.
            this.setAccessibilityRole(Some(unsafe { NSAccessibilityCellRole }));
        }
        let _ = this.ivars().label.set(label);
        this
    }

    fn set_row_selected(&self, selected: bool) {
        let iv = self.ivars();
        if iv.row_selected.replace(selected) == selected {
            return;
        }
        // The marker in accent would fall under 3:1 against the selection.
        if let Some(bar) = iv.bar.get() {
            let c = if selected {
                theme::accent_hi()
            } else {
                theme::accent()
            };
            widgets::fill(bar, Some(&c));
        }
        self.paint_text();
    }

    /// Dims a played queue row, or undims a reused cell that showed one.
    fn set_played(&self, played: bool) {
        let iv = self.ivars();
        iv.played.set(played);
        self.paint_text();
        let alpha = if played { PLAYED_COVER } else { 1.0 };
        if let Some(cover) = iv.cover.get() {
            cover.setAlphaValue(alpha);
        }
        if let Some(b) = iv.badge.get() {
            b.setAlphaValue(alpha);
        }
    }

    /// A selected row raises its text for contrast against the selection, played or not.
    fn paint_text(&self) {
        let iv = self.ivars();
        let Some(l) = iv.label.get() else { return };
        let c = match (iv.secondary, iv.row_selected.get(), iv.played.get()) {
            (false, false, true) => theme::muted(),
            (false, _, _) => theme::ink(),
            (true, true, _) => theme::ink_2(),
            (true, false, true) => theme::muted_2(),
            (true, false, false) => theme::muted(),
        };
        l.setTextColor(Some(&c));
    }
}

fn add_rule(mtm: MainThreadMarker, cell: &NSView, w: CGFloat) {
    let rule = widgets::panel(
        mtm,
        rect(0.0, 0.0, w, 1.0),
        FLEX_W | PIN_BOTTOM,
        &theme::line(),
    );
    cell.addSubview(&rule);
}

pub struct AddCellIvars {
    button: OnceCell<Retained<HoverButton>>,
    /// Showing the check that confirms an add.
    added: Cell<bool>,
    /// Bumped on every change, so a stale revert leaves a newer check alone.
    generation: Cell<u64>,
    row_selected: Cell<bool>,
}

define_class!(
    /// The results' cell before Time: a "+" ghost button shown while the row is hovered, or
    /// while it confirms an add with a check.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AddCellIvars]
    pub struct AddCell;
);

impl AddCell {
    /// Without a `target` the cell has no button.
    fn new(mtm: MainThreadMarker, id: &NSString, target: Option<&Controller>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AddCellIvars {
            button: OnceCell::new(),
            added: Cell::new(false),
            generation: Cell::new(0),
            row_selected: Cell::new(false),
        });
        let w = ADD_W;
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, w, ROW_H)] };
        this.setIdentifier(Some(id));
        this.setWantsLayer(true);
        add_rule(mtm, &this, w);
        let Some(target) = target else { return this };
        let b = HoverButton::new(
            mtm,
            rect((w - 28.0) / 2.0, (ROW_H - 28.0) / 2.0, 28.0, 28.0),
            Icon::Plus,
            ADD_ICON,
            &theme::muted(),
            None,
            &theme::line_2(),
            theme::RADIUS,
        );
        b.setAutoresizingMask(widgets::PIN_RIGHT | PIN_LEFT | PIN_TOP | PIN_BOTTOM);
        b.set_action(target, sel!(addClicked:));
        widgets::describe(&b, "Add to queue (⌘↩)", Some("Add to queue"));
        b.setHidden(true);
        this.addSubview(&b);
        let _ = this.ivars().button.set(b);
        this
    }

    /// Swaps "+" for a check for `ADDED_FOR`, fading both ways. The check shows without
    /// the pointer on the row, after a keyboard or menu add.
    fn confirm(&self) {
        self.fade_added(true);
        if let Some(b) = self.ivars().button.get() {
            b.set_shown(true);
        }
        let generation = self.ivars().generation.get();
        let Ok(when) = DispatchTime::try_from(ADDED_FOR) else {
            return;
        };
        let cell = MainThreadBound::new(self.retain(), self.mtm());
        let _ = DispatchQueue::main().after(when, move || {
            cell.get_on_main(|cell| {
                if cell.ivars().generation.get() == generation {
                    cell.fade_added(false);
                }
            });
        });
    }

    fn fade_added(&self, added: bool) {
        // The cell, not the button, so the button hiding fades too.
        widgets::crossfade(self, widgets::motion(widgets::MICRO));
        self.set_added(added);
        if !added && let Some(b) = self.ivars().button.get() {
            // SAFETY: a superview holds its subviews, so it is alive while `self` is in it.
            let row = unsafe { self.superview() };
            let hovered = row
                .and_then(|v| v.downcast::<RowView>().ok())
                .is_some_and(|rv| rv.ivars().hovered.get());
            b.set_shown(hovered);
        }
    }

    fn set_added(&self, added: bool) {
        let iv = self.ivars();
        iv.generation.set(iv.generation.get() + 1);
        if iv.added.replace(added) == added {
            return;
        }
        if let Some(b) = iv.button.get() {
            let icon = if added { Icon::Check } else { Icon::Plus };
            b.set_icon(icon, ADD_ICON);
            b.setContentTintColor(Some(&self.tint()));
        }
    }

    fn set_row_selected(&self, selected: bool) {
        let iv = self.ivars();
        if iv.row_selected.replace(selected) == selected || !iv.added.get() {
            return;
        }
        if let Some(b) = iv.button.get() {
            b.setContentTintColor(Some(&self.tint()));
        }
    }

    fn tint(&self) -> Retained<NSColor> {
        let iv = self.ivars();
        if !iv.added.get() {
            theme::muted()
        } else if iv.row_selected.get() {
            // The check in accent would fall under 3:1 against the selection, as the
            // current-row marker would.
            theme::accent_hi()
        } else {
            theme::accent()
        }
    }
}

pub struct HeaderIvars {
    table: OnceCell<Weak<NSTableView>>,
    /// Per column: title and whether it is right-aligned.
    titles: Vec<(&'static str, bool)>,
    kind: Kind,
}

define_class!(
    /// Column titles above the scroll view: uppercase, tracked, muted-2 on bg-2, with a 1 px
    /// rule below. Outside the scroll view because a table header view gets macOS 26's
    /// scroll-edge effect, which is translucent and cannot be rendered offscreen.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = HeaderIvars]
    pub struct ColumnHeader;

    impl ColumnHeader {
        #[unsafe(method(clipFrameChanged:))]
        fn clip_frame_changed(&self, _notification: &NSNotification) {
            self.fit_columns();
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            theme::bg_2().setFill();
            NSBezierPath::fillRect(b);
            theme::line().setFill();
            NSBezierPath::fillRect(rect(0.0, 0.0, b.size.width, 1.0));
            let Some(table) = self.ivars().table.get().and_then(Weak::load) else {
                return;
            };
            let size = 11.0;
            let attrs = widgets::attrs(
                &theme::font(size, Weight::Medium),
                // muted, not the spec's muted-2: 2.74:1 on bg-2 is too faint to read.
                &theme::muted(),
                Some(0.12 * size),
            );
            let columns = table.tableColumns();
            for (i, (title, right)) in self.ivars().titles.iter().enumerate() {
                if title.is_empty() || columns.objectAtIndex(i).isHidden() {
                    continue;
                }
                let col = table.rectOfColumn(i as NSInteger);
                // SAFETY: `attrs` maps the font, colour and kern keys to matching values.
                let s = unsafe {
                    NSAttributedString::new_with_attributes(
                        &NSString::from_str(&title.to_uppercase()),
                        &attrs,
                    )
                };
                let sz: NSSize = s.size();
                let x = if *right {
                    col.origin.x + col.size.width - PAD - sz.width
                } else {
                    col.origin.x + PAD
                };
                let y = (1.0 + (b.size.height - 1.0 - sz.height) / 2.0).round();
                s.drawAtPoint(NSPoint::new(x, y));
            }
        }
    }
);

impl ColumnHeader {
    /// Sizes the text columns to the visible width: title, artists and album at 46/30/24,
    /// with album dropped below `ALBUM_MIN_W`. Runs whenever the clip view resizes, which
    /// covers window resizes and a legacy scroller appearing.
    fn fit_columns(&self) {
        let Some(table) = self.ivars().table.get().and_then(Weak::load) else {
            return;
        };
        let Some(scroll) = table.enclosingScrollView() else {
            return;
        };
        let width = scroll.contentSize().width;
        let kind = self.ivars().kind;
        let widths = column_widths(width, kind.adds());
        for c in table.tableColumns().iter() {
            match Col::from_id(&c.identifier().to_string()) {
                Some(Col::Title) => c.setWidth(widths.title),
                Some(Col::Artists) => c.setWidth(widths.artists),
                Some(Col::Album) => {
                    c.setHidden(widths.album.is_none());
                    if let Some(w) = widths.album {
                        c.setWidth(w);
                    }
                }
                _ => {}
            }
        }
        self.setNeedsDisplay(true);
    }
}

const ADD_W: CGFloat = 44.0;
const ADD_ICON: CGFloat = 16.0;
const DURATION_W: CGFloat = 80.0;
/// Narrower tables drop the album column.
const ALBUM_MIN_W: CGFloat = 640.0;
/// How long a "+" shows its check after an add.
const ADDED_FOR: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Widths {
    title: CGFloat,
    artists: CGFloat,
    album: Option<CGFloat>,
}

/// Text-column widths for a table `width` wide, with or without the "+" column. They fill
/// what the fixed columns leave, rounding into the last, so the columns end flush with the
/// table.
fn column_widths(width: CGFloat, add: bool) -> Widths {
    let add_w = if add { ADD_W } else { 0.0 };
    let flex = (width - add_w - DURATION_W).max(0.0);
    if width < ALBUM_MIN_W {
        let title = (flex * 46.0 / 76.0).floor();
        Widths {
            title,
            artists: flex - title,
            album: None,
        }
    } else {
        let title = (flex * 0.46).floor();
        let artists = (flex * 0.30).floor();
        Widths {
            title,
            artists,
            album: Some(flex - title - artists),
        }
    }
}

pub struct Table {
    /// Header and scroll view.
    pub view: Retained<NSView>,
    header: Retained<ColumnHeader>,
    pub table: Retained<NSTableView>,
    pub controller: Retained<Controller>,
}

impl Table {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, kind: Kind) -> Table {
        let (w, h) = (frame.size.width, frame.size.height);
        let table = KeyTable::new_table(mtm, rect(0.0, 0.0, w, h));
        table.setStyle(NSTableViewStyle::Plain);
        table.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::None);
        table.setIntercellSpacing(NSSize::new(0.0, 0.0));
        table.setRowHeight(ROW_H);
        table.setBackgroundColor(&objc2_app_kit::NSColor::clearColor());
        table.setUsesAlternatingRowBackgroundColors(false);
        // `ColumnHeader::fit_columns` sizes the columns.
        table.setColumnAutoresizingStyle(NSTableViewColumnAutoresizingStyle::NoColumnAutoresizing);
        table.setAllowsColumnResizing(false);
        table.setAllowsColumnReordering(false);
        table.setAllowsMultipleSelection(false);
        table.setAllowsEmptySelection(true);
        table.setFocusRingType(NSFocusRingType::None);

        // Time stays the last column, flush with the table's right edge in both tables. Only
        // results have the "+" column.
        let cols: &[(Col, &str, CGFloat)] = &[
            (Col::Title, "Title", 0.0),
            (Col::Artists, "Artists", 0.0),
            (Col::Album, "Album", 0.0),
            (Col::Add, "", ADD_W),
            (Col::Duration, "Time", DURATION_W),
        ];
        let add = kind.adds();
        let mut titles = Vec::with_capacity(cols.len());
        for &(col, title, width) in cols.iter().filter(|c| add || c.0 != Col::Add) {
            let c = NSTableColumn::initWithIdentifier(
                NSTableColumn::alloc(mtm),
                &NSString::from_str(col.id()),
            );
            titles.push((title, col == Col::Duration));
            c.setWidth(width);
            if matches!(col, Col::Duration | Col::Add) {
                c.setMinWidth(width);
                c.setMaxWidth(width);
                c.setResizingMask(NSTableColumnResizingOptions::NoResizing);
            } else {
                c.setMinWidth(0.0);
                c.setResizingMask(NSTableColumnResizingOptions::NoResizing);
            }
            table.addTableColumn(&c);
        }
        table.setHeaderView(None);
        table.setCornerView(None);

        let controller = Controller::alloc(mtm).set_ivars(ControllerIvars {
            kind,
            rows: RefCell::new(Vec::new()),
            current: Cell::new(None),
            enabled: Cell::new(true),
            table: OnceCell::new(),
            menu_row: Cell::new(None),
        });
        // SAFETY: NSObject's `init`.
        let controller: Retained<Controller> = unsafe { msg_send![super(controller), init] };
        let _ = controller.ivars().table.set(Weak::from_retained(&table));
        // SAFETY: the table's data source, delegate and target, its menu's delegate and its menu
        // items' targets, are weak. The controller lives in the returned `Table` next to the table view, and the
        // installed `Ui` holds both tables until exit, so the references stay valid. It
        // implements the actions set here.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*controller)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            table.setTarget(Some(&controller));
            table.setDoubleAction(Some(sel!(rowDoubleClicked:)));
            let menu = context_menu(mtm, kind, &controller);
            menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            table.setMenu(Some(&menu));
        }

        let view = widgets::view(mtm, frame, FLEX_W | FLEX_H);
        let header = ColumnHeader::alloc(mtm).set_ivars(HeaderIvars {
            table: OnceCell::new(),
            titles,
            kind,
        });
        let hframe = rect(0.0, h - HEADER_H, w, HEADER_H);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let header: Retained<ColumnHeader> =
            unsafe { msg_send![super(header), initWithFrame: hframe] };
        header.setAutoresizingMask(FLEX_W | PIN_TOP);
        let _ = header.ivars().table.set(Weak::from_retained(&table));
        view.addSubview(&header);

        let scroll = PassScroll::new(mtm, rect(0.0, 0.0, w, h - HEADER_H));
        scroll.setAutoresizingMask(FLEX_W | FLEX_H);
        scroll.setDocumentView(Some(&table));
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setBorderType(NSBorderType::NoBorder);
        scroll.contentView().setDrawsBackground(false);
        scroll.setFocusRingType(NSFocusRingType::None);
        view.addSubview(&scroll);
        // Outset sideways, into the page gutter, so it never covers the current-row marker
        // on the rows' left edge, and lifted off the bottom, where the player bar's border is.
        let sf = scroll.frame();
        let ring_frame = rect(
            sf.origin.x - RING_OUTSET,
            sf.origin.y + RING_OUTSET,
            sf.size.width + 2.0 * RING_OUTSET,
            sf.size.height - RING_OUTSET,
        );
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let ring: Retained<FocusRing> =
            unsafe { msg_send![FocusRing::alloc(mtm), initWithFrame: ring_frame] };
        ring.setAutoresizingMask(FLEX_W | FLEX_H);
        view.setClipsToBounds(false);
        widgets::border(&ring, &theme::accent(), FOCUS_RING);
        ring.setHidden(true);
        view.addSubview(&ring);
        if let Some(t) = table.downcast_ref::<KeyTable>() {
            let _ = t.ivars().ring.set(ring);
        }
        let clip = scroll.contentView();
        // SAFETY: the header implements the selector, and it lives as long as the table, in
        // the installed `Ui`, until exit; the observation goes with it.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &header,
                sel!(clipFrameChanged:),
                Some(NSViewFrameDidChangeNotification),
                Some(&clip),
            );
        }
        header.fit_columns();
        Table {
            view,
            header,
            table,
            controller,
        }
    }

    pub fn set_rows(&self, rows: Vec<RowText>, current: Option<usize>) {
        let selected = usize::try_from(self.table.selectedRow()).ok();
        let was = selected.and_then(|i| self.controller.ivars().rows.borrow().get(i).cloned());
        self.controller.set_rows(rows, current);
        self.table.reloadData();
        // Reloading drops the selection. It stays with its track: on the same row, else where
        // that track moved, else nowhere. Callers that want a different one (a new search, an
        // edit's landing row) select or deselect after.
        if let Some(at) = selected
            && let Some(was) = was
            && self.table.selectedRow() < 0
        {
            let now = reselect(&self.controller.ivars().rows.borrow(), &was, at);
            if let Some(i) = now {
                self.table.selectRowIndexes_byExtendingSelection(
                    &NSIndexSet::indexSetWithIndex(i),
                    false,
                );
            }
        }
        if let Some(t) = self.table.downcast_ref::<KeyTable>() {
            t.show_focus();
        }
        // Column widths settle when the table tiles.
        self.header.setNeedsDisplay(true);
    }

    /// Dims and disables the rows' "+" buttons; playing rows goes through the view model,
    /// which drops it while disconnected.
    pub fn set_enabled(&self, enabled: bool) {
        if self.controller.ivars().enabled.replace(enabled) == enabled {
            return;
        }
        for_visible_rows(&self.table, |_, rv| {
            for v in rv.subviews().iter() {
                if let Some(add) = v.downcast_ref::<AddCell>()
                    && let Some(b) = add.ivars().button.get()
                {
                    b.set_enabled(enabled);
                }
            }
        });
    }

    pub fn selected_row(&self) -> Option<usize> {
        usize::try_from(self.table.selectedRow()).ok()
    }

    /// The height that shows `rows` rows under the header, with nothing to scroll.
    pub fn height_for(rows: usize) -> CGFloat {
        HEADER_H + rows as CGFloat * ROW_H
    }

    /// Leaves the wheel to the page the table sits in, or takes it back.
    pub fn set_passes_scroll(&self, pass: bool) {
        if let Some(scroll) = self.table.enclosingScrollView()
            && let Some(s) = scroll.downcast_ref::<PassScroll>()
        {
            s.ivars().pass.set(pass);
        }
    }

    /// Leaves `inset` below the last row, so it can scroll clear of an overlay.
    pub fn set_bottom_inset(&self, inset: CGFloat) {
        let Some(scroll) = self.table.enclosingScrollView() else {
            return;
        };
        scroll.setAutomaticallyAdjustsContentInsets(false);
        scroll.setContentInsets(NSEdgeInsets {
            top: 0.0,
            left: 0.0,
            bottom: inset,
            right: 0.0,
        });
    }

    /// Swaps row `row`'s "+" for a check, if the row is on screen.
    pub fn confirm_add(&self, row: usize) {
        let Ok(row) = NSInteger::try_from(row) else {
            return;
        };
        let col = self
            .table
            .columnWithIdentifier(&NSString::from_str(Col::Add.id()));
        if col < 0 {
            return;
        }
        if let Some(cell) = self
            .table
            .viewAtColumn_row_makeIfNecessary(col, row, false)
            .and_then(|v| v.downcast::<AddCell>().ok())
        {
            cell.confirm();
        }
    }

    /// Selects the first row unless a row is already selected.
    pub fn select_first(&self) {
        if self.table.numberOfRows() > 0 && self.table.selectedRow() < 0 {
            self.table
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(0), false);
            self.table.scrollRowToVisible(0);
        }
    }

    /// Selects the current row, else the first, unless a row is already selected.
    pub fn select_current(&self) {
        if self.table.selectedRow() < 0 {
            self.select(self.controller.ivars().current.get().unwrap_or(0));
        }
    }

    pub fn deselect(&self) {
        // SAFETY: a nil sender is allowed.
        unsafe { self.table.deselectAll(None) };
    }

    pub fn select(&self, row: usize) {
        let Ok(i) = NSInteger::try_from(row) else {
            return;
        };
        if i < self.table.numberOfRows() {
            self.table
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row), false);
            self.table.scrollRowToVisible(i);
        }
    }
}

/// The right-click menu. Its items act on the clicked row, which `Controller` validates.
fn context_menu(mtm: MainThreadMarker, kind: Kind, target: &Controller) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::new());
    // `None` is a separator.
    let items: &[Option<(&str, Sel)>] = match kind {
        Kind::Results | Kind::Collection => &[
            Some(("Play", sel!(contextPlay:))),
            Some(("Add to Queue", sel!(contextAdd:))),
        ],
        Kind::Queue => &[
            Some(("Play", sel!(contextPlay:))),
            Some(("Remove from Queue", sel!(contextRemove:))),
            Some(("Move Up", sel!(contextMoveUp:))),
            Some(("Move Down", sel!(contextMoveDown:))),
            None,
            Some(("Clear Queue", sel!(contextClear:))),
        ],
    };
    for &entry in items {
        let Some((title, action)) = entry else {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        };
        // SAFETY: `Controller` implements every action listed; `Table::new` covers the target.
        let item = unsafe {
            let i = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::new(),
            );
            i.setTarget(Some(target));
            i
        };
        menu.addItem(&item);
    }
    menu
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use requiem_core::{ProviderId, TrackId};

    use super::*;

    fn track(title: &str, artists: &[&str], album: Option<&str>, secs: Option<u64>) -> Track {
        Track {
            id: TrackId {
                provider: ProviderId("test".into()),
                id: title.into(),
            },
            title: title.into(),
            artists: artists.iter().map(|a| (*a).into()).collect(),
            artist_ids: Vec::new(),
            album: album.map(Into::into),
            duration: secs.map(Duration::from_secs),
            artwork_url: None,
        }
    }

    fn row(title: &str, artists: &str, album: &str, duration: &str) -> RowText {
        RowText {
            title: title.into(),
            artists: artists.into(),
            album: album.into(),
            duration: duration.into(),
            artwork: None,
            badge: None,
        }
    }

    fn named(titles: &[&str]) -> Vec<RowText> {
        titles.iter().map(|t| row(t, "", "", "")).collect()
    }

    #[test]
    fn only_focus_that_came_by_the_keyboard_rings_the_table() {
        // Tab, ↓ from the field, and an open by Return all land during the key press.
        assert!(focus_by_key(Some(NSEventType::KeyDown), false));
        // A click opens a page on the mouse's release, or focuses the table on its press,
        // whatever key went just before.
        for mouse in [
            NSEventType::LeftMouseDown,
            NSEventType::LeftMouseUp,
            NSEventType::RightMouseUp,
            NSEventType::OtherMouseUp,
        ] {
            assert!(!focus_by_key(Some(mouse), true), "{mouse:?}");
        }
        // Deferred, focus goes by whether a key was just pressed.
        assert!(focus_by_key(None, true));
        assert!(!focus_by_key(None, false));
        assert!(!focus_by_key(Some(NSEventType::AppKitDefined), false));

        assert!(ring_shown(true, true, false));
        // A mouse-opened page shows no ring.
        assert!(!ring_shown(true, false, false));
        // A selected row shows focus instead, and an unfocused table shows none.
        assert!(!ring_shown(true, true, true));
        assert!(!ring_shown(false, true, false));
    }

    #[test]
    fn reselect_keeps_the_same_row() {
        let rows = named(&["a", "b", "c"]);
        assert_eq!(reselect(&rows, &row("c", "", "", ""), 2), Some(2));
    }

    #[test]
    fn reselect_follows_its_track_up_when_a_row_above_goes() {
        let rows = named(&["b", "c", "d"]);
        assert_eq!(reselect(&rows, &row("c", "", "", ""), 2), Some(1));
    }

    #[test]
    fn reselect_drops_a_removed_track() {
        let rows = named(&["a", "b", "d"]);
        assert_eq!(reselect(&rows, &row("c", "", "", ""), 2), None);
    }

    #[test]
    fn reselect_follows_its_track_down() {
        let rows = named(&["x", "y", "a", "b", "c"]);
        assert_eq!(reselect(&rows, &row("c", "", "", ""), 2), Some(4));
    }

    #[test]
    fn reselect_picks_the_nearest_duplicate() {
        let rows = named(&["c", "a", "b", "c", "c", "c"]);
        assert_eq!(reselect(&rows, &row("c", "", "", ""), 2), Some(3));
    }

    #[test]
    fn only_the_current_title_reads_as_now_playing() {
        assert_eq!(spoken_title("One", true, false, None), "Now playing, One");
        assert_eq!(spoken_title("One", false, false, None), "One");
        assert_eq!(spoken_title("One", false, true, None), "One, played");
    }

    #[test]
    fn a_badged_title_ends_with_its_service() {
        let spotify = Some(Service::Spotify);
        assert_eq!(spoken_title("One", false, false, spotify), "One, Spotify");
        assert_eq!(
            spoken_title("One", true, false, Some(Service::YouTubeMusic)),
            "Now playing, One, YouTube Music"
        );
        assert_eq!(
            spoken_title("One", false, true, spotify),
            "One, played, Spotify"
        );
    }

    #[test]
    fn columns_split_46_30_24_and_fill_the_table() {
        let w = column_widths(800.0, true);
        assert_eq!(w.title, 310.0);
        assert_eq!(w.artists, 202.0);
        assert_eq!(w.album, Some(164.0));
        assert_eq!(
            w.title + w.artists + w.album.unwrap() + ADD_W + DURATION_W,
            800.0
        );
    }

    #[test]
    fn the_queue_gives_the_plus_column_to_the_text() {
        let w = column_widths(800.0, false);
        assert_eq!(w.title, 331.0);
        assert_eq!(w.artists, 216.0);
        assert_eq!(w.album, Some(173.0));
        assert_eq!(w.title + w.artists + w.album.unwrap() + DURATION_W, 800.0);
    }

    #[test]
    fn narrow_tables_drop_the_album_column() {
        let w = column_widths(ALBUM_MIN_W - 1.0, true);
        assert_eq!(w.album, None);
        assert_eq!(w.title + w.artists + ADD_W + DURATION_W, ALBUM_MIN_W - 1.0);
        assert!(w.title > w.artists);
        assert!(column_widths(ALBUM_MIN_W, true).album.is_some());
    }

    #[test]
    fn rows_format_each_column_and_blank_missing_fields() {
        let rows = rows(&[
            track("One", &["A", "B"], Some("LP"), Some(225)),
            track("Two", &[], None, None),
            track("Long", &["C"], None, Some(3725)),
        ]);
        assert_eq!(
            rows,
            vec![
                row("One", "A, B", "LP", "3:45"),
                row("Two", "", "", ""),
                row("Long", "C", "", "1:02:05"),
            ]
        );
    }

    fn from(provider: &str, title: &str) -> Track {
        let mut t = track(title, &[], None, None);
        t.id.provider = ProviderId(provider.into());
        t
    }

    #[test]
    fn rows_badge_each_tracks_service_only_in_a_mixed_list() {
        let mixed = rows(&[from("spotify", "One"), from("ytmusic", "Two")]);
        assert_eq!(mixed[0].badge, Some(Service::Spotify));
        assert_eq!(mixed[1].badge, Some(Service::YouTubeMusic));
        let one = rows(&[from("spotify", "One"), from("spotify", "Two")]);
        assert!(one.iter().all(|r| r.badge.is_none()));
        // A provider with no service gets no badge, though it makes the list mixed.
        let odd = rows(&[from("spotify", "One"), from("other", "Two")]);
        assert_eq!(odd[0].badge, Some(Service::Spotify));
        assert_eq!(odd[1].badge, None);
    }

    #[test]
    fn rows_carry_the_artwork_url() {
        let mut t = track("One", &["A"], None, None);
        t.artwork_url = Some("https://lh3.googleusercontent.com/x=w120-h120".into());
        let rows = rows(&[t, track("Two", &[], None, None)]);
        assert_eq!(
            rows[0].artwork.as_deref(),
            Some("https://lh3.googleusercontent.com/x=w120-h120")
        );
        assert_eq!(rows[1].artwork, None);
    }
}
