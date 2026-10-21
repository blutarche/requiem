//! AppKit and MediaPlayer side. Everything here runs on the main thread.

pub mod art;
mod mini;
mod now_playing;
mod status;

use std::cell::{Cell, OnceCell, RefCell};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageFileType,
    NSControl, NSEvent, NSEventModifierFlags, NSEventType, NSMenu, NSMenuDelegate, NSMenuItem,
    NSPopoverDelegate, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSDictionary, NSNotification, NSObject, NSPoint, NSRect};
use requiem_core::PlayState;

use self::mini::MiniPlayer;
use self::now_playing::NowPlaying;
use self::status::{StatusMenu, menu_action};
use crate::view::{PAUSED_IDLE_AFTER, changed, position_now, status_idle};
use crate::{PlayerView, UiAction, UiUpdate, WANTS_POSITIONS};

/// Below the tracker's 2 s, so a seek it reported still applies after up to 1 s of main-queue
/// delay.
const APPLY_TOLERANCE: Duration = Duration::from_secs(1);
/// A click on the status item while the mini player is open closes it on mouse-down, before
/// the button's mouse-up action; that action must not open it again.
const REOPEN_GUARD: Duration = Duration::from_millis(400);
/// The daemon's artwork budget: the mini player shows one cover at a time, about 1.1 MB at
/// 268 pt on a 2x screen.
const DAEMON_ARTWORK_CAPACITY: usize = 2 * 1024 * 1024;

thread_local! {
    /// Set on the main thread while `run` is in `app.run()`.
    static CONTROLLER: RefCell<Option<Retained<Controller>>> = const { RefCell::new(None) };
}

/// Playing or about to be: the toggle pauses, and the icon shows playback.
fn is_active(state: PlayState) -> bool {
    matches!(state, PlayState::Playing | PlayState::Buffering)
}

