//! Settings' control that picks the window's appearance: System, Light or Dark.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityRadioGroupRole, NSAttributedStringNSStringDrawing, NSButton,
    NSButtonType, NSCellImagePosition, NSControl, NSControlStateValueOff, NSControlStateValueOn,
    NSEvent, NSFocusRingType, NSResponder, NSTextAlignment, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSRect, NSString};

use super::theme::{self, Weight};
use super::ui;
use super::widgets::{self, PIN_LEFT, rect};
use crate::vm::Appearance;

/// The room around the segments, inside the border.
const INSET: CGFloat = 2.0;
/// The space either side of the widest name.
const PAD: CGFloat = 16.0;
const RING_W: CGFloat = 2.0;
const FONT: CGFloat = 13.0;

pub struct ChoiceIvars {
    appearance: Appearance,
    active: Cell<bool>,
    hovered: Cell<bool>,
    focused: Cell<bool>,
}

define_class!(
    /// One choice: accent-soft while it is the chosen one. A radio button, so VoiceOver reads
    /// it as one of three, and with keyboard navigation on Tab reaches it and Space chooses it.
    // SAFETY: NSButton has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ChoiceIvars]
    struct Choice;

    impl Choice {
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hovered.set(true);
            self.paint();
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hovered.set(false);
            self.paint();
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Also called when an ancestor is hidden, which sends no `mouseExited:`.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            if self.ivars().hovered.replace(false) {
                self.paint();
            }
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: forwards to the superclass implementation.
            let ok: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if ok {
                self.ivars().focused.set(true);
                self.paint();
            }
            ok
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: forwards to the superclass implementation.
            let ok: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if ok {
                self.ivars().focused.set(false);
                self.paint();
            }
            ok
        }

        #[unsafe(method(choose:))]
        fn choose(&self, _sender: Option<&AnyObject>) {
            let appearance = self.ivars().appearance;
            ui::with(|ui| ui.choose_appearance(appearance));
        }
    }
);

impl Choice {
    fn new(mtm: MainThreadMarker, frame: NSRect, appearance: Appearance) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChoiceIvars {
            appearance,
            active: Cell::new(false),
            hovered: Cell::new(false),
            focused: Cell::new(false),
        });
        // SAFETY: `initWithFrame:` is NSButton's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setButtonType(NSButtonType::Radio);
        this.setBordered(false);
        this.setImagePosition(NSCellImagePosition::NoImage);
        this.setAlignment(NSTextAlignment::Center);
        this.setFocusRingType(NSFocusRingType::None);
        widgets::round(&this, theme::RADIUS - INSET);
        // SAFETY: the button is its own target, and implements the action.
        unsafe {
            this.setTarget(Some(&this));
            this.setAction(Some(sel!(choose:)));
        }
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this.paint();
        this
    }

    fn set_active(&self, active: bool) {
        self.setState(if active {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        if self.ivars().active.replace(active) != active {
            self.paint();
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let active = iv.active.get();
        let fill = if active {
            Some(theme::accent_soft())
        } else if iv.hovered.get() {
            Some(theme::row_hover())
        } else {
            None
        };
        widgets::fill(self, fill.as_deref());
        let ring = if iv.focused.get() { RING_W } else { 0.0 };
        widgets::border(self, &theme::accent(), ring);
        let (weight, color) = if active {
            (Weight::SemiBold, theme::ink())
        } else {
            (Weight::Medium, theme::ink_2())
        };
        let title = widgets::attributed(iv.appearance.label(), &theme::font(FONT, weight), &color);
        self.setAttributedTitle(&title);
    }
}

/// System, Light and Dark side by side, in a bordered box.
pub struct AppearanceSwitch {
    pub view: Retained<NSView>,
    choices: Vec<Retained<Choice>>,
}

impl AppearanceSwitch {
    pub const H: CGFloat = 32.0;

    pub fn new(mtm: MainThreadMarker, chosen: Appearance) -> AppearanceSwitch {
        let font = theme::font(FONT, Weight::SemiBold);
        let each = Appearance::CHOICES
            .iter()
            .map(|a| {
                widgets::attributed(a.label(), &font, &theme::ink())
                    .size()
                    .width
            })
            .fold(0.0, CGFloat::max)
            .ceil()
            + 2.0 * PAD;
        let n = Appearance::CHOICES.len() as CGFloat;
        let view = widgets::view(
            mtm,
            rect(0.0, 0.0, n * each + 2.0 * INSET, Self::H),
            PIN_LEFT,
        );
        widgets::round(&view, theme::RADIUS);
        widgets::border(&view, &theme::line_2(), 1.0);
        view.setAccessibilityElement(true);
        // SAFETY: the role static is an immutable framework constant.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityRadioGroupRole }));
        view.setAccessibilityLabel(Some(&NSString::from_str("Appearance")));
        let choices = Appearance::CHOICES
            .into_iter()
            .enumerate()
            .map(|(i, a)| {
                let frame = rect(
                    INSET + i as CGFloat * each,
                    INSET,
                    each,
                    Self::H - 2.0 * INSET,
                );
                let c = Choice::new(mtm, frame, a);
                view.addSubview(&c);
                c
            })
            .collect();
        let this = AppearanceSwitch { view, choices };
        this.set(chosen);
        this
    }

    /// Shows `chosen` as the chosen one, for VoiceOver too.
    pub fn set(&self, chosen: Appearance) {
        for c in &self.choices {
            c.set_active(c.ivars().appearance == chosen);
        }
        let value = NSString::from_str(chosen.label());
        // SAFETY: a string is a value an accessibility element can have.
        unsafe { self.view.setAccessibilityValue(Some(&value)) };
    }
}
