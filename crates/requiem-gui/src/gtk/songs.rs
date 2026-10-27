//! A song table: the search results, the queue, or an open collection's tracks, in a
//! recycling list under a header of column names.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};

use super::art::Cover;
use super::icons::{self, Icon};
use super::theme;
use super::ui;
use super::widgets::{self, Columns, attach, attached};
use crate::view::{RowText, track_number};
use crate::vm::{self, GoToArtist, Intent, List, Place, QueueEdit};

pub const ROW_H: i32 = 36;
pub const HEADER_H: i32 = 32;
const THUMB: i32 = 32;
/// How long a "+" shows its check after an add.
const ADDED_FOR: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Results,
    Queue,
    Collection,
}

impl Kind {
    fn adds(self) -> bool {
        matches!(self, Kind::Results | Kind::Collection)
    }

    fn list(self) -> Option<List> {
        match self {
            Kind::Results => Some(List::Results),
            Kind::Collection => Some(List::Collection),
            Kind::Queue => None,
        }
    }

    fn place(self) -> Place {
        match self {
            Kind::Results => Place::Search,
            Kind::Collection => Place::Collection,
            Kind::Queue => Place::Queue,
        }
    }
}

/// One row's item in the model.
struct Row {
    index: usize,
    text: RowText,
}

/// A bound row's widgets.
struct Parts {
    columns: Columns,
    cover: Cover,
    number: gtk::Label,
    title: gtk::Label,
    artists: gtk::Label,
    album: gtk::Label,
    add: gtk::Button,
    add_icon: gtk::DrawingArea,
    time: gtk::Label,
    index: Cell<usize>,
}

struct State {
    kind: Kind,
    current: Cell<Option<usize>>,
    numbered: Cell<bool>,
    album: Cell<bool>,
    enabled: Cell<bool>,
    /// The row whose "+" shows its check.
    added: Cell<Option<usize>>,
    added_serial: Cell<u64>,
}

pub struct Songs {
    pub root: gtk::Box,
    pub list: gtk::ListView,
    pub scroll: gtk::ScrolledWindow,
    header: Columns,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    state: Rc<State>,
}

