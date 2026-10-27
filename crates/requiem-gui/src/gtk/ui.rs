//! The window: its pages, and the view model's changes pushed into them.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use requiem_core::{Collection, CollectionId, CollectionKind, Item, ProviderId, Shelf, Track};
use requiem_ipc::UpdateRequest;

use super::grid::CoverGrid;
use super::icons::{self, Icon};
use super::pages::{
    BannerView, Bar, Chips, CodePanel, CollectionHead, LibraryBar, NowPlayingPage, Palette,
    SearchLead, SettingsPage, Sidebar, ToastView,
};
use super::prefs;
use super::shelves::{Shelves, Surface};
use super::songs::{self, Kind, Songs};
use super::theme;
use super::widgets::{self, EmptyState};
use crate::backend::{Backend, Msg};
use crate::search_keys::{Dir, Move, Shape, Spot};
use crate::view::{
    AppView, RowText, SnapshotState, back_to, collection_shelves, lists_album, queue_count_text,
    rows, unix_now,
};
use crate::vm::{
    self, AccountRow, AccountRowState, Banner, CollectionPage, Controls, Dirty, HomeView, Intent,
    LibraryChip, LibrarySort, LibraryView, List, PageView, PaletteAction, Place, QueueEdit,
    SearchBody, SearchChip, SearchView, Selection, Service, ServiceFilter, Toast, UiState, Vm,
};

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

pub fn install(ui: Rc<Ui>) {
    UI.with(|u| *u.borrow_mut() = Some(ui));
}

/// Runs `f` on the installed window; `None` before `install`. Re-entrant.
pub fn with<R>(f: impl FnOnce(&Ui) -> R) -> Option<R> {
    let ui = UI.with(|u| u.borrow().clone())?;
    Some(f(&ui))
}

pub const WIN_W: i32 = 1100;
pub const WIN_H: i32 = 720;
const MIN_W: i32 = 800;
const MIN_H: i32 = 500;
const PAD: i32 = 24;
const TOP: i32 = 28;
const HOW_TO_COPY: &str = "https://github.com/blutarche/requiem#sign-in-with-your-browser-session";
/// How often a sign-in code's countdown is redrawn.
const EXPIRY_TICK: Duration = Duration::from_secs(30);
/// How long "Quit and Stop Music" waits for the player's goodbye.
const QUIT_GRACE: Duration = Duration::from_secs(3);

/// The shown toast's time left, which stands still while the pointer rests on the toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Countdown {
    /// Ends then.
    Running(Instant),
    /// This much is left.
    Held(Duration),
}

impl Countdown {
    fn new(now: Instant, held: bool) -> Countdown {
        if held {
            Countdown::Held(vm::TOAST_DURATION)
        } else {
            Countdown::Running(now + vm::TOAST_DURATION)
        }
    }

    fn hold(self, now: Instant) -> Countdown {
        match self {
            Countdown::Running(end) => Countdown::Held(end.saturating_duration_since(now)),
            held => held,
        }
    }

    fn resume(self, now: Instant) -> Countdown {
        match self {
            Countdown::Held(left) => Countdown::Running(now + left),
            running => running,
        }
    }
}

struct ToastState {
    view: ToastView,
    id: Cell<u64>,
    /// The toast whose countdown runs.
    counting: Cell<u64>,
    shown: Cell<bool>,
    hovered: Cell<bool>,
    clock: Cell<Countdown>,
}

/// What the results area lists that the keyboard can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchList {
    Nothing,
    Table,
    Grid,
    All,
}

pub struct Ui {
    pub window: gtk::ApplicationWindow,
    /// Everything the window shows, which a snapshot draws.
    pub root: gtk::Overlay,
    vm: RefCell<Vm>,
    backend: Backend,
    view: Cell<AppView>,
    pages: gtk::Stack,
    sidebar: Sidebar,
    bar: Bar,
    banner: BannerView,
    pub palette: Palette,
    toast: ToastState,

    home_stack: gtk::Stack,
    pub shelves: Shelves,
    home_state: EmptyState,
    /// The service Home's shelves on screen are shown for, while they show.
    home_shown: Cell<Option<Option<Service>>>,

    search: gtk::Entry,
    status: gtk::Label,
    search_chips: Chips,
    search_stack: gtk::Stack,
    results: Songs,
    results_empty: EmptyState,
    search_shelves: Shelves,
    search_lead: RefCell<Option<SearchLead>>,
    all_spot: Cell<Option<Spot>>,
    /// The All view has the keyboard: the window's focus rests on nothing while it does.
    all_keys: Cell<bool>,
    search_grid: CoverGrid,
    search_list: Cell<SearchList>,

    library_head: gtk::Label,
    pub library_bar: LibraryBar,
    library_stack: gtk::Stack,
    library_grid: CoverGrid,
    library_state: EmptyState,
    sign_in: CodePanel,
    library_shown: RefCell<Option<LibraryShown>>,
    expiry_scheduled: Cell<bool>,

    queue_stack: gtk::Stack,
    queue_count: gtk::Label,
    queue: Songs,

    collection_head: CollectionHead,
    collection_stack: gtk::Stack,
    collection_table: Songs,
    /// The table's place while it lists a playlist's or an album's tracks.
    table_holder: gtk::Box,
    collection_state: EmptyState,
    artist_shelves: Shelves,
    /// "Top songs" and the table, above an artist's shelves.
    artist_lead: gtk::Box,
    artist_shown: RefCell<Option<CollectionId>>,

    now_playing: NowPlayingPage,
    now_playing_from: Cell<AppView>,
    settings: SettingsPage,
    settings_from: Cell<AppView>,

    tick_at: Cell<Option<Instant>>,
    /// A hash of the clipboard text last sent as a session, to clear it once taken.
    pasted: Cell<Option<u64>>,
}

/// What a list of the Library's collections is of: a new list for the same keeps the scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LibraryShown {
    only: Option<Service>,
    chip: Option<LibraryChip>,
    text: String,
    sort: LibrarySort,
}

fn stack() -> gtk::Stack {
    let s = gtk::Stack::new();
    s.set_transition_type(gtk::StackTransitionType::None);
    s.set_hhomogeneous(false);
    s.set_vhomogeneous(false);
    s.set_vexpand(true);
    s.set_hexpand(true);
    s
}

fn margins(w: &impl IsA<gtk::Widget>, start: i32, end: i32) {
    w.set_margin_start(start);
    w.set_margin_end(end);
}

fn remove_from_parent(w: &impl IsA<gtk::Widget>) {
    if let Some(p) = w.parent().and_downcast::<gtk::Box>() {
        p.remove(w);
    }
}

