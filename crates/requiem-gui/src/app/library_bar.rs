//! The Library's toolbar: chips for the kind of collection, a filter field and the sort.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::{NSPopUpButton, NSView};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSRect, NSString};

use super::chips::ChipRow;
use super::theme;
use super::ui;
use super::widgets::{self, FLEX_W, Field, PIN_RIGHT, PIN_TOP, rect};
use crate::vm::{LibraryChip, LibrarySort};

const PAD: CGFloat = 24.0;
const FILTER_W: CGFloat = 200.0;
const SORT_W: CGFloat = 112.0;
const SORT_H: CGFloat = 26.0;
const GAP: CGFloat = 8.0;

pub struct LibraryBar {
    pub view: Retained<NSView>,
    chips: ChipRow<LibraryChip>,
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
        let chips = ChipRow::new(
            mtm,
            &view,
            LibraryChip::CHOICES.map(|chip| (chip, chip.label())),
            |chip| {
                ui::with(|ui| ui.choose_library_chip(chip));
            },
        );

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
        self.chips.set(chips, self.view.frame().size.height);
        let index = LibrarySort::CHOICES.iter().position(|s| *s == sort);
        if let Some(i) = index
            && self.sort.indexOfSelectedItem() != i as isize
        {
            self.sort.selectItemAtIndex(i as isize);
        }
    }
}
