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
    NSAlert, NSAlertFirstButtonReturn, NSAnimatablePropertyContainer, NSApplication,
    NSAttributedStringNSStringDrawing, NSColor, NSEvent, NSFont, NSLineBreakMode, NSPasteboard,
    NSPasteboardTypeString, NSResponder, NSTextAlignment, NSTextField, NSTextFieldDelegate,
    NSTrackingAreaOptions, NSView, NSWindow, NSWindowStyleMask, NSWindowTitleVisibility,
    NSWorkspace,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSURL};
use requiem_core::{Collection, Item, Shelf};
use requiem_macos::artwork::Cover;

use super::backend::{Backend, Msg};
use super::bar::PlayerBar;
use super::palette::Palette;
use super::shelves::Shelves;
use super::table::{self, Kind, Table};
use super::theme::{self, Icon, Weight};
use super::widgets::{
    self, EmptyState, FLEX_H, FLEX_W, Field, HoverButton, NavItem, PIN_BOTTOM, PIN_LEFT, PIN_RIGHT,
    PIN_TOP, rect,
};
use super::{Delegate, MainWindow};
use crate::vm::{
    self, Banner, Controls, Dirty, EmptyText, Intent, LibraryView, List, PageView, PaletteAction,
    Place, QueueEdit, Selection, Toast, UiState, Vm,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppView {
    Home,
    Search,
    Library,
    Queue,
    /// A playlist or album opened from Home or the Library; not in the sidebar.
    Collection,
}

impl AppView {
    fn place(self) -> Place {
        match self {
            AppView::Home => Place::Home,
            AppView::Search => Place::Search,
            AppView::Library => Place::Library,
            AppView::Queue => Place::Queue,
            AppView::Collection => Place::Collection,
        }
    }

    fn of(place: Place) -> AppView {
        match place {
            Place::Home => AppView::Home,
            Place::Search => AppView::Search,
            Place::Library => AppView::Library,
            Place::Queue => AppView::Queue,
            Place::Collection => AppView::Collection,
        }
    }
}

/// Whether a snapshot of a connected window has what it waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotState {
    Waiting,
    /// The page is in; its covers may still be arriving.
    Loaded {
        covers: bool,
    },
    Failed(String),
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
const COLLECTION_COVER: CGFloat = 120.0;
const BACK_H: CGFloat = 28.0;
const BUTTON_H: CGFloat = 36.0;
const SIGN_IN_W: CGFloat = 560.0;
const SIGN_IN_H: CGFloat = 250.0;
/// How often the sign-in panel's countdown is redrawn.
const EXPIRY_TICK: Duration = Duration::from_secs(30);
const TOAST_H: CGFloat = 40.0;
/// Tall enough for a compact button.
const TOAST_ACTION_H: CGFloat = 48.0;
const TOAST_BUTTON_H: CGFloat = 32.0;
/// How far below its place a toast starts as it fades in.
const TOAST_RISE: CGFloat = 8.0;
/// The toast's distance from the window's right edge, and from the player bar.
const TOAST_RIGHT: CGFloat = 24.0;
const TOAST_BOTTOM: CGFloat = 16.0;
/// Leaving is quicker than entering.
const TOAST_EXIT: Duration = Duration::from_millis(150);
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
    /// The toast shown, or about to be once a swap's fade-out ends.
    id: Cell<u64>,
    /// The toast whose countdown runs: the view model's, seen or not.
    counting: Cell<u64>,
    /// The toast and text last read out.
    said: RefCell<(u64, String)>,
    /// The toast to lay out once the shown one has faded out.
    pending: RefCell<Option<Toast>>,
    /// False from the start of the exit; the view hides once the exit ends.
    shown: Cell<bool>,
    /// The pointer is on the toast.
    hovered: Cell<bool>,
    clock: Cell<Countdown>,
}

/// The Library's device-code panel.
struct SignInPanel {
    view: Retained<NSView>,
    sub: Retained<NSTextField>,
    code: Retained<NSTextField>,
    expiry: Retained<NSTextField>,
    copy: Retained<HoverButton>,
    open: Retained<HoverButton>,
    cancel: Retained<HoverButton>,
}

/// An open collection's cover, name and buttons.
struct CollectionHead {
    view: Retained<NSView>,
    back: Retained<HoverButton>,
    cover: Retained<Cover>,
    title: Retained<NSTextField>,
    sub: Retained<NSTextField>,
    play: Retained<HoverButton>,
    add: Retained<HoverButton>,
}

