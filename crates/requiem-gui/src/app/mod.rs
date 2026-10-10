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
    NSApplicationDelegate, NSButton, NSControl, NSControlTextEditingDelegate, NSEvent, NSEventMask,
    NSEventModifierFlags, NSMenu, NSMenuItem, NSResponder, NSText, NSTextFieldDelegate, NSTextView,
    NSWindowDelegate,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSString};

use crate::args::{Args, View};
use crate::demo;
use crate::vm::{Intent, SEEK_STEP_SECS, VOLUME_STEP, Vm};
use backend::{Backend, Live};
use ui::{AppView, Ui};

define_class!(
    /// App, window and text-field delegate, and the target of buttons and menu items.
    // SAFETY: NSObject has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct Delegate;

    // SAFETY: no requirements.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the implemented methods have the protocol's signatures.
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationDidUnhide:))]
        fn application_did_unhide(&self, _notification: &NSNotification) {
            ui::with(|ui| ui.catch_up());
        }
    }

    // SAFETY: the implemented methods have the protocol's signatures.
    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn window_did_change_occlusion_state(&self, _notification: &NSNotification) {
            ui::with(|ui| ui.catch_up());
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _notification: &NSNotification) {
            ui::with(|ui| ui.catch_up());
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
        fn do_command(&self, control: &NSControl, text: &NSTextView, command: Sel) -> bool {
            ui::with(|ui| field_command(ui, control, text, command)).unwrap_or(false)
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

        #[unsafe(method(toastAction:))]
        fn toast_action(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.toast_action());
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

        #[unsafe(method(emptyAction:))]
        fn empty_action(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.empty_action());
        }

        #[unsafe(method(openPalette:))]
        fn open_palette(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.toggle_palette());
        }

        #[unsafe(method(playSelected:))]
        fn play_selected(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.activate_selection(false));
        }

        #[unsafe(method(addToQueue:))]
        fn add_to_queue(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.activate_selection(true));
        }

        #[unsafe(method(volumeUp:))]
        fn volume_up(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.intent(Intent::VolumeBy(VOLUME_STEP)));
        }

        #[unsafe(method(volumeDown:))]
        fn volume_down(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.intent(Intent::VolumeBy(-VOLUME_STEP)));
        }

        #[unsafe(method(quitAndStop:))]
        fn quit_and_stop(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.quit_and_stop());
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            ui::with(|ui| validate(ui, item)).unwrap_or(true)
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: NSObject's `init`; the class has no ivars to initialise.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/// Enables our menu items. Arrow, Return and volume items step aside while a text field has
/// focus, so its own editing keys work, and Return also for a focused button.
fn validate(ui: &Ui, item: &NSMenuItem) -> bool {
    let Some(action) = item.action() else {
        return true;
    };
    let responder = ui.window.firstResponder();
    let is = |class| {
        responder
            .as_deref()
            .is_some_and(|r: &NSResponder| r.isKindOfClass(class))
    };
    let in_text = is(NSText::class());
    let on_button = is(NSButton::class());
    let (playing, can) = ui.playback();
    if action == sel!(toggle:) {
        item.setTitle(&NSString::from_str(if playing { "Pause" } else { "Play" }));
        can.transport
    } else if action == sel!(next:) || action == sel!(prev:) {
        !in_text && can.transport
    } else if action == sel!(volumeUp:) || action == sel!(volumeDown:) {
        !in_text && can.player
    } else if action == sel!(playSelected:) || action == sel!(addToQueue:) {
        let append = action == sel!(addToQueue:);
        !in_text && !on_button && can.player && ui.has_selection(append)
    } else {
        true
    }
}

