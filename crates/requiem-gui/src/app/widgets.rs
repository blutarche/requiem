//! Small views. Backgrounds, borders and radii are layer properties rather than `drawRect:`
//! so they cost no backing store; only the seek bar draws.

use std::cell::{Cell, OnceCell};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSAutoresizingMaskOptions, NSBezierPath, NSButton,
    NSCellImagePosition, NSColor, NSControl, NSEvent, NSEventMask, NSEventTrackingRunLoopMode,
    NSEventType, NSFocusRingType, NSFont, NSImageScaling, NSImageView, NSLineBreakMode,
    NSResponder, NSTextAlignment, NSTextField, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDate, NSDictionary, NSNotification, NSPoint, NSRect,
    NSSize, NSString,
};

use super::theme::{self, Icon, Weight};
use super::ui::{self, AppView};

pub fn rect(x: CGFloat, y: CGFloat, w: CGFloat, h: CGFloat) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

pub type Mask = NSAutoresizingMaskOptions;
pub const FLEX_W: Mask = NSAutoresizingMaskOptions::ViewWidthSizable;
pub const FLEX_H: Mask = NSAutoresizingMaskOptions::ViewHeightSizable;
/// Keeps the distance to the superview's top edge.
pub const PIN_TOP: Mask = NSAutoresizingMaskOptions::ViewMinYMargin;
/// Keeps the distance to the superview's right edge.
pub const PIN_RIGHT: Mask = NSAutoresizingMaskOptions::ViewMinXMargin;
/// Keeps the distance to the superview's bottom edge.
pub const PIN_BOTTOM: Mask = NSAutoresizingMaskOptions::ViewMaxYMargin;
/// Keeps the distance to the superview's left edge.
pub const PIN_LEFT: Mask = NSAutoresizingMaskOptions::ViewMaxXMargin;

pub fn view(mtm: MainThreadMarker, frame: NSRect, mask: Mask) -> Retained<NSView> {
    let v = NSView::initWithFrame(NSView::alloc(mtm), frame);
    v.setAutoresizingMask(mask);
    v.setWantsLayer(true);
    v
}

/// A view filled with `color`.
pub fn panel(
    mtm: MainThreadMarker,
    frame: NSRect,
    mask: Mask,
    color: &NSColor,
) -> Retained<NSView> {
    let v = view(mtm, frame, mask);
    fill(&v, Some(color));
    v
}

pub fn fill(v: &NSView, color: Option<&NSColor>) {
    v.setWantsLayer(true);
    if let Some(layer) = v.layer() {
        layer.setBackgroundColor(color.map(|c| c.CGColor()).as_deref());
    }
}

pub fn round(v: &NSView, radius: CGFloat) {
    v.setWantsLayer(true);
    if let Some(layer) = v.layer() {
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(true);
    }
}

pub fn border(v: &NSView, color: &NSColor, width: CGFloat) {
    v.setWantsLayer(true);
    if let Some(layer) = v.layer() {
        layer.setBorderColor(Some(&color.CGColor()));
        layer.setBorderWidth(width);
    }
}

pub fn label(
    mtm: MainThreadMarker,
    text: &str,
    font: &NSFont,
    color: &NSColor,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(font));
    l.setTextColor(Some(color));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l.setUsesSingleLineMode(true);
    l
}

/// A label of `size` pt type, `width` wide, placed with its text box vertically centred on
/// `mid_y`.
#[allow(clippy::too_many_arguments)]
pub fn placed_label(
    mtm: MainThreadMarker,
    text: &str,
    font: &NSFont,
    color: &NSColor,
    x: CGFloat,
    mid_y: CGFloat,
    width: CGFloat,
    mask: Mask,
) -> Retained<NSTextField> {
    let l = label(mtm, text, font, color);
    let h = line_height(font);
    l.setFrame(rect(x, (mid_y - h / 2.0).round(), width, h));
    l.setAutoresizingMask(mask);
    l
}

/// Height an `NSTextField` label needs for one line of `font`.
pub fn line_height(font: &NSFont) -> CGFloat {
    (font.ascender() - font.descender() + font.leading()).ceil() + 2.0
}

