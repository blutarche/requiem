//! The window and its regions, and the main-thread glue between AppKit, the view model and
//! the backend.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSTextAlignment, NSTextField, NSTextFieldDelegate, NSView,
    NSWindow, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSSize, NSString};

use super::Delegate;
use super::backend::{Backend, Msg};
use super::bar::PlayerBar;
use super::palette::Palette;
use super::table::{self, Kind, Table};
use super::theme::{self, Icon, Weight};
use super::widgets::{
    self, EmptyState, FLEX_H, FLEX_W, Field, HoverButton, NavItem, PIN_BOTTOM, PIN_LEFT, PIN_RIGHT,
    PIN_TOP, rect,
};
use crate::vm::{self, Banner, Dirty, Intent, PaletteAction, Toast, UiState, Vm};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppView {
    Search,
    Queue,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

pub fn install(ui: Rc<Ui>) {
    UI.with(|u| *u.borrow_mut() = Some(ui));
}

/// Runs `f` on the installed UI; `None` before `install`. Re-entrant.
pub fn with<R>(f: impl FnOnce(&Ui) -> R) -> Option<R> {
    let ui = UI.with(|u| u.borrow().clone())?;
    Some(f(&ui))
}

const WIN_W: CGFloat = 1100.0;
const WIN_H: CGFloat = 720.0;
const MIN_W: CGFloat = 800.0;
const MIN_H: CGFloat = 500.0;
const SIDEBAR_W: CGFloat = 240.0;
const BANNER_H: CGFloat = 32.0;
const PAD: CGFloat = 24.0;
/// The window has a transparent title bar over the content; the page headers start below
/// it.
const TOP: CGFloat = 20.0;
const STATUS_H: CGFloat = 26.0;
const TOAST_H: CGFloat = 40.0;

struct ToastView {
    view: Retained<NSView>,
    accent: Retained<NSView>,
    label: Retained<NSTextField>,
    id: Cell<u64>,
}

pub struct Ui {
    mtm: MainThreadMarker,
    pub window: Retained<NSWindow>,
    pub root: Retained<NSView>,
    vm: RefCell<Vm>,
    backend: Backend,
    view: Cell<AppView>,
    nav_search: Retained<NavItem>,
    nav_queue: Retained<NavItem>,
    main: Retained<NSView>,
    banner: Retained<NSView>,
    banner_label: Retained<NSTextField>,
    search_page: Retained<NSView>,
    pub search: Field,
    status: Retained<NSTextField>,
    pub results: Table,
    results_empty: EmptyState,
    queue_page: Retained<NSView>,
    queue_count: Retained<NSTextField>,
    pub queue: Table,
    queue_empty: EmptyState,
    bar: PlayerBar,
    toast: ToastView,
    pub palette: Palette,
}

impl Ui {
    /// `target` receives the buttons' actions and is the text fields' delegate.
    pub fn new(mtm: MainThreadMarker, vm: Vm, backend: Backend, target: &Delegate) -> Rc<Ui> {
        let delegate = ProtocolObject::<dyn NSTextFieldDelegate>::from_ref(target);
        let target: &AnyObject = target.as_ref();
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        // SAFETY: a standard initializer call; the window is retained by `Ui`, and released-
        // when-closed is turned off below before anything can close it.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, WIN_W, WIN_H),
                style,
                NSBackingStoreType::Buffered,
                true,
            )
        };
        // SAFETY: `Ui` owns the window for the life of the process.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("requiem"));
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        window.setContentMinSize(NSSize::new(MIN_W, MIN_H));
        window.setBackgroundColor(Some(&theme::bg()));
        window.center();

        let root = widgets::panel(
            mtm,
            rect(0.0, 0.0, WIN_W, WIN_H),
            FLEX_W | FLEX_H,
            &theme::bg(),
        );
        window.setContentView(Some(&root));

        let bar = PlayerBar::new(mtm, WIN_W, target);
        root.addSubview(&bar.view);

        let side_h = WIN_H - PlayerBar::H;
        let sidebar = widgets::panel(
            mtm,
            rect(0.0, PlayerBar::H, SIDEBAR_W, side_h),
            FLEX_H | PIN_LEFT,
            &theme::chrome_bg(),
        );
        let edge = widgets::panel(
            mtm,
            rect(SIDEBAR_W - 1.0, 0.0, 1.0, side_h),
            FLEX_H | PIN_RIGHT,
            &theme::chrome_border(),
        );
        sidebar.addSubview(&edge);
        let brand = widgets::placed_label(
            mtm,
            "requiem",
            &theme::font(16.0, Weight::SemiBold),
            &theme::chrome_ink(),
            24.0,
            side_h - 60.0,
            SIDEBAR_W - 48.0,
            PIN_TOP,
        );
        sidebar.addSubview(&brand);
        let nav_y = side_h - 96.0;
        let nav = |y, target, icon, text| {
            NavItem::new(
                mtm,
                rect(12.0, y, SIDEBAR_W - 24.0, 36.0),
                target,
                icon,
                text,
            )
        };
        let nav_search = nav(nav_y - 36.0, AppView::Search, Icon::Search, "Search");
        let nav_queue = nav(nav_y - 76.0, AppView::Queue, Icon::ListMusic, "Queue");
        sidebar.addSubview(&nav_search);
        sidebar.addSubview(&nav_queue);
        root.addSubview(&sidebar);

        let mw = WIN_W - SIDEBAR_W;
        let mh = WIN_H - PlayerBar::H;
        let main = widgets::view(mtm, rect(SIDEBAR_W, PlayerBar::H, mw, mh), FLEX_W | FLEX_H);
        root.addSubview(&main);

        // Search page.
        let search_page = widgets::view(mtm, rect(0.0, 0.0, mw, mh), FLEX_W | FLEX_H);
        let field_y = mh - TOP - Field::H;
        let search = Field::new(
            mtm,
            rect(PAD, field_y, mw - 2.0 * PAD, Field::H),
            FLEX_W | PIN_TOP,
            "Search songs, artists, albums",
            Some(Icon::Search),
        );
        // SAFETY: the delegate is the app delegate, alive for the life of the process.
        unsafe { search.text.setDelegate(Some(delegate)) };
        let status = widgets::placed_label(
            mtm,
            "",
            &theme::font(13.0, Weight::Regular),
            &theme::muted(),
            PAD,
            field_y - STATUS_H / 2.0 - 4.0,
            mw - 2.0 * PAD,
            FLEX_W | PIN_TOP,
        );
        status.setHidden(true);
        let list = rect(PAD, 0.0, mw - 2.0 * PAD, field_y - 16.0);
        let results = Table::new(mtm, list, Kind::Results);
        let results_empty = EmptyState::new(mtm, list, Icon::Search);
        search_page.addSubview(&search.ring);
        search_page.addSubview(&status);
        search_page.addSubview(&results.view);
        search_page.addSubview(&results_empty.view);
        main.addSubview(&search_page);

        // Queue page.
        let queue_page = widgets::view(mtm, rect(0.0, 0.0, mw, mh), FLEX_W | FLEX_H);
        let head_y = field_y + Field::H / 2.0;
        let heading = widgets::placed_label(
            mtm,
            "Queue",
            &theme::font(20.0, Weight::SemiBold),
            &theme::ink(),
            PAD,
            head_y,
            200.0,
            PIN_TOP | PIN_LEFT,
        );
        let queue_count = widgets::placed_label(
            mtm,
            "",
            &theme::font(13.0, Weight::Regular),
            &theme::muted(),
            mw - PAD - 200.0,
            head_y,
            200.0,
            PIN_TOP | PIN_RIGHT,
        );
        queue_count.setAlignment(NSTextAlignment::Right);
        let queue = Table::new(mtm, list, Kind::Queue);
        let queue_empty = EmptyState::new(mtm, list, Icon::ListMusic);
        queue_empty.set(
            Icon::ListMusic,
            "Queue is empty",
            "Play or add a search result",
        );
        queue_page.addSubview(&heading);
        queue_page.addSubview(&queue_count);
        queue_page.addSubview(&queue.view);
        queue_page.addSubview(&queue_empty.view);
        main.addSubview(&queue_page);

        let banner = widgets::panel(
            mtm,
            rect(0.0, mh - BANNER_H, mw, BANNER_H),
            FLEX_W | PIN_TOP,
            &theme::bg_3(),
        );
        let banner_rule = widgets::panel(mtm, rect(0.0, 0.0, mw, 1.0), FLEX_W, &theme::line_2());
        let banner_label = widgets::placed_label(
            mtm,
            "",
            &theme::font(13.0, Weight::Medium),
            &theme::ink_2(),
            PAD,
            BANNER_H / 2.0,
            mw - 2.0 * PAD,
            FLEX_W,
        );
        banner_label.setAlignment(NSTextAlignment::Center);
        banner.addSubview(&banner_rule);
        banner.addSubview(&banner_label);
        banner.setHidden(true);
        main.addSubview(&banner);

        let toast = Self::toast_view(mtm, target);
        root.addSubview(&toast.view);

        let palette = Palette::new(mtm, root.bounds(), delegate);
        root.addSubview(&palette.overlay);

        let ui = Rc::new(Ui {
            mtm,
            window,
            root,
            vm: RefCell::new(vm),
            backend,
            view: Cell::new(AppView::Search),
            nav_search,
            nav_queue,
            main,
            banner,
            banner_label,
            search_page,
            search,
            status,
            results,
            results_empty,
            queue_page,
            queue_count,
            queue,
            queue_empty,
            bar,
            toast,
            palette,
        });
        let query = ui.vm.borrow().ui().query.clone();
        ui.search.set_value(&query);
        ui.show(AppView::Search);
        ui
    }

    fn toast_view(mtm: MainThreadMarker, target: &AnyObject) -> ToastView {
        let w = 320.0;
        let view = widgets::panel(
            mtm,
            rect(WIN_W - 16.0 - w, PlayerBar::H + 16.0, w, TOAST_H),
            PIN_RIGHT | PIN_BOTTOM,
            &theme::bg_3(),
        );
        widgets::round(&view, theme::RADIUS);
        widgets::border(&view, &theme::line_2(), 1.0);
        let accent = widgets::panel(mtm, rect(0.0, 0.0, 3.0, TOAST_H), FLEX_H, &theme::bad());
        let label = widgets::placed_label(
            mtm,
            "",
            &theme::font(13.0, Weight::Regular),
            &theme::ink_2(),
            16.0,
            TOAST_H / 2.0,
            w - 16.0 - 40.0,
            FLEX_W,
        );
        let close = HoverButton::new(
            mtm,
            rect(w - 32.0, (TOAST_H - 24.0) / 2.0, 24.0, 24.0),
            Icon::X,
            14.0,
            &theme::muted(),
            None,
            &theme::line_2(),
            theme::RADIUS,
        );
        close.setAutoresizingMask(PIN_RIGHT);
        close.set_action(target, sel!(closeToast:));
        view.addSubview(&accent);
        view.addSubview(&label);
        view.addSubview(&close);
        view.setHidden(true);
        ToastView {
            view,
            accent,
            label,
            id: Cell::new(0),
        }
    }

    pub fn current_view(&self) -> AppView {
        self.view.get()
    }

    pub fn show(&self, view: AppView) {
        self.view.set(view);
        self.search_page.setHidden(view != AppView::Search);
        self.queue_page.setHidden(view != AppView::Queue);
        self.nav_search.set_active(view == AppView::Search);
        self.nav_queue.set_active(view == AppView::Queue);
        let table = match view {
            AppView::Search => &self.results.table,
            AppView::Queue => &self.queue.table,
        };
        // Keeps Space and the arrow keys from landing in a hidden text field.
        self.window.makeFirstResponder(Some(table));
    }

    pub fn focus_search(&self) {
        self.show(AppView::Search);
        self.window.makeFirstResponder(Some(&self.search.text));
    }

    pub fn open_palette(&self) {
        if !self.palette.is_open() {
            self.palette.open();
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
            PaletteAction::FocusSearch => self.focus_search(),
            PaletteAction::ShowSearch => self.show(AppView::Search),
            PaletteAction::ShowQueue => self.show(AppView::Queue),
            PaletteAction::VolumeUp => self.intent(Intent::VolumeBy(vm::VOLUME_STEP)),
            PaletteAction::VolumeDown => self.intent(Intent::VolumeBy(-vm::VOLUME_STEP)),
        }
    }

    pub fn intent(&self, intent: Intent) {
        let cmds = self.vm.borrow_mut().intent(intent);
        for cmd in cmds {
            let events = self.backend.send(cmd);
            let mut vm = self.vm.borrow_mut();
            for ev in events {
                vm.apply(ev);
            }
        }
        self.render();
    }

    pub fn search_submitted(&self) {
        self.intent(Intent::Search(self.search.value()));
    }

    /// Plays (or with `append`, appends) the keyboard-selected row of the visible table.
    pub fn activate_selection(&self, append: bool) {
        match self.view.get() {
            AppView::Search => {
                if let Some(i) = self.results.selected_row() {
                    self.intent(if append {
                        Intent::AppendResult(i)
                    } else {
                        Intent::PlayResult(i)
                    });
                }
            }
            AppView::Queue => {
                if let Some(i) = self.queue.selected_row() {
                    self.intent(Intent::PlayQueue(i));
                }
            }
        }
    }

    pub fn seek_fraction(&self, fraction: f64) {
        let d = self.vm.borrow().ui().duration;
        if let Some(d) = d {
            self.intent(Intent::SeekTo(d.mul_f64(fraction.clamp(0.0, 1.0))));
        }
    }

    pub fn set_volume(&self, volume: u8) {
        self.intent(Intent::SetVolume(volume));
    }

    pub fn close_toast(&self) {
        self.dismiss_toast(self.toast.id.get());
    }

    fn dismiss_toast(&self, id: u64) {
        self.vm.borrow_mut().dismiss_toast(id);
        self.render();
    }

    pub fn receive(&self, batch: Vec<Msg>) {
        let mut raise = false;
        {
            let mut vm = self.vm.borrow_mut();
            for m in batch {
                match m {
                    Msg::Event(ev) => vm.apply(ev),
                    Msg::Link(l) => vm.link(l),
                    Msg::Raise => raise = true,
                }
            }
        }
        self.render();
        if raise {
            self.raise();
        }
    }

    pub fn raise(&self) {
        let app = NSApplication::sharedApplication(self.mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        if self.window.isMiniaturized() {
            self.window.deminiaturize(None);
        }
        self.window.makeKeyAndOrderFront(None);
    }

    /// Pushes the view model's changed regions into the views.
    pub fn render(&self) {
        let patch = {
            let mut vm = self.vm.borrow_mut();
            let dirty = vm.take_dirty();
            if dirty.is_empty() {
                return;
            }
            Patch::new(dirty, vm.ui())
        };
        self.apply(patch);
    }

    fn apply(&self, p: Patch) {
        if let Some(status) = p.search_status {
            match &status {
                Some((text, error)) => {
                    widgets::set_text(&self.status, text);
                    let c = if *error { theme::bad() } else { theme::muted() };
                    self.status.setTextColor(Some(&c));
                }
                None => widgets::set_text(&self.status, ""),
            }
            self.status.setHidden(status.is_none());
            self.layout_search(status.is_some());
        }
        if let Some((rows, empty)) = p.results {
            let has_rows = !rows.is_empty();
            self.results.set_rows(rows, None);
            self.results.deselect();
            self.results.view.setHidden(!has_rows);
            self.results_empty.view.setHidden(has_rows);
            if let Some((heading, sub)) = empty {
                self.results_empty.set(Icon::Search, &heading, &sub);
            }
        }
        if let Some((rows, current)) = p.queue {
            let n = rows.len();
            widgets::set_text(
                &self.queue_count,
                &match n {
                    0 => String::new(),
                    1 => "1 track".to_owned(),
                    n => format!("{n} tracks"),
                },
            );
            self.queue.set_rows(rows, current);
            self.queue.view.setHidden(n == 0);
            self.queue_empty.view.setHidden(n != 0);
        }
        if let Some(now) = p.now_playing {
            let track = now.track.as_ref().map(|(t, a)| (t.as_str(), a.as_str()));
            self.bar.set_track(track, &now.total);
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
        if let Some(t) = p.toast {
            self.show_toast(t);
        }
        if let Some(b) = p.banner {
            self.show_banner(b);
        }
    }

    fn layout_search(&self, status: bool) {
        let b = self.search_page.bounds();
        let field_y = b.size.height - TOP - Field::H;
        let top = if status {
            field_y - 8.0 - STATUS_H
        } else {
            field_y - 16.0
        };
        let list = rect(PAD, 0.0, b.size.width - 2.0 * PAD, top);
        self.results.view.setFrame(list);
        self.results_empty.view.setFrame(list);
    }

    fn show_banner(&self, banner: Option<Banner>) {
        let b = self.main.bounds();
        let shown = banner.is_some();
        if let Some(banner) = banner {
            widgets::set_text(&self.banner_label, banner.text);
            let c = if banner.error {
                theme::bad()
            } else {
                theme::ink_2()
            };
            self.banner_label.setTextColor(Some(&c));
        }
        self.banner.setHidden(!shown);
        let h = b.size.height - if shown { BANNER_H } else { 0.0 };
        let page = rect(0.0, 0.0, b.size.width, h);
        self.search_page.setFrame(page);
        self.queue_page.setFrame(page);
    }

    fn show_toast(&self, toast: Option<Toast>) {
        let t = &self.toast;
        let Some(toast) = toast else {
            t.view.setHidden(true);
            return;
        };
        t.id.set(toast.id);
        widgets::set_text(&t.label, &toast.text);
        t.accent.setHidden(!toast.error);
        let text_w = t.label.fittingSize().width;
        let w: CGFloat = (16.0 + text_w + 8.0 + 40.0).clamp(240.0, 440.0);
        let root = self.root.bounds();
        t.view.setFrame(rect(
            root.size.width - 16.0 - w,
            PlayerBar::H + 16.0,
            w,
            TOAST_H,
        ));
        t.label.setToolTip(Some(&NSString::from_str(&toast.text)));
        t.view.setHidden(false);
        let id = toast.id;
        if let Ok(when) = DispatchTime::try_from(vm::TOAST_DURATION) {
            let _ = DispatchQueue::main().after(when, move || {
                with(|ui| ui.dismiss_toast(id));
            });
        }
    }
}

type EmptyText = (String, String);

struct NowPlaying {
    track: Option<(String, String)>,
    total: String,
}

/// Owned copies of the dirty regions, taken so the view-model borrow ends before AppKit
/// runs.
struct Patch {
    /// Rows, and the empty-state heading and subtext when there are none.
    results: Option<(Vec<table::RowText>, Option<EmptyText>)>,
    search_status: Option<Option<(String, bool)>>,
    queue: Option<(Vec<table::RowText>, Option<usize>)>,
    now_playing: Option<NowPlaying>,
    elapsed: Option<String>,
    progress: Option<f64>,
    playing: Option<bool>,
    volume: Option<u8>,
    toast: Option<Option<Toast>>,
    banner: Option<Option<Banner>>,
}

impl Patch {
    fn new(d: Dirty, s: &UiState) -> Patch {
        let any = |r: Dirty| d.contains(r);
        Patch {
            results: any(Dirty::RESULTS).then(|| (table::rows(&s.results), s.results_empty())),
            search_status: any(Dirty::SEARCH_STATUS).then(|| s.search_status_text()),
            queue: any(Dirty::QUEUE).then(|| (table::rows(&s.queue), s.current)),
            now_playing: (any(Dirty::NOW_PLAYING) || any(Dirty::STATE)).then(|| NowPlaying {
                track: s
                    .now_playing
                    .as_ref()
                    .map(|t| (t.title.clone(), vm::artists(t))),
                total: s.total_text(),
            }),
            elapsed: (any(Dirty::ELAPSED) || any(Dirty::STATE) || any(Dirty::NOW_PLAYING))
                .then(|| s.elapsed_text()),
            progress: (any(Dirty::SEEK) || any(Dirty::NOW_PLAYING) || any(Dirty::STATE))
                .then(|| s.progress()),
            playing: any(Dirty::STATE).then(|| s.is_playing()),
            volume: any(Dirty::VOLUME).then_some(s.volume),
            toast: any(Dirty::TOAST).then(|| s.toast.clone()),
            banner: any(Dirty::CONNECTION).then(|| s.banner()),
        }
    }
}