/// Keys the search and palette fields handle themselves; true when handled.
fn field_command(ui: &Ui, control: &NSControl, text: &NSTextView, command: Sel) -> bool {
    if ui.search.is(control) {
        if command == sel!(insertNewline:) {
            ui.search_submitted();
        } else if command == sel!(moveDown:) {
            ui.show(AppView::Search);
            ui.results.select_first();
        } else if command == sel!(cancelOperation:) {
            if text.string().length() > 0 {
                text.setString(&NSString::new());
            } else {
                ui.show(AppView::Search);
            }
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
const KEY_UP: u16 = 126;

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
    let responder = ui.window.firstResponder();
    if responder
        .as_deref()
        .is_some_and(|r| r.isKindOfClass(NSText::class()))
    {
        return false;
    }
    // Space and Return press a focused button, with keyboard navigation on.
    let on_button = responder
        .as_deref()
        .is_some_and(|r| r.isKindOfClass(NSButton::class()));
    let flags = event.modifierFlags();
    let cmd = flags.contains(NSEventModifierFlags::Command);
    let plain = !flags.intersects(
        NSEventModifierFlags::Command
            | NSEventModifierFlags::Control
            | NSEventModifierFlags::Option,
    );
    match code {
        KEY_SPACE if plain && !on_button => ui.intent(Intent::Toggle),
        KEY_LEFT if plain => ui.intent(Intent::SeekBy(-SEEK_STEP_SECS)),
        KEY_RIGHT if plain => ui.intent(Intent::SeekBy(SEEK_STEP_SECS)),
        KEY_UP if plain && ui.results_at_top() => ui.focus_search(),
        KEY_RETURN | KEY_ENTER if (plain || cmd) && !on_button => ui.activate_selection(cmd),
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

/// NSMenuItem key equivalents for the arrow keys: AppKit's function-key characters.
const RIGHT_ARROW: &str = "\u{F703}";
const LEFT_ARROW: &str = "\u{F702}";
const UP_ARROW: &str = "\u{F700}";
const DOWN_ARROW: &str = "\u{F701}";

/// The main menu, and the Window menu to register with AppKit.
fn menu_bar(mtm: MainThreadMarker, target: &Delegate) -> (Retained<NSMenu>, Retained<NSMenu>) {
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
    let with_mods = |i: Retained<NSMenuItem>, mods: NSEventModifierFlags| {
        i.setKeyEquivalentModifierMask(mods);
        i
    };
    let cmd_opt = NSEventModifierFlags::Command | NSEventModifierFlags::Option;
    let sep = || NSMenuItem::separatorItem(mtm);
    let submenu = |title: &str, items: Vec<Retained<NSMenuItem>>| {
        let m = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for i in &items {
            m.addItem(i);
        }
        let top = item(title, None, "", false);
        top.setSubmenu(Some(&m));
        bar.addItem(&top);
        m
    };
    submenu(
        "requiem",
        vec![
            item(
                "About requiem",
                Some(sel!(orderFrontStandardAboutPanel:)),
                "",
                false,
            ),
            sep(),
            item("Hide requiem", Some(sel!(hide:)), "h", false),
            with_mods(
                item(
                    "Hide Others",
                    Some(sel!(hideOtherApplications:)),
                    "h",
                    false,
                ),
                cmd_opt,
            ),
            item("Show All", Some(sel!(unhideAllApplications:)), "", false),
            sep(),
            // Quitting the window leaves the music playing; this one stops the player too.
            with_mods(
                item("Quit and Stop Music", Some(sel!(quitAndStop:)), "q", true),
                cmd_opt,
            ),
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
            sep(),
            item("Find", Some(sel!(focusSearch:)), "f", true),
        ],
    );
    submenu(
        "View",
        vec![
            item("Search", Some(sel!(showSearch:)), "1", true),
            item("Queue", Some(sel!(showQueue:)), "2", true),
            sep(),
            item("Command Palette", Some(sel!(openPalette:)), "k", true),
        ],
    );
    submenu(
        "Playback",
        vec![
            // Titled Play or Pause by `validate`.
            item("Play", Some(sel!(toggle:)), "", true),
            item("Next", Some(sel!(next:)), RIGHT_ARROW, true),
            item("Previous", Some(sel!(prev:)), LEFT_ARROW, true),
            sep(),
            // The key monitor handles Return first; the menu shows the key.
            with_mods(
                item("Play Selected", Some(sel!(playSelected:)), "\r", true),
                NSEventModifierFlags::empty(),
            ),
            item("Add to Queue", Some(sel!(addToQueue:)), "\r", true),
            sep(),
            item("Volume Up", Some(sel!(volumeUp:)), UP_ARROW, true),
            item("Volume Down", Some(sel!(volumeDown:)), DOWN_ARROW, true),
        ],
    );
    let window = submenu(
        "Window",
        vec![
            item("Minimize", Some(sel!(performMiniaturize:)), "m", false),
            item("Zoom", Some(sel!(performZoom:)), "", false),
            item("Close", Some(sel!(performClose:)), "w", false),
            sep(),
            item("Bring All to Front", Some(sel!(arrangeInFront:)), "", false),
        ],
    );
    (bar, window)
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
    ui.window
        .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The window is not on screen yet, which `render` waits for.
    ui.render_now();
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
    let (menu, window_menu) = menu_bar(mtm, &delegate);
    app.setMainMenu(Some(&menu));
    app.setWindowsMenu(Some(&window_menu));
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
