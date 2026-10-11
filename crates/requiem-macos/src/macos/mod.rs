//! AppKit and MediaPlayer side. Everything here runs on the main thread.

mod now_playing;
mod status;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSMenuItem,
};
use objc2_foundation::{NSObject, NSPoint};
use requiem_core::PlayState;

use self::now_playing::NowPlaying;
use self::status::{StatusMenu, menu_action};
use crate::view::changed;
use crate::{PlayerView, UiAction};

/// Below the tracker's 2 s, so a seek it reported still applies after up to 1 s of main-queue
/// delay.
const APPLY_TOLERANCE: Duration = Duration::from_secs(1);

thread_local! {
    /// Set on the main thread while `run` is in `app.run()`.
    static CONTROLLER: RefCell<Option<Retained<Controller>>> = const { RefCell::new(None) };
}

/// Playing or about to be: the toggle pauses, and the icon shows playback.
fn is_active(state: PlayState) -> bool {
    matches!(state, PlayState::Playing | PlayState::Buffering)
}

struct Ivars {
    views: Receiver<Option<PlayerView>>,
    actions: Sender<UiAction>,
    stop: Receiver<()>,
    status: StatusMenu,
    now_playing: NowPlaying,
    shown: RefCell<Option<(PlayerView, Instant)>>,
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
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's `init` takes no arguments and returns the initialized object.
        unsafe { msg_send![super(this), init] }
    }

    /// Checks `stop`, then applies only the latest pending view.
    fn drain(&self) {
        let ivars = self.ivars();
        if !matches!(ivars.stop.try_recv(), Err(TryRecvError::Empty)) {
            self.stop_app();
            return;
        }
        if let Some(view) = ivars.views.try_iter().last() {
            self.show(view);
        }
    }

    fn show(&self, view: Option<PlayerView>) {
        let ivars = self.ivars();
        let mut shown = ivars.shown.borrow_mut();
        let now = Instant::now();
        if !changed(shown.as_ref(), view.as_ref(), now, APPLY_TOLERANCE) {
            return;
        }
        ivars.status.show(view.as_ref());
        ivars.now_playing.show(view.as_ref());
        *shown = view.map(|v| (v, now));
    }

    fn stop_app(&self) {
        self.ivars().status.cancel_tracking();
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

pub(crate) fn run(
    views: Receiver<Option<PlayerView>>,
    actions: Sender<UiAction>,
    stop: Receiver<()>,
) {
    let mtm =
        MainThreadMarker::new().expect("requiem_macos::run must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let status = StatusMenu::new(mtm, sel!(menuAction:));
    let now_playing = NowPlaying::new(actions.clone());
    let controller = Controller::new(
        mtm,
        Ivars {
            views,
            actions,
            stop,
            status,
            now_playing,
            shown: RefCell::new(None),
        },
    );
    controller.ivars().status.set_target(&controller);
    CONTROLLER.set(Some(controller.clone()));
    // Picks up anything sent before `CONTROLLER` was set. Queued rather than called, because
    // `stop:` before `run` would not end it.
    wake();

    app.run();

    CONTROLLER.set(None);
    controller.ivars().now_playing.close();
    controller.ivars().status.remove();
}

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::*;

    #[test]
    fn controller_implements_its_selectors() {
        assert!(Controller::class().responds_to(sel!(menuAction:)));
    }
}
