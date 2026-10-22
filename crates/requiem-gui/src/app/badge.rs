//! The service badge: a small disc on a cover's bottom-right corner that names the item's
//! service, in lists that mix services.

use std::cell::RefCell;
use std::collections::HashMap;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject};
use objc2::{MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSControl, NSImage, NSImageScaling, NSImageView, NSLineCapStyle,
    NSResponder, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use super::theme;
use super::widgets::{self, rect};
use crate::vm::Service;

/// The ring in the window's background colour that sets the disc off busy artwork.
const RING: CGFloat = 1.5;

/// A badge's side, ring included, on a cover `art` points square: 14 pt on a table row's
/// thumbnail, growing to 20 pt on a 128 pt card and no larger.
fn side(art: CGFloat) -> CGFloat {
    (12.0 + art / 16.0).round().clamp(14.0, 20.0)
}

/// Where the badge sits on `cover`: on its bottom-right corner, reaching a fifth of its side
/// past the cover's right and bottom edges, but no more than the `room` below the cover.
/// `flipped` says whether y grows downwards in the cover's superview.
pub fn frame(cover: NSRect, room: CGFloat, flipped: bool) -> NSRect {
    let side = side(cover.size.width);
    let out = (side / 5.0).round().min(room).max(0.0);
    let x = cover.origin.x + cover.size.width + out - side;
    let y = if flipped {
        cover.origin.y + cover.size.height + out - side
    } else {
        cover.origin.y - out
    };
    rect(x, y, side, side)
}

define_class!(
    /// A badge, hidden while it has no service. Decorative, and clicks go to what is under it.
    // SAFETY: NSImageView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSImageView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RequiemServiceBadge"]
    pub struct Badge;

    impl Badge {
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> {
            None
        }
    }
);

impl Badge {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Badge> {
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Badge> = unsafe { msg_send![Badge::alloc(mtm), initWithFrame: frame] };
        this.setImageScaling(NSImageScaling::ScaleNone);
        widgets::hide_from_accessibility(&this);
        this.setHidden(true);
        this
    }

    /// Shows `service`'s badge at the view's size, or hides it for `None`.
    pub fn set(&self, service: Option<Service>) {
        let image = service.map(|s| image(s, self.frame().size.width, true));
        self.setImage(image.as_deref());
        self.setHidden(image.is_none());
    }
}

/// `service`'s mark alone, `side` points square, as a button shows it before its title.
pub fn mark(service: Service, side: CGFloat) -> Retained<NSImage> {
    image(service, side, false)
}

/// `service`'s badge, `side` points square, with or without its `ring`; drawn once per
/// service, side and ring.
fn image(service: Service, side: CGFloat, ring: bool) -> Retained<NSImage> {
    type Images = HashMap<(Service, u64, bool), Retained<NSImage>>;
    thread_local! {
        static IMAGES: RefCell<Images> = RefCell::new(HashMap::new());
    }
    IMAGES.with(|images| {
        images
            .borrow_mut()
            .entry((service, side.to_bits(), ring))
            .or_insert_with(|| draw(service, side, ring))
            .clone()
    })
}

fn draw(service: Service, side: CGFloat, ring: bool) -> Retained<NSImage> {
    let ring = ring.then(theme::bg);
    let draw = RcBlock::new(move |r: NSRect| -> Bool {
        let inset = match &ring {
            Some(ring) => {
                ring.setFill();
                NSBezierPath::bezierPathWithOvalInRect(r).fill();
                RING
            }
            None => 0.0,
        };
        let d = r.size.width - 2.0 * inset;
        disc(service, rect(r.origin.x + inset, r.origin.y + inset, d, d));
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(side, side), false, &draw)
}

/// Both services' discs, each `side` points across, side by side: YouTube Music's in front
/// and set off from Spotify's by a ring. The image has room for the ring all round.
pub fn pair(side: CGFloat) -> Retained<NSImage> {
    thread_local! {
        static PAIRS: RefCell<HashMap<u64, Retained<NSImage>>> = RefCell::new(HashMap::new());
    }
    PAIRS.with(|pairs| {
        pairs
            .borrow_mut()
            .entry(side.to_bits())
            .or_insert_with(|| draw_pair(side))
            .clone()
    })
}

fn draw_pair(side: CGFloat) -> Retained<NSImage> {
    let ring = theme::bg();
    // The front disc covers a third of the back one.
    let step = side - side / 3.0;
    let draw = RcBlock::new(move |r: NSRect| -> Bool {
        let back = rect(r.origin.x, r.origin.y + RING, side, side);
        disc(Service::Spotify, back);
        let front = rect(back.origin.x + step, back.origin.y, side, side);
        ring.setFill();
        NSBezierPath::bezierPathWithOvalInRect(rect(
            front.origin.x - RING,
            front.origin.y - RING,
            side + 2.0 * RING,
            side + 2.0 * RING,
        ))
        .fill();
        disc(Service::YouTubeMusic, front);
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(
        NSSize::new(step + side + RING, side + 2.0 * RING),
        false,
        &draw,
    )
}

/// `service`'s coloured disc with its mark, filling the square `r`.
fn disc(service: Service, r: NSRect) {
    let fill = match service {
        Service::Spotify => theme::spotify_green(),
        Service::YouTubeMusic => theme::youtube_red(),
    };
    fill.setFill();
    NSBezierPath::bezierPathWithOvalInRect(r).fill();
    let d = r.size.width;
    let centre = NSPoint::new(r.origin.x + d / 2.0, r.origin.y + d / 2.0);
    match service {
        Service::Spotify => arcs(centre, d),
        Service::YouTubeMusic => play(centre, d),
    }
}

/// Spotify's mark, simplified: three black arcs stacked on a disc `d` across, the widest on
/// top.
fn arcs(c: NSPoint, d: CGFloat) {
    NSColor::blackColor().setStroke();
    // Each arc's crown above the centre, half its width, how far its ends fall below the
    // crown, and its stroke, as fractions of `d`.
    for (crown, half, fall, stroke) in [
        (0.17, 0.30, 0.13, 0.12),
        (-0.02, 0.25, 0.1, 0.1),
        (-0.19, 0.19, 0.07, 0.085),
    ] {
        let (top, half, fall) = (c.y + crown * d, half * d, fall * d);
        let path = NSBezierPath::bezierPath();
        path.moveToPoint(NSPoint::new(c.x - half, top - fall));
        // The quadratic whose peak is the crown, as a cubic.
        path.curveToPoint_controlPoint1_controlPoint2(
            NSPoint::new(c.x + half, top - fall),
            NSPoint::new(c.x - half / 3.0, top + fall / 3.0),
            NSPoint::new(c.x + half / 3.0, top + fall / 3.0),
        );
        path.setLineWidth(stroke * d);
        path.setLineCapStyle(NSLineCapStyle::Round);
        path.stroke();
    }
}

/// YouTube Music's mark, simplified: a white play triangle on a disc `d` across, nudged right
/// so it looks centred.
fn play(c: NSPoint, d: CGFloat) {
    NSColor::whiteColor().setFill();
    let path = NSBezierPath::bezierPath();
    path.moveToPoint(NSPoint::new(c.x - 0.14 * d, c.y + 0.22 * d));
    path.lineToPoint(NSPoint::new(c.x + 0.24 * d, c.y));
    path.lineToPoint(NSPoint::new(c.x - 0.14 * d, c.y - 0.22 * d));
    path.closePath();
    path.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_grow_with_the_cover_from_14_to_20_points() {
        assert_eq!(side(32.0), 14.0);
        assert_eq!(side(48.0), 15.0);
        assert_eq!(side(128.0), 20.0);
        assert_eq!(side(387.0), 20.0);
    }

    #[test]
    fn a_badge_reaches_past_the_corner_within_the_room_below() {
        let cover = rect(12.0, 2.0, 32.0, 32.0);
        // A row's thumbnail: 2 pt below it, less the row's 1 pt rule.
        assert_eq!(frame(cover, 1.0, false), rect(31.0, 1.0, 14.0, 14.0));
        // A card's cover in a flipped view, with room to spare.
        let card = rect(6.0, 6.0, 128.0, 128.0);
        assert_eq!(frame(card, 8.0, true), rect(118.0, 118.0, 20.0, 20.0));
    }
}