impl Songs {
    pub fn new(kind: Kind) -> Songs {
        let state = Rc::new(State {
            kind,
            current: Cell::new(None),
            numbered: Cell::new(false),
            album: Cell::new(true),
            enabled: Cell::new(true),
            added: Cell::new(None),
            added_serial: Cell::new(0),
        });
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        let s = state.clone();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let parts = row_parts(&s);
            item.set_child(Some(&parts.columns));
            attach(item, "parts", parts);
        });
        let s = state.clone();
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let (Some(parts), Some(obj)) = (
                attached::<Parts>(item, "parts"),
                item.item().and_downcast::<glib::BoxedAnyObject>(),
            ) else {
                return;
            };
            let row = obj.borrow::<Row>();
            bind(parts, &row, &s);
        });
        let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list.add_css_class("songs");
        list.set_vexpand(true);
        let s = state.clone();
        list.connect_activate(move |_, pos| {
            let row = pos as usize;
            match s.kind {
                Kind::Queue => {
                    ui::with(|ui| ui.play_queue(row));
                }
                k => {
                    if let Some(list) = k.list() {
                        ui::with(|ui| ui.intent(Intent::PlayFrom { list, from: row }));
                    }
                }
            }
        });
        let header_cells: Vec<gtk::Widget> = ["TITLE", "ARTISTS", "ALBUM", "", "TIME"]
            .iter()
            .map(|t| {
                let l = theme::label(t, &[]);
                if *t == "TIME" {
                    l.set_xalign(1.0);
                    l.set_margin_end(12);
                }
                l.upcast()
            })
            .collect();
        let refs: Vec<&gtk::Widget> = header_cells.iter().collect();
        let header = Columns::new(&refs, kind.adds(), true);
        header.add_css_class("table-head");
        header.set_size_request(-1, HEADER_H);
        let scroll = widgets::vscroll(&list);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&scroll);
        Songs {
            root,
            list,
            scroll,
            header,
            store,
            selection,
            state,
        }
    }

    /// Lists `rows`, `current` marked as playing; the selection is cleared.
    pub fn set_rows(&self, rows: Vec<RowText>, current: Option<usize>) {
        self.state.current.set(current);
        let items: Vec<glib::Object> = rows
            .into_iter()
            .enumerate()
            .map(|(index, text)| glib::BoxedAnyObject::new(Row { index, text }).upcast())
            .collect();
        let n = self.store.n_items();
        self.store.splice(0, n, &items);
    }

    pub fn len(&self) -> usize {
        self.store.n_items() as usize
    }

    /// Shows or hides the Album column. Without it, as on an album's page, rows are numbered
    /// in place of their covers.
    pub fn set_album(&self, shown: bool) {
        let numbered = !shown;
        let renumbered = self.state.numbered.replace(numbered) != numbered;
        if self.state.album.replace(shown) != shown || renumbered {
            self.header.set_album(shown);
            self.refresh();
        }
    }

    /// Lets rows play and add, or not, as the player allows.
    pub fn set_enabled(&self, on: bool) {
        if self.state.enabled.replace(on) != on {
            self.refresh();
        }
    }

    /// Binds every row again, for a change of state the rows show.
    fn refresh(&self) {
        let n = self.store.n_items();
        if n > 0 {
            self.store.items_changed(0, n, n);
        }
    }

    pub fn selected_row(&self) -> Option<usize> {
        let s = self.selection.selected();
        (s != gtk::INVALID_LIST_POSITION && (s as usize) < self.len()).then_some(s as usize)
    }

    pub fn deselect(&self) {
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
    }

    /// Selects row `i`, scrolled into view, with the keyboard on it.
    pub fn select(&self, i: usize) {
        if i >= self.len() {
            return;
        }
        self.selection.set_selected(i as u32);
        self.focus_row(i);
    }

    fn focus_row(&self, i: usize) {
        let _ = self
            .list
            .activate_action("list.scroll-to-item", Some(&(i as u32).to_variant()));
    }

    /// Gives the table the keyboard, on the selected row, else the playing one or the first.
    pub fn select_current(&self) {
        match self.selected_row() {
            Some(i) => self.focus_row(i),
            None => self.select(self.state.current.get().unwrap_or(0)),
        }
    }

    pub fn select_first(&self) {
        self.select(0);
    }

    /// The table has the keyboard.
    pub fn focused(&self) -> bool {
        self.list
            .root()
            .and_then(|r| r.focus())
            .is_some_and(|f| f == self.list || f.is_ancestor(&self.list))
    }

    /// Shows the check on row `i`'s "+" for a moment.
    pub fn confirm_add(&self, i: usize) {
        let serial = self.state.added_serial.get() + 1;
        self.state.added_serial.set(serial);
        self.state.added.set(Some(i));
        self.refresh_row(i);
        let state = Rc::downgrade(&self.state);
        let store = self.store.downgrade();
        glib::timeout_add_local_once(ADDED_FOR, move || {
            if let (Some(s), Some(store)) = (state.upgrade(), store.upgrade())
                && s.added_serial.get() == serial
            {
                s.added.set(None);
                if (i as u32) < store.n_items() {
                    store.items_changed(i as u32, 1, 1);
                }
            }
        });
    }

    fn refresh_row(&self, i: usize) {
        if (i as u32) < self.store.n_items() {
            self.store.items_changed(i as u32, 1, 1);
        }
    }

    /// The height that shows `rows` rows under the header, with nothing to scroll.
    pub fn height_for(rows: usize) -> i32 {
        HEADER_H + rows as i32 * ROW_H
    }
}