pub fn set_text(l: &NSTextField, text: &str) {
    if l.stringValue().to_string() != text {
        l.setStringValue(&NSString::from_str(text));
    }
}

pub fn icon_view(
    mtm: MainThreadMarker,
    icon: Icon,
    size: CGFloat,
    tint: &NSColor,
    frame: NSRect,
) -> Retained<NSImageView> {
    let v = NSImageView::initWithFrame(NSImageView::alloc(mtm), frame);
    v.setImage(icon.image(size).as_deref());
    v.setImageScaling(NSImageScaling::ScaleNone);
    v.setContentTintColor(Some(tint));
    v
}

/// Attributes for drawn or attributed text.
pub fn attrs(
    font: &NSFont,
    color: &NSColor,
    kern: Option<CGFloat>,
) -> Retained<NSDictionary<NSString, AnyObject>> {
    use objc2_app_kit::{NSFontAttributeName, NSForegroundColorAttributeName, NSKernAttributeName};
    use objc2_foundation::NSNumber;
    let mut keys: Vec<&NSString> = Vec::with_capacity(3);
    let mut vals: Vec<Retained<AnyObject>> = Vec::with_capacity(3);
    // SAFETY: the attribute-name statics are immutable framework constants.
    unsafe {
        keys.push(NSFontAttributeName);
        vals.push(Retained::into_super(Retained::into_super(font.retain())));
        keys.push(NSForegroundColorAttributeName);
        vals.push(Retained::into_super(Retained::into_super(color.retain())));
        if let Some(k) = kern {
            keys.push(NSKernAttributeName);
            vals.push(Retained::into_super(Retained::into_super(
                Retained::into_super(NSNumber::new_f64(k)),
            )));
        }
    }
    NSDictionary::from_retained_objects(&keys, &vals)
}

pub fn attributed(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    // SAFETY: `attrs` holds a font and a colour under their matching keys.
    unsafe {
        NSAttributedString::new_with_attributes(
            &NSString::from_str(text),
            &attrs(font, color, None),
        )
    }
}

pub fn track_hover(v: &NSView, extra: NSTrackingAreaOptions) {
    let opts = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::ActiveInActiveApp
        | NSTrackingAreaOptions::InVisibleRect
        | extra;
    // SAFETY: the owner is the view itself, which outlives its own tracking area.
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            opts,
            Some(v),
            None,
        )
    };
    v.addTrackingArea(&area);
}

pub struct ButtonIvars {
    normal: Option<Retained<NSColor>>,
    hover: Retained<NSColor>,
    hovered: Cell<bool>,
}

define_class!(
    /// A borderless icon or text button with a hover background.
    // SAFETY: NSButton has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ButtonIvars]
    pub struct HoverButton;

    impl HoverButton {
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

        /// Also called when an ancestor is hidden. A hidden view gets no `mouseExited:`, so
        /// without this the hover background would come back with the view.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            self.clear_hover();
        }
    }
);

