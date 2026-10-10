//! The command palette: a scrim over the window and a centred panel of actions.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSEvent, NSResponder, NSTextAlignment, NSTextField, NSTextFieldDelegate, NSTrackingAreaOptions,
    NSView,
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
    hovered: Cell<bool>,
}

define_class!(
    /// One action; a click runs it.
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

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hovered.set(true);
            ui::with(|ui| ui.palette.repaint());
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hovered.set(false);
            ui::with(|ui| ui.palette.repaint());
        }

        /// Also called when the palette closes. A hidden view gets no `mouseExited:`.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            self.ivars().hovered.set(false);
        }
    }
);

struct Row {
    action: PaletteAction,
    view: Retained<ActionRow>,
    label: Retained<NSTextField>,
}

pub struct Palette {
    pub overlay: Retained<Scrim>,
    panel: Retained<NSView>,
    pub field: Field,
    rows: Vec<Row>,
    none: Retained<NSTextField>,
    shown: RefCell<Vec<usize>>,
    /// Index into `shown`.
    selected: Cell<usize>,
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
        panel.addSubview(&field.ring);

        let font = theme::font(14.0, Weight::Regular);
        let short = theme::font(12.0, Weight::Medium);
        let rows = PaletteAction::ALL
            .iter()
            .enumerate()
            .map(|(index, &action)| {
                let this = ActionRow::alloc(mtm).set_ivars(RowIvars {
                    index,
                    hovered: Cell::new(false),
                });
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
                    &theme::muted_2(),
                    w - 92.0,
                    ROW_H / 2.0,
                    80.0,
                    PIN_RIGHT,
                );
                keys.setAlignment(NSTextAlignment::Right);
                view.addSubview(&label);
                view.addSubview(&keys);
                widgets::track_hover(&view, NSTrackingAreaOptions::empty());
                panel.addSubview(&view);
                Row {
                    action,
                    view,
                    label,
                }
            })
            .collect();
        let none = widgets::placed_label(
            mtm,
            "No matching actions",
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
            selected: Cell::new(0),
            open: Cell::new(false),
        };
        p.filter();
        p
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    pub fn open(&self) {
        self.field.set_value("");
        self.filter();
        self.open.set(true);
        self.overlay.setHidden(false);
        if let Some(w) = self.overlay.window() {
            w.makeFirstResponder(Some(&self.field.text));
        }
    }

    pub fn close(&self) {
        self.open.set(false);
        self.overlay.setHidden(true);
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
        let n = shown.len().max(1);
        let list_h = n as CGFloat * ROW_H;
        let ph = 12.0 + Field::H + PAD + list_h + PAD;
        let b = self.overlay.bounds();
        let top = (b.size.height * 0.2).round();
        self.panel.setFrame(rect(
            ((b.size.width - PANEL_W) / 2.0).round(),
            b.size.height - top - ph,
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
        *self.shown.borrow_mut() = shown;
        self.selected.set(0);
        self.repaint();
    }

    pub fn repaint(&self) {
        let shown = self.shown.borrow();
        let sel = shown.get(self.selected.get()).copied();
        for (i, r) in self.rows.iter().enumerate() {
            let (bg, ink) = if Some(i) == sel {
                (
                    Some(theme::mix(theme::BG_2, theme::ACCENT, theme::SELECTED)),
                    theme::ink(),
                )
            } else if r.view.ivars().hovered.get() {
                (Some(theme::bg_3()), theme::ink_2())
            } else {
                (None, theme::ink_2())
            };
            widgets::fill(&r.view, bg.as_deref());
            r.label.setTextColor(Some(&ink));
        }
    }

    pub fn move_selection(&self, delta: isize) {
        let n = self.shown.borrow().len();
        if n == 0 {
            return;
        }
        let i = (self.selected.get() as isize + delta).rem_euclid(n as isize);
        self.selected.set(i as usize);
        self.repaint();
    }

    pub fn selected_action(&self) -> Option<PaletteAction> {
        let i = *self.shown.borrow().get(self.selected.get())?;
        Some(self.rows[i].action)
    }

    pub fn action(&self, row: usize) -> Option<PaletteAction> {
        self.rows.get(row).map(|r| r.action)
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