fn row_parts(state: &Rc<State>) -> Parts {
    let cover = Cover::new(THUMB, &["small"], true);
    cover.root.set_valign(gtk::Align::Center);
    let number = theme::label("", &["num"]);
    number.set_xalign(0.5);
    number.set_size_request(THUMB, -1);
    let title = theme::label("", &["title"]);
    title.set_hexpand(true);
    let lead = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    lead.append(&cover.root);
    lead.append(&number);
    lead.append(&title);
    let artists = theme::label("", &["artists"]);
    let album = theme::label("", &["album"]);
    let add = gtk::Button::new();
    let add_icon = icons::icon(Icon::Plus, 16);
    add.set_child(Some(&add_icon));
    add.add_css_class("icon-btn");
    add.add_css_class("add");
    add.set_valign(gtk::Align::Center);
    add.set_halign(gtk::Align::Center);
    add.set_tooltip_text(Some("Add to queue (Ctrl+Enter)"));
    add.update_property(&[gtk::accessible::Property::Label("Add to queue")]);
    add.set_focusable(false);
    let time = theme::label("", &["time"]);
    time.set_xalign(1.0);
    time.set_margin_end(12);
    let cells: [&gtk::Widget; 5] = [
        lead.upcast_ref(),
        artists.upcast_ref(),
        album.upcast_ref(),
        add.upcast_ref(),
        time.upcast_ref(),
    ];
    let columns = Columns::new(&cells, state.kind.adds(), true);
    columns.add_css_class("song");
    columns.set_size_request(-1, ROW_H);
    let parts = Parts {
        columns,
        cover,
        number,
        title,
        artists,
        album,
        add,
        add_icon,
        time,
        index: Cell::new(0),
    };
    let s = state.clone();
    let columns = parts.columns.downgrade();
    parts.add.connect_clicked(move |_| {
        let Some(c) = columns.upgrade() else { return };
        let Some(i) = attached::<Cell<usize>>(&c, "index").map(Cell::get) else {
            return;
        };
        if let Some(list) = s.kind.list() {
            ui::with(|ui| ui.append_row(list, i));
        }
    });
    let s = state.clone();
    let columns = parts.columns.downgrade();
    widgets::on_menu(&parts.columns, move |x, y| {
        let Some(c) = columns.upgrade() else { return };
        let Some(i) = attached::<Cell<usize>>(&c, "index").map(Cell::get) else {
            return;
        };
        context_menu(&c, s.kind, i, x, y);
    });
    attach(&parts.columns, "index", Cell::new(0usize));
    parts
}

fn bind(p: &Parts, row: &Row, s: &State) {
    let i = row.index;
    p.index.set(i);
    if let Some(c) = attached::<Cell<usize>>(&p.columns, "index") {
        c.set(i);
    }
    let r = &row.text;
    p.columns.set_album(s.album.get());
    let numbered = s.numbered.get();
    p.cover.root.set_visible(!numbered);
    p.number.set_visible(numbered);
    if numbered {
        theme::set_text(&p.number, &track_number(i));
        p.cover.set_url(None);
    } else {
        p.cover.set_url(r.artwork.as_deref());
        p.cover.set_badge(r.badge);
    }
    theme::set_text(&p.title, &r.title);
    theme::set_text(&p.artists, &r.artists);
    theme::set_text(&p.album, &r.album);
    theme::set_text(&p.time, &r.duration);
    p.title
        .set_tooltip_text((!r.title.is_empty()).then_some(r.title.as_str()));
    let current = s.kind == Kind::Queue && s.current.get() == Some(i);
    let played = s.kind == Kind::Queue && vm::played(i, s.current.get());
    theme::toggle_class(&p.columns, "current", current);
    theme::toggle_class(&p.columns, "played", played);
    p.cover.set_dim(played);
    p.add.set_sensitive(s.enabled.get());
    let added = s.added.get() == Some(i);
    icons::swap(&p.add_icon, if added { Icon::Check } else { Icon::Plus });
    theme::toggle_class(&p.add, "added", added);
}