pub struct Ui {
    mtm: MainThreadMarker,
    pub window: Retained<NSWindow>,
    pub root: Retained<NSView>,
    vm: RefCell<Vm>,
    backend: Backend,
    view: Cell<AppView>,
    nav_home: Retained<NavItem>,
    nav_search: Retained<NavItem>,
    nav_library: Retained<NavItem>,
    nav_queue: Retained<NavItem>,
    main: Retained<NSView>,
    banner: Retained<NSView>,
    banner_label: Retained<NSTextField>,
    /// False from the start of the fade-out; the view hides once it ends.
    banner_shown: Cell<bool>,
    /// How far the banner pushes the pages down.
    banner_inset: Cell<CGFloat>,
    home_page: Retained<NSView>,
    pub shelves: Shelves,
    home_state: EmptyState,
    library_page: Retained<NSView>,
    library_head: Retained<NSView>,
    library_who: Retained<NSTextField>,
    library_table: Table,
    library_state: EmptyState,
    sign_in: SignInPanel,
    expiry_scheduled: Cell<bool>,
    collection_page: Retained<NSView>,
    collection_head: CollectionHead,
    collection_table: Table,
    collection_state: EmptyState,
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
        let nav_home = nav(nav_y - 36.0, AppView::Home, Icon::Home, "Home");
        let nav_search = nav(nav_y - 76.0, AppView::Search, Icon::Search, "Search");
        let nav_library = nav(nav_y - 116.0, AppView::Library, Icon::Library, "Library");
        let nav_queue = nav(nav_y - 156.0, AppView::Queue, Icon::ListMusic, "Queue");
        sidebar.addSubview(&nav_home);
        sidebar.addSubview(&nav_search);
        sidebar.addSubview(&nav_library);
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
        widgets::accessibility_label(&search.text, "Search");
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
        let heading_font = theme::font(20.0, Weight::SemiBold);
        let count_font = theme::font(12.0, Weight::Regular);
        let heading = widgets::placed_label(
            mtm,
            "Queue",
            &heading_font,
            &theme::ink(),
            PAD,
            head_y,
            200.0,
            PIN_TOP | PIN_LEFT,
        );
        let queue_count = widgets::placed_label(
            mtm,
            "",
            &count_font,
            &theme::muted(),
            mw - PAD - 200.0,
            head_y,
            200.0,
            PIN_TOP | PIN_RIGHT,
        );
        queue_count.setAlignment(NSTextAlignment::Right);
        // On the heading's baseline rather than its centre line. Labels set their first
        // baseline the font's ascender, rounded to a point, below their frame's top; the
        // count's own `firstBaselineOffsetFromTop` is no use while it is still empty.
        let baseline = heading.frame().origin.y + heading.frame().size.height
            - heading_font.ascender().round();
        let mut f = queue_count.frame();
        f.origin.y = baseline + count_font.ascender().round() - f.size.height;
        queue_count.setFrame(f);
        let queue = Table::new(mtm, list, Kind::Queue);
        let queue_empty = EmptyState::new(mtm, list, Icon::ListMusic, target, sel!(focusSearch:));
        queue_empty.set(
            Icon::ListMusic,
            "Queue is empty",
            "Play or add a search result",
            Some("Go to search"),
        );
        queue_page.addSubview(&heading);
        queue_page.addSubview(&queue_count);
        queue_page.addSubview(&queue.view);
        queue_page.addSubview(&queue_empty.view);
        main.addSubview(&queue_page);

        // Home page.
        let home_page = widgets::view(mtm, rect(0.0, 0.0, mw, mh), FLEX_W | FLEX_H);
        // Below the titlebar, not TOP: a scroll view with a vertical scroller that reaches under
        // the titlebar gets macOS 26's scroll pocket, whose backdrop layers make an off-screen
        // snapshot of the whole window come out blank.
        let titlebar_h = WIN_H - window.contentLayoutRect().size.height;
        let shelves = Shelves::new(mtm, rect(0.0, 0.0, mw, mh - titlebar_h));
        let home_state = EmptyState::new(
            mtm,
            rect(0.0, 0.0, mw, mh),
            Icon::Home,
            target,
            sel!(refresh:),
        );
        home_page.addSubview(&shelves.view);
        home_page.addSubview(&home_state.view);
        main.addSubview(&home_page);

        // Library page.
        let library_page = widgets::view(mtm, rect(0.0, 0.0, mw, mh), FLEX_W | FLEX_H);
        let library_head = widgets::view(
            mtm,
            rect(0.0, mh - TOP - Field::H - 8.0, mw, Field::H + 8.0),
            FLEX_W | PIN_TOP,
        );
        let hy = (Field::H + 8.0) / 2.0;
        let library_title = widgets::placed_label(
            mtm,
            "Library",
            &heading_font,
            &theme::ink(),
            PAD,
            hy,
            200.0,
            PIN_TOP | PIN_LEFT,
        );
        let library_who = widgets::placed_label(
            mtm,
            "",
            &count_font,
            &theme::muted(),
            mw - PAD - 90.0 - 12.0 - 260.0,
            hy,
            260.0,
            PIN_TOP | PIN_RIGHT,
        );
        library_who.setAlignment(NSTextAlignment::Right);
        let sign_out = HoverButton::secondary(mtm, rect(mw - PAD - 90.0, hy - 16.0, 90.0, 32.0));
        let w = sign_out.set_label("Sign Out");
        sign_out.setFrame(rect(mw - PAD - w, hy - 16.0, w, 32.0));
        sign_out.setAutoresizingMask(PIN_TOP | PIN_RIGHT);
        sign_out.set_action(target, sel!(signOut:));
        library_who.setFrame(rect(
            mw - PAD - w - 12.0 - 260.0,
            library_who.frame().origin.y,
            260.0,
            library_who.frame().size.height,
        ));
        library_head.addSubview(&library_title);
        library_head.addSubview(&library_who);
        library_head.addSubview(&sign_out);
        let library_table = Table::new(mtm, list, Kind::Library);
        let library_state = EmptyState::new(
            mtm,
            rect(0.0, 0.0, mw, mh),
            Icon::Library,
            target,
            sel!(libraryAction:),
        );
        let sign_in = Self::sign_in_panel(mtm, mw, mh, target);
        library_page.addSubview(&library_head);
        library_page.addSubview(&library_table.view);
        library_page.addSubview(&library_state.view);
        library_page.addSubview(&sign_in.view);
        main.addSubview(&library_page);

