//! The window and its regions, and the main-thread glue between AppKit, the view model and
//! the backend.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSApplication, NSAttributedStringNSStringDrawing, NSEvent,
    NSLineBreakMode, NSResponder, NSTextAlignment, NSTextField, NSTextFieldDelegate,
    NSTrackingAreaOptions, NSView, NSWindow, NSWindowOcclusionState, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::backend::{Backend, Msg};
use super::bar::PlayerBar;
use super::palette::Palette;
use super::table::{self, Kind, Table};
use super::theme::{self, Icon, Weight};
use super::widgets::{
    self, EmptyState, FLEX_H, FLEX_W, Field, HoverButton, NavItem, PIN_BOTTOM, PIN_LEFT, PIN_RIGHT,
    PIN_TOP, rect,
};
use super::{Delegate, MainWindow};
use crate::vm::{
    self, Banner, Controls, Dirty, EmptyAction, EmptyText, Intent, PaletteAction, QueueEdit, Toast,
    UiState, Vm,
};

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
/// One line; a second adds a line's height.
const BANNER_H: CGFloat = 32.0;
const PAD: CGFloat = 24.0;
/// The window has a transparent title bar over the content; the page headers start below
/// it.
const TOP: CGFloat = 20.0;
const STATUS_H: CGFloat = 26.0;
const TOAST_H: CGFloat = 40.0;
/// Tall enough for a compact button.
const TOAST_ACTION_H: CGFloat = 48.0;
const TOAST_BUTTON_H: CGFloat = 32.0;
/// How far below its place a toast starts as it fades in.
const TOAST_RISE: CGFloat = 8.0;
/// How long "Quit and Stop Music" waits for the player's goodbye.
const QUIT_GRACE: Duration = Duration::from_secs(3);

define_class!(
    /// The toast's box; the pointer resting on it holds the toast's countdown.
    // SAFETY: NSView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct ToastPanel;

    impl ToastPanel {
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            with(|ui| ui.hover_toast(true));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            with(|ui| ui.hover_toast(false));
        }

        /// A hidden view gets no `mouseExited:`.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            with(|ui| ui.hover_toast(false));
        }
    }
);

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