/// The right-click menu for row `i`.
fn context_menu(widget: &Columns, kind: Kind, i: usize, x: f64, y: f64) {
    let Some((player, track, edits)) = ui::with(|ui| {
        let player = ui.playback().1.player;
        let track = ui.track_at(kind.list(), i);
        let edits = [
            ui.can_edit(QueueEdit::Remove(i)),
            ui.can_edit(QueueEdit::MoveUp(i)),
            ui.can_edit(QueueEdit::MoveDown(i)),
            ui.can_edit(QueueEdit::Clear),
        ];
        (player, track, edits)
    }) else {
        return;
    };
    let mut items: Vec<widgets::MenuItem> = Vec::new();
    match kind {
        Kind::Results | Kind::Collection => {
            let list = kind.list().unwrap_or(List::Results);
            items.push((
                "Play".into(),
                player,
                Box::new(move || {
                    ui::with(|ui| ui.intent(Intent::PlayFrom { list, from: i }));
                }),
            ));
            items.push((
                "Add to Queue".into(),
                player,
                Box::new(move || {
                    ui::with(|ui| ui.append_row(list, i));
                }),
            ));
        }
        Kind::Queue => {
            items.push((
                "Play".into(),
                player,
                Box::new(move || {
                    ui::with(|ui| ui.play_queue(i));
                }),
            ));
            for (label, on, edit) in [
                ("Remove from Queue", edits[0], QueueEdit::Remove(i)),
                ("Move Up", edits[1], QueueEdit::MoveUp(i)),
                ("Move Down", edits[2], QueueEdit::MoveDown(i)),
            ] {
                items.push((
                    label.into(),
                    on,
                    Box::new(move || {
                        ui::with(|ui| ui.edit_queue(edit));
                    }),
                ));
            }
        }
    }
    let pages = match track.map_or(GoToArtist::None, |t| vm::go_to_artist(&t)) {
        GoToArtist::None => Vec::new(),
        GoToArtist::One(page) => vec![page],
        GoToArtist::Several(pages) => pages,
    };
    let from = kind.place();
    if pages.len() <= 1 {
        let page = pages.into_iter().next();
        let on = page.is_some();
        items.push((
            "Go to Artist".into(),
            on,
            Box::new(move || {
                if let Some(p) = page.clone() {
                    ui::with(|ui| ui.go_to_artist(p, from));
                }
            }),
        ));
    } else {
        for page in pages {
            let label = format!("Go to {}", page.title);
            items.push((
                label,
                true,
                Box::new(move || {
                    let p = page.clone();
                    ui::with(|ui| ui.go_to_artist(p, from));
                }),
            ));
        }
    }
    if kind == Kind::Queue {
        items.push((
            "Clear Queue".into(),
            edits[3],
            Box::new(|| {
                ui::with(|ui| ui.edit_queue(QueueEdit::Clear));
            }),
        ));
    }
    widgets::popup_menu(widget, x, y, items);
}

/// Rows of tracks beside a cover, outside a table: Now Playing's Up next and the songs beside
/// the top search result. Each row: cover, title, artists, time.
pub struct TrackRows {
    pub root: gtk::Box,
    rows: RefCell<Vec<TrackRow>>,
    cover: i32,
}

pub struct TrackRow {
    pub root: gtk::Box,
    cover: Cover,
    title: gtk::Label,
    artists: gtk::Label,
    time: gtk::Label,
}

impl TrackRows {
    pub fn new(cover: i32) -> TrackRows {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        TrackRows {
            root,
            rows: RefCell::new(Vec::new()),
            cover,
        }
    }

    /// Shows `rows`; a click on row `i` runs `on_click(i)`.
    pub fn set(&self, rows: &[RowText], on_click: impl Fn(usize) + Clone + 'static) {
        let mut have = self.rows.borrow_mut();
        while have.len() > rows.len() {
            if let Some(r) = have.pop() {
                self.root.remove(&r.root);
            }
        }
        while have.len() < rows.len() {
            let i = have.len();
            let r = TrackRow::new(self.cover);
            let f = on_click.clone();
            widgets::on_click(&r.root, move |_| f(i));
            self.root.append(&r.root);
            have.push(r);
        }
        for (r, t) in have.iter().zip(rows) {
            r.set(t);
        }
    }

    pub fn len(&self) -> usize {
        self.rows.borrow().len()
    }

    /// Marks row `i` selected, or none.
    pub fn select(&self, i: Option<usize>) {
        for (j, r) in self.rows.borrow().iter().enumerate() {
            theme::toggle_class(&r.root, "selected", Some(j) == i);
        }
    }

    pub fn covers_loading(&self) -> bool {
        self.rows.borrow().iter().any(|r| r.cover.is_loading())
    }
}

impl TrackRow {
    fn new(side: i32) -> TrackRow {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        root.add_css_class("track-row");
        let cover = Cover::new(side, &["small"], true);
        cover.root.set_valign(gtk::Align::Center);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let title = theme::label("", &["title"]);
        let artists = theme::label("", &["subtitle"]);
        text.append(&title);
        text.append(&artists);
        let time = theme::label("", &["time"]);
        time.set_xalign(1.0);
        root.append(&cover.root);
        root.append(&text);
        root.append(&time);
        TrackRow {
            root,
            cover,
            title,
            artists,
            time,
        }
    }

    fn set(&self, r: &RowText) {
        self.cover.set_url(r.artwork.as_deref());
        self.cover.set_badge(r.badge);
        theme::set_text(&self.title, &r.title);
        theme::set_text(&self.artists, &r.artists);
        theme::set_text(&self.time, &r.duration);
    }
}