impl HoverButton {
    /// `normal` is the resting background (none for ghost buttons).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        icon: Icon,
        icon_size: CGFloat,
        tint: &NSColor,
        normal: Option<&NSColor>,
        hover: &NSColor,
        radius: CGFloat,
    ) -> Retained<Self> {
        let this = Self::filled(mtm, frame, normal, hover, radius);
        this.setTitle(&NSString::new());
        this.setImagePosition(NSCellImagePosition::ImageOnly);
        this.set_icon(icon, icon_size);
        this.setContentTintColor(Some(tint));
        this
    }

    /// A primary text button: accent fill, ink title. `set_label` sizes its width.
    pub fn primary(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::filled(
            mtm,
            frame,
            Some(&theme::accent()),
            &theme::accent_hi(),
            theme::RADIUS,
        );
        this.setImagePosition(NSCellImagePosition::NoImage);
        this
    }

    fn filled(
        mtm: MainThreadMarker,
        frame: NSRect,
        normal: Option<&NSColor>,
        hover: &NSColor,
        radius: CGFloat,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ButtonIvars {
            normal: normal.map(|c| c.retain()),
            hover: hover.retain(),
            hovered: Cell::new(false),
        });
        // SAFETY: `initWithFrame:` is NSButton's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setBordered(false);
        this.setFocusRingType(NSFocusRingType::None);
        round(&this, radius);
        this.paint();
        track_hover(&this, NSTrackingAreaOptions::empty());
        this
    }

    /// Sets a text button's title and returns the width it needs: the text plus 12 px a side.
    pub fn set_label(&self, title: &str) -> CGFloat {
        let s = attributed(title, &theme::font(14.0, Weight::Medium), &theme::ink());
        self.setAttributedTitle(&s);
        s.size().width.ceil() + 24.0
    }

    pub fn set_icon(&self, icon: Icon, size: CGFloat) {
        self.setImage(icon.image(size).as_deref());
    }

    /// Shows or hides the button; hiding also drops its hover state.
    pub fn set_shown(&self, shown: bool) {
        if !shown {
            self.clear_hover();
        }
        self.setHidden(!shown);
    }

    fn clear_hover(&self) {
        if self.ivars().hovered.replace(false) {
            self.paint();
        }
    }

    pub fn set_action(&self, target: &AnyObject, action: Sel) {
        // SAFETY: the target implements `action` and outlives the button: it is either the
        // app delegate, which `app::run` holds until `app.run()`, which never returns, or a
        // table controller, which its `Table` holds for as long as the installed `Ui` lives:
        // until exit.
        unsafe {
            self.setTarget(Some(target));
            self.setAction(Some(action));
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let c = if iv.hovered.get() {
            Some(&*iv.hover)
        } else {
            iv.normal.as_deref()
        };
        fill(self, c);
    }
}

pub struct NavIvars {
    target: AppView,
    active: Cell<bool>,
    hovered: Cell<bool>,
    icon: OnceCell<Retained<NSImageView>>,
    label: OnceCell<Retained<NSTextField>>,
}

define_class!(
    /// A sidebar entry: icon and label, accent-soft when active.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavIvars]
    pub struct NavItem;

    impl NavItem {
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
            let target = self.ivars().target;
            ui::with(|ui| ui.show(target));
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Clicks on the icon or label land on the item itself.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let f = self.frame();
            let inside = point.x >= f.origin.x
                && point.x < f.origin.x + f.size.width
                && point.y >= f.origin.y
                && point.y < f.origin.y + f.size.height;
            inside.then(|| Retained::into_super(self.retain()))
        }
    }
);

impl NavItem {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        target: AppView,
        icon: Icon,
        text: &str,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavIvars {
            target,
            active: Cell::new(false),
            hovered: Cell::new(false),
            icon: OnceCell::new(),
            label: OnceCell::new(),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setAutoresizingMask(PIN_TOP);
        round(&this, theme::RADIUS);
        let h = frame.size.height;
        let icon = icon_view(
            mtm,
            icon,
            18.0,
            &theme::chrome_muted(),
            rect(12.0, (h - 18.0) / 2.0, 18.0, 18.0),
        );
        let font = theme::font(14.0, Weight::Medium);
        let label = placed_label(
            mtm,
            text,
            &font,
            &theme::chrome_ink(),
            40.0,
            h / 2.0,
            frame.size.width - 52.0,
            FLEX_W,
        );
        this.addSubview(&icon);
        this.addSubview(&label);
        let _ = this.ivars().icon.set(icon);
        let _ = this.ivars().label.set(label);
        track_hover(&this, NSTrackingAreaOptions::empty());
        this.paint();
        this
    }

