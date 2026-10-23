//! Chips: the pills that pick one of a few choices, on the Library and Search pages.

use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAttributedStringNSStringDrawing, NSEvent, NSResponder, NSTextField,
    NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::theme::{self, Weight};
use super::widgets::{self, FLEX_W, rect};

pub const CHIP_H: CGFloat = 28.0;
const PAD: CGFloat = 24.0;
const CHIP_GAP: CGFloat = 8.0;
/// A chip's text, and the space before and after it.
const CHIP_PAD: CGFloat = 14.0;
/// The room a text field's cell keeps on each side of its text.
const LABEL_INSET: CGFloat = 4.0;

pub struct ChipIvars {
    press: Rc<dyn Fn()>,
    selected: Cell<bool>,
    hovered: Cell<bool>,
    text: OnceCell<Retained<NSTextField>>,
}

define_class!(
    /// A button that picks one of a page's choices: a pill that is filled while it is the one
    /// in force.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ChipIvars]
    pub struct Chip;

    impl Chip {
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

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.choose();
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            self.choose();
            true
        }

        /// A plain view reports itself disabled, which VoiceOver reads as dimmed.
        #[unsafe(method(isAccessibilityEnabled))]
        fn accessibility_enabled(&self) -> bool {
            true
        }

        /// Also called when an ancestor is hidden; a hidden view gets no `mouseExited:`.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            if self.ivars().hovered.replace(false) {
                self.paint();
            }
        }

        /// Clicks on the label land on the chip itself.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let f = self.frame();
            let inside = point.x >= f.origin.x
                && point.x < f.origin.x + f.size.width
                && point.y >= f.origin.y
                && point.y < f.origin.y + f.size.height;
            inside.then(|| Retained::into_super(objc2::Message::retain(self)))
        }
    }
);

impl Chip {
    fn new(mtm: MainThreadMarker, label: &'static str, press: Rc<dyn Fn()>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChipIvars {
            press,
            selected: Cell::new(false),
            hovered: Cell::new(false),
            text: OnceCell::new(),
        });
        let w = Self::width(label);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, w, CHIP_H)] };
        widgets::round(&this, CHIP_H / 2.0);
        let font = theme::font(13.0, Weight::Medium);
        let text = widgets::placed_label(
            mtm,
            label,
            &font,
            &theme::ink_2(),
            CHIP_PAD - LABEL_INSET,
            CHIP_H / 2.0,
            w - 2.0 * (CHIP_PAD - LABEL_INSET),
            FLEX_W,
        );
        this.addSubview(&text);
        let _ = this.ivars().text.set(text);
        widgets::as_button(&this, label);
        this.setAccessibilitySelected(false);
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this.paint();
        this
    }

    /// The width of a chip labelled `label`, which the selected weight does not outgrow.
    fn width(label: &str) -> CGFloat {
        let font = theme::font(13.0, Weight::SemiBold);
        widgets::attributed(label, &font, &theme::ink())
            .size()
            .width
            .ceil()
            + 2.0 * CHIP_PAD
    }

    fn choose(&self) {
        (self.ivars().press)();
    }

    fn set_selected(&self, selected: bool) {
        self.setAccessibilitySelected(selected);
        if self.ivars().selected.replace(selected) != selected {
            self.paint();
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let selected = iv.selected.get();
        let fill = if selected {
            Some(theme::accent_soft())
        } else if iv.hovered.get() {
            Some(theme::row_hover())
        } else {
            None
        };
        widgets::fill(self, fill.as_deref());
        widgets::border(self, &theme::line_2(), if selected { 0.0 } else { 1.0 });
        if let Some(l) = iv.text.get() {
            let (weight, color) = if selected {
                (Weight::SemiBold, theme::ink())
            } else {
                (Weight::Medium, theme::ink_2())
            };
            l.setFont(Some(&theme::font(13.0, weight)));
            l.setTextColor(Some(&color));
        }
    }
}

/// A row of chips for the choices `K`, left to right, one of them selected.
pub struct ChipRow<K> {
    chips: Vec<(K, Retained<Chip>)>,
}

impl<K: Copy + PartialEq + 'static> ChipRow<K> {
    /// Adds a chip to `parent` for each of `choices`, with its label; pressing one calls
    /// `choose` with its choice.
    pub fn new(
        mtm: MainThreadMarker,
        parent: &NSView,
        choices: impl IntoIterator<Item = (K, &'static str)>,
        choose: fn(K),
    ) -> ChipRow<K> {
        let chips = choices
            .into_iter()
            .map(|(key, label)| {
                let c = Chip::new(mtm, label, Rc::new(move || choose(key)));
                parent.addSubview(&c);
                (key, c)
            })
            .collect();
        ChipRow { chips }
    }

    /// Shows the chips in `state`, `selected` or not, left to right, centred in a bar `h`
    /// high; the rest are hidden.
    pub fn set(&self, state: &[(K, bool)], h: CGFloat) {
        let mut x = PAD;
        for (key, chip) in &self.chips {
            let found = state.iter().find(|(k, _)| k == key);
            chip.setHidden(found.is_none());
            let Some(&(_, selected)) = found else {
                continue;
            };
            chip.set_selected(selected);
            let width = chip.frame().size.width;
            chip.setFrame(rect(x, ((h - CHIP_H) / 2.0).round(), width, CHIP_H));
            x += width + CHIP_GAP;
        }
    }
}
