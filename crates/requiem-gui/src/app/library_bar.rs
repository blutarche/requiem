//! The Library's toolbar: chips for the kind of collection, a filter field and the sort.

use std::cell::RefCell;

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
/// The narrowest the filter gets beside the chips before it moves under them.
const FILTER_MIN_W: CGFloat = 160.0;
const SORT_W: CGFloat = 112.0;
const GAP: CGFloat = 8.0;
/// The room kept between the chips and the filter.
const CHIPS_GAP: CGFloat = 16.0;

/// Where the filter and the sort go in a bar `width` wide.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    /// The filter and the sort sit on a row of their own under the chips.
    stacked: bool,
    filter_x: CGFloat,
    filter_w: CGFloat,
    sort_x: CGFloat,
}

/// The sort stays at the right edge. The filter shrinks to `FILTER_MIN_W` to keep clear of the
/// chips, which end at `chips_end`, and fills the row under them when that is not enough.
fn layout(width: CGFloat, chips_end: CGFloat) -> Layout {
    let sort_x = width - PAD - SORT_W;
    let filter_right = sort_x - GAP;
    let room = filter_right - chips_end - CHIPS_GAP;
    if room >= FILTER_MIN_W {
        let filter_w = room.min(FILTER_W);
        Layout {
            stacked: false,
            filter_x: filter_right - filter_w,
            filter_w,
            sort_x,
        }
    } else {
        Layout {
            stacked: true,
            filter_x: PAD,
            filter_w: (filter_right - PAD).max(0.0),
            sort_x,
        }
    }
}

pub struct LibraryBar {
    pub view: Retained<NSView>,
    chips: ChipRow<LibraryChip>,
    shown: RefCell<Vec<(LibraryChip, bool)>>,
    pub filter: Field,
    sort: Retained<NSPopUpButton>,
    /// The popup's height, which the filter field shares.
    control_h: CGFloat,
}

impl LibraryBar {
    pub const H: CGFloat = Field::H + 4.0;

    /// `frame` is the bar's place on its page, one row high; `target` is told `librarySort:`
    /// when the sort changes.
    pub fn new(mtm: MainThreadMarker, frame: NSRect, target: &AnyObject) -> LibraryBar {
        let view = widgets::view(mtm, frame, FLEX_W | PIN_TOP);
        let chips = ChipRow::new(
            mtm,
            &view,
            LibraryChip::CHOICES.map(|chip| (chip, chip.label())),
            |chip| {
                ui::with(|ui| ui.choose_library_chip(chip));
            },
        );

        let sort =
            NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::ZERO, false);
        let control_h = sort.intrinsicContentSize().height;
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
            rect(0.0, 0.0, FILTER_W, control_h),
            PIN_RIGHT,
            "Filter",
            Some(theme::Icon::Search),
        );
        widgets::accessibility_label(&filter.text, "Filter");
        view.addSubview(&filter.ring);

        let bar = LibraryBar {
            view,
            chips,
            shown: RefCell::new(Vec::new()),
            filter,
            sort,
            control_h,
        };
        bar.fit(frame.size.width, frame.origin.y + frame.size.height);
        bar
    }

    /// Shows the chips that have something to keep, `selected` or not, left to right, and
    /// the sort in force. `fit` places them.
    pub fn set(&self, chips: &[(LibraryChip, bool)], sort: LibrarySort) {
        *self.shown.borrow_mut() = chips.to_vec();
        let index = LibrarySort::CHOICES.iter().position(|s| *s == sort);
        if let Some(i) = index
            && self.sort.indexOfSelectedItem() != i as isize
        {
            self.sort.selectItemAtIndex(i as isize);
        }
    }

    /// Lays the bar out `width` wide with its top at `top` on its page, one row high or two
    /// when the filter and the sort do not fit beside the chips; returns the bar's bottom.
    pub fn fit(&self, width: CGFloat, top: CGFloat) -> CGFloat {
        let chips_end = self.chips.end(&self.shown.borrow());
        let l = layout(width, chips_end);
        let h = if l.stacked { 2.0 * Self::H } else { Self::H };
        self.view.setFrame(rect(0.0, top - h, width, h));
        // The chips' row is the top one; the filter and the sort take the bottom one.
        self.chips.set(&self.shown.borrow(), 2.0 * h - Self::H);
        let y = (Self::H / 2.0 - self.control_h / 2.0).round();
        self.sort
            .setFrame(rect(l.sort_x, y, SORT_W, self.control_h));
        let ring = rect(l.filter_x, y, l.filter_w, self.control_h);
        self.filter.ring.setFrame(rect(
            ring.origin.x - Field::RING,
            ring.origin.y - Field::RING,
            ring.size.width + 2.0 * Field::RING,
            ring.size.height + 2.0 * Field::RING,
        ));
        top - h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar's narrowest width: the window's minimum less the sidebar.
    const MIN_BAR_W: CGFloat = 560.0;

    #[test]
    fn the_filter_and_sort_never_overlap_the_chips_or_each_other() {
        for chips_end in [PAD, 160.0, 292.0, 340.0] {
            for width in (MIN_BAR_W as u32..2400).map(CGFloat::from) {
                let l = layout(width, chips_end);
                let filter_right = l.filter_x + l.filter_w;
                assert!(filter_right + GAP <= l.sort_x, "{width} {chips_end}");
                assert!(l.sort_x + SORT_W + PAD <= width, "{width} {chips_end}");
                assert!(l.filter_x >= PAD, "{width} {chips_end}");
                if !l.stacked {
                    assert!(l.filter_x >= chips_end + CHIPS_GAP, "{width} {chips_end}");
                    assert!(l.filter_w >= FILTER_MIN_W, "{width} {chips_end}");
                }
            }
        }
    }

    #[test]
    fn the_filter_shrinks_before_it_moves_under_the_chips() {
        let chips_end = 292.0;
        let wide = layout(1200.0, chips_end);
        assert!(!wide.stacked);
        assert_eq!(wide.filter_w, FILTER_W);
        // Just wide enough for the narrowest filter beside the chips.
        let tight = chips_end + CHIPS_GAP + FILTER_MIN_W + GAP + SORT_W + PAD;
        let l = layout(tight, chips_end);
        assert!(!l.stacked);
        assert_eq!(l.filter_w, FILTER_MIN_W);
        let l = layout(tight - 1.0, chips_end);
        assert!(l.stacked);
        assert_eq!(l.filter_x, PAD);
    }

    #[test]
    fn the_window_minimum_stacks_the_bar_of_four_chips() {
        assert!(layout(MIN_BAR_W, 292.0).stacked);
    }
}
