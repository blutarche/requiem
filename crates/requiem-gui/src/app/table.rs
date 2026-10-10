//! The results and queue tables: view-based `NSTableView`s with themed rows, cells and
//! headers.

use std::cell::{Cell, OnceCell, RefCell};
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSBorderType, NSControlTextEditingDelegate,
    NSEvent, NSFocusRingType, NSResponder, NSScrollView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableRowView, NSTableView, NSTableViewColumnAutoresizingStyle,
    NSTableViewDataSource, NSTableViewDelegate, NSTableViewSelectionHighlightStyle,
    NSTableViewStyle, NSTextAlignment, NSTextField, NSTrackingAreaOptions,
    NSUserInterfaceItemIdentification, NSView, NSViewFrameDidChangeNotification,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSIndexSet, NSInteger, NSNotification,
    NSNotificationCenter, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use requiem_core::Track;

use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{self, FLEX_H, FLEX_W, HoverButton, PIN_BOTTOM, PIN_LEFT, PIN_TOP, rect};
use crate::vm::{self, Intent};

pub const ROW_H: CGFloat = 36.0;
const HEADER_H: CGFloat = 32.0;
const PAD: CGFloat = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Results,
    Queue,
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

/// One row's text, formatted once per list change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowText {
    title: String,
    artists: String,
    album: String,
    duration: String,
}

pub fn rows(tracks: &[Track]) -> Vec<RowText> {
    tracks
        .iter()
        .map(|t| RowText {
            title: t.title.clone(),
            artists: vm::artists(t),
            album: t.album.clone().unwrap_or_default(),
            duration: t.duration.map(vm::fmt_time).unwrap_or_default(),
        })
        .collect()
}

pub struct ControllerIvars {
    kind: Kind,
    rows: RefCell<Vec<RowText>>,
    current: Cell<Option<usize>>,
    /// Whether the rows' "+" buttons take clicks.
    enabled: Cell<bool>,
    table: OnceCell<Weak<NSTableView>>,
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
            }
        }
    }

    impl Controller {
        #[unsafe(method(rowDoubleClicked:))]
        fn row_double_clicked(&self, _sender: Option<&AnyObject>) {
            let Some(table) = self.table() else { return };
            let Ok(row) = usize::try_from(table.clickedRow()) else { return };
            match self.ivars().kind {
                Kind::Results => ui::with(|ui| ui.intent(Intent::PlayResult(row))),
                Kind::Queue => ui::with(|ui| ui.play_queue(row)),
            };
        }

        #[unsafe(method(addClicked:))]
        fn add_clicked(&self, sender: Option<&NSView>) {
            let (Some(table), Some(sender)) = (self.table(), sender) else { return };
            let row = table.rowForView(sender);
            let Ok(index) = usize::try_from(row) else { return };
            ui::with(|ui| ui.intent(Intent::AppendResult(index)));
            let col = table.columnWithIdentifier(&NSString::from_str(Col::Add.id()));
            if let Some(cell) = table
                .viewAtColumn_row_makeIfNecessary(col, row, false)
                .and_then(|v| v.downcast::<AddCell>().ok())
            {
                cell.confirm();
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
            let target = (self.ivars().kind == Kind::Results).then_some(self);
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
        let iv = cell.ivars();
        if let Some(l) = iv.label.get() {
            widgets::set_text(l, text);
        }
        if let Some(bar) = iv.bar.get() {
            bar.setHidden(self.ivars().current.get() != Some(row));
        }
        Some(Retained::into_super(cell))
    }
}

pub struct KeyTableIvars {
    focused: Cell<bool>,
}

define_class!(
    /// A table that tells its rows when it holds keyboard focus. It draws no focus ring;
    /// the selected row turns from neutral to accent instead.
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
                self.set_focused(true);
            }
            ok
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
                b.set_shown(hovered);
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
            theme::BG_3
        } else if iv.odd.get() {
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
        for v in self.subviews().iter() {
            if let Some(cell) = v.downcast_ref::<TextCell>() {
                cell.set_row_selected(selected);
            }
        }
    }
}

pub struct TextCellIvars {
    label: OnceCell<Retained<NSTextField>>,
    bar: OnceCell<Retained<NSView>>,
    /// Muted text, raised to ink-2 on a selected row.
    secondary: bool,
    row_selected: Cell<bool>,
}

define_class!(
    /// A cell: one truncating label, the row's 1 px bottom rule, and for the title column the
    /// queue's 2 px current-row marker.
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
            secondary: col != Col::Title,
            row_selected: Cell::new(false),
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
        let label = widgets::placed_label(
            mtm,
            "",
            &font,
            &color,
            PAD,
            ROW_H / 2.0,
            w - 2.0 * PAD,
            FLEX_W | PIN_TOP | PIN_BOTTOM,
        );
        if col == Col::Duration {
            label.setAlignment(NSTextAlignment::Right);
        }
        this.addSubview(&label);
        add_rule(mtm, &this, w);
        if col == Col::Title {
            let bar = widgets::panel(
                mtm,
                rect(0.0, 0.0, 2.0, ROW_H),
                FLEX_H | PIN_LEFT,
                &theme::accent(),
            );
            bar.setHidden(true);
            this.addSubview(&bar);
            let _ = this.ivars().bar.set(bar);
        }
        let _ = this.ivars().label.set(label);
        this
    }

    fn set_row_selected(&self, selected: bool) {
        let iv = self.ivars();
        if !iv.secondary || iv.row_selected.replace(selected) == selected {
            return;
        }
        if let Some(l) = iv.label.get() {
            let c = if selected {
                theme::ink_2()
            } else {
                theme::muted()
            };
            l.setTextColor(Some(&c));
        }
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
}