struct Ivars {
    updates: Receiver<UiUpdate>,
    actions: Sender<UiAction>,
    stop: Receiver<()>,
    status: StatusMenu,
    now_playing: NowPlaying,
    shown: RefCell<Option<(PlayerView, Instant)>>,
    volume: Cell<Option<u8>>,
    mini: OnceCell<MiniPlayer>,
    mini_closed_at: Cell<Option<Instant>>,
    /// When the session last entered Paused; unaffected by views that change while paused.
    paused_since: Cell<Option<Instant>>,
    /// Bumped on every transition into or out of Paused, so a pending idle wake can tell it is
    /// stale.
    paused_gen: Cell<u64>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Controller` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "RequiemStatusController"]
    #[ivars = Ivars]
    struct Controller;

    impl Controller {
        #[unsafe(method(menuAction:))]
        fn on_menu_action(&self, sender: &NSMenuItem) {
            if let Some(action) = menu_action(sender.tag()) {
                let _ = self.ivars().actions.send(action);
            }
        }

        /// Right-click and Control-click open the menu; a click toggles the mini player.
        #[unsafe(method(statusClicked:))]
        fn on_status_clicked(&self, _sender: &AnyObject) {
            let event = NSApplication::sharedApplication(self.mtm()).currentEvent();
            let menu = event.is_some_and(|e| {
                e.r#type() == NSEventType::RightMouseUp
                    || e.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if menu {
                self.ivars().status.open_menu();
            } else {
                self.toggle_mini();
            }
        }

        #[unsafe(method(miniAction:))]
        fn on_mini_action(&self, sender: &NSControl) {
            let Some(action) = menu_action(sender.tag()) else {
                return;
            };
            if action == UiAction::OpenGui {
                self.close_mini();
            }
            let _ = self.ivars().actions.send(action);
        }

        #[unsafe(method(miniSeek:))]
        fn on_mini_seek(&self, _sender: &NSControl) {
            let dragging = self.dragging();
            let action = self.ivars().mini.get().and_then(|m| m.seek_moved(dragging, Instant::now()));
            if let Some(action) = action {
                let _ = self.ivars().actions.send(action);
            }
        }

        #[unsafe(method(miniVolume:))]
        fn on_mini_volume(&self, _sender: &NSControl) {
            let dragging = self.dragging();
            let action = self
                .ivars()
                .mini
                .get()
                .and_then(|m| m.volume_moved(dragging, Instant::now()));
            if let Some(action) = action {
                let _ = self.ivars().actions.send(action);
            }
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            self.ivars().status.menu_closed();
        }
    }

    unsafe impl NSPopoverDelegate for Controller {
        #[unsafe(method(popoverWillClose:))]
        fn popover_will_close(&self, _notification: &NSNotification) {
            self.ivars().mini_closed_at.set(Some(Instant::now()));
        }

        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, _notification: &NSNotification) {
            self.mini_closed();
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's `init` takes no arguments and returns the initialized object.
        unsafe { msg_send![super(this), init] }
    }

    /// Checks `stop`, then applies the latest update offer, the latest pending view, any
    /// later position, then the latest volume.
    fn drain(&self) {
        let ivars = self.ivars();
        if !matches!(ivars.stop.try_recv(), Err(TryRecvError::Empty)) {
            self.stop_app();
            return;
        }
        let (mut view, mut position, mut volume, mut updates) = (None, None, None, None);
        for update in ivars.updates.try_iter() {
            match update {
                UiUpdate::View(v) => {
                    view = Some(v);
                    position = None;
                }
                UiUpdate::Position(at) => position = Some(at),
                UiUpdate::Volume(v) => volume = Some(v),
                UiUpdate::Updates { check, ready } => updates = Some((check, ready)),
            }
        }
        if let Some((check, ready)) = updates {
            ivars.status.show_updates(check, ready.as_deref());
        }
        if let Some(view) = view {
            self.show(view);
        }
        let now = Instant::now();
        let open = self.open_mini();
        if let (Some(at), Some(mini)) = (position, open) {
            mini.set_position(at, now);
        }
        if let Some(v) = volume {
            ivars.volume.set(Some(v));
            if let Some(mini) = open {
                mini.set_volume(v, now);
            }
        }
    }

    fn show(&self, view: Option<PlayerView>) {
        let ivars = self.ivars();
        let now = Instant::now();
        {
            let mut shown = ivars.shown.borrow_mut();
            if !changed(shown.as_ref(), view.as_ref(), now, APPLY_TOLERANCE) {
                return;
            }
            let was_paused = shown
                .as_ref()
                .is_some_and(|(v, _)| v.state == PlayState::Paused);
            *shown = view.clone().map(|v| (v, now));
            drop(shown);
            self.track_pause(view.as_ref(), was_paused, now);
        }
        let state = view.as_ref().map_or(PlayState::Stopped, |v| v.state);
        let idle = status_idle(state, ivars.paused_since.get(), now);
        ivars.status.show(view.as_ref(), idle);
        ivars.now_playing.show(view.as_ref());
        if let Some(mini) = self.open_mini() {
            mini.show(view.as_ref(), now);
        }
    }

    /// Starts the pause clock on entering Paused and schedules the wake that idles the status
    /// item, since an unchanged view is not rendered again.
    fn track_pause(&self, view: Option<&PlayerView>, was_paused: bool, now: Instant) {
        let ivars = self.ivars();
        let paused = view.is_some_and(|v| v.state == PlayState::Paused);
        if paused == was_paused {
            return;
        }
        let generation = ivars.paused_gen.get().wrapping_add(1);
        ivars.paused_gen.set(generation);
        ivars.paused_since.set(paused.then_some(now));
        if paused && let Ok(when) = DispatchTime::try_from(PAUSED_IDLE_AFTER) {
            let _ = DispatchQueue::main().after(when, move || {
                CONTROLLER.with_borrow(|controller| {
                    if let Some(controller) = controller {
                        controller.paused_idle(generation);
                    }
                });
            });
        }
    }

    fn paused_idle(&self, generation: u64) {
        let ivars = self.ivars();
        if ivars.paused_gen.get() != generation || ivars.paused_since.get().is_none() {
            return;
        }
        let view = ivars.shown.borrow().as_ref().map(|(v, _)| v.clone());
        ivars.status.show(view.as_ref(), true);
    }

    /// The mini player while it is open. Closed, it is not kept up to date.
    fn open_mini(&self) -> Option<&MiniPlayer> {
        self.ivars().mini.get().filter(|m| m.is_shown())
    }

    fn toggle_mini(&self) {
        let ivars = self.ivars();
        let mini = ivars.mini.get_or_init(|| {
            let mini = MiniPlayer::new(self.mtm(), Some(self));
            mini.set_delegate(ProtocolObject::from_ref(self));
            mini
        });
        if mini.is_shown() {
            self.close_mini();
            return;
        }
        let now = Instant::now();
        if ivars
            .mini_closed_at
            .get()
            .is_some_and(|at| now.saturating_duration_since(at) < REOPEN_GUARD)
        {
            return;
        }
        let Some(button) = ivars.status.button() else {
            return;
        };
        let view = ivars.shown.borrow().clone().map(|(view, at)| PlayerView {
            position: position_now(&view, at, now),
            ..view
        });
        mini.show(view.as_ref(), now);
        if let Some(v) = ivars.volume.get() {
            mini.set_volume(v, now);
        }
        WANTS_POSITIONS.store(true, Ordering::Relaxed);
        let app = NSApplication::sharedApplication(self.mtm());
        // An accessory app's popover takes key and closes on clicks elsewhere only while the
        // app is active.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        mini.open(&button);
    }

    /// `close` may skip the delegate, so this does its part too.
    fn close_mini(&self) {
        if let Some(mini) = self.ivars().mini.get() {
            mini.close();
        }
        self.mini_closed();
    }

    fn mini_closed(&self) {
        WANTS_POSITIONS.store(false, Ordering::Relaxed);
        if let Some(mini) = self.ivars().mini.get() {
            mini.closed();
        }
    }

    /// Whether the event being handled continues a slider drag rather than ending one.
    fn dragging(&self) -> bool {
        let event = NSApplication::sharedApplication(self.mtm()).currentEvent();
        event.is_some_and(|e| {
            matches!(
                e.r#type(),
                NSEventType::LeftMouseDown | NSEventType::LeftMouseDragged
            )
        })
    }

    fn stop_app(&self) {
        self.ivars().status.cancel_tracking();
        self.close_mini();
        let app = NSApplication::sharedApplication(self.mtm());
        app.stop(None);
        // `stop:` takes effect after the next event, and a main-queue block is not one.
        let wake = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::ZERO,
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
        if let Some(wake) = wake {
            app.postEvent_atStart(&wake, true);
        }
    }
}

/// Main-queue blocks run in the run loop's common modes, which include menu tracking, so the
/// status menu still updates while it is open.
pub(crate) fn wake() {
    DispatchQueue::main().exec_async(|| {
        CONTROLLER.with_borrow(|controller| {
            if let Some(controller) = controller {
                controller.drain();
            }
        });
    });
}

pub(crate) fn run(updates: Receiver<UiUpdate>, actions: Sender<UiAction>, stop: Receiver<()>) {
    let mtm =
        MainThreadMarker::new().expect("requiem_macos::run must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    art::set_capacity(mtm, DAEMON_ARTWORK_CAPACITY);

    let status = StatusMenu::new(mtm, sel!(menuAction:), sel!(statusClicked:));
    let now_playing = NowPlaying::new(actions.clone());
    let controller = Controller::new(
        mtm,
        Ivars {
            updates,
            actions,
            stop,
            status,
            now_playing,
            shown: RefCell::new(None),
            volume: Cell::new(None),
            mini: OnceCell::new(),
            mini_closed_at: Cell::new(None),
            paused_since: Cell::new(None),
            paused_gen: Cell::new(0),
        },
    );
    controller
        .ivars()
        .status
        .set_target(&controller, ProtocolObject::from_ref(&*controller));
    CONTROLLER.set(Some(controller.clone()));
    // Picks up anything sent before `CONTROLLER` was set. Queued rather than called, because
    // `stop:` before `run` would not end it.
    wake();

    app.run();

    CONTROLLER.set(None);
    WANTS_POSITIONS.store(false, Ordering::Relaxed);
    controller.ivars().now_playing.close();
    controller.ivars().status.remove();
}

/// Renders the mini player showing `view` and `volume` to a PNG at `path`, in a window that is
/// never shown. Artwork shows only if cached.
#[doc(hidden)]
pub fn mini_player_snapshot(
    view: Option<&PlayerView>,
    volume: u8,
    dark: bool,
    path: &Path,
) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let mini = MiniPlayer::new(mtm, None);
    let now = Instant::now();
    mini.show(view, now);
    mini.set_volume(volume, now);

    let root = mini.root();
    // SAFETY: framework constants.
    let name = unsafe {
        if dark {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    // SAFETY: the window is never shown, and `setReleasedWhenClosed(false)` keeps AppKit from
    // releasing it under the `Retained`.
    let window = unsafe {
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::ZERO, root.frame().size),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        );
        window.setReleasedWhenClosed(false);
        window
    };
    window.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
    window.setContentView(Some(root));
    root.layoutSubtreeIfNeeded();
    let bounds = root.bounds();
    let rep = root
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or("cannot allocate a bitmap")?;
    root.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // SAFETY: PNG encoding accepts an empty property dictionary.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("cannot encode the PNG")?;
    std::fs::write(path, png.to_vec()).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::*;

    #[test]
    fn controller_implements_its_selectors() {
        for selector in [
            sel!(menuAction:),
            sel!(statusClicked:),
            sel!(miniAction:),
            sel!(miniSeek:),
            sel!(miniVolume:),
            sel!(menuDidClose:),
            sel!(popoverWillClose:),
            sel!(popoverDidClose:),
        ] {
            assert!(Controller::class().responds_to(selector), "{selector:?}");
        }
    }
}
