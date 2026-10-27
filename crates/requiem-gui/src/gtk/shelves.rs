//! Shelves of cards, paged sideways: Home, the All view of search results, and an artist's
//! albums. Cards exist only for the columns near the visible area of shelves near the
//! visible part of the page, so covers load only there.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};
use requiem_core::{CollectionKind, Item, Shelf};

use super::art::Cover;
use super::icons::{self, Icon};
use super::theme;
use super::ui;
use super::widgets;
use crate::view::{card_parts, is_artist, is_grid, shown_subtitle};
use crate::vm::{self, List, Service};

pub const PAD: f64 = 24.0;
pub const CARD_W: i32 = 128;
/// A card's hover fill reaches this far past its content, on every side.
const INSET: f64 = 6.0;
const SHELF_GAP: i32 = 28;
const GRID_COL_W: f64 = 300.0;
const GRID_ROWS: usize = 4;
const GRID_ROW_H: i32 = 56;
const GRID_COVER: i32 = 44;
/// How far past the visible area cards are kept, to each side.
const SIDE_MARGIN: f64 = 72.0;
const PAGE_TIME: Duration = Duration::from_millis(250);

/// The page whose shelves a `Shelves` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Home,
    Search,
    Artist,
}

/// How a row's columns fit the width it shows: as many as fit at least `PAD` apart, the room
/// left spread into the gaps, so a page holds whole columns. As on macOS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub count: usize,
    pub pitch: f64,
}

impl Fit {
    pub fn of(width: f64, col_w: f64) -> Fit {
        let room = width - 2.0 * PAD;
        let count = ((room + PAD) / (col_w + PAD)).floor().max(1.0) as usize;
        let pitch = if count > 1 {
            let gap = ((room - count as f64 * col_w) / (count - 1) as f64)
                .floor()
                .max(PAD);
            (col_w + gap).max(((width - PAD) / count as f64).ceil())
        } else {
            (width - PAD).max(col_w + PAD)
        };
        Fit { count, pitch }
    }

    /// The scrollable width for `cols` columns in a row `width` wide.
    pub fn content_w(self, width: f64, col_w: f64, cols: usize) -> f64 {
        if cols > self.count {
            width + (cols - self.count) as f64 * self.pitch
        } else {
            (2.0 * PAD + cols.saturating_sub(1) as f64 * self.pitch + col_w).min(width)
        }
    }

    /// The offset that shows column `i` whole from `offset`.
    pub fn reveal(self, offset: f64, width: f64, col_w: f64, i: usize, max: f64) -> f64 {
        let x = PAD + i as f64 * self.pitch;
        let to = if x < offset {
            i as f64 * self.pitch
        } else if x + col_w > offset + width {
            (i + 1).saturating_sub(self.count) as f64 * self.pitch
        } else {
            offset
        };
        to.clamp(0.0, max.max(0.0))
    }
}

fn col_w(grid: bool) -> f64 {
    if grid { GRID_COL_W } else { f64::from(CARD_W) }
}

/// A card: a 128-point cover, its title on one line and a muted subtitle on up to two.
pub struct Card {
    pub root: gtk::Box,
    pub cover: Cover,
}

impl Card {
    pub fn new(item: &Item, artists: bool, badge: Option<Service>) -> Card {
        Card::sized(item, artists, badge, CARD_W)
    }

    /// A card whose cover is `side` points square.
    pub fn sized(item: &Item, artists: bool, badge: Option<Service>, side: i32) -> Card {
        let (title, subtitle, art) = card_parts(item);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("card");
        root.set_size_request(side + 2 * INSET as i32, -1);
        let cover = Cover::new(side, if is_artist(item) { &["round"] } else { &[] }, true);
        cover.set_url(art);
        cover.set_badge(badge);
        let t = theme::label(title, &["card-title"]);
        t.set_margin_top(8);
        t.set_size_request(side, -1);
        t.set_max_width_chars(1);
        let s = theme::label(shown_subtitle(&subtitle, artists), &["card-sub"]);
        theme::two_lines(&s);
        s.set_max_width_chars(1);
        s.set_size_request(side, -1);
        s.set_margin_top(2);
        s.set_lines(2);
        s.set_valign(gtk::Align::Start);
        root.append(&cover.root);
        root.append(&t);
        root.append(&s);
        root.set_tooltip_text(Some(title));
        Card { root, cover }
    }
}

