//! The AppKit app. Everything here runs on the main thread; the IPC threads in `backend` hand
//! their messages over through the main dispatch queue.

mod backend;
mod badge;
mod bar;
mod now_playing;
mod palette;
mod settings;
mod shelves;
mod snapshot;
mod table;
pub mod theme;
mod ui;
mod widgets;

use std::cell::RefCell;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{ClassType, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSBackingStoreType, NSButton, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSControlTextEditingDelegate, NSEvent, NSEventMask,
    NSEventModifierFlags, NSMenu, NSMenuItem, NSResponder, NSText, NSTextFieldDelegate, NSTextView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSRect, NSString};

use requiem_macos::artwork;

use crate::args::{Args, View};
use crate::demo;
use crate::vm::{Intent, PaletteAction, QueueEdit, SEEK_STEP_SECS, Service, VOLUME_STEP, Vm};
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

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            ui::with(|ui| ui.window_resized());
        }
    }

    // SAFETY: the implemented methods have the protocol's signatures.
    unsafe impl NSControlTextEditingDelegate for Delegate {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            ui::with(|ui| {
                let Some(control) = notification
                    .object()
                    .and_then(|o| o.downcast::<NSControl>().ok())
                else {
                    return;
                };
                if ui.palette.field.is(&control) {
                    ui.palette.filter();
                } else if let Some(row) = ui.settings.form_row(&control) {
                    ui.settings.fields_changed(row);
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

        #[unsafe(method(showHome:))]
        fn show_home(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.show(AppView::Home);
            });
        }

        #[unsafe(method(showLibrary:))]
        fn show_library(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.show(AppView::Library);
            });
        }

        #[unsafe(method(refresh:))]
        fn refresh(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.refresh());
        }

        #[unsafe(method(goBack:))]
        fn go_back(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.go_back());
        }

        #[unsafe(method(signIn:))]
        fn sign_in(&self, sender: Option<&AnyObject>) {
            if let Some(service) = sender_service(sender) {
                ui::with(|ui| ui.sign_in(service.provider()));
            }
        }

        #[unsafe(method(signOut:))]
        fn sign_out(&self, sender: Option<&AnyObject>) {
            if let Some(service) = sender_service(sender) {
                ui::with(|ui| ui.confirm_sign_out(service.provider()));
            }
        }

        #[unsafe(method(signOutOfAccount:))]
        fn sign_out_of_account(&self, sender: Option<&AnyObject>) {
            ui::with(|ui| ui.sign_out_menu(sender));
        }

        #[unsafe(method(libraryAction:))]
        fn library_action(&self, sender: Option<&AnyObject>) {
            let index = sender_tag(sender).and_then(|t| usize::try_from(t).ok());
            ui::with(|ui| ui.library_action(index.unwrap_or(0)));
        }

        #[unsafe(method(homeAction:))]
        fn home_action(&self, sender: Option<&AnyObject>) {
            let index = sender_tag(sender).and_then(|t| usize::try_from(t).ok());
            ui::with(|ui| ui.home_action(index.unwrap_or(0)));
        }

        #[unsafe(method(copyCode:))]
        fn copy_code(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.copy_code());
        }

        #[unsafe(method(openHost:))]
        fn open_host(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.open_host());
        }

        #[unsafe(method(cancelSignIn:))]
        fn cancel_sign_in(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.cancel_sign_in());
        }

        #[unsafe(method(chooseClientFile:))]
        fn choose_client_file(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.choose_client_file());
        }

        #[unsafe(method(openSetupGuide:))]
        fn open_setup_guide(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.open_setup_guide());
        }

        #[unsafe(method(cancelSetup:))]
        fn cancel_setup(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.cancel_setup());
        }

        #[unsafe(method(enterClientID:))]
        fn enter_client_id(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.enter_client_id());
        }

        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.show(AppView::Settings);
            });
        }

        #[unsafe(method(settingsSignIn:))]
        fn settings_sign_in(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.settings_sign_in(row));
            }
        }

        #[unsafe(method(settingsSignOut:))]
        fn settings_sign_out(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.settings_sign_out(row));
            }
        }

        #[unsafe(method(settingsCopyCode:))]
        fn settings_copy_code(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.settings_copy_code(row));
            }
        }

        #[unsafe(method(settingsOpenHost:))]
        fn settings_open_host(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.settings_open_host(row));
            }
        }

        #[unsafe(method(settingsCancelSignIn:))]
        fn settings_cancel_sign_in(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.settings_cancel_sign_in(row));
            }
        }

        #[unsafe(method(changeClient:))]
        fn change_client(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.change_client(row));
            }
        }

        #[unsafe(method(saveClient:))]
        fn save_client(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.save_client(row));
            }
        }

        #[unsafe(method(settingsChooseClientFile:))]
        fn settings_choose_client_file(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.settings_choose_client_file(row));
            }
        }

        #[unsafe(method(useBrowserSession:))]
        fn use_browser_session(&self, sender: Option<&AnyObject>) {
            if let Some(row) = sender_row(sender) {
                ui::with(|ui| ui.use_browser_session(row));
            }
        }

        #[unsafe(method(sheetUseBrowserSession:))]
        fn sheet_use_browser_session(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.sheet_use_browser_session());
        }

        #[unsafe(method(pasteSession:))]
        fn paste_session(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.paste_session());
        }

        /// Cmd+V outside a text field, whose field editor takes it first.
        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.paste_session());
        }

        #[unsafe(method(howToCopy:))]
        fn how_to_copy(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.how_to_copy());
        }

        #[unsafe(method(playCollection:))]
        fn play_collection(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.play_collection());
        }

        #[unsafe(method(addCollection:))]
        fn add_collection(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.add_collection());
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

        #[unsafe(method(showNowPlaying:))]
        fn show_now_playing(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| {
                ui.close_palette();
                ui.show_now_playing();
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

        #[unsafe(method(removeFromQueue:))]
        fn remove_from_queue(&self, _sender: Option<&AnyObject>) {
            edit_selected(QueueEdit::Remove);
        }

        // Not `moveUp:`/`moveDown:`, which text views answer along the responder chain.
        #[unsafe(method(moveTrackUp:))]
        fn move_track_up(&self, _sender: Option<&AnyObject>) {
            edit_selected(QueueEdit::MoveUp);
        }

        #[unsafe(method(moveTrackDown:))]
        fn move_track_down(&self, _sender: Option<&AnyObject>) {
            edit_selected(QueueEdit::MoveDown);
        }

        #[unsafe(method(clearQueue:))]
        fn clear_queue(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.edit_queue(QueueEdit::Clear));
        }

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.check_for_updates());
        }

        #[unsafe(method(toggleAutoUpdate:))]
        fn toggle_auto_update(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.toggle_auto_update());
        }

        #[unsafe(method(restartToUpdate:))]
        fn restart_to_update(&self, _sender: Option<&AnyObject>) {
            ui::with(|ui| ui.restart_to_update());
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

define_class!(
    /// The window. NSWindow answers `undo:` itself, for the text fields' typing undo, so the
    /// action never reaches the delegate; the queue's undo is added here.
    // SAFETY: NSWindow has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct MainWindow;

    impl MainWindow {
        /// Typing undo in a text field; elsewhere the replaced or cleared queue's.
        #[unsafe(method(undo:))]
        fn undo(&self, sender: Option<&AnyObject>) {
            if in_text(self) {
                // SAFETY: forwards to the superclass implementation.
                let _: () = unsafe { msg_send![super(self), undo: sender] };
            } else {
                ui::with(|ui| ui.undo());
            }
        }

        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            let undo = item.action() == Some(sel!(undo:));
            if undo && !in_text(self) {
                let (title, enabled) = ui::with(|ui| ui.undo_item()).unwrap_or(("Undo", false));
                item.setTitle(&NSString::from_str(title));
                enabled
            } else {
                if undo {
                    // The superclass titles it for typing, if there is any to undo.
                    item.setTitle(&NSString::from_str("Undo"));
                }
                // SAFETY: forwards to the superclass implementation.
                unsafe { msg_send![super(self), validateMenuItem: item] }
            }
        }
    }
);