impl Ui {
    pub fn new(app: &gtk::Application, vm: Vm, backend: Backend) -> Rc<Ui> {
        let window = gtk::ApplicationWindow::new(app);
        window.set_title(Some("Requiem"));
        window.add_css_class("requiem");
        window.set_default_size(WIN_W, WIN_H);
        window.set_icon_name(Some(super::APP_ID));

        let sidebar = Sidebar::new();
        let bar = Bar::new();
        let banner = BannerView::new();
        let pages = stack();

        // Home.
        let shelves = Shelves::new(Surface::Home);
        let home_state = EmptyState::new();
        let home_stack = stack();
        home_stack.add_named(&shelves.root, Some("shelves"));
        home_stack.add_named(&home_state.root, Some("state"));
        pages.add_named(&home_stack, Some("home"));

        // Search.
        let search_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let search = widgets::field(
            "Search songs, artists, albums",
            "Search",
            Some(Icon::Search),
        );
        search.set_margin_top(TOP);
        margins(&search, PAD, PAD);
        let status = theme::label("", &["muted", "small"]);
        status.set_margin_top(8);
        margins(&status, PAD, PAD);
        status.set_visible(false);
        let search_chips = Chips::new();
        search_chips.root.set_margin_top(12);
        margins(&search_chips.root, PAD, PAD);
        search_chips.root.set_visible(false);
        let results = Songs::new(Kind::Results);
        margins(&results.root, PAD, PAD);
        results.root.set_margin_top(16);
        let results_empty = EmptyState::new();
        let search_shelves = Shelves::new(Surface::Search);
        let search_grid = CoverGrid::new(Place::Search);
        search_grid.root.set_margin_top(12);
        let search_stack = stack();
        search_stack.add_named(&results.root, Some("songs"));
        search_stack.add_named(&search_shelves.root, Some("all"));
        search_stack.add_named(&search_grid.root, Some("grid"));
        search_stack.add_named(&results_empty.root, Some("empty"));
        search_stack.set_visible_child_name("empty");
        search_page.append(&search);
        search_page.append(&status);
        search_page.append(&search_chips.root);
        search_page.append(&search_stack);
        pages.add_named(&search_page, Some("search"));

        // Library.
        let library_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let library_head = widgets::page_title("Library");
        library_head.set_margin_top(TOP);
        margins(&library_head, PAD, PAD);
        let library_bar = LibraryBar::new();
        library_bar.root.set_margin_top(14);
        library_bar.root.set_margin_bottom(8);
        margins(&library_bar.root, PAD, PAD);
        let library_grid = CoverGrid::new(Place::Library);
        let library_state = EmptyState::new();
        let sign_in = CodePanel::new(
            true,
            || {
                with(|ui| ui.copy_code());
            },
            || {
                with(|ui| ui.open_host());
            },
            || {
                with(|ui| ui.cancel_sign_in());
            },
        );
        let library_stack = stack();
        library_stack.add_named(&library_grid.root, Some("grid"));
        library_stack.add_named(&library_state.root, Some("state"));
        library_stack.add_named(&sign_in.root, Some("panel"));
        library_page.append(&library_head);
        library_page.append(&library_bar.root);
        library_page.append(&library_stack);
        pages.add_named(&library_page, Some("library"));

        // Queue.
        let queue_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let queue_head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        queue_head.set_margin_top(TOP);
        margins(&queue_head, PAD, PAD);
        let queue_title = widgets::page_title("Queue");
        queue_title.set_hexpand(true);
        queue_title.set_valign(gtk::Align::Baseline);
        let queue_count = theme::label("", &["muted", "small"]);
        queue_count.set_xalign(1.0);
        queue_count.set_valign(gtk::Align::Baseline);
        queue_head.append(&queue_title);
        queue_head.append(&queue_count);
        let queue = Songs::new(Kind::Queue);
        queue.root.set_margin_top(16);
        margins(&queue.root, PAD, PAD);
        let queue_empty = EmptyState::new();
        queue_empty.set(
            Icon::ListMusic,
            "Queue is empty",
            "Play or add a search result",
            Some((
                "Go to search",
                Box::new(|| {
                    with(|ui| ui.focus_search());
                }),
            )),
        );
        let queue_stack = stack();
        queue_stack.add_named(&queue.root, Some("list"));
        queue_stack.add_named(&queue_empty.root, Some("empty"));
        queue_page.append(&queue_head);
        queue_page.append(&queue_stack);
        pages.add_named(&queue_page, Some("queue"));

        // Collection.
        let collection_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let collection_head = CollectionHead::new();
        let collection_table = Songs::new(Kind::Collection);
        let table_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        table_holder.set_margin_top(16);
        margins(&table_holder, PAD, PAD);
        table_holder.append(&collection_table.root);
        let collection_state = EmptyState::new();
        let artist_shelves = Shelves::new(Surface::Artist);
        let artist_lead = gtk::Box::new(gtk::Orientation::Vertical, 10);
        margins(&artist_lead, PAD, PAD);
        artist_lead.append(&theme::label("Top songs", &["shelf-title"]));
        let collection_stack = stack();
        collection_stack.add_named(&table_holder, Some("table"));
        collection_stack.add_named(&artist_shelves.root, Some("artist"));
        collection_stack.add_named(&collection_state.root, Some("state"));
        collection_page.append(&collection_head.root);
        collection_page.append(&collection_stack);
        pages.add_named(&collection_page, Some("collection"));

        let now_playing = NowPlayingPage::new();
        pages.add_named(&now_playing.root, Some("now-playing"));
        let settings = SettingsPage::new();
        pages.add_named(&settings.root, Some("settings"));

        let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main.add_css_class("page");
        main.set_hexpand(true);
        main.append(&banner.root);
        main.append(&pages);
        let upper = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        upper.set_vexpand(true);
        upper.append(&sidebar.root);
        upper.append(&main);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&upper);
        column.append(&bar.root);
        let root = gtk::Overlay::new();
        root.add_css_class("root");
        root.set_size_request(MIN_W, MIN_H);
        root.set_child(Some(&column));
        let toast_view = ToastView::new();
        toast_view.root.set_margin_bottom(72 + 16);
        root.add_overlay(&toast_view.root);
        let palette = Palette::new();
        root.add_overlay(&palette.root);
        window.set_child(Some(&root));

