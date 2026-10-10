//! The AppKit app. Everything here runs on the main thread; the IPC threads in `backend` hand
//! their messages over through the main dispatch queue.

mod backend;
mod bar;
mod palette;
mod snapshot;
mod table;
pub mod theme;
mod ui;
mod widgets;

use std::cell::RefCell;
use std::ptr::{self, NonNull};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{ClassType, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSControl, NSControlTextEditingDelegate, NSEvent, NSEventMask,
    NSEventModifierFlags, NSMenu, NSMenuItem, NSText, NSTextFieldDelegate, NSTextView,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSString};

use crate::args::{Args, View};
use crate::demo;
use crate::vm::{Intent, SEEK_STEP_SECS, Vm};
use backend::{Backend, Live};
use ui::{AppView, Ui};

define_class!(
    /// App delegate, text-field delegate, and the target of buttons and menu items.
    // SAFETY: NSObject has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct Delegate;

    // SAFETY: no requirements.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the implemented method has the protocol's signature.
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    // SAFETY: the implemented methods have the protocol's signatures.
    unsafe impl NSControlTextEditingDelegate for Delegate {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            ui::with(|ui| {
                let palette = notification
                    .object()
                    .and_then(|o| o.downcast::<NSControl>().ok())
                    .is_some_and(|c| ui.palette.field.is(&c));
                if palette {
                    ui.palette.filter();
                }
            });
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, control: &NSControl, _text: &NSTextView, command: Sel) -> bool {
            ui::with(|ui| field_command(ui, control, command)).unwrap_or(false)
        }
    }

    // SAFETY: no required methods.
    unsafe impl NSTextFieldDelegate for Delegate {}

    impl Delegate {
        #[unsafe(method(prev:))]
        fn prev(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.intent(Intent::Prev));
        }

        #[unsafe(method(next:))]
        fn next(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.intent(Intent::Next));
        }

        #[unsafe(method(toggle:))]
        fn toggle(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.intent(Intent::Toggle));
        }

        #[unsafe(method(closeToast:))]
        fn close_toast(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.close_toast());
        }

        #[unsafe(method(showSearch:))]
        fn show_search(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.show(AppView::Search);
            });
        }

        #[unsafe(method(showQueue:))]
        fn show_queue(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.show(AppView::Queue);
            });
        }

        #[unsafe(method(focusSearch:))]
        fn focus_search(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.focus_search();
            });
        }

        #[unsafe(method(openPalette:))]
        fn open_palette(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.open_palette());
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: NSObject's `init`; the class has no ivars to initialise.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/// Keys the search and palette fields handle themselves; true when handled.
fn field_command(ui: &Ui, control: &NSControl, command: Sel) -> bool {
    if ui.search.is(control) {
        if command == sel!(insertNewline:) {
            ui.search_submitted();
        } else if command == sel!(moveDown:) {
            ui.show(AppView::Search);
            ui.results.select_first();
        } else if command == sel!(cancelOperation:) {
            ui.show(AppView::Search);
        } else {
            return false;
        }
        return true;
    }
    if ui.palette.field.is(control) {
        if command == sel!(moveUp:) {
            ui.palette.move_selection(-1);
        } else if command == sel!(moveDown:) {
            ui.palette.move_selection(1);
        } else if command == sel!(insertNewline:) {
            if let Some(a) = ui.palette.selected_action() {
                ui.run_action(a);
            }
        } else if command == sel!(cancelOperation:) {
            ui.close_palette();
        } else {
            return false;
        }
        return true;
    }
    false
}

const KEY_RETURN: u16 = 36;
const KEY_SPACE: u16 = 49;
const KEY_ESCAPE: u16 = 53;
const KEY_ENTER: u16 = 76;
const KEY_LEFT: u16 = 123;
const KEY_RIGHT: u16 = 124;

/// Window-wide keys outside text fields. True when handled.
fn key_down(ui: &Ui, event: &NSEvent) -> bool {
    let code = event.keyCode();
    if ui.palette.is_open() {
        if code == KEY_ESCAPE {
            ui.close_palette();
            return true;
        }
        return false;
    }
    let in_text = ui
        .window
        .firstResponder()
        .is_some_and(|r| r.isKindOfClass(NSText::class()));
    if in_text {
        return false;
    }
    let flags = event.modifierFlags();
    let cmd = flags.contains(NSEventModifierFlags::Command);
    let plain = !flags.intersects(
        NSEventModifierFlags::Command
            | NSEventModifierFlags::Control
            | NSEventModifierFlags::Option,
    );
    match code {
        KEY_SPACE if plain => ui.intent(Intent::Toggle),
        KEY_LEFT if plain => ui.intent(Intent::SeekBy(-SEEK_STEP_SECS)),
        KEY_RIGHT if plain => ui.intent(Intent::SeekBy(SEEK_STEP_SECS)),
        KEY_RETURN | KEY_ENTER if plain || cmd => ui.activate_selection(cmd),
        _ => return false,
    }
    true
}