        // Collection page.
        let collection_page = widgets::view(mtm, rect(0.0, 0.0, mw, mh), FLEX_W | FLEX_H);
        let collection_head = Self::collection_head(mtm, mw, mh, target);
        let collection_list = rect(
            PAD,
            0.0,
            mw - 2.0 * PAD,
            collection_head.view.frame().origin.y - 16.0,
        );
        let collection_table = Table::new(mtm, collection_list, Kind::Collection);
        let collection_state = EmptyState::new(
            mtm,
            collection_list,
            Icon::ListMusic,
            target,
            sel!(refresh:),
        );
        collection_page.addSubview(&collection_head.view);
        collection_page.addSubview(&collection_table.view);
        collection_page.addSubview(&collection_state.view);
        main.addSubview(&collection_page);

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
            view: Cell::new(AppView::Home),
            nav_home,
            nav_search,
            nav_library,
            nav_queue,
            main,
            banner,
            banner_label,
            banner_shown: Cell::new(false),
            banner_inset: Cell::new(0.0),
            home_page,
            shelves,
            home_state,
            library_page,
            library_head,
            library_who,
            library_table,
            library_state,
            sign_in,
            expiry_scheduled: Cell::new(false),
            collection_page,
            collection_head,
            collection_table,
            collection_state,
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
        ui.show(AppView::Home);
        ui
    }

    fn sign_in_panel(
        mtm: MainThreadMarker,
        mw: CGFloat,
        mh: CGFloat,
        target: &AnyObject,
    ) -> SignInPanel {
        let view = widgets::view(
            mtm,
            rect(
                ((mw - SIGN_IN_W) / 2.0).round(),
                ((mh - SIGN_IN_H) / 2.0 + 8.0).round(),
                SIGN_IN_W,
                SIGN_IN_H,
            ),
            PIN_LEFT | PIN_RIGHT | PIN_TOP | PIN_BOTTOM,
        );
        let centred = |text: &str, font: &NSFont, color: &NSColor, mid_y: CGFloat| {
            let l = widgets::placed_label(mtm, text, font, color, 0.0, mid_y, SIGN_IN_W, FLEX_W);
            l.setAlignment(NSTextAlignment::Center);
            l
        };
        let heading = centred(
            "Sign in to Google",
            &theme::font(16.0, Weight::SemiBold),
            &theme::ink(),
            SIGN_IN_H - 20.0,
        );
        let sub = centred(
            "",
            &theme::font(14.0, Weight::Regular),
            &theme::muted(),
            SIGN_IN_H - 48.0,
        );
        let code = centred(
            "",
            &theme::font(32.0, Weight::SemiBold),
            &theme::ink(),
            SIGN_IN_H - 100.0,
        );
        let expiry = centred(
            "",
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
            SIGN_IN_H - 144.0,
        );
        let copy = HoverButton::primary(mtm, rect(0.0, 0.0, 0.0, BUTTON_H));
        copy.set_action(target, sel!(copyCode:));
        let open = HoverButton::secondary(mtm, rect(0.0, 0.0, 0.0, BUTTON_H));
        open.set_action(target, sel!(openHost:));
        let cancel = HoverButton::secondary(mtm, rect(0.0, 0.0, 0.0, BUTTON_H));
        cancel.set_action(target, sel!(cancelSignIn:));
        // A long address wraps rather than losing its end.
        sub.setUsesSingleLineMode(false);
        sub.setMaximumNumberOfLines(2);
        sub.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        let two_lines = 2.0 * widgets::line_height(&theme::font(14.0, Weight::Regular));
        let mut f = sub.frame();
        f.origin.y -= two_lines - f.size.height;
        f.size.height = two_lines;
        sub.setFrame(f);
        for l in [&sub, &code] {
            l.setSelectable(true);
        }
        for v in [&heading, &sub, &code, &expiry] {
            view.addSubview(v);
        }
        for b in [&copy, &open, &cancel] {
            view.addSubview(b);
        }
        view.setHidden(true);
        SignInPanel {
            view,
            sub,
            code,
            expiry,
            copy,
            open,
            cancel,
        }
    }

    fn collection_head(
        mtm: MainThreadMarker,
        mw: CGFloat,
        mh: CGFloat,
        target: &AnyObject,
    ) -> CollectionHead {
        let h = BACK_H + 12.0 + COLLECTION_COVER;
        let view = widgets::view(mtm, rect(0.0, mh - TOP - h, mw, h), FLEX_W | PIN_TOP);
        let back = HoverButton::secondary(mtm, rect(PAD, COLLECTION_COVER + 12.0, 0.0, BACK_H));
        back.set_action(target, sel!(goBack:));
        back.setAutoresizingMask(PIN_TOP | PIN_LEFT);
        let cover = widgets::cover(
            mtm,
            rect(PAD, 0.0, COLLECTION_COVER, COLLECTION_COVER),
            8.0,
            40.0,
        );
        let x = PAD + COLLECTION_COVER + 20.0;
        let title = widgets::placed_label(
            mtm,
            "",
            &theme::font(20.0, Weight::SemiBold),
            &theme::ink(),
            x,
            COLLECTION_COVER - 14.0,
            mw - x - PAD,
            FLEX_W | PIN_TOP,
        );
        let sub = widgets::placed_label(
            mtm,
            "",
            &theme::font(14.0, Weight::Regular),
            &theme::muted(),
            x,
            COLLECTION_COVER - 42.0,
            mw - x - PAD,
            FLEX_W | PIN_TOP,
        );
        let play = HoverButton::primary(mtm, rect(x, 8.0, 0.0, BUTTON_H));
        play.set_action(target, sel!(playCollection:));
        let w = play.set_label("Play");
        play.setFrame(rect(x, 8.0, w, BUTTON_H));
        let add = HoverButton::secondary(mtm, rect(x + w + 8.0, 8.0, 0.0, BUTTON_H));
        add.set_action(target, sel!(addCollection:));
        let aw = add.set_label("Add to Queue");
        add.setFrame(rect(x + w + 8.0, 8.0, aw, BUTTON_H));
        for v in [&back as &NSView, &cover, &title, &sub, &play, &add] {
            view.addSubview(v);
        }
        CollectionHead {
            view,
            back,
            cover,
            title,
            sub,
            play,
            add,
        }
    }

    fn toast_view(mtm: MainThreadMarker, target: &AnyObject) -> ToastView {
        let w = 320.0;
        let frame = rect(
            WIN_W - TOAST_RIGHT - w,
            PlayerBar::H + TOAST_BOTTOM,
            w,
            TOAST_H,
        );
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
            counting: Cell::new(0),
            said: RefCell::new((0, String::new())),
            pending: RefCell::new(None),
            shown: Cell::new(false),
            hovered: Cell::new(false),
            clock: Cell::new(Countdown::Held(Duration::ZERO)),
        }
    }

    pub fn current_view(&self) -> AppView {
        self.view.get()
    }

    pub fn show(&self, view: AppView) {
        self.switch_to(view);
        self.visit(view.place());
        if view == AppView::Search && self.vm.borrow().ui().results.is_empty() {
            self.window.makeFirstResponder(Some(&self.search.text));
            return;
        }
        let table = match view {
            AppView::Search => &self.results,
            AppView::Queue => &self.queue,
            AppView::Library => &self.library_table,
            AppView::Collection => &self.collection_table,
            AppView::Home => {
                // Keeps Space and the arrow keys from landing in a hidden text field.
                self.window.makeFirstResponder(None);
                return;
            }
        };
        // Keeps Space and the arrow keys from landing in a hidden text field.
        self.window.makeFirstResponder(Some(&table.table));
        // Focus rests on a row; the ring around the whole table is for when there is none.
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
        self.view.set(view);
        self.home_page.setHidden(view != AppView::Home);
        self.search_page.setHidden(view != AppView::Search);
        self.library_page.setHidden(view != AppView::Library);
        self.queue_page.setHidden(view != AppView::Queue);
        self.collection_page.setHidden(view != AppView::Collection);
        // An open collection keeps the nav item of the page it was opened from lit.
        let lit = match view {
            AppView::Collection => self
                .vm
                .borrow()
                .ui()
                .collection
                .as_ref()
                .map_or(AppView::Home, |c| AppView::of(c.from)),
            v => v,
        };
        self.nav_home.set_active(lit == AppView::Home);
        self.nav_search.set_active(lit == AppView::Search);
        self.nav_library.set_active(lit == AppView::Library);
        self.nav_queue.set_active(lit == AppView::Queue);
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
    pub fn selection(&self) -> Selection {
        match self.view.get() {
            AppView::Search => {
                let row = self.results.selected_row().is_some();
                Selection { row, result: row }
            }
            AppView::Collection => {
                let row = self.collection_table.selected_row().is_some();
                Selection { row, result: row }
            }
            AppView::Library => Selection {
                row: self.library_table.selected_row().is_some(),
                result: false,
            },
            AppView::Queue => Selection {
                row: self.queue.selected_row().is_some(),
                result: false,
            },
            AppView::Home => Selection::default(),
        }
    }

    /// Whether `action` would do anything now; for the menus and the palette.
    pub fn can_run(&self, action: PaletteAction) -> bool {
        self.vm.borrow().ui().can_run(action, self.selection())
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
        // Not `show`, which would select a result for a table that focus passes straight
        // through.
        self.switch_to(AppView::Search);
        self.window.makeFirstResponder(Some(&self.search.text));
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
            PaletteAction::ShowHome => self.show(AppView::Home),
            PaletteAction::ShowSearch => self.show(AppView::Search),
            PaletteAction::ShowLibrary => self.show(AppView::Library),
            PaletteAction::ShowQueue => self.show(AppView::Queue),
            PaletteAction::Refresh => self.intent(Intent::Refresh),
            PaletteAction::SignIn => self.intent(Intent::SignIn),
            PaletteAction::SignOut => self.confirm_sign_out(),
            PaletteAction::VolumeUp => self.intent(Intent::VolumeBy(vm::VOLUME_STEP)),
            PaletteAction::VolumeDown => self.intent(Intent::VolumeBy(-vm::VOLUME_STEP)),
            PaletteAction::ClearQueue => self.edit_queue(QueueEdit::Clear),
        }
    }

    pub fn intent(&self, intent: Intent) {
        self.run(intent);
    }

    /// Appends item `i` of `list`, and confirms it on the row's "+" and to VoiceOver when it
    /// was sent.
    pub fn append_row(&self, list: List, i: usize) {
        if self.run(Intent::Append { list, index: i }) {
            match list {
                List::Results => self.results.confirm_add(i),
                List::Collection => self.collection_table.confirm_add(i),
                List::Shelf(_) => {}
            }
            widgets::announce(self.mtm, "Added to queue");
        }
    }

    /// A click on a Home card: a track plays its shelf from there; a playlist or album opens.
    pub fn card_clicked(&self, shelf: usize, index: usize) {
        match self.shelves.item(shelf, index) {
            Some(Item::Track(_)) => self.intent(Intent::PlayFrom {
                list: List::Shelf(shelf),
                from: index,
            }),
            Some(Item::Collection(c)) => self.open_collection(c, Place::Home),
            None => {}
        }
    }

    pub fn card_add(&self, shelf: usize, index: usize) {
        self.append_row(List::Shelf(shelf), index);
    }

    pub fn open_library_row(&self, row: usize) {
        let c = self
            .vm
            .borrow()
            .ui()
            .library_collections()
            .get(row)
            .map(|c| (*c).clone());
        if let Some(c) = c {
            self.open_collection(c, Place::Library);
        }
    }

    fn open_collection(&self, collection: Collection, from: Place) {
        self.run(Intent::OpenCollection { collection, from });
        self.show(AppView::Collection);
    }

    /// Back from an open collection to the page it was opened from; that page kept its scroll.
    pub fn go_back(&self) {
        if self.view.get() != AppView::Collection {
            return;
        }
        let from = self
            .vm
            .borrow()
            .ui()
            .collection
            .as_ref()
            .map_or(Place::Home, |c| c.from);
        self.show(AppView::of(from));
    }

    pub fn can_go_back(&self) -> bool {
        self.view.get() == AppView::Collection
    }

    pub fn refresh(&self) {
        self.intent(Intent::Refresh);
    }

    /// The Library empty state's button: Sign in, or Try again.
    pub fn library_action(&self) {
        let signed_out = self.vm.borrow().ui().library_view() == LibraryView::SignedOut;
        self.intent(if signed_out {
            Intent::SignIn
        } else {
            Intent::Refresh
        });
    }

    pub fn play_collection(&self) {
        self.intent(Intent::PlayFrom {
            list: List::Collection,
            from: 0,
        });
    }

    pub fn add_collection(&self) {
        if self.run(Intent::AppendAll(List::Collection)) {
            widgets::announce(self.mtm, "Added to queue");
        }
    }

    pub fn cancel_sign_in(&self) {
        self.intent(Intent::SignOut);
    }

    /// Asks before signing out; the Library goes with the account.
    pub fn confirm_sign_out(&self) {
        let alert = NSAlert::new(self.mtm);
        alert.setMessageText(&NSString::from_str("Sign out of Google?"));
        alert.setInformativeText(&NSString::from_str(
            "Your Library will be unavailable until you sign in again.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Sign Out"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        if alert.runModal() == NSAlertFirstButtonReturn {
            self.intent(Intent::SignOut);
        }
    }

    pub fn copy_code(&self) {
        if let LibraryView::Pending { user_code, .. } = self.vm.borrow().ui().library_view() {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            // SAFETY: the type static is an immutable framework constant.
            pb.setString_forType(&NSString::from_str(&user_code), unsafe {
                NSPasteboardTypeString
            });
            widgets::announce(self.mtm, "Code copied");
        }
    }

    pub fn open_host(&self) {
        if let LibraryView::Pending {
            verification_url, ..
        } = self.vm.borrow().ui().library_view()
            && let Some(url) = NSURL::URLWithString(&NSString::from_str(&verification_url))
        {
            NSWorkspace::sharedWorkspace().openURL(&url);
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

    /// The results empty state's button, "Edit search".
    pub fn empty_action(&self) {
        self.focus_search();
    }

    pub fn search_submitted(&self) {
        self.intent(Intent::Search(self.search.value()));
    }

    /// Plays (or with `append`, appends) the keyboard-selected row of the visible table.
    pub fn activate_selection(&self, append: bool) {
        match self.view.get() {
            AppView::Search => match self.results.selected_row() {
                Some(i) if append => self.append_row(List::Results, i),
                Some(i) => self.intent(Intent::PlayFrom {
                    list: List::Results,
                    from: i,
                }),
                None => {}
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
                if let Some(i) = self.library_table.selected_row().filter(|_| !append) {
                    self.open_library_row(i);
                }
            }
            AppView::Home => {}
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

    /// Plays queue row `i`. The selection stays, so the arrow keys, Delete and the moves
    /// carry on from that row; its current-row marker shows under the selection.
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
        self.schedule_dismissal(t.counting.get(), clock);
    }

    /// Starts toast `id`'s countdown, held if the pointer already rests on `frame`.
    fn start_countdown(&self, id: u64, frame: NSRect) {
        let at = self
            .root
            .convertPoint_fromView(self.window.mouseLocationOutsideOfEventStream(), None);
        let (o, s) = (frame.origin, frame.size);
        let over = at.x >= o.x && at.x < o.x + s.width && at.y >= o.y && at.y < o.y + s.height;
        self.start_clock(id, over);
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
        if let Ok(when) = DispatchTime::try_from(wait) {
            let _ = DispatchQueue::main().after(when, move || {
                with(|ui| ui.countdown_ended(id, end));
            });
        }
    }

    /// Dismisses toast `id`, unless another replaced it or its countdown was held since.
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
            self.count_toast();
            self.schedule_tick();
        }
    }

    /// Renders what changed while the window couldn't be seen, once it can, without motion.
    pub fn catch_up(&self) {
        if self.on_screen() {
            widgets::still(|| self.render_now());
        }
    }

    /// Starts the countdown of a toast raised while the window can't be seen, so one that
    /// runs out meanwhile is dropped rather than shown late.
    fn count_toast(&self) {
        let id = self.vm.borrow().ui().toast.as_ref().map(|t| t.id);
        if let Some(id) = id
            && self.toast.counting.replace(id) != id
        {
            self.start_clock(id, false);
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
        if self.palette.is_open() {
            self.palette.set_enabled(|a| self.can_run(a));
        }
        self.schedule_tick();
    }

    fn on_screen(&self) -> bool {
        widgets::window_on_screen(&self.window)
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
                    .set(Icon::Search, &e.heading, &e.sub, e.action);
            }
        }
        if let Some((view, shelves)) = p.home {
            self.show_home(&view, &shelves);
        }
        if let Some((view, rows)) = p.library {
            self.show_library(&view, rows);
        }
        if let Some(c) = p.collection {
            self.show_collection(c);
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
            self.bar
                .set_track(track, &now.total, now.artwork.as_deref());
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
            self.library_table.set_enabled(c.player);
            self.collection_table.set_enabled(c.player);
            self.collection_head.play.set_enabled(c.player);
            self.collection_head.add.set_enabled(c.player);
        }
        if let Some(t) = p.toast {
            self.show_toast(t);
        }
        if let Some(b) = p.banner {
            self.show_banner(b);
        }
    }

    fn pages(&self) -> [&Retained<NSView>; 5] {
        [
            &self.home_page,
            &self.search_page,
            &self.library_page,
            &self.queue_page,
            &self.collection_page,
        ]
    }

    fn show_home(&self, view: &PageView, shelves: &[Shelf]) {
        let ready = *view == PageView::Ready && shelves.iter().any(|s| !s.items.is_empty());
        self.shelves.set_shelves(if ready { shelves } else { &[] });
        self.shelves.view.setHidden(!ready);
        self.home_state.view.setHidden(ready);
        if !ready {
            Self::page_state(&self.home_state, Icon::Home, view, "Home");
        }
    }

    /// A page that is loading, failed or empty, in its empty state.
    fn page_state(state: &EmptyState, icon: Icon, view: &PageView, name: &str) {
        match view {
            PageView::Loading => state.set(icon, &format!("Loading {name}"), "", None),
            PageView::Failed(e) => {
                state.set(icon, &format!("Couldn't load {name}"), e, Some("Try again"));
            }
            PageView::Ready => state.set(icon, &format!("{name} is empty"), "", None),
        }
    }

    fn show_library(&self, view: &LibraryView, rows: Vec<table::RowText>) {
        let panel = matches!(view, LibraryView::Pending { .. });
        let list = matches!(
            view,
            LibraryView::SignedIn {
                page: PageView::Ready,
                ..
            }
        ) && !rows.is_empty();
        let head = matches!(view, LibraryView::SignedIn { .. });
        self.sign_in.view.setHidden(!panel);
        self.library_head.setHidden(!head);
        self.library_table.view.setHidden(!list);
        self.library_state.view.setHidden(panel || list);
        let state = &self.library_state;
        match view {
            LibraryView::Loading => state.set(Icon::Library, "Loading Library", "", None),
            LibraryView::Unavailable(text) => {
                state.set(Icon::Library, "Google sign-in isn't set up", text, None);
            }
            LibraryView::SignedOut => state.set(
                Icon::Library,
                "Sign in to see your Library",
                "Your playlists and albums from YouTube Music appear here.",
                Some("Sign in"),
            ),
            LibraryView::Pending {
                verification_url,
                user_code,
                expires_at_unix,
            } => self.show_sign_in(verification_url, user_code, *expires_at_unix),
            LibraryView::SignedIn { name, page } => {
                widgets::set_text(
                    &self.library_who,
                    &name
                        .as_deref()
                        .map_or_else(|| "Signed in".to_owned(), |n| format!("Signed in as {n}")),
                );
                if *page == PageView::Ready && rows.is_empty() {
                    state.set(Icon::Library, "Your Library is empty", "", None);
                } else if *page != PageView::Ready {
                    Self::page_state(state, Icon::Library, page, "Library");
                }
            }
        }
        self.library_table
            .set_rows(if list { rows } else { Vec::new() }, None);
        if list {
            self.library_table.deselect();
        }
    }

    fn show_sign_in(&self, url: &str, code: &str, expires_at_unix: u64) {
        let p = &self.sign_in;
        widgets::set_text(&p.sub, &vm::sign_in_hint(url));
        widgets::set_text(&p.code, code);
        widgets::set_text(&p.expiry, &vm::expires_text(expires_at_unix, unix_now()));
        let buttons = [
            (&p.copy, "Copy Code".to_owned()),
            (&p.open, "Open in Browser".to_owned()),
            (&p.cancel, "Cancel".to_owned()),
        ];
        let widths: Vec<CGFloat> = buttons.iter().map(|(b, t)| b.set_label(t)).collect();
        let total = widths.iter().sum::<CGFloat>() + 8.0 * (widths.len() as CGFloat - 1.0);
        let mut x = ((SIGN_IN_W - total) / 2.0).round();
        for ((b, _), w) in buttons.iter().zip(widths) {
            b.setFrame(rect(x, 20.0, w, BUTTON_H));
            x += w + 8.0;
        }
        self.schedule_expiry();
    }

    /// Redraws the countdown while the panel shows; "Code expires in N min" moves each minute.
    fn schedule_expiry(&self) {
        if self.expiry_scheduled.replace(true) {
            return;
        }
        if let Ok(when) = DispatchTime::try_from(EXPIRY_TICK) {
            let _ = DispatchQueue::main().after(when, || {
                with(|ui| ui.expiry_tick());
            });
        }
    }

    fn expiry_tick(&self) {
        self.expiry_scheduled.set(false);
        if let LibraryView::Pending {
            expires_at_unix, ..
        } = self.vm.borrow().ui().library_view()
        {
            widgets::set_text(
                &self.sign_in.expiry,
                &vm::expires_text(expires_at_unix, unix_now()),
            );
            self.schedule_expiry();
        }
    }

    fn show_collection(&self, c: Option<CollectionPatch>) {
        let Some(c) = c else {
            self.collection_table.set_rows(Vec::new(), None);
            return;
        };
        let h = &self.collection_head;
        widgets::set_text(&h.title, &c.collection.title);
        widgets::set_text(&h.sub, c.collection.subtitle.as_deref().unwrap_or(""));
        h.cover.set_url(c.collection.artwork_url.as_deref());
        let back = match c.from {
            Place::Library => "\u{2039} Library",
            _ => "\u{2039} Home",
        };
        let w = h.back.set_label(back);
        h.back
            .setFrame(rect(PAD, h.back.frame().origin.y, w, BACK_H));
        widgets::accessibility_label(&h.back, &format!("Back to {}", &back[4..]));
        let ready = c.view == PageView::Ready && !c.rows.is_empty();
        self.collection_table.view.setHidden(!ready);
        self.collection_state.view.setHidden(ready);
        if !ready {
            Self::page_state(&self.collection_state, Icon::ListMusic, &c.view, "playlist");
        }
        self.collection_table
            .set_rows(if ready { c.rows } else { Vec::new() }, None);
        h.play.set_shown(true);
        h.add.set_shown(true);
        h.play
            .set_enabled(ready && self.vm.borrow().ui().controls().player);
        h.add
            .set_enabled(ready && self.vm.borrow().ui().controls().player);
    }

    /// What a snapshot of the connected window waits for.
    pub fn snapshot_state(&self) -> SnapshotState {
        let vm = self.vm.borrow();
        let ui = vm.ui();
        let page = match self.view.get() {
            AppView::Home => ui.home.view(),
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
        };
        match page {
            PageView::Loading => SnapshotState::Waiting,
            PageView::Failed(e) => SnapshotState::Failed(e),
            PageView::Ready => SnapshotState::Loaded {
                covers: self.shelves.covers_loading()
                    || self.library_table.covers_loading()
                    || self.collection_head.cover.is_loading(),
            },
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
        if self.palette.is_open() {
            self.palette.layout();
        }
        if !self.banner_shown.get() {
            return;
        }
        let h = self.layout_banner();
        if self.banner_inset.replace(h) != h {
            let b = self.main.bounds();
            let page = rect(0.0, 0.0, b.size.width, b.size.height - h);
            for p in self.pages() {
                p.setFrame(page);
            }
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
            for p in self.pages() {
                p.animator().setFrame(page);
            }
        };
        let changed = self.banner_shown.replace(shown) != shown;
        // Also when a shown banner grows to a second line.
        let moved = self.banner_inset.replace(inset) != inset;
        let shift = if moved && !widgets::reduce_motion() {
            widgets::STANDARD
        } else {
            Duration::ZERO
        };
        // The banner's own curve, so the pages and the fade move as one.
        widgets::animate(&self.main, shift, widgets::EASE_OUT, place_pages, || {});
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
            t.pending.take();
            if t.shown.replace(false) {
                self.leave_toast();
            }
            return;
        };
        let swap = t.shown.replace(true);
        t.id.set(toast.id);
        // A new toast, or an amended text, is read out; a re-render is not.
        let said = (toast.id, toast.text.clone());
        if *t.said.borrow() != said {
            widgets::announce(self.mtm, &spoken(&toast));
            *t.said.borrow_mut() = said;
        }
        // A toast amended in place keeps its countdown, as does one counted while hidden.
        let new = t.counting.replace(toast.id) != toast.id;
        if swap {
            if new {
                self.start_clock(toast.id, t.hovered.get());
            }
            if t.pending.replace(Some(toast)).is_none() {
                self.fade_toast_out();
            }
            return;
        }
        let frame = self.lay_out_toast(&toast);
        if new {
            self.start_countdown(toast.id, frame);
        }
        self.enter_toast();
    }

    /// Fades the shown toast's content out at its own size, then lays out the newest pending
    /// toast and fades that in as the box takes its size. Resizing the box mid-fade-out would
    /// clip the old text.
    fn fade_toast_out(&self) {
        let t = &self.toast;
        widgets::animate(
            &t.view,
            widgets::motion(widgets::MICRO),
            widgets::EASE_OUT,
            || {
                for v in t.view.subviews().iter() {
                    v.animator().setAlphaValue(0.0);
                }
            },
            || {
                with(|ui| ui.fade_toast_in());
            },
        );
    }

    fn fade_toast_in(&self) {
        let t = &self.toast;
        // Dismissed meanwhile: the exit is under way.
        let Some(toast) = t.pending.take() else {
            return;
        };
        let from = t.view.frame();
        let to = self.lay_out_toast(&toast);
        let duration = widgets::motion(widgets::MICRO);
        let resize = to != from && !duration.is_zero() && !widgets::reduce_motion();
        if resize {
            // Laying out left the box at its new size and the content placed for it. Back at
            // the old size, the animator grows or shrinks it into the new one, and the content
            // follows by autoresizing.
            t.view.setFrame(from);
        }
        widgets::animate(
            &t.view,
            duration,
            widgets::EASE_OUT,
            || {
                if resize {
                    t.view.animator().setFrame(to);
                }
                for v in t.view.subviews().iter() {
                    v.animator().setAlphaValue(1.0);
                }
            },
            || {},
        );
    }

    /// Sizes the toast for `toast` and fills it in; returns its frame. The tables scroll
    /// clear of it.
    fn lay_out_toast(&self, toast: &Toast) -> NSRect {
        let t = &self.toast;
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
        let frame = rect(
            root.size.width - TOAST_RIGHT - w,
            PlayerBar::H + TOAST_BOTTOM,
            w,
            h,
        );
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
        self.set_toast_inset(TOAST_BOTTOM + h);
        frame
    }

    fn set_toast_inset(&self, inset: CGFloat) {
        self.results.set_bottom_inset(inset);
        self.queue.set_bottom_inset(inset);
        self.library_table.set_bottom_inset(inset);
        self.collection_table.set_bottom_inset(inset);
        self.shelves.set_bottom_inset(inset);
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
        // A swap's fade-out cut short by a dismissal leaves the content clear.
        for c in v.subviews().iter() {
            c.setAlphaValue(1.0);
        }
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
            widgets::motion(TOAST_EXIT),
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
                        ui.set_toast_inset(0.0);
                    }
                });
            },
        );
    }
}