        let ui = Rc::new(Ui {
            window,
            root,
            vm: RefCell::new(vm),
            backend,
            view: Cell::new(AppView::Home),
            pages,
            sidebar,
            bar,
            banner,
            palette,
            toast: ToastState {
                view: toast_view,
                id: Cell::new(0),
                counting: Cell::new(0),
                shown: Cell::new(false),
                hovered: Cell::new(false),
                clock: Cell::new(Countdown::Held(Duration::ZERO)),
            },
            home_stack,
            shelves,
            home_state,
            home_shown: Cell::new(None),
            search,
            status,
            search_chips,
            search_stack,
            results,
            results_empty,
            search_shelves,
            search_lead: RefCell::new(None),
            all_spot: Cell::new(None),
            all_keys: Cell::new(false),
            search_grid,
            search_list: Cell::new(SearchList::Nothing),
            library_head,
            library_bar,
            library_stack,
            library_grid,
            library_state,
            sign_in,
            library_shown: RefCell::new(None),
            expiry_scheduled: Cell::new(false),
            queue_stack,
            queue_count,
            queue,
            collection_head,
            collection_stack,
            collection_table,
            table_holder,
            collection_state,
            artist_shelves,
            artist_lead,
            artist_shown: RefCell::new(None),
            now_playing,
            now_playing_from: Cell::new(AppView::Home),
            settings,
            settings_from: Cell::new(AppView::Home),
            tick_at: Cell::new(None),
            pasted: Cell::new(None),
        });
        ui.hook_search_field();
        ui.hook_window();
        let query = ui.vm.borrow().ui().query.clone();
        ui.search.set_text(&query);
        ui.switch_to(AppView::Home);
        ui
    }

    fn hook_search_field(&self) {
        widgets::on_field_key(&self.search, |e, key, mods| {
            use gdk::{Key, ModifierType};
            if mods.intersects(ModifierType::CONTROL_MASK | ModifierType::ALT_MASK) {
                return false;
            }
            with(|ui| match key {
                Key::Down | Key::KP_Down => {
                    let _ = ui.results_move_down();
                    true
                }
                Key::Tab => ui.results_move_down(),
                Key::Escape => {
                    if e.text().is_empty() {
                        ui.show(AppView::Search);
                    } else {
                        e.set_text("");
                    }
                    true
                }
                _ => false,
            })
            .unwrap_or(false)
        });
        self.search.connect_activate(|e| {
            let q = e.text().to_string();
            with(|ui| ui.intent(Intent::Search(q)));
        });
    }

    fn hook_window(&self) {
        // Focus that lands anywhere takes the keyboard from the All view.
        self.window.connect_focus_widget_notify(|w| {
            if GtkWindowExt::focus(w).is_some() {
                with(|ui| ui.all_keys.set(false));
            }
        });
        // A click clears the All view's keyboard selection, as on macOS.
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(|_, _, _, _| {
            with(|ui| {
                if ui.all_spot.get().is_some() {
                    ui.select_all(None);
                }
            });
        });
        self.root.add_controller(click);
        // Now Playing's cover follows the page's size.
        let sizer = gtk::DrawingArea::new();
        sizer.set_hexpand(true);
        sizer.set_vexpand(true);
        sizer.set_can_target(false);
        sizer.connect_resize(|_, w, h| {
            theme::later(move || {
                with(|ui| ui.now_playing.fit(w, h));
            });
        });
        self.now_playing.root.add_named(&sizer, Some("sizer"));
        // The stack is as large as its largest child; the sizer sits under the page.
        if let Some(page) = self.now_playing.root.child_by_name("page") {
            self.now_playing.root.set_visible_child(&page);
        }
    }

    pub fn current_view(&self) -> AppView {
        self.view.get()
    }

    pub fn show(&self, view: AppView) {
        self.switch_to(view);
        self.visit(view.place());
        if view == AppView::Search
            && matches!(
                self.search_list.get(),
                SearchList::Nothing | SearchList::All
            )
        {
            self.select_all(None);
            self.search.grab_focus_without_selecting();
            return;
        }
        if view == AppView::Search && self.search_list.get() == SearchList::Grid {
            self.search_grid.focus();
            return;
        }
        let table = match view {
            AppView::Search => &self.results,
            AppView::Queue => &self.queue,
            AppView::Collection => &self.collection_table,
            AppView::Library => {
                if self.library_stack.visible_child_name().as_deref() == Some("grid") {
                    self.library_grid.grid.grab_focus();
                } else {
                    GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
                }
                return;
            }
            AppView::Home | AppView::NowPlaying | AppView::Settings => {
                // Keeps Space and the arrows from landing in a hidden text field.
                GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
                return;
            }
        };
        if table.len() == 0 {
            GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
            return;
        }
        table.select_current();
    }

    /// Tells the view model where the window is, which fetches a page not yet loaded.
    fn visit(&self, place: Place) {
        let cmds = self.vm.borrow_mut().intent(Intent::Visit(place));
        self.send(cmds);
        self.render();
    }

    /// Shows `view`'s page and marks its nav item, leaving focus where it is.
    fn switch_to(&self, view: AppView) {
        let from = self.view.replace(view);
        if view == AppView::Settings && from != AppView::Settings {
            self.settings_from.set(from);
        }
        self.pages.set_visible_child_name(match view {
            AppView::Home => "home",
            AppView::Search => "search",
            AppView::Library => "library",
            AppView::Queue => "queue",
            AppView::Collection => "collection",
            AppView::NowPlaying => "now-playing",
            AppView::Settings => "settings",
        });
        // An open collection keeps the nav item of the page it was opened from lit; Now
        // Playing lights none.
        let lit = match view {
            AppView::Collection => self
                .vm
                .borrow()
                .ui()
                .collection
                .as_ref()
                .map_or(AppView::Home, |c| AppView::of(c.origin())),
            v => v,
        };
        let s = &self.sidebar;
        s.home.set_active(lit == AppView::Home);
        s.search.set_active(lit == AppView::Search);
        s.library.set_active(lit == AppView::Library);
        s.queue.set_active(lit == AppView::Queue);
        s.settings.set_active(lit == AppView::Settings);
    }

    fn grid_focused(&self) -> bool {
        match self.view.get() {
            AppView::Library => self.library_grid.focused(),
            AppView::Search => self.search_grid.focused(),
            _ => false,
        }
    }

    /// Moves the keyboard into the search results; false when there are none to move into.
    pub fn results_move_down(&self) -> bool {
        if self.search_list.get() == SearchList::Nothing {
            return false;
        }
        self.show(AppView::Search);
        match self.search_list.get() {
            SearchList::Table => self.results.select_first(),
            SearchList::Grid => self.search_grid.focus(),
            SearchList::All => {
                GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
                self.all_keys.set(true);
                self.move_all_selection(Dir::Down);
            }
            SearchList::Nothing => {}
        }
        true
    }

    fn all_focused(&self) -> bool {
        self.view.get() == AppView::Search
            && self.search_list.get() == SearchList::All
            && self.all_keys.get()
            && GtkWindowExt::focus(&self.window).is_none()
    }

    pub fn all_spot(&self) -> Option<Spot> {
        self.all_spot.get()
    }

    fn all_shape(&self) -> Shape {
        let lead = self.search_lead.borrow();
        Shape {
            top: lead.is_some(),
            songs: lead.as_ref().map_or(0, SearchLead::songs),
            rows: self.search_shelves.row_lengths(),
        }
    }

    /// Moves the All view's selection by an arrow key; up from the top, to the search field.
    pub fn move_all_selection(&self, dir: Dir) {
        match self.all_shape().step(self.all_spot.get(), dir) {
            Move::To(spot) => self.select_all(Some(spot)),
            Move::Field => self.focus_search(),
            Move::Stay => {}
        }
    }

    fn select_all(&self, spot: Option<Spot>) {
        self.all_spot.set(spot);
        if let Some(lead) = &*self.search_lead.borrow() {
            lead.select(spot);
        }
        let card = match spot {
            Some(Spot::Card { row, col }) => Some((row, col)),
            _ => None,
        };
        self.search_shelves.select(card);
    }

    /// The results table has focus with its first row selected.
    fn results_at_top(&self) -> bool {
        self.view.get() == AppView::Search
            && self.search_list.get() == SearchList::Table
            && self.results.focused()
            && self.results.selected_row() == Some(0)
    }

    /// Whether the visible list has something Return (or with `append`, Ctrl+Return) acts on.
    fn selection(&self) -> Selection {
        match self.view.get() {
            AppView::Search => match self.search_list.get() {
                SearchList::Table => {
                    let row = self.results.selected_row().is_some();
                    Selection { row, result: row }
                }
                SearchList::Grid => Selection {
                    row: self.search_grid.selected().is_some(),
                    result: false,
                },
                SearchList::All => {
                    let spot = self.all_spot.get();
                    let lead = self.search_lead.borrow();
                    let song = spot.zip(lead.as_ref()).and_then(|(s, l)| l.song_at(s));
                    Selection {
                        row: spot.is_some(),
                        result: song.is_some(),
                    }
                }
                SearchList::Nothing => Selection::default(),
            },
            AppView::Collection => {
                let row = self.collection_table.selected_row().is_some();
                Selection { row, result: row }
            }
            AppView::Library => Selection {
                row: self.library_grid.selected().is_some(),
                result: false,
            },
            AppView::Queue => Selection {
                row: self.queue.selected_row().is_some(),
                result: false,
            },
            AppView::Home | AppView::NowPlaying | AppView::Settings => Selection::default(),
        }
    }

    pub fn can_run(&self, action: PaletteAction) -> bool {
        self.vm.borrow().ui().can_run(action, self.selection())
    }

    /// Whether something plays, and which controls are live.
    pub fn playback(&self) -> (bool, Controls) {
        let vm = self.vm.borrow();
        (vm.ui().is_playing(), vm.ui().controls())
    }

    pub fn can_stop_player(&self) -> bool {
        self.vm.borrow().ui().can_stop_player()
    }

    /// The banner's Restart to Update.
    pub fn restart_to_update(&self) {
        self.backend.request_update(UpdateRequest::Install);
    }

    /// Stops the player, then the window closes on its goodbye.
    pub fn quit_and_stop(&self) {
        if !self.can_stop_player() || !self.backend.stop_player() {
            return;
        }
        // A player that never says goodbye still lets the window go.
        glib::timeout_add_local_once(QUIT_GRACE, || {
            if let Some(app) = gio::Application::default() {
                app.quit();
            }
        });
    }

    pub fn focus_search(&self) {
        self.switch_to(AppView::Search);
        self.visit(Place::Search);
        self.select_all(None);
        self.search.grab_focus_without_selecting();
    }

    pub fn open_palette(&self) {
        if !self.palette.is_open() {
            self.palette.open(|a| self.can_run(a));
        }
    }

    pub fn toggle_palette(&self) {
        if self.palette.is_open() {
            self.close_palette();
        } else {
            self.palette.open(|a| self.can_run(a));
        }
    }

    pub fn close_palette(&self) {
        if self.palette.is_open() {
            self.palette.close();
            self.show(self.view.get());
        }
    }

    pub fn palette_run(&self, row: usize) {
        if let Some(a) = self.palette.action(row) {
            self.run_action(a);
        }
    }

    pub fn run_action(&self, action: PaletteAction) {
        self.close_palette();
        match action {
            PaletteAction::PlayPause => self.intent(Intent::Toggle),
            PaletteAction::Next => self.intent(Intent::Next),
            PaletteAction::Previous => self.intent(Intent::Prev),
            PaletteAction::PlaySelected => self.activate_selection(false),
            PaletteAction::AddToQueue => self.activate_selection(true),
            PaletteAction::FocusSearch => self.focus_search(),
            PaletteAction::ShowHome => self.show(AppView::Home),
            PaletteAction::ShowSearch => self.show(AppView::Search),
            PaletteAction::ShowLibrary => self.show(AppView::Library),
            PaletteAction::ShowQueue => self.show(AppView::Queue),
            PaletteAction::ShowNowPlaying => self.show_now_playing(),
            PaletteAction::ShowSettings => self.show(AppView::Settings),
            PaletteAction::ShowServices(filter) => self.choose_filter(filter),
            PaletteAction::Refresh => self.intent(Intent::Refresh),
            PaletteAction::SignIn(service) => self.sign_in(service.provider()),
            PaletteAction::SignOut(service) => self.confirm_sign_out(service.provider()),
            PaletteAction::VolumeUp => self.intent(Intent::VolumeBy(vm::VOLUME_STEP)),
            PaletteAction::VolumeDown => self.intent(Intent::VolumeBy(-vm::VOLUME_STEP)),
            PaletteAction::ClearQueue => self.edit_queue(QueueEdit::Clear),
        }
    }

    pub fn intent(&self, intent: Intent) {
        self.run(intent);
    }

    fn demo(&self) -> bool {
        matches!(self.backend, Backend::Demo(_))
    }

    /// The user's choice of service: shown at once, and kept for the next launch. Demo runs
    /// leave the saved choice alone.
    pub fn choose_filter(&self, filter: ServiceFilter) {
        self.run(Intent::SetFilter(filter));
        if !self.demo() {
            prefs::save_service_filter(filter);
        }
    }

    pub fn choose_library_chip(&self, chip: LibraryChip) {
        self.run(Intent::SetLibraryChip(chip));
    }

    pub fn choose_search_chip(&self, chip: SearchChip) {
        self.run(Intent::SetSearchChip(chip));
        if self.view.get() == AppView::Search {
            self.show(AppView::Search);
        }
    }

    pub fn choose_library_sort(&self, sort: LibrarySort) {
        self.run(Intent::SetLibrarySort(sort));
        if !self.demo() {
            prefs::save_library_sort(sort);
        }
    }

    pub fn library_filter_changed(&self) {
        self.run(Intent::SetLibraryText(
            self.library_bar.filter.text().to_string(),
        ));
    }

    /// Moves the keyboard from the Library filter to the grid.
    pub fn focus_library_grid(&self) {
        self.library_grid.focus();
    }

    /// Appends item `i` of `list`, and confirms it on the row's "+" when it was sent.
    pub fn append_row(&self, list: List, i: usize) {
        if self.run(Intent::Append { list, index: i }) {
            match list {
                List::Results => self.results.confirm_add(i),
                List::Collection => self.collection_table.confirm_add(i),
                List::TopArtist | List::Shelf(_) => {}
            }
        }
    }

    fn shelves_of(&self, surface: Surface) -> &Shelves {
        match surface {
            Surface::Home => &self.shelves,
            Surface::Search => &self.search_shelves,
            Surface::Artist => &self.artist_shelves,
        }
    }

    /// A click on a card: on Home, a track plays its shelf from there; a playlist or album
    /// opens, from the page the card is on.
    pub fn card_clicked(&self, surface: Surface, shelf: usize, index: usize) {
        match (surface, self.shelves_of(surface).item(shelf, index)) {
            (Surface::Home, Some(Item::Track(_))) => self.intent(Intent::PlayFrom {
                list: List::Shelf(shelf),
                from: index,
            }),
            (Surface::Home, Some(Item::Collection(c))) => self.open_collection(c, Place::Home),
            (Surface::Search, Some(Item::Collection(c))) => self.open_collection(c, Place::Search),
            (Surface::Artist, Some(Item::Collection(c))) => {
                self.open_collection(c, Place::Collection);
            }
            (Surface::Search | Surface::Artist, Some(Item::Track(_))) | (_, None) => {}
        }
    }

    pub fn open_collection(&self, collection: Collection, from: Place) {
        self.run(Intent::OpenCollection { collection, from });
        self.show(AppView::Collection);
    }

    pub fn track_at(&self, list: Option<List>, row: usize) -> Option<Track> {
        self.vm.borrow().ui().track_at(list, row)
    }

    pub fn go_to_playing_artist(&self) {
        let artist = self.vm.borrow().ui().playing_artist();
        if let Some(artist) = artist {
            self.go_to_artist(artist, self.view.get().place());
        }
    }

    /// Opens `artist`'s page from `from`, unless it is the page showing.
    pub fn go_to_artist(&self, artist: Collection, from: Place) {
        let showing = self.view.get() == AppView::Collection
            && self
                .vm
                .borrow()
                .ui()
                .collection
                .as_ref()
                .is_some_and(|c| c.collection.id == artist.id);
        if !showing {
            self.open_collection(artist, from);
        }
    }

    /// Shows Now Playing, which Back leaves for the view showing now.
    pub fn show_now_playing(&self) {
        let from = self.view.get();
        if from != AppView::NowPlaying {
            self.now_playing_from.set(from);
        }
        self.show(AppView::NowPlaying);
    }

    pub fn go_back(&self) {
        match self.view.get() {
            AppView::Collection => {
                let (from, previous) = self
                    .vm
                    .borrow()
                    .ui()
                    .collection
                    .as_ref()
                    .map_or((Place::Home, false), |c| (c.from, c.previous.is_some()));
                if previous {
                    self.run(Intent::CollectionBack);
                    self.show(AppView::Collection);
                } else {
                    // Settings, returned to, still leaves for where it was opened from.
                    let settings_from = self.settings_from.get();
                    self.show(AppView::of(from));
                    self.settings_from.set(settings_from);
                }
            }
            AppView::NowPlaying => {
                let origin = self.collection_origin();
                self.show(back_to(
                    self.now_playing_from.get(),
                    AppView::NowPlaying,
                    origin,
                ));
            }
            AppView::Settings => {
                let origin = self.collection_origin();
                self.show(back_to(self.settings_from.get(), AppView::Settings, origin));
            }
            _ => {}
        }
    }

    fn collection_origin(&self) -> Option<Place> {
        self.vm
            .borrow()
            .ui()
            .collection
            .as_ref()
            .map(CollectionPage::origin)
    }

    /// The Library empty state's button `index`: Sign in to that provider, or Try again.
    fn library_action(&self, index: usize) {
        let view = self.vm.borrow().ui().library_view();
        match view {
            LibraryView::SignedOut(providers) => {
                if let Some(provider) = providers.into_iter().nth(index) {
                    self.sign_in(provider);
                }
            }
            _ => self.intent(Intent::Refresh),
        }
    }

    /// The Home empty state's button `index`: Sign in to that provider, or Try again.
    fn home_action(&self, index: usize) {
        let view = self.vm.borrow().ui().home_view();
        match view {
            HomeView::SignedOut(providers) => {
                if let Some(provider) = providers.into_iter().nth(index) {
                    self.sign_in(provider);
                }
            }
            HomeView::Page(_) => self.intent(Intent::Refresh),
        }
    }

    /// Starts a sign-in, or opens the session form for a provider that takes one, and shows
    /// where the code or the form appears.
    pub fn sign_in(&self, provider: ProviderId) {
        let to = self.vm.borrow().ui().sign_in_place(&provider);
        self.intent(Intent::SignIn(provider));
        if let Some(place) = to {
            self.show(AppView::of(place));
        }
    }

    pub fn play_collection(&self) {
        self.intent(Intent::PlayFrom {
            list: List::Collection,
            from: 0,
        });
    }

    pub fn add_collection(&self) {
        self.run(Intent::AppendAll(List::Collection));
    }

    /// Up next's row `i` on Now Playing: the queue's track that many after the current one.
    pub fn play_up_next(&self, i: usize) {
        let current = self.vm.borrow().ui().current;
        if let Some(c) = current {
            self.intent(Intent::PlayQueue(c + 1 + i));
        }
    }

    pub fn cancel_sign_in(&self) {
        let view = self.vm.borrow().ui().library_view();
        if let LibraryView::Pending { provider, .. } = view {
            self.intent(Intent::SignOut(provider));
        }
    }

    /// Asks before signing out; the provider's part of the Library goes with the account.
    pub fn confirm_sign_out(&self, provider: ProviderId) {
        let service = vm::provider_name(&provider.0).to_owned();
        self.ask(
            &format!("Sign out of {service}?"),
            &vm::sign_out_info(&provider),
            "Sign Out",
            move |ui| ui.intent(Intent::SignOut(provider.clone())),
        );
    }

    pub fn copy_code(&self) {
        let view = self.vm.borrow().ui().library_view();
        if let LibraryView::Pending { user_code, .. } = view {
            self.put_code(&user_code);
        }
    }

    fn put_code(&self, code: &str) {
        self.window.clipboard().set_text(code);
    }

    pub fn open_host(&self) {
        let view = self.vm.borrow().ui().library_view();
        if let LibraryView::Pending {
            verification_url, ..
        } = view
        {
            open_url(&verification_url);
        }
    }

    pub fn how_to_copy(&self) {
        open_url(HOW_TO_COPY);
    }

    pub fn use_browser_session(&self, i: usize) {
        if let Some(row) = self.settings.row(i) {
            self.intent(Intent::UseBrowserSession(row.provider));
        }
    }

    fn can_paste_session(&self) -> bool {
        self.view.get() == AppView::Settings && self.settings.session_form()
    }

    /// Paste from Clipboard, or Ctrl+V outside a text field: sends the clipboard's text as
    /// the open session form's session. The text goes into the import and nowhere else.
    pub fn paste_session(&self) {
        if !self.can_paste_session() {
            return;
        }
        self.window
            .clipboard()
            .read_text_async(None::<&gio::Cancellable>, |text| {
                let text = text
                    .ok()
                    .flatten()
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                with(|ui| {
                    let paste = vm::session_json(&text);
                    // Set first: a backend that answers at once has taken the session by the
                    // time `run` returns.
                    ui.pasted.set(Some(hash(&text)));
                    if !ui.run(Intent::ImportSession(paste)) {
                        ui.pasted.set(None);
                    }
                });
            });
    }

    /// Once a pasted session is taken, clears the clipboard it came from, unless something
    /// else was copied since.
    fn clear_paste(&self) {
        if !self.vm.borrow_mut().take_session_saved() {
            return;
        }
        let Some(pasted) = self.pasted.take() else {
            return;
        };
        let clipboard = self.window.clipboard();
        let c = clipboard.clone();
        clipboard.read_text_async(None::<&gio::Cancellable>, move |text| {
            if text.ok().flatten().is_some_and(|t| hash(&t) == pasted) {
                c.set_text("");
            }
        });
    }

    pub fn cancel_setup(&self) {
        self.intent(Intent::CancelSetup);
    }

    /// Settings' Sign In… or Sign Out… on row `i`; Sign Out asks first.
    pub fn settings_action(&self, i: usize) {
        let Some(row) = self.settings.row(i) else {
            return;
        };
        match row.state {
            AccountRowState::SignedIn { .. } => self.confirm_sign_out(row.provider),
            AccountRowState::SignedOut => self.sign_in(row.provider),
            _ => {}
        }
    }

    pub fn settings_copy_code(&self, i: usize) {
        if let Some(AccountRowState::Pending { user_code, .. }) =
            self.settings.row(i).map(|r| r.state)
        {
            self.put_code(&user_code);
        }
    }

    pub fn settings_open_host(&self, i: usize) {
        if let Some(AccountRowState::Pending {
            verification_url, ..
        }) = self.settings.row(i).map(|r| r.state)
        {
            open_url(&verification_url);
        }
    }

    pub fn settings_cancel_sign_in(&self, i: usize) {
        if let Some(row) = self.settings.row(i) {
            self.intent(Intent::SignOut(row.provider));
        }
    }

    /// Asks `message` in a dialog with `confirm`, destructive, and Cancel; `then` runs on
    /// `confirm`.
    fn ask(&self, message: &str, info: &str, confirm: &str, then: impl Fn(&Ui) + 'static) {
        let dialog = gtk::MessageDialog::new(
            Some(&self.window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            message,
        );
        dialog.add_css_class("requiem");
        dialog.set_secondary_text(Some(info));
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        if let Some(b) = dialog
            .add_button(confirm, gtk::ResponseType::Accept)
            .downcast_ref::<gtk::Button>()
        {
            b.add_css_class("destructive-action");
        }
        dialog.set_default_response(gtk::ResponseType::Accept);
        dialog.connect_response(move |d, response| {
            d.close();
            if response == gtk::ResponseType::Accept {
                with(&then);
            }
        });
        dialog.present();
    }

    /// `intent`; true when it sent anything.
    fn run(&self, intent: Intent) -> bool {
        let cmds = self.vm.borrow_mut().intent(intent);
        let sent = !cmds.is_empty();
        self.send(cmds);
        self.render();
        sent
    }

    /// Sends `cmds`, then whatever the view model raised in answer: the demo backend answers
    /// at once, and its events can raise more.
    fn send(&self, mut cmds: Vec<requiem_core::Command>) {
        while !cmds.is_empty() {
            for cmd in cmds {
                let events = self.backend.send(cmd);
                let mut vm = self.vm.borrow_mut();
                for ev in events {
                    vm.apply(ev);
                }
            }
            cmds = self.vm.borrow_mut().take_commands();
        }
        self.clear_paste();
    }

    /// Plays (or with `append`, appends) the keyboard-selected row of the visible list; in
    /// Settings, pastes the session form's session.
    pub fn activate_selection(&self, append: bool) {
        match self.view.get() {
            AppView::Search => match self.search_list.get() {
                SearchList::Table => match self.results.selected_row() {
                    Some(i) if append => self.append_row(List::Results, i),
                    Some(i) => self.intent(Intent::PlayFrom {
                        list: List::Results,
                        from: i,
                    }),
                    None => {}
                },
                SearchList::Grid => {
                    if !append {
                        self.search_grid.open_selected();
                    }
                }
                SearchList::All => self.activate_all(append),
                SearchList::Nothing => {}
            },
            AppView::Collection => match self.collection_table.selected_row() {
                Some(i) if append => self.append_row(List::Collection, i),
                Some(i) => self.intent(Intent::PlayFrom {
                    list: List::Collection,
                    from: i,
                }),
                None => {}
            },
            AppView::Library => {
                if !append {
                    self.library_grid.open_selected();
                }
            }
            AppView::Settings if !append => self.paste_session(),
            AppView::Home | AppView::NowPlaying | AppView::Settings => {}
            AppView::Queue => {
                if let Some(i) = self.queue.selected_row().filter(|_| !append) {
                    self.play_queue(i);
                }
            }
        }
    }

    /// Return on the All view's selection does what a click on it does; with `append`, a song
    /// joins the queue instead.
    fn activate_all(&self, append: bool) {
        let Some(spot) = self.all_spot.get() else {
            return;
        };
        if let Spot::Card { .. } = spot {
            if let Some((shelf, index)) = self.search_shelves.selected_item() {
                self.card_clicked(Surface::Search, shelf, index);
            }
            return;
        }
        let song = match &*self.search_lead.borrow() {
            Some(lead) => lead.song_at(spot),
            None => return,
        };
        match song {
            Some(i) if append => self.append_row(List::Results, i),
            _ if append => {}
            _ => {
                // Out of the borrow first: a press can bring new results, and a new lead.
                let press = self
                    .search_lead
                    .borrow()
                    .as_ref()
                    .and_then(|l| l.press_fn(spot));
                if let Some(p) = press {
                    p();
                }
            }
        }
    }

    /// `edit` applied to the selected queue row, while the Queue view shows one.
    fn selected_edit(&self, edit: fn(usize) -> QueueEdit) -> Option<QueueEdit> {
        if self.view.get() != AppView::Queue {
            return None;
        }
        self.queue.selected_row().map(edit)
    }

    pub fn can_edit(&self, edit: QueueEdit) -> bool {
        self.vm.borrow().ui().edit_command(edit).is_some()
    }

    pub fn edit_queue(&self, edit: QueueEdit) {
        let selected = self.queue.selected_row();
        self.intent(Intent::EditQueue { edit, selected });
    }

    pub fn play_queue(&self, i: usize) {
        self.intent(Intent::PlayQueue(i));
    }

    pub fn seek_fraction(&self, fraction: f64) {
        let to = self.vm.borrow().ui().seek_target(fraction);
        if let Some(to) = to {
            self.intent(Intent::SeekTo(to));
        }
    }

    /// Shows where a seek-bar drag would land in the elapsed label.
    pub fn preview_seek(&self, fraction: f64) {
        let to = self.vm.borrow().ui().seek_target(fraction);
        if let Some(to) = to {
            self.bar.preview_seek(&vm::fmt_time(to));
        }
    }

    pub fn set_volume(&self, volume: u8) {
        self.intent(Intent::SetVolume(volume));
    }

    pub fn close_toast(&self) {
        self.dismiss_toast(self.toast.id.get());
    }

    pub fn toast_action(&self) {
        self.intent(Intent::Undo);
    }

    /// Whether Ctrl+Z outside a text field undoes anything now.
    fn can_undo(&self) -> bool {
        let vm = self.vm.borrow();
        vm.undo_title().is_some() && vm.ui().controls().player
    }

    /// Holds the toast's countdown while the pointer is on it, and resumes it with the time
    /// that was left.
    pub fn hover_toast(&self, over: bool) {
        let t = &self.toast;
        if t.hovered.replace(over) == over || !t.shown.get() {
            return;
        }
        let now = Instant::now();
        let clock = if over {
            t.clock.get().hold(now)
        } else {
            t.clock.get().resume(now)
        };
        t.clock.set(clock);
        self.schedule_dismissal(t.counting.get(), clock);
    }

    fn start_clock(&self, id: u64, held: bool) {
        let t = &self.toast;
        t.hovered.set(held);
        let clock = Countdown::new(Instant::now(), held);
        t.clock.set(clock);
        self.schedule_dismissal(id, clock);
    }

    fn schedule_dismissal(&self, id: u64, clock: Countdown) {
        let Countdown::Running(end) = clock else {
            return;
        };
        let wait = end.saturating_duration_since(Instant::now());
        glib::timeout_add_local_once(wait, move || {
            with(|ui| ui.countdown_ended(id, end));
        });
    }

    fn countdown_ended(&self, id: u64, end: Instant) {
        let t = &self.toast;
        if t.counting.get() == id && t.clock.get() == Countdown::Running(end) {
            self.dismiss_toast(id);
        }
    }

    fn dismiss_toast(&self, id: u64) {
        self.vm.borrow_mut().dismiss_toast(id);
        self.render();
    }

    pub fn receive(&self, batch: Vec<Msg>) {
        let mut raise = false;
        let cmds = {
            let mut vm = self.vm.borrow_mut();
            for m in batch {
                match m {
                    Msg::Event(ev) => vm.apply(ev),
                    Msg::Link(l) => vm.link(l),
                    Msg::Update(status) => vm.update(status),
                    Msg::Raise => raise = true,
                }
            }
            vm.take_commands()
        };
        self.send(cmds);
        self.render();
        if raise {
            self.window.present();
        }
    }

    /// Pushes the view model's changed regions into the widgets.
    pub fn render(&self) {
        self.render_now();
    }

    pub fn render_now(&self) {
        let (patch, select) = {
            let mut vm = self.vm.borrow_mut();
            let dirty = vm.take_dirty();
            let patch = (!dirty.is_empty()).then(|| Patch::new(dirty, vm.ui()));
            (patch, vm.take_queue_selection())
        };
        if let Some(patch) = patch {
            self.apply(patch);
        }
        if let Some(row) = select {
            self.queue.select(row);
        }
        if self.palette.is_open() {
            self.palette.set_enabled(|a| self.can_run(a));
        }
        self.schedule_tick();
    }

    /// Wakes the view model when the banner or the clock is due to change on its own.
    fn schedule_tick(&self) {
        let next = self.vm.borrow().next_tick();
        if next == self.tick_at.get() {
            return;
        }
        self.tick_at.set(next);
        let Some(at) = next else { return };
        let wait = at.saturating_duration_since(Instant::now());
        glib::timeout_add_local_once(wait, move || {
            with(|ui| ui.tick(at));
        });
    }

    fn tick(&self, at: Instant) {
        if self.tick_at.get() != Some(at) {
            return;
        }
        self.tick_at.set(None);
        self.vm.borrow_mut().tick();
        self.render();
    }

    fn apply(&self, p: Patch) {
        if let Some(status) = p.search_status {
            match &status {
                Some((text, error)) => {
                    theme::set_text(&self.status, text);
                    theme::toggle_class(&self.status, "error-text", *error);
                }
                None => theme::set_text(&self.status, ""),
            }
            self.status.set_visible(status.is_some());
        }
        if let Some(r) = p.results {
            self.show_results(r);
        }
        if let Some(on) = p.top_play
            && let Some(lead) = &*self.search_lead.borrow()
        {
            lead.set_play_enabled(on);
        }
        if let Some((shown, filter)) = p.filter {
            self.sidebar.set_filter(shown, filter);
        }
        if let Some((view, shelves, only)) = p.home {
            self.show_home(&view, &shelves, only);
        }
        if let Some((view, grid, only)) = p.library {
            self.show_library(&view, grid, only);
        }
        if let Some(rows) = p.settings
            && self.settings.set(rows)
        {
            self.schedule_expiry();
        }
        if let Some(c) = p.collection {
            self.show_collection(c);
        }
        if let Some(badge) = p.queue_badge {
            self.sidebar.queue.set_count(badge.as_deref());
        }
        if let Some((rows, current)) = p.queue {
            let n = rows.len();
            theme::set_text(&self.queue_count, &queue_count_text(n));
            self.queue.set_rows(rows, current);
            self.queue_stack
                .set_visible_child_name(if n == 0 { "empty" } else { "list" });
        }
        if let Some(now) = p.now_playing {
            let track = now.track.as_ref().map(|(t, a)| (t.as_str(), a.as_str()));
            self.bar.set_track(
                track,
                now.link.as_deref(),
                &now.total,
                now.artwork.as_deref(),
                now.badge,
            );
        }
        if let Some((track, next, badge)) = p.now_page {
            self.now_playing.set(track.as_ref(), &next, badge);
        }
        if let Some(e) = p.elapsed {
            self.bar.set_elapsed(&e);
        }
        if let Some(f) = p.progress {
            self.bar.set_progress(f);
        }
        if let Some(playing) = p.playing {
            self.bar.set_playing(playing);
        }
        if let Some(v) = p.volume {
            self.bar.set_volume(v);
        }
        if let Some(c) = p.controls {
            self.bar.set_controls(c);
            self.results.set_enabled(c.player);
            self.queue.set_enabled(c.player);
            self.collection_table.set_enabled(c.player);
            let can = c.player && self.collection_table.len() > 0;
            self.collection_head.play.set_sensitive(can);
            self.collection_head.add.set_sensitive(can);
        }
        if let Some(t) = p.toast {
            self.show_toast(t);
        }
        if let Some(b) = p.banner {
            self.banner.show(b.as_ref());
        }
    }

    fn show_results(&self, r: ResultsPatch) {
        let ResultsPatch { rows, view } = r;
        let SearchView { chips, body } = view;
        let chips_shown = !chips.is_empty();
        self.search_chips.root.set_visible(chips_shown);
        if chips_shown {
            self.search_chips.set_search(&chips);
        }
        let (mut songs, mut all, mut grid, mut empty) = (false, false, false, None);
        match body {
            SearchBody::Empty(e) => empty = Some(e),
            SearchBody::Songs => {
                self.results.set_rows(rows, None);
                self.results.deselect();
                songs = true;
            }
            SearchBody::All {
                top,
                songs: next,
                artists,
                albums,
                playlists,
                mixed,
            } => {
                let lead = top.map(|t| SearchLead::new(&t, &next, mixed));
                if let Some(l) = &lead {
                    l.set_play_enabled(self.vm.borrow().ui().top_artist_playable());
                }
                self.search_shelves
                    .set_lead(lead.as_ref().map(|l| l.root.clone().upcast()));
                *self.search_lead.borrow_mut() = lead;
                let shelf = |title: &str, items: Vec<Collection>| Shelf {
                    title: title.to_owned(),
                    items: items.into_iter().map(Item::Collection).collect(),
                };
                self.search_shelves.set_shelves(
                    &[
                        shelf("Artists", artists),
                        shelf("Albums", albums),
                        shelf("Playlists", playlists),
                    ],
                    false,
                );
                all = true;
            }
            SearchBody::Collections { items, mixed } => {
                let artists = self.vm.borrow().ui().search_chip == SearchChip::Artists;
                self.search_grid.set_artists(artists);
                self.search_grid.set(&items, mixed, false);
                grid = true;
            }
        }
        if !all && self.search_lead.borrow().is_some() {
            self.search_shelves.set_lead(None);
            *self.search_lead.borrow_mut() = None;
        }
        self.search_list.set(match (songs, grid, all) {
            (true, _, _) => SearchList::Table,
            (_, true, _) => SearchList::Grid,
            (_, _, true) => SearchList::All,
            _ => SearchList::Nothing,
        });
        // New results keep the All view's selection where they still have its place.
        let kept = self
            .all_spot
            .get()
            .filter(|&s| all && self.all_shape().holds(s));
        self.select_all(kept);
        self.search_stack.set_visible_child_name(if songs {
            "songs"
        } else if all {
            "all"
        } else if grid {
            "grid"
        } else {
            "empty"
        });
        if let Some(e) = empty {
            let action: Option<widgets::Action> = e.action.map(|a| {
                let f: Box<dyn Fn()> = Box::new(|| {
                    with(|ui| ui.focus_search());
                });
                (a, f)
            });
            self.results_empty
                .set(Icon::Search, &e.heading, &e.sub, action);
        }
    }

    fn show_home(&self, view: &HomeView, shelves: &[Shelf], only: Option<Service>) {
        let ready =
            *view == HomeView::Page(PageView::Ready) && shelves.iter().any(|s| !s.items.is_empty());
        // New shelves for the same page, as a refresh brings, keep the place scrolled to.
        let same = ready && self.home_shown.get() == Some(only);
        self.home_shown.set(ready.then_some(only));
        self.shelves
            .set_shelves(if ready { shelves } else { &[] }, same);
        self.home_stack
            .set_visible_child_name(if ready { "shelves" } else { "state" });
        match view {
            _ if ready => {}
            HomeView::SignedOut(providers) => {
                self.home_state.set_actions(
                    Icon::Home,
                    "Sign in to see your music",
                    "Search works without signing in.",
                    sign_in_actions(providers, |i| {
                        with(|ui| ui.home_action(i));
                    }),
                );
            }
            HomeView::Page(page) => {
                page_state(&self.home_state, Icon::Home, page, "Home", only, || {
                    with(|ui| ui.home_action(0));
                });
            }
        }
    }

    fn show_library(&self, view: &LibraryView, grid: LibraryPatch, only: Option<Service>) {
        let panel = matches!(view, LibraryView::Pending { .. });
        let list = matches!(
            view,
            LibraryView::SignedIn {
                page: PageView::Ready,
                ..
            }
        ) && grid.stocked;
        // The filter text can leave nothing to list; the toolbar stays to clear it.
        let cards = list && !grid.collections.is_empty();
        self.library_head.set_visible(!panel);
        self.library_bar.root.set_visible(list);
        self.library_stack.set_visible_child_name(if panel {
            "panel"
        } else if cards {
            "grid"
        } else {
            "state"
        });
        let state = &self.library_state;
        let retry = || {
            with(|ui| ui.library_action(0));
        };
        match view {
            LibraryView::Loading => state.set(Icon::Library, "Loading Library", "", None),
            LibraryView::Unavailable(text) => {
                state.set(Icon::Library, "Sign-in isn't set up", text, None);
            }
            LibraryView::SignedOut(providers) => {
                state.set_actions(
                    Icon::Library,
                    "Sign in to see your Library",
                    "Your playlists and albums appear here.",
                    sign_in_actions(providers, |i| {
                        with(|ui| ui.library_action(i));
                    }),
                );
            }
            LibraryView::Pending {
                provider,
                verification_url,
                user_code,
                expires_at_unix,
            } => {
                self.sign_in
                    .show(provider, verification_url, user_code, *expires_at_unix);
                self.schedule_expiry();
            }
            LibraryView::SignedIn { page } => {
                if *page == PageView::Ready && !grid.stocked && only.is_none() {
                    state.set(Icon::Library, "Your Library is empty", "", None);
                } else if *page != PageView::Ready || !grid.stocked {
                    page_state(state, Icon::Library, page, "Library", only, retry);
                } else if !cards {
                    state.set(
                        Icon::Library,
                        "Nothing matches",
                        "Clear the filter to see everything.",
                        None,
                    );
                }
            }
        }
        if list {
            self.library_bar.set(&grid.chips, grid.sort, &grid.text);
        }
        let shown = LibraryShown {
            only,
            chip: grid.chips.iter().find(|c| c.1).map(|c| c.0),
            text: grid.text,
            sort: grid.sort,
        };
        let same = cards && self.library_shown.borrow().as_ref() == Some(&shown);
        *self.library_shown.borrow_mut() = cards.then_some(shown);
        if cards {
            self.library_grid.set(&grid.collections, grid.mixed, same);
        } else {
            self.library_grid.set(&[], false, false);
        }
    }

    /// Redraws the codes' countdowns while one shows; "Code expires in N min" moves each
    /// minute.
    fn schedule_expiry(&self) {
        if self.expiry_scheduled.replace(true) {
            return;
        }
        glib::timeout_add_local_once(EXPIRY_TICK, || {
            with(|ui| ui.expiry_tick());
        });
    }

    fn expiry_tick(&self) {
        self.expiry_scheduled.set(false);
        let now = unix_now();
        let view = self.vm.borrow().ui().library_view();
        let mut showing = self.settings.refresh_expiry(now);
        if matches!(view, LibraryView::Pending { .. }) {
            self.sign_in.tick(now);
            showing = true;
        }
        if showing {
            self.schedule_expiry();
        }
    }

    fn show_collection(&self, c: Option<CollectionPatch>) {
        let Some(c) = c else {
            self.collection_table.set_rows(Vec::new(), None);
            return;
        };
        let artist = c.collection.kind == CollectionKind::Artist;
        let back = vm::back_label(c.back.as_deref(), c.from);
        self.collection_head.set(&c.collection, &back);
        let songs = c.rows.len();
        let loaded = c.view == PageView::Ready;
        // An artist's page shows with its shelves alone; another collection needs tracks.
        let shelved = artist && loaded && (songs > 0 || !c.shelves.is_empty());
        let ready = shelved || (loaded && songs > 0);
        if !ready {
            page_state(
                &self.collection_state,
                Icon::ListMusic,
                &c.view,
                if artist { "artist" } else { "playlist" },
                None,
                || {
                    with(|ui| ui.intent(Intent::Refresh));
                },
            );
        }
        self.collection_table
            .set_album(lists_album(c.collection.kind));
        self.place_collection_table(shelved && songs > 0, songs);
        self.collection_table.root.set_visible(ready && songs > 0);
        self.collection_table
            .set_rows(if ready { c.rows } else { Vec::new() }, None);
        if shelved {
            let id = Some(c.collection.id.clone());
            let same = *self.artist_shown.borrow() == id;
            *self.artist_shown.borrow_mut() = id;
            self.artist_shelves
                .set_lead((songs > 0).then(|| self.artist_lead.clone().upcast()));
            self.artist_shelves.set_shelves(&c.shelves, same);
        } else if self.artist_shown.take().is_some() {
            self.artist_shelves.set_lead(None);
            self.artist_shelves.set_shelves(&[], false);
        }
        self.collection_stack.set_visible_child_name(if !ready {
            "state"
        } else if shelved {
            "artist"
        } else {
            "table"
        });
        let can = songs > 0 && ready && self.vm.borrow().ui().controls().player;
        self.collection_head.play.set_sensitive(can);
        self.collection_head.add.set_sensitive(can);
    }

    /// Puts the collection table in the artist page's lead, fitted to its `songs` rows and
    /// leaving the wheel to the page, or back under the head.
    fn place_collection_table(&self, in_lead: bool, songs: usize) {
        let t = &self.collection_table;
        let parent = t.root.parent();
        if in_lead {
            if parent.as_ref() != Some(self.artist_lead.upcast_ref()) {
                remove_from_parent(&t.root);
                self.artist_lead.append(&t.root);
                t.scroll.set_vscrollbar_policy(gtk::PolicyType::Never);
            }
            t.root.set_size_request(-1, Songs::height_for(songs));
            t.scroll.set_min_content_height(songs as i32 * songs::ROW_H);
        } else if parent.as_ref() != Some(self.table_holder.upcast_ref()) {
            remove_from_parent(&t.root);
            self.table_holder.append(&t.root);
            t.scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
            t.root.set_size_request(-1, -1);
            t.scroll.set_min_content_height(-1);
        }
    }

    /// What a snapshot of the connected window waits for.
    pub fn snapshot_state(&self) -> SnapshotState {
        let page = {
            let vm = self.vm.borrow();
            let ui = vm.ui();
            match self.view.get() {
                AppView::Home => match ui.home_view() {
                    HomeView::SignedOut(_) => PageView::Ready,
                    HomeView::Page(page) => page,
                },
                AppView::Library => match ui.library_view() {
                    LibraryView::Loading => PageView::Loading,
                    LibraryView::SignedIn { page, .. } => page,
                    _ => PageView::Ready,
                },
                AppView::Collection => ui
                    .collection
                    .as_ref()
                    .map_or(PageView::Loading, |c| c.browse.view()),
                _ => PageView::Ready,
            }
        };
        match page {
            PageView::Loading => SnapshotState::Waiting,
            PageView::Failed(e) => SnapshotState::Failed(e),
            PageView::Ready => SnapshotState::Loaded {
                covers: self.covers_loading(),
            },
        }
    }

    fn covers_loading(&self) -> bool {
        let view = self.view.get();
        (view == AppView::Home && self.shelves.covers_loading())
            || (view == AppView::Search && self.search_covers_loading())
            || (view == AppView::Library && self.library_grid.covers_loading())
            || (view == AppView::Collection
                && (self.artist_shelves.covers_loading()
                    || self.collection_head.cover.is_loading()))
            || (view == AppView::NowPlaying && self.now_playing.covers_loading())
            || self.bar.covers_loading()
    }

    fn search_covers_loading(&self) -> bool {
        self.search_shelves.covers_loading()
            || self.search_grid.covers_loading()
            || self
                .search_lead
                .borrow()
                .as_ref()
                .is_some_and(SearchLead::covers_loading)
    }

    fn show_toast(&self, toast: Option<Toast>) {
        let t = &self.toast;
        let Some(toast) = toast else {
            t.shown.set(false);
            t.view.hide();
            return;
        };
        t.shown.set(true);
        t.id.set(toast.id);
        // A toast amended in place keeps its countdown.
        if t.counting.replace(toast.id) != toast.id {
            self.start_clock(toast.id, t.hovered.get());
        }
        t.view.show(&toast);
    }

    /// Window-wide keys, heard before the focused widget. True when handled.
    pub fn key(&self, key: gdk::Key, mods: gdk::ModifierType) -> bool {
        use gdk::{Key, ModifierType as M};
        let ctrl = mods.contains(M::CONTROL_MASK);
        let alt = mods.contains(M::ALT_MASK);
        let shift = mods.contains(M::SHIFT_MASK);
        let plain = !mods.intersects(M::CONTROL_MASK | M::ALT_MASK | M::SUPER_MASK);
        let focus = GtkWindowExt::focus(&self.window);
        let in_text = focus
            .as_ref()
            .is_some_and(|f| f.is::<gtk::Text>() || f.is::<gtk::TextView>());
        let on_button = focus.as_ref().is_some_and(|f| {
            f.is::<gtk::Button>() || f.ancestor(gtk::Button::static_type()).is_some()
        });
        let key = key.to_lower();
        // Shortcuts that work wherever focus is.
        if ctrl && !alt {
            let handled = match key {
                Key::k => {
                    self.toggle_palette();
                    true
                }
                Key::f => {
                    self.close_palette();
                    self.focus_search();
                    true
                }
                Key::_1 | Key::KP_1 => self.palette_show(AppView::Home),
                Key::_2 | Key::KP_2 => self.palette_show(AppView::Search),
                Key::_3 | Key::KP_3 => self.palette_show(AppView::Library),
                Key::_4 | Key::KP_4 => self.palette_show(AppView::Queue),
                Key::l => {
                    self.close_palette();
                    self.show_now_playing();
                    true
                }
                Key::comma => self.palette_show(AppView::Settings),
                Key::r => {
                    self.intent(Intent::Refresh);
                    true
                }
                Key::bracketleft => {
                    self.go_back();
                    true
                }
                Key::q if !shift => {
                    self.window.close();
                    true
                }
                _ => false,
            };
            if handled {
                return true;
            }
        }
        if ctrl && alt && key == Key::q {
            self.quit_and_stop();
            return true;
        }
        if self.palette.is_open() {
            if key == Key::Escape {
                self.close_palette();
                return true;
            }
            return false;
        }
        if alt && !ctrl && matches!(key, Key::Left | Key::KP_Left) && !in_text {
            self.go_back();
            return true;
        }
        if in_text {
            return false;
        }
        if ctrl && alt {
            let edit: Option<fn(usize) -> QueueEdit> = match key {
                Key::Up | Key::KP_Up => Some(QueueEdit::MoveUp),
                Key::Down | Key::KP_Down => Some(QueueEdit::MoveDown),
                _ => None,
            };
            return match edit
                .and_then(|e| self.selected_edit(e))
                .filter(|&e| self.can_edit(e))
            {
                Some(e) => {
                    self.edit_queue(e);
                    true
                }
                None => false,
            };
        }
        if ctrl {
            let action = match key {
                Key::Right | Key::KP_Right => PaletteAction::Next,
                Key::Left | Key::KP_Left => PaletteAction::Previous,
                Key::Up | Key::KP_Up => PaletteAction::VolumeUp,
                Key::Down | Key::KP_Down => PaletteAction::VolumeDown,
                Key::Return | Key::KP_Enter if !on_button => PaletteAction::AddToQueue,
                Key::z if !shift => {
                    if self.can_undo() {
                        self.intent(Intent::Undo);
                    }
                    return true;
                }
                Key::v if self.can_paste_session() => {
                    self.paste_session();
                    return true;
                }
                _ => return false,
            };
            if self.can_run(action) {
                self.run_action(action);
            }
            return true;
        }
        if !plain {
            return false;
        }
        let grid = self.grid_focused();
        let all = self.all_focused();
        // Left and Right seek until something in the All view is selected.
        let along = all && self.all_spot.get().is_some();
        match key {
            Key::Escape if self.view.get() == AppView::NowPlaying => self.go_back(),
            Key::space if !on_button => self.intent(Intent::Toggle),
            // A grid of covers takes the arrows itself.
            Key::Left | Key::Right | Key::Up | Key::Down if grid => return false,
            Key::Left if along => self.move_all_selection(Dir::Left),
            Key::Right if along => self.move_all_selection(Dir::Right),
            Key::Up if all => self.move_all_selection(Dir::Up),
            Key::Down if all => self.move_all_selection(Dir::Down),
            Key::Left | Key::KP_Left => self.intent(Intent::SeekBy(-vm::SEEK_STEP_SECS)),
            Key::Right | Key::KP_Right => self.intent(Intent::SeekBy(vm::SEEK_STEP_SECS)),
            Key::Up if self.results_at_top() => self.focus_search(),
            Key::Return | Key::KP_Enter if !on_button => self.activate_selection(false),
            Key::Delete | Key::KP_Delete | Key::BackSpace => {
                match self
                    .selected_edit(QueueEdit::Remove)
                    .filter(|&e| self.can_edit(e))
                {
                    Some(e) => self.edit_queue(e),
                    None => return false,
                }
            }
            _ => return false,
        }
        true
    }

    /// Closes the palette, if open, and shows `view`; for the window's shortcuts.
    fn palette_show(&self, view: AppView) -> bool {
        self.close_palette();
        self.show(view);
        true
    }
}

