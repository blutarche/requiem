//! The Library's toolbar: chips for the kind of collection, a filter field and the sort.

use std::cell::{Cell, OnceCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAttributedStringNSStringDrawing, NSEvent, NSPopUpButton, NSResponder,
    NSTextField, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSString};

use super::theme::{self, Weight};
use super::ui;
use super::widgets::{self, FLEX_W, Field, PIN_RIGHT, PIN_TOP, rect};
use crate::vm::{LibraryChip, LibrarySort};

const PAD: CGFloat = 24.0;
const CHIP_H: CGFloat = 28.0;
const CHIP_GAP: CGFloat = 8.0;
/// A chip's text, and the space before and after it.
const CHIP_PAD: CGFloat = 14.0;
/// The room a text field's cell keeps on each side of its text.
const LABEL_INSET: CGFloat = 4.0;
const FILTER_W: CGFloat = 200.0;
const SORT_W: CGFloat = 112.0;
const SORT_H: CGFloat = 26.0;
const GAP: CGFloat = 8.0;

pub struct ChipIvars {
    chip: LibraryChip,
    selected: Cell<bool>,
    hovered: Cell<bool>,
    label: OnceCell<Retained<NSTextField>>,
}

define_class!(
    /// A button that keeps one kind of collection on the Library page: a pill that is filled
    /// while it is the one in force.
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
    fn new(mtm: MainThreadMarker, chip: LibraryChip) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChipIvars {
            chip,
            selected: Cell::new(false),
            hovered: Cell::new(false),
            label: OnceCell::new(),
        });
        let w = Self::width(chip);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, w, CHIP_H)] };
        widgets::round(&this, CHIP_H / 2.0);
        let font = theme::font(13.0, Weight::Medium);
        let label = widgets::placed_label(
            mtm,
            chip.label(),
            &font,
            &theme::ink_2(),
            CHIP_PAD - LABEL_INSET,
            CHIP_H / 2.0,
            w - 2.0 * (CHIP_PAD - LABEL_INSET),
            FLEX_W,
        );
        this.addSubview(&label);
        let _ = this.ivars().label.set(label);
        widgets::as_button(&this, chip.label());
        this.setAccessibilitySelected(false);
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this.paint();
        this
    }

    /// The width of `chip`'s label, which the selected weight does not outgrow.
    fn width(chip: LibraryChip) -> CGFloat {
        let font = theme::font(13.0, Weight::SemiBold);
        widgets::attributed(chip.label(), &font, &theme::ink())
            .size()
            .width
            .ceil()
            + 2.0 * CHIP_PAD
    }

    fn choose(&self) {
        let chip = self.ivars().chip;
        ui::with(|ui| ui.choose_library_chip(chip));
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
        if let Some(l) = iv.label.get() {
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

pub struct LibraryBar {
    pub view: Retained<NSView>,
    chips: Vec<Retained<Chip>>,
    pub filter: Field,
    sort: Retained<NSPopUpButton>,
}

impl LibraryBar {
    pub const H: CGFloat = Field::H + 4.0;

    /// `frame` is the bar's place on its page; `target` is told `librarySort:` when the sort
    /// changes.
    pub fn new(mtm: MainThreadMarker, frame: NSRect, target: &AnyObject) -> LibraryBar {
        let (w, h) = (frame.size.width, frame.size.height);
        let view = widgets::view(mtm, frame, FLEX_W | PIN_TOP);
        let chips: Vec<Retained<Chip>> = LibraryChip::CHOICES
            .into_iter()
            .map(|chip| {
                let c = Chip::new(mtm, chip);
                view.addSubview(&c);
                c
            })
            .collect();

        let sort_x = w - PAD - SORT_W;
        let sort = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            rect(sort_x, ((h - SORT_H) / 2.0).round(), SORT_W, SORT_H),
            false,
        );
        for s in LibrarySort::CHOICES {
            sort.addItemWithTitle(&NSString::from_str(s.label()));
        }
        sort.setAutoresizingMask(PIN_RIGHT);
        // SAFETY: the target implements `librarySort:` and is the app delegate, which
        // `app::run` holds until `app.run()`, which never returns.
        unsafe {
            sort.setTarget(Some(target));
            sort.setAction(Some(sel!(librarySort:)));
        }
        widgets::accessibility_label(&sort, "Sort");
        view.addSubview(&sort);

        let filter = Field::new(
            mtm,
            rect(
                sort_x - GAP - FILTER_W,
                ((h - Field::H) / 2.0).round(),
                FILTER_W,
                Field::H,
            ),
            PIN_RIGHT,
            "Filter",
            Some(theme::Icon::Search),
        );
        widgets::accessibility_label(&filter.text, "Filter");
        view.addSubview(&filter.ring);

        LibraryBar {
            view,
            chips,
            filter,
            sort,
        }
    }

    /// Shows the chips that have something to keep, `selected` or not, left to right, and
    /// the sort in force.
    pub fn set(&self, chips: &[(LibraryChip, bool)], sort: LibrarySort) {
        let h = self.view.frame().size.height;
        let mut x = PAD;
        for chip in &self.chips {
            let state = chips.iter().find(|(c, _)| *c == chip.ivars().chip);
            chip.setHidden(state.is_none());
            let Some(&(_, selected)) = state else {
                continue;
            };
            chip.set_selected(selected);
            let width = chip.frame().size.width;
            chip.setFrame(rect(x, ((h - CHIP_H) / 2.0).round(), width, CHIP_H));
            x += width + CHIP_GAP;
        }
        let index = LibrarySort::CHOICES.iter().position(|s| *s == sort);
        if let Some(i) = index
            && self.sort.indexOfSelectedItem() != i as isize
        {
            self.sort.selectItemAtIndex(i as isize);
        }
    }
}