/// What VoiceOver reads for `toast`: an undoable one also names the key that undoes it,
/// which outlasts the toast and its button.
fn spoken(toast: &Toast) -> String {
    match toast.action {
        Some("Undo") => format!("{}. Undo with Command-Z", toast.text),
        _ => toast.text.clone(),
    }
}

struct CollectionPatch {
    collection: Collection,
    from: Place,
    view: PageView,
    rows: Vec<table::RowText>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

struct NowPlaying {
    track: Option<(String, String)>,
    total: String,
    artwork: Option<String>,
}

/// Owned copies of the dirty regions, taken so the view-model borrow ends before AppKit
/// runs.
struct Patch {
    /// Rows, and the empty state when there are none.
    results: Option<(Vec<table::RowText>, Option<EmptyText>)>,
    search_status: Option<Option<(String, bool)>>,
    queue: Option<(Vec<table::RowText>, Option<usize>)>,
    queue_badge: Option<Option<String>>,
    home: Option<(PageView, Vec<Shelf>)>,
    library: Option<(LibraryView, Vec<table::RowText>)>,
    collection: Option<Option<CollectionPatch>>,
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
            home: any(Dirty::HOME).then(|| (s.home.view(), s.home.shelves.clone())),
            library: (any(Dirty::LIBRARY) || any(Dirty::ACCOUNT)).then(|| {
                (
                    s.library_view(),
                    table::collection_rows(&s.library_collections()),
                )
            }),
            collection: any(Dirty::COLLECTION).then(|| {
                s.collection.as_ref().map(|c| CollectionPatch {
                    collection: c.collection.clone(),
                    from: c.from,
                    view: c.browse.view(),
                    rows: table::rows(&c.tracks),
                })
            }),
            now_playing: (any(Dirty::NOW_PLAYING) || any(Dirty::STATE)).then(|| NowPlaying {
                track: s
                    .now_playing
                    .as_ref()
                    .map(|t| (t.title.clone(), vm::artists(t))),
                total: s.total_text(),
                artwork: s.now_playing.as_ref().and_then(|t| t.artwork_url.clone()),
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
        assert!(p.home.is_none() && p.library.is_none() && p.collection.is_none());
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
        assert_eq!(now.artwork, demo::artwork_url(0));
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
    fn an_undoable_toast_is_read_with_its_key() {
        let toast = |action| Toast {
            id: 1,
            text: "Queue cleared".into(),
            error: false,
            action,
        };
        assert_eq!(
            spoken(&toast(Some("Undo"))),
            "Queue cleared. Undo with Command-Z"
        );
        assert_eq!(spoken(&toast(None)), "Queue cleared");
    }

    #[test]
    fn empty_results_carry_their_empty_state() {
        let (vm, _) = demo::start_in(crate::args::View::Empty);
        let (rows, empty) = Patch::new(Dirty::RESULTS, vm.ui()).results.unwrap();
        assert!(rows.is_empty());
        assert_eq!(empty.unwrap().heading, "Search YouTube Music");
    }
}