fn hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn open_url(url: &str) {
    if let Err(e) = gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>) {
        eprintln!("requiem-gui: couldn't open {url}: {e}");
    }
}

/// A page that is loading, failed or empty, in its empty state. `only` is the service its
/// items are filtered to; `retry` is Try again's.
fn page_state(
    state: &EmptyState,
    icon: Icon,
    view: &PageView,
    name: &str,
    only: Option<Service>,
    retry: impl Fn() + 'static,
) {
    match view {
        PageView::Loading => state.set(icon, &format!("Loading {name}"), "", None),
        PageView::Failed(e) => {
            state.set(
                icon,
                &format!("Couldn't load {name}"),
                e,
                Some(("Try again", Box::new(retry))),
            );
        }
        PageView::Ready => match only {
            Some(s) => state.set(
                icon,
                &format!("{name} has nothing from {}", s.name()),
                "Choose All to see every service.",
                None,
            ),
            None => state.set(icon, &format!("{name} is empty"), "", None),
        },
    }
}

/// The sign-in buttons, one per provider: a title and the provider's mark. A click on button
/// `i` runs `run(i)`.
fn sign_in_actions(
    providers: &[ProviderId],
    run: impl Fn(usize) + Clone + 'static,
) -> Vec<widgets::MarkedAction> {
    providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let title = format!("Sign in to {}", vm::provider_name(&p.0));
            let mark = Service::of(p).map(|s| icons::mark(s, 14).upcast());
            let f = run.clone();
            let on: Box<dyn Fn()> = Box::new(move || f(i));
            (title, mark, on)
        })
        .collect()
}

