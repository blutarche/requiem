//! The command palette: a scrim over the window and a centred panel of actions.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButtonRole, NSEvent, NSResponder, NSTextAlignment, NSTextField,
    NSTextFieldDelegate, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect};

use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{self, FLEX_H, FLEX_W, Field, PIN_LEFT, PIN_RIGHT, PIN_TOP, rect};
use crate::vm::PaletteAction;

const PANEL_W: CGFloat = 520.0;
const PAD: CGFloat = 8.0;
const ROW_H: CGFloat = 36.0;
const NO_MATCH: &str = "No matching actions";

define_class!(
    /// The backdrop; a click outside the panel closes the palette.
    // SAFETY: NSView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct Scrim;

    impl Scrim {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            ui::with(|ui| ui.palette.click_outside(p));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, _event: &NSEvent) {}
    }
);

pub struct RowIvars {
    index: usize,
}

define_class!(
    /// One action; a click runs it, and the pointer entering selects it.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RowIvars]
    pub struct ActionRow;

    impl ActionRow {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let i = self.ivars().index;
            ui::with(|ui| ui.palette_run(i));
        }

        /// The pointer and the arrow keys share one selection, so only one row is ever
        /// highlighted: whichever moved last.
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            let i = self.ivars().index;
            ui::with(|ui| ui.palette.select_row(i));
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            let i = self.ivars().index;
            ui::with(|ui| ui.palette_run(i));
            true
        }
    }
);

struct Row {
    action: PaletteAction,
    /// It would do something now; a disabled row is dimmed and skipped.
    enabled: Cell<bool>,
    view: Retained<ActionRow>,
    label: Retained<NSTextField>,
    keys: Retained<NSTextField>,
}

pub struct Palette {
    pub overlay: Retained<Scrim>,
    panel: Retained<NSView>,
    pub field: Field,
    rows: Vec<Row>,
    none: Retained<NSTextField>,
    shown: RefCell<Vec<usize>>,
    /// Index into `shown`; always an enabled row.
    selected: Cell<Option<usize>>,
    open: Cell<bool>,
}