/// A compact row of a grid shelf: a 44-point cover, the title and the artists.
fn tile(item: &Item, badge: Option<Service>) -> (gtk::Box, Cover) {
    let (title, subtitle, art) = card_parts(item);
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    root.add_css_class("tile");
    root.set_size_request(GRID_COL_W as i32, GRID_ROW_H);
    let cover = Cover::new(GRID_COVER, &["small"], true);
    cover.set_url(art);
    cover.set_badge(badge);
    cover.root.set_valign(gtk::Align::Center);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    let t = theme::label(title, &["title"]);
    t.set_max_width_chars(1);
    let s = theme::label(&subtitle, &["subtitle"]);
    s.set_max_width_chars(1);
    text.append(&t);
    text.append(&s);
    root.append(&cover.root);
    root.append(&text);
    root.set_tooltip_text(Some(title));
    (root, cover)
}

/// The natural height of a card, measured once from a sample.
fn card_h() -> i32 {
    thread_local! {
        static H: Cell<i32> = const { Cell::new(0) };
    }
    if H.get() == 0 {
        let sample = Item::Collection(requiem_core::Collection {
            id: requiem_core::CollectionId {
                provider: requiem_core::ProviderId("x".into()),
                id: String::new(),
            },
            kind: CollectionKind::Playlist,
            title: "Title".into(),
            subtitle: Some("Two\nlines".into()),
            artwork_url: None,
        });
        let c = Card::new(&sample, false, None);
        let (_, nat, _, _) = c.root.measure(gtk::Orientation::Vertical, -1);
        H.set(nat.max(CARD_W + 60));
    }
    H.get()
}

/// What a strip shows and where it is.
pub struct Model {
    surface: Surface,
    shelf: usize,
    items: Vec<Item>,
    grid: bool,
    artists: bool,
    mixed: bool,
}