define_class!(
    /// The cell before Time: in results, a "+" ghost button shown while the row is hovered;
    /// in the queue, empty, so both tables keep the same columns.
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

    /// Swaps "+" for a check for `ADDED_FOR`, fading both ways.
    fn confirm(&self) {
        self.fade_added(true);
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
        if let Some(b) = self.ivars().button.get() {
            widgets::crossfade(b, widgets::MICRO);
        }
        self.set_added(added);
    }

    fn set_added(&self, added: bool) {
        let iv = self.ivars();
        iv.generation.set(iv.generation.get() + 1);
        if iv.added.replace(added) == added {
            return;
        }
        if let Some(b) = iv.button.get() {
            let (icon, tint) = if added {
                (Icon::Check, theme::accent())
            } else {
                (Icon::Plus, theme::muted())
            };
            b.set_icon(icon, ADD_ICON);
            b.setContentTintColor(Some(&tint));
        }
    }
}

pub struct HeaderIvars {
    table: OnceCell<Weak<NSTableView>>,
    /// Per column: title and whether it is right-aligned.
    titles: Vec<(&'static str, bool)>,
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
                &theme::muted_2(),
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
        let widths = column_widths(scroll.contentSize().width);
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

/// Text-column widths for a table `width` wide. They fill what the fixed columns leave,
/// rounding into the last, so the columns end flush with the table.
fn column_widths(width: CGFloat) -> Widths {
    let flex = (width - ADD_W - DURATION_W).max(0.0);
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

        // Time stays the last column, flush with the table's right edge in both tables.
        let cols: &[(Col, &str, CGFloat)] = &[
            (Col::Title, "Title", 0.0),
            (Col::Artists, "Artists", 0.0),
            (Col::Album, "Album", 0.0),
            (Col::Add, "", ADD_W),
            (Col::Duration, "Time", DURATION_W),
        ];
        let mut titles = Vec::with_capacity(cols.len());
        for &(col, title, width) in cols {
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
        });
        // SAFETY: NSObject's `init`.
        let controller: Retained<Controller> = unsafe { msg_send![super(controller), init] };
        let _ = controller.ivars().table.set(Weak::from_retained(&table));
        // SAFETY: the table's data source, delegate and target are weak. The controller lives
        // in the returned `Table` next to the table view, and the installed `Ui` holds both
        // tables until exit, so the references stay valid. It implements the actions set here.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*controller)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            table.setTarget(Some(&controller));
            table.setDoubleAction(Some(sel!(rowDoubleClicked:)));
        }

        let view = widgets::view(mtm, frame, FLEX_W | FLEX_H);
        let header = ColumnHeader::alloc(mtm).set_ivars(HeaderIvars {
            table: OnceCell::new(),
            titles,
        });
        let hframe = rect(0.0, h - HEADER_H, w, HEADER_H);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let header: Retained<ColumnHeader> =
            unsafe { msg_send![super(header), initWithFrame: hframe] };
        header.setAutoresizingMask(FLEX_W | PIN_TOP);
        let _ = header.ivars().table.set(Weak::from_retained(&table));
        view.addSubview(&header);

        let scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, w, h - HEADER_H));
        scroll.setAutoresizingMask(FLEX_W | FLEX_H);
        scroll.setDocumentView(Some(&table));
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setBorderType(NSBorderType::NoBorder);
        scroll.contentView().setDrawsBackground(false);
        scroll.setFocusRingType(NSFocusRingType::None);
        view.addSubview(&scroll);
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
        self.controller.set_rows(rows, current);
        self.table.reloadData();
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

    /// Selects the first row unless a row is already selected.
    pub fn select_first(&self) {
        if self.table.numberOfRows() > 0 && self.table.selectedRow() < 0 {
            self.table
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(0), false);
            self.table.scrollRowToVisible(0);
        }
    }

    pub fn deselect(&self) {
        // SAFETY: a nil sender is allowed.
        unsafe { self.table.deselectAll(None) };
    }
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
        }
    }

    #[test]
    fn columns_split_46_30_24_and_fill_the_table() {
        let w = column_widths(800.0);
        assert_eq!(w.title, 310.0);
        assert_eq!(w.artists, 202.0);
        assert_eq!(w.album, Some(164.0));
        assert_eq!(
            w.title + w.artists + w.album.unwrap() + ADD_W + DURATION_W,
            800.0
        );
    }

    #[test]
    fn narrow_tables_drop_the_album_column() {
        let w = column_widths(ALBUM_MIN_W - 1.0);
        assert_eq!(w.album, None);
        assert_eq!(w.title + w.artists + ADD_W + DURATION_W, ALBUM_MIN_W - 1.0);
        assert!(w.title > w.artists);
        assert!(column_widths(ALBUM_MIN_W).album.is_some());
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
}