fn install_key_monitor() {
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes an event that is valid for the duration of the call.
        let ev = unsafe { event.as_ref() };
        if ui::with(|ui| key_down(ui, ev)).unwrap_or(false) {
            ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    // SAFETY: the handler returns the event it was given or null, as the API requires.
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
    };
    // The monitor lives as long as the process.
    std::mem::forget(monitor);
}

fn menu_bar(mtm: MainThreadMarker, target: &Delegate) -> Retained<NSMenu> {
    let bar = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::new());
    let ours: &AnyObject = target.as_ref();
    let item = |title: &str, action: Option<Sel>, key: &str, mine: bool| {
        // SAFETY: each action is implemented by `target` when `mine`, and otherwise by
        // AppKit along the responder chain.
        unsafe {
            let i = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                action,
                &NSString::from_str(key),
            );
            if mine {
                i.setTarget(Some(ours));
            }
            i
        }
    };
    let sep = || NSMenuItem::separatorItem(mtm);
    let submenu = |title: &str, items: Vec<Retained<NSMenuItem>>| {
        let m = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for i in &items {
            m.addItem(i);
        }
        let top = item(title, None, "", false);
        top.setSubmenu(Some(&m));
        bar.addItem(&top);
    };
    submenu(
        "requiem",
        vec![
            item("Hide requiem", Some(sel!(hide:)), "h", false),
            sep(),
            item("Quit requiem", Some(sel!(terminate:)), "q", false),
        ],
    );
    submenu(
        "Edit",
        vec![
            item("Undo", Some(sel!(undo:)), "z", false),
            item("Redo", Some(sel!(redo:)), "Z", false),
            sep(),
            item("Cut", Some(sel!(cut:)), "x", false),
            item("Copy", Some(sel!(copy:)), "c", false),
            item("Paste", Some(sel!(paste:)), "v", false),
            item("Select All", Some(sel!(selectAll:)), "a", false),
        ],
    );
    submenu(
        "View",
        vec![
            item("Search", Some(sel!(showSearch:)), "1", true),
            item("Queue", Some(sel!(showQueue:)), "2", true),
            sep(),
            item("Find", Some(sel!(focusSearch:)), "f", true),
            item("Command Palette", Some(sel!(openPalette:)), "k", true),
        ],
    );
    submenu(
        "Playback",
        vec![
            item("Play/Pause", Some(sel!(toggle:)), "", true),
            item("Next", Some(sel!(next:)), "", true),
            item("Previous", Some(sel!(prev:)), "", true),
        ],
    );
    submenu(
        "Window",
        vec![
            item("Minimize", Some(sel!(performMiniaturize:)), "m", false),
            item("Close", Some(sel!(performClose:)), "w", false),
        ],
    );
    bar
}

pub fn run(args: Args) {
    let mtm = MainThreadMarker::new().expect("requiem-gui must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // SAFETY: the appearance-name static is an immutable framework constant.
    let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
    app.setAppearance(dark.as_deref());
    theme::register_fonts();

    let delegate = Delegate::new(mtm);
    // Live messages are dispatched to the main queue, which drains only once `app.run()`
    // starts, after the UI is installed.
    let (vm, backend) = if args.uses_demo_data() {
        let (vm, daemon) = demo::start_in(args.view);
        (vm, Backend::Demo(RefCell::new(daemon)))
    } else {
        (Vm::new(), Backend::Live(Live::start()))
    };
    let ui = Ui::new(mtm, vm, backend, &delegate);
    ui::install(ui.clone());
    ui.render();
    match args.view {
        View::Queue => ui.show(AppView::Queue),
        View::Palette => ui.open_palette(),
        View::Search | View::Empty | View::Disconnected | View::Toast | View::OutOfDate => {
            ui.show(AppView::Search)
        }
    }

    if let Some(path) = &args.snapshot {
        match snapshot::write(&ui.root, path) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("requiem-gui: snapshot failed: {e}");
                std::process::exit(1);
            }
        }
    }

    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.setMainMenu(Some(&menu_bar(mtm, &delegate)));
    install_key_monitor();
    ui.window.makeKeyAndOrderFront(None);
    if !ui.palette.is_open() {
        ui.show(ui.current_view());
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    if args.smoke
        && let Ok(when) = DispatchTime::try_from(Duration::from_secs(2))
    {
        let _ = DispatchQueue::main().after(when, || std::process::exit(0));
    }
    app.run();
}