/// A column's widgets, and the covers in them.
pub struct Column {
    widget: gtk::Widget,
    covers: Vec<Cover>,
    /// The column's cards or tiles, by item index.
    cells: Vec<(usize, gtk::Widget)>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Strip {
        pub model: RefCell<Option<Rc<Model>>>,
        pub columns: RefCell<BTreeMap<usize, Column>>,
        pub offset: Cell<f64>,
        pub fit: Cell<Option<Fit>>,
        pub width: Cell<f64>,
        pub near: Cell<bool>,
        pub filling: Cell<bool>,
        pub selected: Cell<Option<usize>>,
        pub animation: Cell<u64>,
        pub on_change: RefCell<Option<Box<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Strip {
        const NAME: &'static str = "RequiemStrip";
        type Type = super::Strip;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Strip {
        fn dispose(&self) {
            self.columns.borrow_mut().clear();
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Strip {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                return (0, 0, -1, -1);
            }
            let h = match &*self.model.borrow() {
                Some(m) if m.grid => GRID_ROWS.min(m.items.len()) as i32 * GRID_ROW_H,
                Some(_) => card_h(),
                None => 0,
            };
            (h, h, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let obj = self.obj();
            let Some(model) = self.model.borrow().clone() else {
                return;
            };
            let w = f64::from(width);
            let cw = col_w(model.grid);
            let fit = Fit::of(w, cw);
            let widened = self.width.replace(w) != w;
            let resized = self.fit.replace(Some(fit)) != Some(fit) || widened;
            if resized {
                // Keeps the offset on a column after a resize.
                let max = obj.max_offset();
                let col = (self.offset.get() / fit.pitch).round();
                self.offset.set((col * fit.pitch).clamp(0.0, max));
            }
            let offset = self.offset.get();
            let want = obj.wanted();
            let columns = self.columns.borrow();
            for (&c, col) in columns.iter() {
                let x = PAD + c as f64 * fit.pitch - offset;
                let (x, cw_alloc, h) = if model.grid {
                    (x, GRID_COL_W, height)
                } else {
                    (x - INSET, cw + 2.0 * INSET, height)
                };
                let shown = want.contains(&c);
                col.widget.set_child_visible(shown);
                if shown {
                    col.widget.size_allocate(
                        &gtk::Allocation::new(x.round() as i32, 0, cw_alloc as i32, h),
                        -1,
                    );
                }
            }
            let missing = want.clone().any(|c| !columns.contains_key(&c));
            let extra = columns.keys().any(|c| !want.contains(c));
            drop(columns);
            if (missing || extra) && !self.filling.replace(true) {
                let weak = obj.downgrade();
                theme::later(move || {
                    if let Some(s) = weak.upgrade() {
                        s.imp().filling.set(false);
                        s.fill();
                    }
                });
            }
            if resized {
                // Not inside the allocation: it shows and hides the paging buttons.
                let weak = obj.downgrade();
                theme::later(move || {
                    if let Some(s) = weak.upgrade()
                        && let Some(f) = &*s.imp().on_change.borrow()
                    {
                        f();
                    }
                });
            }
        }
    }
}

glib::wrapper! {
    pub struct Strip(ObjectSubclass<imp::Strip>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Strip {
    fn new(model: Model) -> Strip {
        let s: Strip = glib::Object::new();
        s.set_overflow(gtk::Overflow::Hidden);
        s.set_hexpand(true);
        *s.imp().model.borrow_mut() = Some(Rc::new(model));
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::HORIZONTAL | gtk::EventControllerScrollFlags::KINETIC,
        );
        let weak = s.downgrade();
        scroll.connect_scroll(move |c, dx, _dy| {
            let Some(s) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if dx == 0.0 {
                return glib::Propagation::Proceed;
            }
            let step = if c.unit() == gdk::ScrollUnit::Wheel {
                dx * 60.0
            } else {
                dx
            };
            s.stop_animation();
            s.set_offset(s.imp().offset.get() + step);
            glib::Propagation::Stop
        });
        s.add_controller(scroll);
        s
    }

    fn model(&self) -> Option<Rc<Model>> {
        self.imp().model.borrow().clone()
    }

    fn columns_count(&self) -> usize {
        self.model().map_or(0, |m| {
            if m.grid {
                m.items.len().div_ceil(GRID_ROWS)
            } else {
                m.items.len()
            }
        })
    }

    pub fn max_offset(&self) -> f64 {
        let (Some(fit), Some(m)) = (self.imp().fit.get(), self.model()) else {
            return 0.0;
        };
        let w = self.imp().width.get();
        (fit.content_w(w, col_w(m.grid), self.columns_count()) - w).max(0.0)
    }

    /// The columns that should exist now: near the visible span, of a strip near the view.
    fn wanted(&self) -> std::ops::Range<usize> {
        let imp = self.imp();
        let (Some(fit), true) = (imp.fit.get(), imp.near.get()) else {
            return 0..0;
        };
        let n = self.columns_count();
        let (lo, hi) = (imp.offset.get(), imp.offset.get() + imp.width.get());
        let first = ((lo - SIDE_MARGIN - PAD) / fit.pitch).floor().max(0.0) as usize;
        let last = (((hi + SIDE_MARGIN - PAD) / fit.pitch).floor().max(-1.0) + 1.0) as usize;
        first.min(n)..last.min(n)
    }

    /// Makes the wanted columns and drops the rest.
    fn fill(&self) {
        let Some(model) = self.model() else { return };
        let want = self.wanted();
        let mut columns = self.imp().columns.borrow_mut();
        let gone: Vec<usize> = columns
            .keys()
            .copied()
            .filter(|c| !want.contains(c))
            .collect();
        for c in gone {
            if let Some(col) = columns.remove(&c) {
                col.widget.unparent();
            }
        }
        for c in want {
            if columns.contains_key(&c) {
                continue;
            }
            let col = make_column(&model, c);
            col.widget.set_parent(self);
            columns.insert(c, col);
        }
        drop(columns);
        self.show_selected();
        self.queue_allocate();
    }

    fn set_near(&self, near: bool) {
        if self.imp().near.replace(near) != near {
            self.queue_allocate();
        }
    }

    fn set_offset(&self, to: f64) {
        let to = to.clamp(0.0, self.max_offset());
        if self.imp().offset.replace(to) != to {
            self.queue_allocate();
            if let Some(f) = &*self.imp().on_change.borrow() {
                f();
            }
        }
    }

    fn stop_animation(&self) {
        let imp = self.imp();
        imp.animation.set(imp.animation.get() + 1);
    }

    /// Moves to `to`, eased.
    fn animate_to(&self, to: f64) {
        let to = to.clamp(0.0, self.max_offset());
        self.stop_animation();
        let serial = self.imp().animation.get();
        let from = self.imp().offset.get();
        let start = std::time::Instant::now();
        let animate = gtk::Settings::default().is_none_or(|s| s.is_gtk_enable_animations());
        if !animate || from == to {
            self.set_offset(to);
            return;
        }
        self.add_tick_callback(move |s, _| {
            if s.imp().animation.get() != serial {
                return glib::ControlFlow::Break;
            }
            let t = (start.elapsed().as_secs_f64() / PAGE_TIME.as_secs_f64()).min(1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            s.set_offset(from + (to - from) * eased);
            if t >= 1.0 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    /// A page forward or back.
    pub fn page(&self, forward: bool) {
        let Some(fit) = self.imp().fit.get() else {
            return;
        };
        let step = fit.count as f64 * fit.pitch;
        let base = self.imp().offset.get();
        self.animate_to(if forward { base + step } else { base - step });
    }

    pub fn can_page(&self) -> (bool, bool) {
        let o = self.imp().offset.get();
        (o > 0.5, o < self.max_offset() - 0.5)
    }

    /// Marks item `index` selected, paged into view; `None` clears it.
    fn select(&self, index: Option<usize>) {
        self.imp().selected.set(index);
        self.show_selected();
        if let (Some(i), Some(fit), Some(m)) = (index, self.imp().fit.get(), self.model()) {
            let col = if m.grid { i / GRID_ROWS } else { i };
            let to = fit.reveal(
                self.imp().offset.get(),
                self.imp().width.get(),
                col_w(m.grid),
                col,
                self.max_offset(),
            );
            self.set_offset(to);
        }
    }

    fn show_selected(&self) {
        let sel = self.imp().selected.get();
        for col in self.imp().columns.borrow().values() {
            for (i, w) in &col.cells {
                theme::toggle_class(w, "selected", Some(*i) == sel);
            }
        }
    }

    fn covers_loading(&self) -> bool {
        self.imp()
            .columns
            .borrow()
            .values()
            .any(|c| c.covers.iter().any(Cover::is_loading))
    }
}

fn make_column(model: &Rc<Model>, c: usize) -> Column {
    let badge = |item: &Item| vm::badge(vm::item_provider(item), model.mixed);
    if model.grid {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut covers = Vec::new();
        let mut cells = Vec::new();
        for i in c * GRID_ROWS..((c + 1) * GRID_ROWS).min(model.items.len()) {
            let item = &model.items[i];
            let (t, cover) = tile(item, badge(item));
            hook_card(&t, model, i);
            b.append(&t);
            covers.push(cover);
            cells.push((i, t.upcast()));
        }
        Column {
            widget: b.upcast(),
            covers,
            cells,
        }
    } else {
        let item = &model.items[c];
        let card = Card::new(item, model.artists, badge(item));
        hook_card(&card.root, model, c);
        Column {
            widget: card.root.clone().upcast(),
            covers: vec![card.cover],
            cells: vec![(c, card.root.upcast())],
        }
    }
}

/// Clicks on a card: open or play; on Home, a track's right-click offers Play and Add.
fn hook_card(w: &gtk::Box, model: &Rc<Model>, index: usize) {
    let (surface, shelf) = (model.surface, model.shelf);
    widgets::on_click(w, move |_| {
        ui::with(|ui| ui.card_clicked(surface, shelf, index));
    });
    if surface == Surface::Home && matches!(model.items.get(index), Some(Item::Track(_))) {
        let weak = w.downgrade();
        widgets::on_menu(w, move |x, y| {
            let Some(w) = weak.upgrade() else { return };
            let player = ui::with(|ui| ui.playback().1.player).unwrap_or(false);
            widgets::popup_menu(
                &w,
                x,
                y,
                vec![
                    (
                        "Play".into(),
                        player,
                        Box::new(move || {
                            ui::with(|ui| ui.card_clicked(surface, shelf, index));
                        }),
                    ),
                    (
                        "Add to Queue".into(),
                        player,
                        Box::new(move || {
                            ui::with(|ui| ui.append_row(List::Shelf(shelf), index));
                        }),
                    ),
                ],
            );
        });
    }
}

/// A shelf on the page: its title row and its strip.
struct Section {
    root: gtk::Box,
    strip: Strip,
    items: usize,
}

pub struct Shelves {
    pub root: gtk::ScrolledWindow,
    content: gtk::Box,
    lead: RefCell<Option<gtk::Widget>>,
    sections: Rc<RefCell<Vec<Section>>>,
    shelves: RefCell<Vec<Shelf>>,
    surface: Surface,
    /// The selected card's row and column.
    selected: Cell<Option<(usize, usize)>>,
}

impl Shelves {
    pub fn new(surface: Surface) -> Shelves {
        let content = gtk::Box::new(gtk::Orientation::Vertical, SHELF_GAP);
        content.set_margin_top(20);
        content.set_margin_bottom(24);
        let root = widgets::vscroll(&content);
        let sections: Rc<RefCell<Vec<Section>>> = Rc::new(RefCell::new(Vec::new()));
        let adj = root.vadjustment();
        let watch = {
            let sections = Rc::downgrade(&sections);
            let scroll = root.downgrade();
            move || {
                if let (Some(s), Some(scroll)) = (sections.upgrade(), scroll.upgrade()) {
                    mark_near(&s.borrow(), &scroll);
                }
            }
        };
        let w = watch.clone();
        adj.connect_value_changed(move |_| w());
        let w = watch.clone();
        adj.connect_changed(move |_| w());
        Shelves {
            root,
            content,
            lead: RefCell::new(None),
            sections,
            shelves: RefCell::new(Vec::new()),
            surface,
            selected: Cell::new(None),
        }
    }

    /// Puts `lead` above the shelves, or takes it away.
    pub fn set_lead(&self, lead: Option<gtk::Widget>) {
        if let Some(old) = self.lead.borrow_mut().take() {
            self.content.remove(&old);
        }
        if let Some(l) = lead {
            self.content.prepend(&l);
            *self.lead.borrow_mut() = Some(l);
        }
    }

    /// Shows `shelves`; `same` keeps the scroll, for new shelves of the same page.
    pub fn set_shelves(&self, shelves: &[Shelf], same: bool) {
        let keep = same.then(|| self.root.vadjustment().value());
        let mut sections = self.sections.borrow_mut();
        for s in sections.drain(..) {
            self.content.remove(&s.root);
        }
        let artists_section = |s: &Shelf| !s.items.is_empty() && s.items.iter().all(is_artist);
        let mixed = vm::mixed(
            shelves
                .iter()
                .flat_map(|s| s.items.iter().map(vm::item_provider)),
        );
        for (i, shelf) in shelves.iter().enumerate() {
            if shelf.items.is_empty() {
                continue;
            }
            let model = Model {
                surface: self.surface,
                shelf: i,
                items: shelf.items.clone(),
                grid: is_grid(shelf),
                artists: artists_section(shelf),
                mixed,
            };
            sections.push(section(&shelf.title, model));
        }
        for s in sections.iter() {
            self.content.append(&s.root);
        }
        drop(sections);
        *self.shelves.borrow_mut() = shelves.to_vec();
        let adj = self.root.vadjustment();
        match keep {
            Some(v) => {
                let adj2 = adj.clone();
                theme::later(move || adj2.set_value(v));
            }
            None => adj.set_value(0.0),
        }
        let sections = Rc::downgrade(&self.sections);
        let scroll = self.root.downgrade();
        theme::later(move || {
            if let (Some(s), Some(scroll)) = (sections.upgrade(), scroll.upgrade()) {
                mark_near(&s.borrow(), &scroll);
            }
        });
        // Before the first allocation nothing is placed yet; strips start near.
        for s in self.sections.borrow().iter() {
            s.strip.set_near(true);
        }
    }

    /// The item `index` of shelf `shelf`, as last set.
    pub fn item(&self, shelf: usize, index: usize) -> Option<Item> {
        self.shelves.borrow().get(shelf)?.items.get(index).cloned()
    }

    /// The cards in each row of covers, in order.
    pub fn row_lengths(&self) -> Vec<usize> {
        self.sections.borrow().iter().map(|s| s.items).collect()
    }

    /// The shelf and item of card `col` in row `row`.
    pub fn item_at(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let sections = self.sections.borrow();
        let s = sections.get(row)?;
        let shelf = s.strip.model()?.shelf;
        (col < s.items).then_some((shelf, col))
    }

    /// Selects card `col` of row `row`, scrolled into view; `None` clears the selection.
    pub fn select(&self, card: Option<(usize, usize)>) {
        self.selected.set(card);
        for (r, s) in self.sections.borrow().iter().enumerate() {
            let pick = card.filter(|c| c.0 == r).map(|c| c.1);
            s.strip.select(pick);
            if pick.is_some()
                && let Some((y, h)) = widgets::span_in(&s.root, &self.content)
            {
                widgets::reveal(&self.root, y, h);
            }
        }
    }

    /// The shelf and item of the selected card.
    pub fn selected_item(&self) -> Option<(usize, usize)> {
        let (row, col) = self.selected.get()?;
        self.item_at(row, col)
    }

    pub fn scroll_to_end(&self) {
        let adj = self.root.vadjustment();
        adj.set_value(adj.upper() - adj.page_size());
    }

    /// The first row of cards, not a grid of tracks.
    pub fn first_card_row(&self) -> Option<usize> {
        self.sections
            .borrow()
            .iter()
            .position(|s| s.strip.model().is_some_and(|m| !m.grid))
    }

    pub fn page(&self, row: usize, forward: bool) {
        if let Some(s) = self.sections.borrow().get(row) {
            s.strip.page(forward);
        }
    }

    pub fn covers_loading(&self) -> bool {
        self.sections
            .borrow()
            .iter()
            .any(|s| s.strip.covers_loading())
    }
}

/// Marks the strips within a screen of the visible part of `scroll` near, and the rest not.
fn mark_near(sections: &[Section], scroll: &gtk::ScrolledWindow) {
    let adj = scroll.vadjustment();
    let (top, page) = (adj.value(), adj.page_size().max(1.0));
    let Some(content) = scroll.child() else {
        return;
    };
    for s in sections {
        let near = match widgets::span_in(&s.strip, &content) {
            Some((y, h)) => y + h >= top - page && y <= top + 2.0 * page,
            None => true,
        };
        s.strip.set_near(near);
    }
}

fn section(title: &str, model: Model) -> Section {
    let items = model.items.len();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    head.set_margin_start(PAD as i32);
    head.set_margin_end(PAD as i32);
    let t = theme::label(title, &["shelf-title"]);
    t.set_hexpand(true);
    let prev = icons::icon_button(
        Icon::ChevronLeft,
        16,
        &format!("Show previous in {title}"),
        &["icon-btn"],
    );
    let next = icons::icon_button(
        Icon::ChevronRight,
        16,
        &format!("Show more in {title}"),
        &["icon-btn"],
    );
    prev.set_focusable(false);
    next.set_focusable(false);
    head.append(&t);
    head.append(&prev);
    head.append(&next);
    let strip = Strip::new(model);
    let s = strip.downgrade();
    prev.connect_clicked(move |_| {
        if let Some(s) = s.upgrade() {
            s.page(false);
        }
    });
    let s = strip.downgrade();
    next.connect_clicked(move |_| {
        if let Some(s) = s.upgrade() {
            s.page(true);
        }
    });
    let (p, n, s) = (prev.downgrade(), next.downgrade(), strip.downgrade());
    *strip.imp().on_change.borrow_mut() = Some(Box::new(move || {
        let (Some(p), Some(n), Some(s)) = (p.upgrade(), n.upgrade(), s.upgrade()) else {
            return;
        };
        let (back, more) = s.can_page();
        let pages = back || more;
        p.set_visible(pages);
        n.set_visible(pages);
        p.set_sensitive(back);
        n.set_sensitive(more);
    }));
    prev.set_visible(false);
    next.set_visible(false);
    root.append(&head);
    root.append(&strip);
    Section { root, strip, items }
}
