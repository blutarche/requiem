//! The results and queue tables: view-based `NSTableView`s with themed rows, cells and
//! headers.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSBorderType, NSControlTextEditingDelegate,
    NSEvent, NSFocusRingType, NSResponder, NSScrollView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableRowView, NSTableView, NSTableViewColumnAutoresizingStyle,
    NSTableViewDataSource, NSTableViewDelegate, NSTableViewSelectionHighlightStyle,
    NSTableViewStyle, NSTextAlignment, NSTextField, NSTrackingAreaOptions,
    NSUserInterfaceItemIdentification, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSIndexSet, NSInteger, NSNotification, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString,
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
        #[unsafe(method(rowClicked:))]
        fn row_clicked(&self, _sender: Option<&AnyObject>) {
            let Some(table) = self.table() else { return };
            if let (Kind::Queue, Ok(row)) = (self.ivars().kind, usize::try_from(table.clickedRow())) {
                ui::with(|ui| ui.intent(Intent::PlayQueue(row)));
            }
        }

        #[unsafe(method(rowDoubleClicked:))]
        fn row_double_clicked(&self, _sender: Option<&AnyObject>) {
            let Some(table) = self.table() else { return };
            if let (Kind::Results, Ok(row)) = (self.ivars().kind, usize::try_from(table.clickedRow())) {
                ui::with(|ui| ui.intent(Intent::PlayResult(row)));
            }
        }

        #[unsafe(method(addClicked:))]
        fn add_clicked(&self, sender: Option<&NSView>) {
            let (Some(table), Some(sender)) = (self.table(), sender) else { return };
            if let Ok(row) = usize::try_from(table.rowForView(sender)) {
                ui::with(|ui| ui.intent(Intent::AppendResult(row)));
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
            let cell = reused.unwrap_or_else(|| AddCell::new(mtm, &id, self));
            // A reused cell may come from a hovered row.
            cell.ivars().button.get()?.setHidden(true);
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
            self.ivars().hovered.set(false);
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
                b.setHidden(!hovered);
            }
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let base = if iv.hovered.get() {
            theme::BG_3
        } else if iv.odd.get() {
            theme::BG_2
        } else {
            theme::BG
        };
        let color = if iv.selected.get() {
            theme::mix(base, theme::ACCENT, theme::SELECTED)
        } else if iv.current.get() {
            theme::mix(base, theme::ACCENT, theme::SOFT)
        } else {
            theme::mix(base, base, 0.0)
        };
        widgets::fill(self, Some(&color));
    }
}

pub struct TextCellIvars {
    label: OnceCell<Retained<NSTextField>>,
    bar: OnceCell<Retained<NSView>>,
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
}

define_class!(
    /// The results table's cell before Time: a "+" ghost button shown while the row is
    /// hovered.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AddCellIvars]
    pub struct AddCell;
);

impl AddCell {
    fn new(mtm: MainThreadMarker, id: &NSString, target: &Controller) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AddCellIvars {
            button: OnceCell::new(),
        });
        let w = ADD_W;
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, w, ROW_H)] };
        this.setIdentifier(Some(id));
        this.setWantsLayer(true);
        let b = HoverButton::new(
            mtm,
            rect((w - 28.0) / 2.0, (ROW_H - 28.0) / 2.0, 28.0, 28.0),
            Icon::Plus,
            16.0,
            &theme::muted(),
            None,
            &theme::line_2(),
            theme::RADIUS,
        );
        b.setAutoresizingMask(widgets::PIN_RIGHT | PIN_LEFT | PIN_TOP | PIN_BOTTOM);
        b.set_action(target, sel!(addClicked:));
        b.setHidden(true);
        this.addSubview(&b);
        add_rule(mtm, &this, w);
        let _ = this.ivars().button.set(b);
        this
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
            for (i, (title, right)) in self.ivars().titles.iter().enumerate() {
                if title.is_empty() {
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

const ADD_W: CGFloat = 44.0;
const DURATION_W: CGFloat = 80.0;

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
        let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), rect(0.0, 0.0, w, h));
        table.setStyle(NSTableViewStyle::Plain);
        table.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::None);
        table.setIntercellSpacing(NSSize::new(0.0, 0.0));
        table.setRowHeight(ROW_H);
        table.setBackgroundColor(&objc2_app_kit::NSColor::clearColor());
        table.setUsesAlternatingRowBackgroundColors(false);
        table.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::UniformColumnAutoresizingStyle,
        );
        table.setAllowsColumnResizing(false);
        table.setAllowsColumnReordering(false);
        table.setAllowsMultipleSelection(false);
        table.setAllowsEmptySelection(true);
        table.setFocusRingType(NSFocusRingType::None);

        let fixed = DURATION_W + if kind == Kind::Results { ADD_W } else { 0.0 };
        let flex = w - fixed;
        // Time stays the last column, flush with the table's right edge in both tables.
        let cols: &[(Col, &str, CGFloat)] = &[
            (Col::Title, "Title", (flex * 0.42).floor()),
            (Col::Artists, "Artists", (flex * 0.29).floor()),
            (Col::Album, "Album", (flex * 0.29).floor()),
            (Col::Add, "", ADD_W),
            (Col::Duration, "Time", DURATION_W),
        ];
        let mut titles = Vec::with_capacity(cols.len());
        for &(col, title, width) in cols {
            if col == Col::Add && kind == Kind::Queue {
                continue;
            }
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
                c.setMinWidth(80.0);
                c.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
            }
            table.addTableColumn(&c);
        }
        table.setHeaderView(None);
        table.setCornerView(None);

        let controller = Controller::alloc(mtm).set_ivars(ControllerIvars {
            kind,
            rows: RefCell::new(Vec::new()),
            current: Cell::new(None),
            table: OnceCell::new(),
        });
        // SAFETY: NSObject's `init`.
        let controller: Retained<Controller> = unsafe { msg_send![super(controller), init] };
        let _ = controller.ivars().table.set(Weak::from_retained(&table));
        // SAFETY: the controller is retained by the `Ui` for the life of the process, so the
        // table's weak data source and delegate references stay valid; it implements the
        // actions set here.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*controller)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            table.setTarget(Some(&controller));
            match kind {
                Kind::Results => table.setDoubleAction(Some(sel!(rowDoubleClicked:))),
                Kind::Queue => table.setAction(Some(sel!(rowClicked:))),
            }
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