impl MainWindow {
    fn create(
        mtm: MainThreadMarker,
        frame: NSRect,
        style: NSWindowStyleMask,
    ) -> Retained<NSWindow> {
        // SAFETY: NSWindow's designated initializer. `Ui` retains the window and turns off
        // released-when-closed before anything can close it.
        let this: Retained<Self> = unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: true
            ]
        };
        Retained::into_super(this)
    }
}

/// A text field is being edited.
fn in_text(window: &NSWindow) -> bool {
    window
        .firstResponder()
        .is_some_and(|r| r.isKindOfClass(NSText::class()))
}

fn edit_selected(edit: fn(usize) -> QueueEdit) {
    ui::with(|ui| {
        if let Some(e) = ui.selected_edit(edit) {
            ui.edit_queue(e);
        }
    });
}

/// Enables our menu items. Arrow, Return, Delete and volume items step aside while a text
/// field has focus, so its own editing keys work, and Return also for a focused button.
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
    if action == sel!(toggle:) {
        let (playing, _) = ui.playback();
        item.setTitle(&NSString::from_str(if playing { "Pause" } else { "Play" }));
    }
    if let Some(a) = palette_action(item) {
        // These keys belong to a focused field, or for Return a focused button.
        let taken = match a {
            PaletteAction::Next
            | PaletteAction::Previous
            | PaletteAction::VolumeUp
            | PaletteAction::VolumeDown => in_text,
            PaletteAction::PlaySelected | PaletteAction::AddToQueue => in_text || on_button,
            _ => false,
        };
        !taken && ui.can_run(a)
    } else if let Some(edit) = row_edit(action) {
        // For Remove, the key monitor's conditions for Delete, which its item only shows.
        !in_text && ui.selected_edit(edit).is_some_and(|e| ui.can_edit(e))
    } else if action == sel!(goBack:) {
        ui.can_go_back()
    } else if action == sel!(quitAndStop:) {
        ui.can_stop_player()
    } else if action == sel!(checkForUpdates:) {
        ui.can_check_for_updates()
    } else if action == sel!(paste:) {
        !in_text && ui.can_paste_session()
    } else if action == sel!(toggleAutoUpdate:) {
        let auto = ui.auto_update();
        item.setState(if auto == Some(true) {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        auto.is_some()
    } else {
        true
    }
}

/// The tag of the menu item or button that sent an action.
fn sender_tag(sender: Option<&AnyObject>) -> Option<isize> {
    let sender = sender?;
    if let Some(item) = sender.downcast_ref::<NSMenuItem>() {
        Some(item.tag())
    } else {
        sender.downcast_ref::<NSControl>().map(|c| c.tag())
    }
}

/// The Settings row a button is tagged with.
fn sender_row(sender: Option<&AnyObject>) -> Option<usize> {
    sender_tag(sender).and_then(|t| usize::try_from(t).ok())
}

/// The service a sign-in or sign-out item is tagged with.
fn sender_service(sender: Option<&AnyObject>) -> Option<Service> {
    sender_tag(sender)
        .and_then(|t| usize::try_from(t).ok())
        .and_then(|i| Service::ALL.get(i).copied())
}

/// The palette action a menu item runs, so both enable it alike.
fn palette_action(item: &NSMenuItem) -> Option<PaletteAction> {
    let action = item.action()?;
    let service = usize::try_from(item.tag())
        .ok()
        .and_then(|i| Service::ALL.get(i).copied());
    if let Some(service) = service {
        if action == sel!(signIn:) {
            return Some(PaletteAction::SignIn(service));
        } else if action == sel!(signOut:) {
            return Some(PaletteAction::SignOut(service));
        }
    }
    [
        (sel!(toggle:), PaletteAction::PlayPause),
        (sel!(next:), PaletteAction::Next),
        (sel!(prev:), PaletteAction::Previous),
        (sel!(playSelected:), PaletteAction::PlaySelected),
        (sel!(addToQueue:), PaletteAction::AddToQueue),
        (sel!(focusSearch:), PaletteAction::FocusSearch),
        (sel!(showHome:), PaletteAction::ShowHome),
        (sel!(showSearch:), PaletteAction::ShowSearch),
        (sel!(showLibrary:), PaletteAction::ShowLibrary),
        (sel!(showQueue:), PaletteAction::ShowQueue),
        (sel!(showNowPlaying:), PaletteAction::ShowNowPlaying),
        (sel!(showSettings:), PaletteAction::ShowSettings),
        (sel!(refresh:), PaletteAction::Refresh),
        (sel!(volumeUp:), PaletteAction::VolumeUp),
        (sel!(volumeDown:), PaletteAction::VolumeDown),
        (sel!(clearQueue:), PaletteAction::ClearQueue),
    ]
    .into_iter()
    .find_map(|(s, a)| (s == action).then_some(a))
}

fn row_edit(action: Sel) -> Option<fn(usize) -> QueueEdit> {
    if action == sel!(removeFromQueue:) {
        Some(QueueEdit::Remove)
    } else if action == sel!(moveTrackUp:) {
        Some(QueueEdit::MoveUp)
    } else if action == sel!(moveTrackDown:) {
        Some(QueueEdit::MoveDown)
    } else {
        None
    }
}

/// Keys the search, palette and client fields handle themselves; true when handled.
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
        } else if command == sel!(insertTab:) || command == sel!(insertBacktab:) {
            // The palette has nothing else to focus.
        } else {
            return false;
        }
        return true;
    }
    // Save is the client form's default button.
    if let Some(row) = ui.settings.form_row(control)
        && command == sel!(insertNewline:)
    {
        ui.save_client(row);
        return true;
    }
    false
}