struct CollectionPatch {
    collection: Collection,
    from: Place,
    back: Option<String>,
    view: PageView,
    rows: Vec<RowText>,
    shelves: Vec<Shelf>,
}

struct NowPlaying {
    track: Option<(String, String)>,
    link: Option<String>,
    total: String,
    artwork: Option<String>,
    badge: Option<Service>,
}

struct LibraryPatch {
    collections: Vec<Collection>,
    stocked: bool,
    mixed: bool,
    chips: Vec<(LibraryChip, bool)>,
    text: String,
    sort: LibrarySort,
}

/// Orders titles whatever their case, by the user's locale.
fn localized_cmp(a: &str, b: &str) -> Ordering {
    glib::CollationKey::from(a.to_lowercase()).cmp(&glib::CollationKey::from(b.to_lowercase()))
}

struct ResultsPatch {
    rows: Vec<RowText>,
    view: SearchView,
}

/// Owned copies of the dirty regions, taken so the view-model borrow ends before GTK runs.
struct Patch {
    results: Option<ResultsPatch>,
    top_play: Option<bool>,
    search_status: Option<Option<(String, bool)>>,
    queue: Option<(Vec<RowText>, Option<usize>)>,
    queue_badge: Option<Option<String>>,
    home: Option<(HomeView, Vec<Shelf>, Option<Service>)>,
    library: Option<(LibraryView, LibraryPatch, Option<Service>)>,
    filter: Option<(bool, ServiceFilter)>,
    settings: Option<Vec<AccountRow>>,
    collection: Option<Option<CollectionPatch>>,
    now_playing: Option<NowPlaying>,
    now_page: Option<(Option<Track>, Vec<Track>, Option<Service>)>,
    elapsed: Option<String>,
    progress: Option<f64>,
    playing: Option<bool>,
    volume: Option<u8>,
    controls: Option<Controls>,
    toast: Option<Option<Toast>>,
    banner: Option<Option<Banner>>,
}