    pub fn set_active(&self, active: bool) {
        if self.ivars().active.replace(active) != active {
            self.paint();
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let active = iv.active.get();
        let bg = if active {
            Some(theme::accent_soft())
        } else if iv.hovered.get() {
            Some(theme::chrome_bg_hover())
        } else {
            None
        };
        fill(self, bg.as_deref());
        if let Some(l) = iv.label.get() {
            let (weight, color) = if active {
                (Weight::SemiBold, theme::ink())
            } else {
                (Weight::Medium, theme::chrome_ink())
            };
            l.setFont(Some(&theme::font(14.0, weight)));
            l.setTextColor(Some(&color));
        }
        if let Some(i) = iv.icon.get() {
            let tint = if active {
                theme::accent()
            } else {
                theme::chrome_muted()
            };
            i.setContentTintColor(Some(&tint));
        }
    }
}

pub struct FieldIvars {
    ring: OnceCell<Weak<NSView>>,
    boxv: OnceCell<Weak<NSView>>,
    focused: Cell<bool>,
}

define_class!(
    /// The editable part of a `Field`; it repaints the field's border and ring on focus
    /// changes.
    // SAFETY: NSTextField has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSTextField, NSControl, NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = FieldIvars]
    pub struct FieldText;

    impl FieldText {
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: forwards to the superclass implementation.
            let ok: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if ok {
                self.set_focused(true);
            }
            ok
        }

        #[unsafe(method(textDidEndEditing:))]
        fn text_did_end_editing(&self, notification: &NSNotification) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), textDidEndEditing: notification] };
            self.set_focused(false);
        }
    }
);

impl FieldText {
    fn set_focused(&self, focused: bool) {
        let iv = self.ivars();
        if iv.focused.replace(focused) == focused {
            return;
        }
        if let Some(ring) = iv.ring.get().and_then(Weak::load) {
            let c = focused.then(theme::accent_ring);
            fill(&ring, c.as_deref());
        }
        if let Some(b) = iv.boxv.get().and_then(Weak::load) {
            let line = if focused {
                theme::accent()
            } else {
                theme::line()
            };
            border(&b, &line, 1.0);
        }
    }
}

/// A text input: bg-4 fill, 1 px line, radius 6; on focus an accent border and a 2 px
/// accent ring at 25%.
pub struct Field {
    /// The outer view, 2 pt larger than the box on every side.
    pub ring: Retained<NSView>,
    pub text: Retained<FieldText>,
}

impl Field {
    pub const H: CGFloat = 36.0;
    const RING: CGFloat = 2.0;