const KEY_RETURN: u16 = 36;
const KEY_SPACE: u16 = 49;
const KEY_DELETE: u16 = 51;
const KEY_ESCAPE: u16 = 53;
const KEY_ENTER: u16 = 76;
const KEY_FORWARD_DELETE: u16 = 117;
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
        KEY_ESCAPE if plain && ui.current_view() == AppView::NowPlaying => ui.go_back(),
        KEY_SPACE if plain && !on_button => ui.intent(Intent::Toggle),
        KEY_LEFT if plain => ui.intent(Intent::SeekBy(-SEEK_STEP_SECS)),
        KEY_RIGHT if plain => ui.intent(Intent::SeekBy(SEEK_STEP_SECS)),
        KEY_UP if plain && ui.results_at_top() => ui.focus_search(),
        KEY_RETURN | KEY_ENTER if (plain || cmd) && !on_button => ui.activate_selection(cmd),
        KEY_DELETE | KEY_FORWARD_DELETE if plain => {
            match ui
                .selected_edit(QueueEdit::Remove)
                .filter(|&e| ui.can_edit(e))
            {
                Some(e) => ui.edit_queue(e),
                None => return false,
            }
        }
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
    let mut app_items = vec![item(
        "About Requiem",
        Some(sel!(orderFrontStandardAboutPanel:)),
        "",
        false,
    )];
    if backend::shows_updates() {
        app_items.extend([
            item(
                "Check for Updates\u{2026}",
                Some(sel!(checkForUpdates:)),
                "",
                true,
            ),
            // Checked or not by `validate`.
            item(
                "Check for Updates Automatically",
                Some(sel!(toggleAutoUpdate:)),
                "",
                true,
            ),
        ]);
    }
    app_items.extend([
        sep(),
        item("Settings\u{2026}", Some(sel!(showSettings:)), ",", true),
        sep(),
        item("Hide Requiem", Some(sel!(hide:)), "h", false),
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
        item("Quit Requiem", Some(sel!(terminate:)), "q", false),
    ]);
    submenu("Requiem", app_items);
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
    // Tagged with the `Service::ALL` index, which `palette_action` reads back.
    let account_items: Vec<Retained<NSMenuItem>> = Service::ALL
        .into_iter()
        .enumerate()
        .flat_map(|(i, service)| {
            [
                (PaletteAction::SignIn(service), sel!(signIn:)),
                (PaletteAction::SignOut(service), sel!(signOut:)),
            ]
            .map(|(a, action)| {
                let it = item(a.label(), Some(action), "", true);
                it.setTag(i as isize);
                it
            })
        })
        .collect();
    submenu(
        "View",
        vec![
            item("Home", Some(sel!(showHome:)), "1", true),
            item("Search", Some(sel!(showSearch:)), "2", true),
            item("Library", Some(sel!(showLibrary:)), "3", true),
            item("Queue", Some(sel!(showQueue:)), "4", true),
            item("Now Playing", Some(sel!(showNowPlaying:)), "l", true),
            sep(),
            item("Refresh", Some(sel!(refresh:)), "r", true),
            item("Back", Some(sel!(goBack:)), "[", true),
            sep(),
            account_items[0].clone(),
            account_items[1].clone(),
            account_items[2].clone(),
            account_items[3].clone(),
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
            // Shown only, like Return: the key monitor handles Delete.
            with_mods(
                item(
                    "Remove from Queue",
                    Some(sel!(removeFromQueue:)),
                    "\u{8}",
                    true,
                ),
                NSEventModifierFlags::empty(),
            ),
            with_mods(
                item("Move Up", Some(sel!(moveTrackUp:)), UP_ARROW, true),
                cmd_opt,
            ),
            with_mods(
                item("Move Down", Some(sel!(moveTrackDown:)), DOWN_ARROW, true),
                cmd_opt,
            ),
            item("Clear Queue", Some(sel!(clearQueue:)), "", true),
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

/// Caches the demo tracks' made-up artwork, so it shows without any fetch.
fn seed_demo_artwork(mtm: MainThreadMarker) {
    for (url, seed) in demo::covers() {
        artwork::insert(mtm, &url, &artwork::demo_cover(seed, 64.0));
    }
}

/// How long a snapshot of the connected window waits for its page and covers.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(20);
const SNAPSHOT_POLL: Duration = Duration::from_millis(100);

/// Writes the connected window's snapshot once its page is in, polling from the run loop;
/// exits 0 when written, 1 when the page failed or never came.
fn snapshot_when_ready(path: std::path::PathBuf, started: Instant) {
    let state = ui::with(|ui| {
        // The window is not on screen, so changes only pile up until forced in.
        ui.render_now();
        ui.snapshot_state()
    });
    let waited = started.elapsed();
    let write = |ui: &Ui| match snapshot::write(&ui.root, &path) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("requiem-gui: snapshot failed: {e}");
            std::process::exit(1);
        }
    };
    match state {
        Some(ui::SnapshotState::Loaded { covers: false }) => {
            ui::with(write);
        }
        Some(ui::SnapshotState::Loaded { covers: true }) if waited >= SNAPSHOT_WAIT => {
            // Covers that never arrive show their placeholders.
            ui::with(write);
        }
        Some(ui::SnapshotState::Failed(e)) => {
            eprintln!("requiem-gui: the page did not load: {e}");
            std::process::exit(1);
        }
        _ if waited >= SNAPSHOT_WAIT => {
            eprintln!(
                "requiem-gui: the page did not load in {} s",
                SNAPSHOT_WAIT.as_secs()
            );
            std::process::exit(1);
        }
        _ => {
            if let Ok(when) = DispatchTime::try_from(SNAPSHOT_POLL) {
                let _ =
                    DispatchQueue::main().after(when, move || snapshot_when_ready(path, started));
            }
        }
    }
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
        seed_demo_artwork(mtm);
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
        View::Home => ui.show(AppView::Home),
        View::HomeScrolled => {
            ui.show(AppView::Home);
            ui.shelves.scroll_to_end();
        }
        View::HomeShelfPaged => {
            ui.show(AppView::Home);
            if let Some(row) = ui.shelves.first_card_row() {
                ui.shelves.page(row, true);
            }
        }
        View::HomeSignedOut => ui.show(AppView::Home),
        View::Library
        | View::LibrarySignedOut
        | View::LibraryUnavailable
        | View::SignIn
        | View::SignInSetup => ui.show(AppView::Library),
        View::Collection => ui.show(AppView::Collection),
        View::Queue => ui.show(AppView::Queue),
        View::Settings | View::SettingsSetup | View::SettingsSession => ui.show(AppView::Settings),
        View::NowPlaying | View::NowPlayingEmpty | View::NowPlayingLast => ui.show_now_playing(),
        View::Palette => ui.open_palette(),
        View::Search
        | View::Empty
        | View::Disconnected
        | View::Toast
        | View::OutOfDate
        | View::UpdateReady
        | View::UpdateFailed
        | View::NotUpdatable
        | View::UpToDate => ui.show(AppView::Search),
    }

    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    if let Some(path) = &args.snapshot {
        if args.uses_demo_data() {
            match snapshot::write(&ui.root, path) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("requiem-gui: snapshot failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        // Connected: the run loop delivers the daemon's answers, which the window then shows.
        snapshot_when_ready(path.clone(), Instant::now());
        app.run();
        return;
    }

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