impl Patch {
    fn new(d: Dirty, s: &UiState) -> Patch {
        let any = |r: Dirty| d.contains(r);
        Patch {
            results: any(Dirty::RESULTS).then(|| {
                let view = s.search_view();
                let songs = view.body == SearchBody::Songs;
                ResultsPatch {
                    rows: if songs {
                        rows(&s.shown_results())
                    } else {
                        Vec::new()
                    },
                    view,
                }
            }),
            top_play: any(Dirty::TOP_ARTIST).then(|| s.top_artist_playable()),
            search_status: any(Dirty::SEARCH_STATUS).then(|| s.search_status_text()),
            queue: any(Dirty::QUEUE).then(|| (rows(&s.queue), s.current)),
            queue_badge: any(Dirty::QUEUE).then(|| s.queue_badge()),
            home: any(Dirty::HOME).then(|| {
                (
                    s.home_view(),
                    s.shown_home().into_owned(),
                    s.shown_service(),
                )
            }),
            library: d
                .intersects(Dirty::LIBRARY | Dirty::ACCOUNT | Dirty::LIBRARY_VIEW)
                .then(|| {
                    let grid = s.library_grid(localized_cmp);
                    let patch = LibraryPatch {
                        collections: grid.collections.into_iter().cloned().collect(),
                        stocked: grid.stocked,
                        mixed: grid.mixed,
                        chips: grid.chips,
                        text: s.library_text.clone(),
                        sort: s.library_sort,
                    };
                    (s.library_view(), patch, s.shown_service())
                }),
            filter: any(Dirty::ACCOUNT).then(|| (s.filter_shown(), s.filter)),
            settings: (any(Dirty::LIBRARY) || any(Dirty::ACCOUNT)).then(|| s.settings_view()),
            collection: any(Dirty::COLLECTION).then(|| {
                s.collection.as_ref().map(|c| CollectionPatch {
                    collection: c.collection.clone(),
                    from: c.from,
                    back: c.previous.as_ref().map(|p| p.collection.title.clone()),
                    view: c.browse.view(),
                    rows: rows(&c.tracks),
                    shelves: collection_shelves(&c.browse.shelves),
                })
            }),
            now_playing: d
                .intersects(Dirty::NOW_PLAYING | Dirty::STATE | Dirty::QUEUE)
                .then(|| NowPlaying {
                    track: s
                        .now_playing
                        .as_ref()
                        .map(|t| (t.title.clone(), vm::artists(t))),
                    link: s.playing_artist().map(|a| a.title),
                    total: s.total_text(),
                    artwork: s.now_playing.as_ref().and_then(|t| t.artwork_url.clone()),
                    badge: s.now_playing_badge(),
                }),
            now_page: d.intersects(Dirty::NOW_PLAYING | Dirty::QUEUE).then(|| {
                (
                    s.now_playing.clone(),
                    vm::up_next(&s.queue, s.current).to_vec(),
                    s.now_playing_badge(),
                )
            }),
            elapsed: (any(Dirty::ELAPSED) || any(Dirty::STATE) || any(Dirty::NOW_PLAYING))
                .then(|| s.elapsed_text()),
            progress: (any(Dirty::SEEK) || any(Dirty::NOW_PLAYING) || any(Dirty::STATE))
                .then(|| s.progress()),
            playing: any(Dirty::STATE).then(|| s.is_playing()),
            volume: any(Dirty::VOLUME).then_some(s.volume),
            controls: d
                .intersects(Dirty::CONNECTION | Dirty::QUEUE | Dirty::NOW_PLAYING | Dirty::STATE)
                .then(|| s.controls()),
            toast: any(Dirty::TOAST).then(|| s.toast.clone()),
            banner: any(Dirty::CONNECTION).then(|| s.banner.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_countdown_holds_and_resumes_with_the_time_left() {
        let t0 = Instant::now();
        let c = Countdown::new(t0, false);
        assert_eq!(c, Countdown::Running(t0 + vm::TOAST_DURATION));
        let held = c.hold(t0 + Duration::from_secs(1));
        assert_eq!(
            held,
            Countdown::Held(vm::TOAST_DURATION - Duration::from_secs(1))
        );
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(
            held.resume(t1),
            Countdown::Running(t1 + vm::TOAST_DURATION - Duration::from_secs(1))
        );
    }
}
