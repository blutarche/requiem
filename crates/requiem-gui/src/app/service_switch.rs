//! The sidebar's control that filters Home, the Library and search results to one service.

use std::cell::{Cell, OnceCell};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSEvent, NSImage, NSImageScaling, NSImageView,
    NSResponder, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSString};

use super::badge;
use super::theme;
use super::ui;
use super::widgets::{self, PIN_BOTTOM, rect};
use crate::vm::ServiceFilter;

/// The room around the segments, inside the border.
const INSET: CGFloat = 2.0;
/// The height of a segment's icon.
const ICON: CGFloat = 16.0;

pub struct SegmentIvars {
    filter: ServiceFilter,
    active: Cell<bool>,
    hovered: Cell<bool>,
    icon: OnceCell<Retained<NSImageView>>,
}

define_class!(
    /// One choice of the control: accent-soft when it is the chosen one.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SegmentIvars]
    struct Segment;

    impl Segment {
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
            let filter = self.ivars().filter;
            ui::with(|ui| ui.choose_filter(filter));
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            let filter = self.ivars().filter;
            ui::with(|ui| ui.choose_filter(filter));
            true
        }

        /// A plain view reports itself disabled, which VoiceOver reads as dimmed.
        #[unsafe(method(isAccessibilityEnabled))]
        fn accessibility_enabled(&self) -> bool {
            true
        }

        /// Clicks on the name land on the segment itself.
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

impl Segment {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        filter: ServiceFilter,
        image: &NSImage,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SegmentIvars {
            filter,
            active: Cell::new(false),
            hovered: Cell::new(false),
            icon: OnceCell::new(),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        widgets::round(&this, theme::RADIUS - INSET);
        let size = image.size();
        let icon = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            rect(
                ((frame.size.width - size.width) / 2.0).round(),
                ((frame.size.height - size.height) / 2.0).round(),
                size.width,
                size.height,
            ),
        );
        icon.setImage(Some(image));
        icon.setImageScaling(NSImageScaling::ScaleNone);
        this.addSubview(&icon);
        let _ = this.ivars().icon.set(icon);
        // The names are spoken in full whatever they show.
        widgets::as_button(&this, filter.label());
        this.setToolTip(Some(&NSString::from_str(filter.label())));
        this.setAccessibilitySelected(false);
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this.paint();
        this
    }

    /// The chosen segment is the selected one, for VoiceOver too.
    fn set_active(&self, active: bool) {
        self.setAccessibilitySelected(active);
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
        widgets::fill(self, bg.as_deref());
        if let Some(icon) = iv.icon.get() {
            let alpha = if active {
                1.0
            } else if iv.hovered.get() {
                0.8
            } else {
                0.5
            };
            icon.setAlphaValue(alpha);
        }
    }
}

/// All, Spotify and YouTube Music side by side; hidden unless both services are signed in.
pub struct ServiceSwitch {
    pub view: Retained<NSView>,
    segments: Vec<Retained<Segment>>,
}

impl ServiceSwitch {
    pub const H: CGFloat = 28.0;

    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> ServiceSwitch {
        let view = widgets::view(mtm, frame, PIN_BOTTOM);
        widgets::round(&view, theme::RADIUS);
        widgets::border(&view, &theme::chrome_border(), 1.0);
        view.setAccessibilityElement(true);
        // SAFETY: the role static is an immutable framework constant.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        view.setAccessibilityLabel(Some(&NSString::from_str("Service")));

        let inner = rect(
            INSET,
            INSET,
            frame.size.width - 2.0 * INSET,
            frame.size.height - 2.0 * INSET,
        );
        let n = ServiceFilter::CHOICES.len();
        let each = (inner.size.width / n as CGFloat).floor();
        let mut x = inner.origin.x;
        let segments = ServiceFilter::CHOICES
            .into_iter()
            .enumerate()
            .map(|(i, filter)| {
                // The last segment takes up whatever the rounding left.
                let w = if i + 1 == n {
                    inner.origin.x + inner.size.width - x
                } else {
                    each
                };
                let image = match filter {
                    ServiceFilter::All => badge::pair(ICON),
                    ServiceFilter::Only(service) => badge::mark(service, ICON),
                };
                let segment = Segment::new(
                    mtm,
                    rect(x, inner.origin.y, w, inner.size.height),
                    filter,
                    &image,
                );
                x += w;
                view.addSubview(&segment);
                segment
            })
            .collect();
        view.setHidden(true);
        ServiceSwitch { view, segments }
    }

    /// Shows the control with `filter` chosen, or hides it.
    pub fn set(&self, shown: bool, filter: ServiceFilter) {
        for segment in &self.segments {
            segment.set_active(segment.ivars().filter == filter);
        }
        self.view.setHidden(!shown);
    }
}