impl Palette {
    pub fn new(
        mtm: MainThreadMarker,
        bounds: NSRect,
        delegate: &ProtocolObject<dyn NSTextFieldDelegate>,
    ) -> Palette {
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let overlay: Retained<Scrim> =
            unsafe { msg_send![Scrim::alloc(mtm), initWithFrame: bounds] };
        overlay.setAutoresizingMask(FLEX_W | FLEX_H);
        widgets::fill(&overlay, Some(&theme::scrim()));
        overlay.setHidden(true);

        let panel = widgets::panel(
            mtm,
            rect(0.0, 0.0, PANEL_W, 100.0),
            PIN_LEFT | PIN_RIGHT | PIN_TOP,
            &theme::bg_2(),
        );
        // `PIN_LEFT | PIN_RIGHT` together leave both side margins flexible: centred.
        widgets::round(&panel, theme::RADIUS_MODAL);
        widgets::border(&panel, &theme::line_2(), 1.0);
        overlay.addSubview(&panel);

        let field = Field::new(
            mtm,
            rect(12.0, 0.0, PANEL_W - 24.0, Field::H),
            FLEX_W,
            "Type a command…",
            Some(Icon::Search),
        );
        // SAFETY: the field's delegate is weak. It is the app delegate, which `app::run` holds
        // in a local until `app.run()`, which never returns.
        unsafe { field.text.setDelegate(Some(delegate)) };
        widgets::accessibility_label(&field.text, "Command");
        panel.addSubview(&field.ring);

        let font = theme::font(14.0, Weight::Regular);
        let short = theme::font(12.0, Weight::Medium);
        let rows = PaletteAction::ALL
            .iter()
            .enumerate()
            .map(|(index, &action)| {
                let this = ActionRow::alloc(mtm).set_ivars(RowIvars { index });
                let f = rect(PAD, 0.0, PANEL_W - 2.0 * PAD, ROW_H);
                // SAFETY: `initWithFrame:` is NSView's designated initializer.
                let view: Retained<ActionRow> = unsafe { msg_send![super(this), initWithFrame: f] };
                view.setWantsLayer(true);
                widgets::round(&view, theme::RADIUS);
                let w = f.size.width;
                let label = widgets::placed_label(
                    mtm,
                    action.label(),
                    &font,
                    &theme::ink_2(),
                    12.0,
                    ROW_H / 2.0,
                    w - 112.0,
                    FLEX_W,
                );
                let keys = widgets::placed_label(
                    mtm,
                    action.shortcut(),
                    &short,
                    &theme::muted(),
                    w - 92.0,
                    ROW_H / 2.0,
                    80.0,
                    PIN_RIGHT,
                );
                keys.setAlignment(NSTextAlignment::Right);
                view.addSubview(&label);
                view.addSubview(&keys);
                view.setAccessibilityElement(true);
                // SAFETY: the role static is an immutable framework constant.
                view.setAccessibilityRole(Some(unsafe { NSAccessibilityButtonRole }));
                view.setAccessibilityLabel(Some(&objc2_foundation::NSString::from_str(
                    action.label(),
                )));
                widgets::hide_from_accessibility(&label);
                widgets::hide_from_accessibility(&keys);
                widgets::track_hover(&view, NSTrackingAreaOptions::empty());
                panel.addSubview(&view);
                Row {
                    action,
                    enabled: Cell::new(true),
                    view,
                    label,
                    keys,
                }
            })
            .collect();
        let none = widgets::placed_label(
            mtm,
            NO_MATCH,
            &font,
            &theme::muted(),
            PAD + 12.0,
            0.0,
            PANEL_W - 2.0 * PAD - 24.0,
            FLEX_W,
        );
        panel.addSubview(&none);

        let p = Palette {
            overlay,
            panel,
            field,
            rows,
            none,
            shown: RefCell::new(Vec::new()),
            selected: Cell::new(None),
            open: Cell::new(false),
        };
        p.filter();
        p
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    /// Opens with the rows `enabled` accepts enabled.
    pub fn open(&self, enabled: impl Fn(PaletteAction) -> bool) {
        for r in &self.rows {
            r.enabled.set(enabled(r.action));
        }
        self.field.set_value("");
        self.filter();
        self.open.set(true);
        self.overlay.setHidden(false);
        if let Some(w) = self.overlay.window() {
            w.makeFirstResponder(Some(&self.field.text));
        }
        self.announce_selected();
    }

    pub fn close(&self) {
        self.open.set(false);
        self.overlay.setHidden(true);
    }

    /// Re-dims the rows for what `enabled` now accepts, keeping the selection where it can.
    pub fn set_enabled(&self, enabled: impl Fn(PaletteAction) -> bool) {
        let mut changed = false;
        for r in &self.rows {
            let e = enabled(r.action);
            changed |= r.enabled.replace(e) != e;
        }
        if !changed {
            return;
        }
        let was = self.selected_row();
        let flags = self.shown_enabled();
        let kept = self.selected.get().filter(|&i| flags[i]);
        self.selected.set(kept.or_else(|| step(&flags, None, 1)));
        self.repaint();
        if self.open.get() && self.selected_row() != was {
            self.announce_selected();
        }
    }

    /// Per shown row, whether it is enabled.
    fn shown_enabled(&self) -> Vec<bool> {
        let shown = self.shown.borrow();
        shown.iter().map(|&i| self.rows[i].enabled.get()).collect()
    }

    /// Re-filters on the field's text and lays the panel out to fit.
    pub fn filter(&self) {
        let matches = PaletteAction::filter(&self.field.value());
        let shown: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches.contains(&r.action))
            .map(|(i, _)| i)
            .collect();
        let was = self.selected_row();
        *self.shown.borrow_mut() = shown;
        self.layout();
        self.selected.set(step(&self.shown_enabled(), None, 1));
        self.repaint();
        // Closed, it is being built or opened, and `open` speaks once focus is in the field.
        if self.open.get() && self.selected_row() != was {
            self.announce_selected();
        }
    }

    /// The highlighted row's index in `rows`.
    fn selected_row(&self) -> Option<usize> {
        let shown = self.shown.borrow();
        self.selected.get().and_then(|s| shown.get(s).copied())
    }

    /// Focus stays in the field, so VoiceOver hears the highlighted row only from this.
    fn announce_selected(&self) {
        let text = match self.selected_row() {
            Some(i) => self.rows[i].action.label(),
            None if self.shown.borrow().is_empty() => NO_MATCH,
            None => return,
        };
        widgets::announce(self.panel.mtm(), text);
    }

    /// Places the panel and its shown rows for the window's current size.
    pub fn layout(&self) {
        let shown = self.shown.borrow();
        let n = shown.len().max(1);
        let list_h = n as CGFloat * ROW_H;
        let ph = 12.0 + Field::H + PAD + list_h + PAD;
        let b = self.overlay.bounds();
        self.panel.setFrame(rect(
            ((b.size.width - PANEL_W) / 2.0).round(),
            b.size.height - panel_top(b.size.height, ph) - ph,
            PANEL_W,
            ph,
        ));
        let ring = self.field.ring.frame();
        self.field.ring.setFrame(rect(
            ring.origin.x,
            ph - 12.0 - Field::H - 2.0,
            ring.size.width,
            ring.size.height,
        ));
        let list_top = ph - 12.0 - Field::H - PAD;
        for r in &self.rows {
            r.view.setHidden(true);
        }
        for (slot, &i) in shown.iter().enumerate() {
            let v = &self.rows[i].view;
            let f = v.frame();
            v.setFrame(rect(
                f.origin.x,
                list_top - (slot as CGFloat + 1.0) * ROW_H,
                f.size.width,
                ROW_H,
            ));
            v.setHidden(false);
        }
        let nf = self.none.frame();
        self.none.setFrame(rect(
            nf.origin.x,
            list_top - ROW_H / 2.0 - nf.size.height / 2.0,
            nf.size.width,
            nf.size.height,
        ));
        self.none.setHidden(!shown.is_empty());
    }