    /// `frame` is the box's frame in the superview.
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        mask: Mask,
        placeholder: &str,
        icon: Option<Icon>,
    ) -> Field {
        let r = Self::RING;
        let ring = view(
            mtm,
            rect(
                frame.origin.x - r,
                frame.origin.y - r,
                frame.size.width + 2.0 * r,
                frame.size.height + 2.0 * r,
            ),
            mask,
        );
        if let Some(l) = ring.layer() {
            l.setCornerRadius(theme::RADIUS + r);
        }
        let (w, h) = (frame.size.width, frame.size.height);
        let boxv = view(mtm, rect(r, r, w, h), FLEX_W | FLEX_H);
        fill(&boxv, Some(&theme::bg_4()));
        if let Some(l) = boxv.layer() {
            l.setCornerRadius(theme::RADIUS);
        }
        border(&boxv, &theme::line(), 1.0);
        ring.addSubview(&boxv);

        let mut x = 12.0;
        if let Some(icon) = icon {
            let iv = icon_view(
                mtm,
                icon,
                16.0,
                &theme::muted(),
                rect(12.0, (h - 16.0) / 2.0, 16.0, 16.0),
            );
            boxv.addSubview(&iv);
            x = 36.0;
        }
        let font = theme::font(14.0, Weight::Regular);
        let text = FieldText::alloc(mtm).set_ivars(FieldIvars {
            ring: OnceCell::new(),
            boxv: OnceCell::new(),
            focused: Cell::new(false),
        });
        let th = line_height(&font);
        let tframe = rect(x, ((h - th) / 2.0).round(), w - x - 12.0, th);
        // SAFETY: `initWithFrame:` is NSTextField's designated initializer.
        let text: Retained<FieldText> = unsafe { msg_send![super(text), initWithFrame: tframe] };
        let _ = text.ivars().ring.set(Weak::from_retained(&ring));
        let _ = text.ivars().boxv.set(Weak::from_retained(&boxv));
        text.setBezeled(false);
        text.setBordered(false);
        text.setDrawsBackground(false);
        text.setEditable(true);
        text.setSelectable(true);
        text.setFocusRingType(NSFocusRingType::None);
        text.setFont(Some(&font));
        text.setTextColor(Some(&theme::ink()));
        text.setUsesSingleLineMode(true);
        text.setLineBreakMode(NSLineBreakMode::ByClipping);
        if let Some(cell) = text.cell() {
            cell.setScrollable(true);
            cell.setWraps(false);
        }
        text.setPlaceholderAttributedString(Some(&attributed(
            placeholder,
            &font,
            &theme::muted_2(),
        )));
        text.setAutoresizingMask(FLEX_W);
        boxv.addSubview(&text);
        Field { ring, text }
    }

    pub fn is(&self, control: &NSControl) -> bool {
        std::ptr::eq::<NSControl>(control, &***self.text)
    }

    pub fn value(&self) -> String {
        self.text.stringValue().to_string()
    }

    pub fn set_value(&self, v: &str) {
        self.text.setStringValue(&NSString::from_str(v));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliderKind {
    /// Seeks on mouse-up.
    Seek,
    /// Sets the volume while dragging.
    Volume,
}

pub struct SliderIvars {
    kind: SliderKind,
    value: Cell<f64>,
    dragging: Cell<bool>,
    hovered: Cell<bool>,
    enabled: Cell<bool>,
}

define_class!(
    /// A thin custom-drawn slider: line-2 track, accent-2 fill, knob on hover or drag.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SliderIvars]
    pub struct Slider;

    impl Slider {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let (x0, w, mid) = self.track();
            let v = self.ivars().value.get();
            theme::line_2().setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(x0, mid - 2.0, w, 4.0), 2.0, 2.0)
                .fill();
            let fill = theme::accent_2();
            if v > 0.0 {
                fill.setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    rect(x0, mid - 2.0, (w * v).max(4.0), 4.0),
                    2.0,
                    2.0,
                )
                .fill();
            }
            let iv = self.ivars();
            if iv.enabled.get() && (iv.hovered.get() || iv.dragging.get()) {
                fill.setFill();
                NSBezierPath::bezierPathWithOvalInRect(rect(x0 + w * v - 6.0, mid - 6.0, 12.0, 12.0))
                    .fill();
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.ivars().enabled.get() {
                self.drag(event);
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hovered.set(true);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hovered.set(false);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl Slider {
    const KNOB_R: CGFloat = 6.0;

    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        mask: Mask,
        kind: SliderKind,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SliderIvars {
            kind,
            value: Cell::new(0.0),
            dragging: Cell::new(false),
            hovered: Cell::new(false),
            enabled: Cell::new(true),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setAutoresizingMask(mask);
        track_hover(&this, NSTrackingAreaOptions::EnabledDuringMouseDrag);
        this
    }

    /// Ignored mid-drag. Redraws only when the fill moves by at least half a point.
    pub fn set_value(&self, v: f64) {
        let iv = self.ivars();
        if iv.dragging.get() {
            return;
        }
        let v = v.clamp(0.0, 1.0);
        let (_, w, _) = self.track();
        let old = iv.value.replace(v);
        if ((old - v) * w).abs() >= 0.5 || (v == 0.0) != (old == 0.0) {
            self.setNeedsDisplay(true);
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        if self.ivars().enabled.replace(enabled) != enabled {
            self.setNeedsDisplay(true);
        }
    }

    /// Track start x, width and centre y.
    fn track(&self) -> (CGFloat, CGFloat, CGFloat) {
        let b = self.bounds();
        let r = Self::KNOB_R;
        (
            b.origin.x + r,
            (b.size.width - 2.0 * r).max(1.0),
            (b.origin.y + b.size.height / 2.0).round(),
        )
    }

    /// Follows the drag to its mouse-up in a local event loop, as AppKit's own controls do, so
    /// `dragging` cannot outlive the gesture even if the window loses focus mid-drag.
    fn drag(&self, down: &NSEvent) {
        self.ivars().dragging.set(true);
        self.track_mouse(down, false);
        if let Some(window) = self.window() {
            let mask = NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp;
            let forever = NSDate::distantFuture();
            // SAFETY: the run-loop-mode static is an immutable framework constant.
            let mode = unsafe { NSEventTrackingRunLoopMode };
            while let Some(ev) = window.nextEventMatchingMask_untilDate_inMode_dequeue(
                mask,
                Some(&forever),
                mode,
                true,
            ) {
                let up = ev.r#type() == NSEventType::LeftMouseUp;
                self.track_mouse(&ev, up);
                if up {
                    break;
                }
            }
        }
        self.ivars().dragging.set(false);
        self.setNeedsDisplay(true);
    }

    fn track_mouse(&self, event: &NSEvent, done: bool) {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        let (x0, w, _) = self.track();
        let v = ((p.x - x0) / w).clamp(0.0, 1.0);
        self.ivars().value.set(v);
        self.setNeedsDisplay(true);
        let kind = self.ivars().kind;
        match kind {
            SliderKind::Seek if done => {
                ui::with(|ui| ui.seek_fraction(v));
            }
            SliderKind::Seek => {}
            SliderKind::Volume => {
                ui::with(|ui| ui.set_volume((v * 100.0).round() as u8));
            }
        }
    }
}

/// Centred icon, heading, subtext and primary action for an empty list.
pub struct EmptyState {
    pub view: Retained<NSView>,
    /// Fixed height, so resizing moves the block without squeezing its rows together.
    group: Retained<NSView>,
    icon: Retained<NSImageView>,
    heading: Retained<NSTextField>,
    sub: Retained<NSTextField>,
    action: Retained<HoverButton>,
}

impl EmptyState {
    const BUTTON_H: CGFloat = 36.0;
    const GROUP_H: CGFloat = 136.0;

    /// The action button sends `focusSearch:` to `target`.
    pub fn new(mtm: MainThreadMarker, frame: NSRect, icon: Icon, target: &AnyObject) -> EmptyState {
        let view = view(mtm, frame, FLEX_W | FLEX_H);
        let (w, h) = (frame.size.width, frame.size.height);
        let gh = Self::GROUP_H;
        // Slightly above centre, which reads as centred.
        let group = self::view(
            mtm,
            rect(0.0, ((h - gh) / 2.0 + 8.0).round(), w, gh),
            FLEX_W | PIN_TOP | PIN_BOTTOM,
        );
        let centred = PIN_LEFT | PIN_RIGHT;
        let (sub_y, heading_y) = (62.0, 86.0);
        let iconv = icon_view(
            mtm,
            icon,
            32.0,
            &theme::muted(),
            rect(((w - 32.0) / 2.0).round(), heading_y + 18.0, 32.0, 32.0),
        );
        iconv.setAutoresizingMask(centred);
        let heading = placed_label(
            mtm,
            "",
            &theme::font(16.0, Weight::SemiBold),
            &theme::ink(),
            0.0,
            heading_y,
            w,
            FLEX_W,
        );
        heading.setAlignment(NSTextAlignment::Center);
        let sub = placed_label(
            mtm,
            "",
            &theme::font(14.0, Weight::Regular),
            &theme::muted(),
            0.0,
            sub_y,
            w,
            FLEX_W,
        );
        sub.setAlignment(NSTextAlignment::Center);
        let action = HoverButton::primary(mtm, rect(0.0, 0.0, 0.0, Self::BUTTON_H));
        action.setAutoresizingMask(centred);
        action.set_action(target, objc2::sel!(focusSearch:));
        group.addSubview(&iconv);
        group.addSubview(&heading);
        group.addSubview(&sub);
        group.addSubview(&action);
        view.addSubview(&group);
        EmptyState {
            view,
            group,
            icon: iconv,
            heading,
            sub,
            action,
        }
    }

    pub fn set(&self, icon: Icon, heading: &str, sub: &str, action: &str) {
        self.icon.setImage(icon.image(32.0).as_deref());
        set_text(&self.heading, heading);
        set_text(&self.sub, sub);
        let w = self.action.set_label(action);
        let x = ((self.group.bounds().size.width - w) / 2.0).round();
        self.action.setFrame(rect(x, 0.0, w, Self::BUTTON_H));
    }
}