struct ToastView {
    view: Retained<NSView>,
    accent: Retained<NSView>,
    label: Retained<NSTextField>,
    action: Retained<HoverButton>,
    id: Cell<u64>,
    /// False from the start of the exit; the view hides once the exit ends.
    shown: Cell<bool>,
    /// The pointer is on the toast.
    hovered: Cell<bool>,
    clock: Cell<Countdown>,
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
    /// False from the start of the fade-out; the view hides once it ends.
    banner_shown: Cell<bool>,
    /// How far the banner pushes the pages down.
    banner_inset: Cell<CGFloat>,
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
    /// The banner deadline a `tick` is already scheduled for.
    tick_at: Cell<Option<Instant>>,
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
        let window = MainWindow::create(mtm, rect(0.0, 0.0, WIN_W, WIN_H), style);
        // SAFETY: `Ui` owns the window for the life of the process.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("requiem"));
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        window.setContentMinSize(NSSize::new(MIN_W, MIN_H));
        window.setBackgroundColor(Some(&theme::bg()));
        // Tab moves between the field and the table, and through the buttons with keyboard
        // navigation on.
        window.setAutorecalculatesKeyViewLoop(true);
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
        // SAFETY: the field's delegate is weak. It is the app delegate, which `app::run` holds
        // in a local until `app.run()`, which never returns.
        unsafe { search.text.setDelegate(Some(delegate)) };
        let status = widgets::placed_label(
            mtm,
            "",
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
            PAD,
            field_y - STATUS_H / 2.0 - 4.0,
            mw - 2.0 * PAD,
            FLEX_W | PIN_TOP,
        );
        status.setHidden(true);
        let list = rect(PAD, 0.0, mw - 2.0 * PAD, field_y - 16.0);
        let results = Table::new(mtm, list, Kind::Results);
        let results_empty = EmptyState::new(mtm, list, Icon::Search, target, sel!(emptyAction:));
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
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
            mw - PAD - 200.0,
            head_y,
            200.0,
            PIN_TOP | PIN_RIGHT,
        );
        queue_count.setAlignment(NSTextAlignment::Right);
        // On the heading's baseline rather than its centre line. Labels draw their text from
        // the top of their frame, so the baseline is the top less the first-baseline offset.
        let baseline = heading.frame().origin.y + heading.frame().size.height
            - heading.firstBaselineOffsetFromTop();
        let mut f = queue_count.frame();
        f.origin.y = baseline + queue_count.firstBaselineOffsetFromTop() - f.size.height;
        queue_count.setFrame(f);
        let queue = Table::new(mtm, list, Kind::Queue);
        let queue_empty = EmptyState::new(mtm, list, Icon::ListMusic, target, sel!(focusSearch:));
        queue_empty.set(
            Icon::ListMusic,
            "Queue is empty",
            "Play or add a search result",
            "Go to search",
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
            &theme::font(12.0, Weight::Medium),
            &theme::ink_2(),
            PAD,
            BANNER_H / 2.0,
            mw - 2.0 * PAD,
            FLEX_W,
        );
        banner_label.setAlignment(NSTextAlignment::Center);
        // The stuck banner can outgrow a narrow window; it wraps rather than losing its end.
        banner_label.setUsesSingleLineMode(false);
        banner_label.setMaximumNumberOfLines(2);
        banner_label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        if let Some(cell) = banner_label.cell() {
            cell.setTruncatesLastVisibleLine(true);
        }
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
            banner_shown: Cell::new(false),
            banner_inset: Cell::new(0.0),
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
            tick_at: Cell::new(None),
        });
        let query = ui.vm.borrow().ui().query.clone();
        ui.search.set_value(&query);
        ui.show(AppView::Search);
        ui
    }

    fn toast_view(mtm: MainThreadMarker, target: &AnyObject) -> ToastView {
        let w = 320.0;
        let frame = rect(WIN_W - 16.0 - w, PlayerBar::H + 16.0, w, TOAST_H);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let panel: Retained<ToastPanel> =
            unsafe { msg_send![ToastPanel::alloc(mtm), initWithFrame: frame] };
        // Always active, so leaving the toast while another app is in front still resumes it.
        widgets::track_hover(&panel, NSTrackingAreaOptions::ActiveAlways);
        let view: Retained<NSView> = Retained::into_super(panel);
        view.setAutoresizingMask(PIN_RIGHT | PIN_BOTTOM);
        widgets::fill(&view, Some(&theme::bg_3()));
        widgets::round(&view, theme::RADIUS);
        widgets::border(&view, &theme::line_2(), 1.0);
        let accent = widgets::panel(mtm, rect(0.0, 0.0, 3.0, TOAST_H), FLEX_H, &theme::bad());
        // Both vertical margins flex, so the rows stay centred as the height changes.
        let centred = PIN_TOP | PIN_BOTTOM;
        let label = widgets::placed_label(
            mtm,
            "",
            &theme::font(14.0, Weight::Regular),
            &theme::ink_2(),
            16.0,
            TOAST_H / 2.0,
            w - 16.0 - 40.0,
            FLEX_W | centred,
        );
        // Long messages wrap to a second line rather than losing their end.
        label.setUsesSingleLineMode(false);
        label.setMaximumNumberOfLines(2);
        label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        if let Some(cell) = label.cell() {
            cell.setTruncatesLastVisibleLine(true);
        }
        let action = HoverButton::secondary(
            mtm,
            rect(0.0, (TOAST_H - TOAST_BUTTON_H) / 2.0, 0.0, TOAST_BUTTON_H),
        );
        action.setAutoresizingMask(PIN_RIGHT | centred);
        action.set_action(target, sel!(toastAction:));
        action.setHidden(true);
        let close = HoverButton::new(
            mtm,
            rect(w - 32.0, (TOAST_H - 24.0) / 2.0, 24.0, 24.0),
            Icon::X,
            16.0,
            &theme::muted(),
            None,
            &theme::line_2(),
            theme::RADIUS,
        );
        close.setAutoresizingMask(PIN_RIGHT | centred);
        close.set_action(target, sel!(closeToast:));
        widgets::describe(&close, "Dismiss", None);
        view.addSubview(&accent);
        view.addSubview(&label);
        view.addSubview(&action);
        view.addSubview(&close);
        view.setHidden(true);
        ToastView {
            view,
            accent,
            label,
            action,
            id: Cell::new(0),
            shown: Cell::new(false),
            hovered: Cell::new(false),
            clock: Cell::new(Countdown::Held(Duration::ZERO)),
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
        if view == AppView::Search && self.vm.borrow().ui().results.is_empty() {
            self.window.makeFirstResponder(Some(&self.search.text));
            return;
        }
        let table = match view {
            AppView::Search => &self.results.table,
            AppView::Queue => &self.queue.table,
        };
        // Keeps Space and the arrow keys from landing in a hidden text field.
        self.window.makeFirstResponder(Some(table));
    }

    /// The results table has focus with its first row selected.
    pub fn results_at_top(&self) -> bool {
        let focused = self
            .window
            .firstResponder()
            .is_some_and(|r| std::ptr::eq::<AnyObject>(r.as_ref(), self.results.table.as_ref()));
        self.view.get() == AppView::Search && focused && self.results.selected_row() == Some(0)
    }

    /// Whether the visible table has a row that Return (or with `append`, Cmd+Return) acts on.
    pub fn has_selection(&self, append: bool) -> bool {
        match self.view.get() {
            AppView::Search => self.results.selected_row().is_some(),
            AppView::Queue => !append && self.queue.selected_row().is_some(),
        }
    }

    /// Whether something plays, and which controls are live; for the menus.
    pub fn playback(&self) -> (bool, Controls) {
        let vm = self.vm.borrow();
        (vm.ui().is_playing(), vm.ui().controls())
    }

    /// Whether "Quit and Stop Music" is available.
    pub fn can_stop_player(&self) -> bool {
        self.vm.borrow().ui().can_stop_player()
    }

    /// Stops the player, then the window closes on its goodbye. Does nothing without a
    /// connected player; the menu item is disabled then.
    pub fn quit_and_stop(&self) {
        if !self.can_stop_player() || !self.backend.stop_player() {
            return;
        }
        // A player that never says goodbye still lets the window go.
        if let Ok(when) = DispatchTime::try_from(QUIT_GRACE) {
            let _ = DispatchQueue::main().after(when, || {
                if let Some(mtm) = MainThreadMarker::new() {
                    NSApplication::sharedApplication(mtm).terminate(None);
                }
            });
        }
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

    pub fn toggle_palette(&self) {
        if self.palette.is_open() {
            self.close_palette();
        } else {
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
        // Clicked or not, the palette is a keyboard tool.
        widgets::note_key_intent();
        self.close_palette();
        match action {
            PaletteAction::PlayPause => self.intent(Intent::Toggle),
            PaletteAction::Next => self.intent(Intent::Next),
            PaletteAction::Previous => self.intent(Intent::Prev),
            PaletteAction::PlaySelected => self.activate_selection(false),
            PaletteAction::AddToQueue => self.activate_selection(true),
            PaletteAction::FocusSearch => self.focus_search(),
            PaletteAction::ShowSearch => self.show(AppView::Search),
            PaletteAction::ShowQueue => self.show(AppView::Queue),
            PaletteAction::VolumeUp => self.intent(Intent::VolumeBy(vm::VOLUME_STEP)),
            PaletteAction::VolumeDown => self.intent(Intent::VolumeBy(-vm::VOLUME_STEP)),
            PaletteAction::ClearQueue => self.edit_queue(QueueEdit::Clear),
        }
    }

    pub fn intent(&self, intent: Intent) {
        self.run(intent);
    }

    /// Appends result `i`, and confirms it on the row's "+" when it was sent.
    pub fn append_result(&self, i: usize) {
        if self.run(Intent::AppendResult(i)) {
            self.results.confirm_add(i);
        }
    }

    /// `intent`; true when it sent anything.
    fn run(&self, intent: Intent) -> bool {
        widgets::note_key_event(self.mtm);
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
    }

    /// The results empty state's button: "Start a search" runs the typed query, or focuses
    /// the empty field; "Edit search" focuses the field.
    pub fn empty_action(&self) {
        let action = self.vm.borrow().ui().results_empty().map(|e| e.action);
        if action == Some(EmptyAction::StartSearch) && !self.search.value().trim().is_empty() {
            self.search_submitted();
        } else {
            self.focus_search();
        }
    }

    pub fn search_submitted(&self) {
        self.intent(Intent::Search(self.search.value()));
    }

    /// Plays (or with `append`, appends) the keyboard-selected row of the visible table.
    pub fn activate_selection(&self, append: bool) {
        match self.view.get() {
            AppView::Search => match self.results.selected_row() {
                Some(i) if append => self.append_result(i),
                Some(i) => self.intent(Intent::PlayResult(i)),
                None => {}
            },
            AppView::Queue => {
                if let Some(i) = self.queue.selected_row().filter(|_| !append) {
                    self.play_queue(i);
                }
            }
        }
    }

    /// `edit` applied to the selected queue row, while the Queue view shows one.
    pub fn selected_edit(&self, edit: fn(usize) -> QueueEdit) -> Option<QueueEdit> {
        if self.view.get() != AppView::Queue {
            return None;
        }
        self.queue.selected_row().map(edit)
    }

    /// Whether `edit` would send anything.
    pub fn can_edit(&self, edit: QueueEdit) -> bool {
        self.vm.borrow().ui().edit_command(edit).is_some()
    }

    pub fn edit_queue(&self, edit: QueueEdit) {
        let selected = self.queue.selected_row();
        self.intent(Intent::EditQueue { edit, selected });
    }

    /// Plays queue row `i` and drops the selection, so the current-row marker is what
    /// stands out.
    pub fn play_queue(&self, i: usize) {
        self.intent(Intent::PlayQueue(i));
        self.queue.deselect();
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
        widgets::note_key_event(self.mtm);
        self.dismiss_toast(self.toast.id.get());
    }

    /// The toast's action button: Undo, its only action.
    pub fn toast_action(&self) {
        self.intent(Intent::Undo);
    }

    /// Cmd+Z outside a text field: the same undo as the toast's button.
    pub fn undo(&self) {
        self.intent(Intent::Undo);
    }

    /// The Undo menu item's title, and whether it is enabled.
    pub fn undo_item(&self) -> (&'static str, bool) {
        let vm = self.vm.borrow();
        let title = vm.undo_title();
        (
            title.unwrap_or("Undo"),
            title.is_some() && vm.ui().controls().player,
        )
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
        self.schedule_dismissal(t.id.get(), clock);
    }

    /// Starts toast `id`'s countdown, held if the pointer already rests on `frame`.
    fn start_countdown(&self, id: u64, frame: NSRect) {
        let at = self
            .root
            .convertPoint_fromView(self.window.mouseLocationOutsideOfEventStream(), None);
        let (o, s) = (frame.origin, frame.size);
        let over = at.x >= o.x && at.x < o.x + s.width && at.y >= o.y && at.y < o.y + s.height;
        let t = &self.toast;
        t.hovered.set(over);
        let clock = Countdown::new(Instant::now(), over);
        t.clock.set(clock);
        self.schedule_dismissal(id, clock);
    }

    fn schedule_dismissal(&self, id: u64, clock: Countdown) {
        let Countdown::Running(end) = clock else {
            return;
        };
        let wait = end.saturating_duration_since(Instant::now());
        if let Ok(when) = DispatchTime::try_from(wait) {
            let _ = DispatchQueue::main().after(when, move || {
                with(|ui| ui.countdown_ended(id, end));
            });
        }
    }

    /// Dismisses toast `id`, unless another replaced it or its countdown was held since.
    fn countdown_ended(&self, id: u64, end: Instant) {
        let t = &self.toast;
        if t.id.get() == id && t.clock.get() == Countdown::Running(end) {
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
                    Msg::Raise => raise = true,
                }
            }
            vm.take_commands()
        };
        self.send(cmds);
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

    /// Pushes the view model's changed regions into the views. While the window can't be
    /// seen they pile up in the view model, for `catch_up`.
    pub fn render(&self) {
        if self.on_screen() {
            self.render_now();
        } else {
            self.schedule_tick();
        }
    }

    /// Renders what changed while the window couldn't be seen, once it can.
    pub fn catch_up(&self) {
        if self.on_screen() {
            self.render_now();
        }
    }

    /// `render`, whether or not the window can be seen.
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
        self.schedule_tick();
    }

    /// Not hidden, minimized or fully covered.
    fn on_screen(&self) -> bool {
        let w = &self.window;
        w.isVisible()
            && !w.isMiniaturized()
            && !NSApplication::sharedApplication(self.mtm).isHidden()
            && w.occlusionState().contains(NSWindowOcclusionState::Visible)
    }

    /// Wakes the view model when the banner is due to change on its own.
    fn schedule_tick(&self) {
        let next = self.vm.borrow().next_tick();
        if next == self.tick_at.get() {
            return;
        }
        self.tick_at.set(next);
        let Some(at) = next else { return };
        let wait = at.saturating_duration_since(Instant::now());
        if let Ok(when) = DispatchTime::try_from(wait) {
            let _ = DispatchQueue::main().after(when, move || {
                with(|ui| ui.tick(at));
            });
        }
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
            if let Some(e) = empty {
                self.results_empty
                    .set(Icon::Search, &e.heading, &e.sub, e.action.label());
            }
        }
        if let Some(badge) = p.queue_badge {
            self.nav_queue.set_count(badge.as_deref());
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
        if let Some(s) = p.seek_spoken {
            self.bar.set_seek_spoken(&s);
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

    /// Fits the banner to its text at the current width: one line, or two when one is too
    /// narrow. Returns its height.
    fn layout_banner(&self) -> CGFloat {
        let b = self.main.bounds();
        let l = &self.banner_label;
        let room = b.size.width - 2.0 * PAD;
        let font = l
            .font()
            .unwrap_or_else(|| theme::font(12.0, Weight::Medium));
        let text = l.stringValue().to_string();
        // The label's cell pads its text by 2 pt a side.
        let text_w = widgets::attributed(&text, &font, &theme::ink_2())
            .size()
            .width
            .ceil()
            + 4.0;
        let lines: CGFloat = if text_w > room { 2.0 } else { 1.0 };
        let line_h = widgets::line_height(&font);
        let h = BANNER_H + (lines - 1.0) * (line_h - 2.0);
        self.banner
            .setFrame(rect(0.0, b.size.height - h, b.size.width, h));
        let text_h = lines * line_h;
        l.setFrame(rect(PAD, ((h - text_h) / 2.0).round(), room, text_h));
        h
    }

    /// Keeps a shown banner fitted to the window's width.
    pub fn window_resized(&self) {
        if !self.banner_shown.get() {
            return;
        }
        let h = self.layout_banner();
        if self.banner_inset.replace(h) != h {
            let b = self.main.bounds();
            let page = rect(0.0, 0.0, b.size.width, b.size.height - h);
            self.search_page.setFrame(page);
            self.queue_page.setFrame(page);
        }
    }

    fn show_banner(&self, banner: Option<Banner>) {
        let b = self.main.bounds();
        let shown = banner.is_some();
        if let Some(banner) = &banner {
            if !self.banner_shown.get()
                || self.banner_label.stringValue().to_string() != banner.text
            {
                widgets::announce(self.mtm, &banner.text);
            }
            widgets::set_text(&self.banner_label, &banner.text);
            // Two lines can still be too few at the minimum width.
            self.banner_label
                .setToolTip(Some(&NSString::from_str(&banner.text)));
            let c = if banner.error {
                theme::bad()
            } else {
                theme::ink_2()
            };
            self.banner_label.setTextColor(Some(&c));
        }
        let inset = if shown { self.layout_banner() } else { 0.0 };
        let page = rect(0.0, 0.0, b.size.width, b.size.height - inset);
        let place_pages = || {
            self.search_page.animator().setFrame(page);
            self.queue_page.animator().setFrame(page);
        };
        let changed = self.banner_shown.replace(shown) != shown;
        // Also when a shown banner grows to a second line.
        let moved = self.banner_inset.replace(inset) != inset;
        let shift = if moved && !widgets::reduce_motion() {
            widgets::STANDARD
        } else {
            Duration::ZERO
        };
        widgets::animate(&self.main, shift, widgets::EASE_IN_OUT, place_pages, || {});
        if !changed {
            return;
        }
        let banner = &self.banner;
        if shown {
            banner.setAlphaValue(0.0);
            banner.setHidden(false);
        }
        widgets::animate(
            banner,
            widgets::STANDARD,
            widgets::EASE_OUT,
            || {
                banner
                    .animator()
                    .setAlphaValue(if shown { 1.0 } else { 0.0 })
            },
            move || {
                if !shown {
                    with(|ui| {
                        if !ui.banner_shown.get() {
                            ui.banner.setHidden(true);
                        }
                    });
                }
            },
        );
    }

    fn show_toast(&self, toast: Option<Toast>) {
        let t = &self.toast;
        let Some(toast) = toast else {
            if t.shown.replace(false) {
                self.leave_toast();
            }
            return;
        };
        let swap = t.shown.replace(true);
        if swap {
            widgets::crossfade(&t.view, widgets::motion(widgets::MICRO));
        }
        // A toast amended in place keeps its countdown.
        let new = t.id.replace(toast.id) != toast.id;
        // An amended text is read out again; a re-render is not.
        if new || t.label.stringValue().to_string() != toast.text {
            widgets::announce(self.mtm, &toast.text);
        }
        widgets::set_text(&t.label, &toast.text);
        t.accent.setHidden(!toast.error);
        let text_w = t.label.fittingSize().width;
        // The action sits between the text and the close button.
        let action_w = toast.action.map_or(0.0, |a| t.action.set_label(a) + 8.0);
        t.action.setHidden(toast.action.is_none());
        let w: CGFloat = (16.0 + text_w + 8.0 + action_w + 40.0).clamp(240.0, 440.0 + action_w);
        let text_room = w - 16.0 - 8.0 - action_w - 40.0;
        let line_h = t.label.font().map_or(18.0, |f| widgets::line_height(&f));
        let lines: CGFloat = if text_w > text_room { 2.0 } else { 1.0 };
        let base_h = if toast.action.is_some() {
            TOAST_ACTION_H
        } else {
            TOAST_H
        };
        let h = base_h + (lines - 1.0) * (line_h - 2.0);
        let root = self.root.bounds();
        let frame = rect(root.size.width - 16.0 - w, PlayerBar::H + 16.0, w, h);
        t.view.setFrame(frame);
        let text_h = lines * line_h;
        t.label.setFrame(rect(
            16.0,
            ((h - text_h) / 2.0).round(),
            text_room + 8.0,
            text_h,
        ));
        if action_w > 0.0 {
            t.action.setFrame(rect(
                w - 40.0 - action_w,
                (h - TOAST_BUTTON_H) / 2.0,
                action_w - 8.0,
                TOAST_BUTTON_H,
            ));
        }
        t.label.setToolTip(Some(&NSString::from_str(&toast.text)));
        if new {
            self.start_countdown(toast.id, frame);
        }
        if !swap {
            self.enter_toast();
        }
    }

    /// Fades the toast in as it rises into the frame it was given.
    fn enter_toast(&self) {
        let v = &self.toast.view;
        let at = v.frame().origin;
        let rise = if widgets::reduce_motion() {
            0.0
        } else {
            TOAST_RISE
        };
        v.setAlphaValue(0.0);
        v.setFrameOrigin(NSPoint::new(at.x, at.y - rise));
        v.setHidden(false);
        widgets::animate(
            v,
            widgets::motion(widgets::STANDARD),
            widgets::EASE_OUT,
            || {
                v.animator().setAlphaValue(1.0);
                v.animator().setFrameOrigin(at);
            },
            || {},
        );
    }

    /// The reverse of `enter_toast`, then hides it unless another toast came meanwhile.
    fn leave_toast(&self) {
        let t = &self.toast;
        let v = &t.view;
        let at = v.frame().origin;
        let fall = if widgets::reduce_motion() {
            0.0
        } else {
            TOAST_RISE
        };
        let id = t.id.get();
        widgets::animate(
            v,
            widgets::motion(widgets::STANDARD),
            widgets::EASE_OUT,
            || {
                v.animator().setAlphaValue(0.0);
                v.animator().setFrameOrigin(NSPoint::new(at.x, at.y - fall));
            },
            move || {
                with(|ui| {
                    let t = &ui.toast;
                    if t.id.get() == id && !t.shown.get() {
                        t.view.setHidden(true);
                    }
                });
            },
        );
    }
}

struct NowPlaying {
    track: Option<(String, String)>,
    total: String,
}

/// Owned copies of the dirty regions, taken so the view-model borrow ends before AppKit
/// runs.
struct Patch {
    /// Rows, and the empty state when there are none.
    results: Option<(Vec<table::RowText>, Option<EmptyText>)>,
    search_status: Option<Option<(String, bool)>>,
    queue: Option<(Vec<table::RowText>, Option<usize>)>,
    queue_badge: Option<Option<String>>,
    now_playing: Option<NowPlaying>,
    elapsed: Option<String>,
    seek_spoken: Option<String>,
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
            results: any(Dirty::RESULTS).then(|| (table::rows(&s.results), s.results_empty())),
            search_status: any(Dirty::SEARCH_STATUS).then(|| s.search_status_text()),
            queue: any(Dirty::QUEUE).then(|| (table::rows(&s.queue), s.current)),
            queue_badge: any(Dirty::QUEUE).then(|| s.queue_badge()),
            now_playing: (any(Dirty::NOW_PLAYING) || any(Dirty::STATE)).then(|| NowPlaying {
                track: s
                    .now_playing
                    .as_ref()
                    .map(|t| (t.title.clone(), vm::artists(t))),
                total: s.total_text(),
            }),
            elapsed: (any(Dirty::ELAPSED) || any(Dirty::STATE) || any(Dirty::NOW_PLAYING))
                .then(|| s.elapsed_text()),
            seek_spoken: (any(Dirty::ELAPSED) || any(Dirty::NOW_PLAYING)).then(|| s.seek_spoken()),
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
    use requiem_core::{Event, PlayState};

    use super::*;
    use crate::demo;

    #[test]
    fn patch_copies_only_dirty_regions() {
        let (vm, _) = demo::start();
        let p = Patch::new(Dirty::VOLUME, vm.ui());
        assert_eq!(p.volume, Some(70));
        assert!(p.results.is_none() && p.search_status.is_none() && p.queue.is_none());
        assert!(p.queue_badge.is_none());
        assert!(p.now_playing.is_none() && p.elapsed.is_none() && p.progress.is_none());
        assert!(p.playing.is_none() && p.toast.is_none() && p.banner.is_none());
        assert!(p.controls.is_none());
    }

    #[test]
    fn patch_of_everything_matches_the_state() {
        let (vm, _) = demo::start();
        let p = Patch::new(Dirty::ALL, vm.ui());
        let (rows, empty) = p.results.unwrap();
        assert_eq!(rows.len(), 30);
        assert_eq!(empty, None);
        assert_eq!(p.search_status, Some(None));
        let (queue, current) = p.queue.unwrap();
        assert_eq!((queue.len(), current), (8, Some(2)));
        assert_eq!(p.queue_badge, Some(Some("8".into())));
        let now = p.now_playing.unwrap();
        assert_eq!(
            now.track,
            Some(("Night Drive Home".into(), "Hollow Coast".into()))
        );
        assert_eq!(now.total, "3:45");
        assert_eq!(p.elapsed.as_deref(), Some("1:23"));
        assert!((p.progress.unwrap() - 83.0 / 225.0).abs() < 1e-9);
        assert_eq!(p.playing, Some(true));
        assert_eq!(p.toast, Some(None));
        assert_eq!(p.banner, Some(None));
        let on = Controls {
            transport: true,
            seek: true,
            player: true,
        };
        assert_eq!(p.controls, Some(on));
    }

    #[test]
    fn state_change_refreshes_the_bar() {
        let (mut vm, _) = demo::start();
        vm.take_dirty();
        vm.apply(Event::State(PlayState::Stopped));
        let p = Patch::new(vm.take_dirty(), vm.ui());
        assert_eq!(p.playing, Some(false));
        let now = p.now_playing.unwrap();
        assert_eq!((now.track, now.total.as_str()), (None, ""));
        assert_eq!(p.elapsed.as_deref(), Some(""));
        assert_eq!(p.progress, Some(0.0));
        assert!(p.results.is_none() && p.queue.is_none());
    }

    #[test]
    fn toast_countdown_holds_and_resumes_with_the_time_left() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        let c = Countdown::new(t0, false);
        assert_eq!(c, Countdown::Running(s(4)));
        let held = c.hold(s(1));
        assert_eq!(held, Countdown::Held(Duration::from_secs(3)));
        // Holding again, or resuming a running one, changes nothing.
        assert_eq!(held.hold(s(2)), held);
        assert_eq!(c.resume(s(2)), c);
        assert_eq!(held.resume(s(10)), Countdown::Running(s(13)));
        // Under the pointer from the start, it waits for the pointer to leave.
        assert_eq!(
            Countdown::new(t0, true).resume(s(5)),
            Countdown::Running(s(9))
        );
        // Held past its end, it resumes with nothing left.
        assert_eq!(c.hold(s(6)).resume(s(7)), Countdown::Running(s(7)));
    }

    #[test]
    fn empty_results_carry_their_empty_state() {
        let (vm, _) = demo::start_in(crate::args::View::Empty);
        let (rows, empty) = Patch::new(Dirty::RESULTS, vm.ui()).results.unwrap();
        assert!(rows.is_empty());
        assert_eq!(empty.unwrap().action, EmptyAction::StartSearch);
    }
}