    pub fn repaint(&self) {
        let shown = self.shown.borrow();
        let sel = self.selected.get().and_then(|s| shown.get(s).copied());
        for (i, r) in self.rows.iter().enumerate() {
            // Muted keys on the accent-tinted row would fall to 3.64:1.
            let (bg, ink, keys) = if Some(i) == sel {
                (
                    Some(theme::mix(theme::BG_2, theme::ACCENT, theme::SELECTED)),
                    theme::ink(),
                    theme::ink_2(),
                )
            } else if r.enabled.get() {
                (None, theme::ink_2(), theme::muted())
            } else {
                (None, theme::muted_2(), theme::muted_2())
            };
            widgets::fill(&r.view, bg.as_deref());
            r.label.setTextColor(Some(&ink));
            r.keys.setTextColor(Some(&keys));
            r.view.setAccessibilityEnabled(r.enabled.get());
            r.view.setAccessibilitySelected(Some(i) == sel);
        }
    }

    /// Moves the selection `delta` rows, over disabled ones.
    pub fn move_selection(&self, delta: isize) {
        let next = step(&self.shown_enabled(), self.selected.get(), delta);
        if next.is_some() && self.selected.replace(next) != next {
            self.repaint();
            self.announce_selected();
        }
    }

    /// Selects the row for `PaletteAction::ALL[row]`, if it is shown and enabled.
    fn select_row(&self, row: usize) {
        if !self.rows.get(row).is_some_and(|r| r.enabled.get()) {
            return;
        }
        let slot = self.shown.borrow().iter().position(|&i| i == row);
        if slot.is_some() && self.selected.replace(slot) != slot {
            self.repaint();
        }
    }

    pub fn selected_action(&self) -> Option<PaletteAction> {
        let i = *self.shown.borrow().get(self.selected.get()?)?;
        Some(self.rows[i].action)
    }

    /// The action of row `row`, unless it is disabled.
    pub fn action(&self, row: usize) -> Option<PaletteAction> {
        self.rows
            .get(row)
            .filter(|r| r.enabled.get())
            .map(|r| r.action)
    }

    fn click_outside(&self, p: NSPoint) {
        let f = self.panel.frame();
        let inside = p.x >= f.origin.x
            && p.x < f.origin.x + f.size.width
            && p.y >= f.origin.y
            && p.y < f.origin.y + f.size.height;
        if !inside {
            ui::with(|ui| ui.close_palette());
        }
    }
}

/// The panel's distance below the window's top edge: a fifth of the window height, or less
/// when that would leave under 16 pt below a `panel_h` panel.
fn panel_top(height: CGFloat, panel_h: CGFloat) -> CGFloat {
    (height * 0.2).round().min(height - panel_h - 16.0)
}

/// The enabled slot `delta` (±1) steps from `from`, wrapping and passing over disabled ones;
/// from no selection, the first enabled slot. `None` when no slot is enabled.
fn step(enabled: &[bool], from: Option<usize>, delta: isize) -> Option<usize> {
    let Some(from) = from else {
        return enabled.iter().position(|&e| e);
    };
    let n = enabled.len() as isize;
    (1..=n)
        .map(|k| (from as isize + k * delta.signum()).rem_euclid(n) as usize)
        .find(|&i| enabled[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_fits_a_minimum_height_window() {
        let full = 12.0 + Field::H + PAD + PaletteAction::ALL.len() as CGFloat * ROW_H + PAD;
        assert_eq!(panel_top(720.0, full), 144.0);
        let top = panel_top(500.0, full);
        assert!(top >= 0.0 && top + full <= 500.0 - 16.0, "top {top}");
        assert_eq!(panel_top(500.0, 100.0), 100.0);
    }

    #[test]
    fn the_selection_skips_disabled_rows() {
        let rows = [false, true, false, true, false];
        assert_eq!(step(&rows, None, 1), Some(1));
        assert_eq!(step(&rows, Some(1), 1), Some(3));
        assert_eq!(step(&rows, Some(3), 1), Some(1));
        assert_eq!(step(&rows, Some(1), -1), Some(3));
        assert_eq!(step(&[true], Some(0), 1), Some(0));
        assert_eq!(step(&[false, false], None, 1), None);
        assert_eq!(step(&[false, false], Some(0), -1), None);
        assert_eq!(step(&[], None, 1), None);
    }
}
