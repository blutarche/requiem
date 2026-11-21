//! The player bar's buffering effect: a thin arc turning round the play button, and a light
//! sweep along the seek track. Both are Core Animation layers with repeating animations the
//! render server runs, so they cost the app nothing per frame, and nothing at all once removed.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSView};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGPath;
use objc2_foundation::{MainThreadMarker, NSArray, NSNumber, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CAGradientLayer, CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer,
    CATransaction, kCALineCapRound,
};

use super::appearance;
use super::theme;
use super::widgets::{self, rect};

/// The fade in and out of the effect once the view model says to show it.
const FADE: Duration = Duration::from_millis(150);
/// One turn of the arc.
const TURN: f64 = 0.9;
/// One sweep along the track.
const SWEEP: f64 = 1.2;
/// One breath of the Reduce Motion pulse, dim to bright.
const PULSE: f64 = 0.8;
const PULSE_LOW: f64 = 0.35;

/// How an effect that is on moves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Motion {
    Moving,
    /// Reduce Motion: it stays put and its opacity pulses.
    Pulsing,
    /// The window can't be seen, as in a snapshot: it stays put.
    Still,
}

impl Motion {
    fn of(view: &NSView) -> Motion {
        if !view.window().is_some_and(|w| widgets::window_on_screen(&w)) {
            Motion::Still
        } else if widgets::reduce_motion() {
            Motion::Pulsing
        } else {
            Motion::Moving
        }
    }
}

fn animation(key_path: &str, from: f64, to: f64, secs: f64) -> Retained<CABasicAnimation> {
    let a = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(key_path)));
    // SAFETY: the animated key paths take numbers.
    unsafe {
        a.setFromValue(Some(&NSNumber::new_f64(from)));
        a.setToValue(Some(&NSNumber::new_f64(to)));
    }
    a.setDuration(secs);
    a.setRepeatCount(f32::INFINITY);
    a
}

fn linear(a: &CABasicAnimation) {
    a.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
        0.0, 0.0, 1.0, 1.0,
    )));
}

/// Opacity of `layer` breathing between dim and bright.
fn pulse(layer: &CALayer) {
    let a = animation("opacity", 1.0, PULSE_LOW, PULSE);
    a.setAutoreverses(true);
    layer.addAnimation_forKey(&a, Some(&NSString::from_str("pulse")));
}

/// Fades `layer` in or out over `FADE`, or at once when `animated` is false; `ended` runs once
/// a fade-out is over.
fn fade(layer: &CALayer, on: bool, animated: bool, ended: impl Fn() + 'static) {
    let opacity = if on { 1.0 } else { 0.0 };
    if !animated {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setOpacity(opacity);
        CATransaction::commit();
        if !on {
            ended();
        }
        return;
    }
    CATransaction::begin();
    CATransaction::setAnimationDuration(FADE.as_secs_f64());
    if !on {
        let done = RcBlock::new(ended);
        // SAFETY: the block is copied by the transaction, and runs on this thread.
        unsafe { CATransaction::setCompletionBlock(Some(&done)) };
    }
    layer.setOpacity(opacity);
    CATransaction::commit();
}

/// `color` as the app's appearance shows it.
fn resolved(
    view: &NSView,
    color: &objc2_app_kit::NSColor,
) -> Retained<objc2_core_graphics::CGColor> {
    let current = NSApplication::sharedApplication(view.mtm()).effectiveAppearance();
    appearance::resolve(color, &current)
}

/// A turning arc round the play button, in a view of its own that sits below the transport so
/// it never takes a click.
pub struct Ring {
    pub view: Retained<NSView>,
    host: Retained<CALayer>,
    arc: Retained<CAShapeLayer>,
    on: Rc<Cell<bool>>,
}

impl Ring {
    const STROKE: CGFloat = 2.0;

    /// `frame` is the square round the button, in the transport's coordinates.
    pub fn new(mtm: MainThreadMarker, frame: CGRect, mask: widgets::Mask) -> Ring {
        let view = widgets::view(mtm, frame, mask);
        let (w, h) = (frame.size.width, frame.size.height);
        let host = CALayer::layer();
        host.setFrame(rect(0.0, 0.0, w, h));
        host.setOpacity(0.0);
        host.setHidden(true);
        let arc = CAShapeLayer::layer();
        arc.setFrame(rect(0.0, 0.0, w, h));
        let inset = Self::STROKE / 2.0;
        // SAFETY: the rectangle is finite and no transform is applied.
        let path = unsafe {
            CGPath::with_ellipse_in_rect(
                rect(inset, inset, w - 2.0 * inset, h - 2.0 * inset),
                std::ptr::null(),
            )
        };
        arc.setPath(Some(&path));
        arc.setFillColor(None);
        arc.setLineWidth(Self::STROKE);
        // A quarter of the circle.
        arc.setStrokeEnd(0.25);
        // SAFETY: the line-cap static is an immutable framework constant.
        arc.setLineCap(unsafe { kCALineCapRound });
        host.addSublayer(&arc);
        if let Some(layer) = view.layer() {
            layer.addSublayer(&host);
        }
        Ring {
            view,
            host,
            arc,
            on: Rc::new(Cell::new(false)),
        }
    }

    pub fn set(&self, on: bool) {
        if self.on.replace(on) == on {
            return;
        }
        let animated = widgets::on_screen(&self.view);
        if on {
            let color = resolved(&self.view, &theme::accent());
            self.arc.setStrokeColor(Some(&color));
            self.arc.removeAllAnimations();
            self.host.setHidden(false);
            match Motion::of(&self.view) {
                Motion::Moving => {
                    // Clockwise, which a positive angle is not in this unflipped layer.
                    let spin = animation("transform.rotation.z", 0.0, -std::f64::consts::TAU, TURN);
                    linear(&spin);
                    self.arc
                        .addAnimation_forKey(&spin, Some(&NSString::from_str("spin")));
                }
                Motion::Pulsing => pulse(&self.arc),
                Motion::Still => {}
            }
            fade(&self.host, true, animated, || {});
        } else {
            let (host, arc, on) = (self.host.clone(), self.arc.clone(), self.on.clone());
            fade(&self.host, false, animated, move || {
                if !on.get() {
                    arc.removeAllAnimations();
                    host.setHidden(true);
                }
            });
        }
    }
}

/// A light band sweeping along the seek track, clipped to it. The slider owns it and places
/// it on its track.
pub struct Shimmer {
    host: Retained<CALayer>,
    band: Retained<CAGradientLayer>,
    on: Rc<Cell<bool>>,
    track: Cell<CGRect>,
    motion: Cell<Motion>,
}

impl Shimmer {
    const BAND: CGFloat = 0.35;

    pub fn new() -> Shimmer {
        let host = CALayer::layer();
        host.setMasksToBounds(true);
        host.setCornerRadius(2.0);
        host.setOpacity(0.0);
        host.setHidden(true);
        let band = CAGradientLayer::layer();
        band.setStartPoint(CGPoint::new(0.0, 0.5));
        band.setEndPoint(CGPoint::new(1.0, 0.5));
        host.addSublayer(&band);
        Shimmer {
            host,
            band,
            on: Rc::new(Cell::new(false)),
            track: Cell::new(CGRect::new(CGPoint::ZERO, CGSize::ZERO)),
            motion: Cell::new(Motion::Still),
        }
    }

    /// Puts the shimmer in `layer`, the slider's, which holds it for good.
    pub fn attach(&self, layer: &CALayer) {
        layer.addSublayer(&self.host);
    }

    /// Follows the slider's track, which a resize moves.
    pub fn place(&self, track: CGRect) {
        if track == self.track.replace(track) {
            return;
        }
        self.host.setFrame(track);
        if self.on.get() {
            self.lay_band();
        }
    }

    /// The band over its start, its middle for a still shimmer, or the whole track for a
    /// pulsing one; and the sweep that moves it.
    fn lay_band(&self) {
        let (w, h) = {
            let t = self.track.get();
            (t.size.width, t.size.height)
        };
        self.band.removeAllAnimations();
        let band_w = (w * Self::BAND).max(24.0);
        match self.motion.get() {
            Motion::Moving => {
                self.band.setFrame(rect(-band_w, 0.0, band_w, h));
                let sweep = animation("position.x", -band_w / 2.0, w + band_w / 2.0, SWEEP);
                linear(&sweep);
                self.band
                    .addAnimation_forKey(&sweep, Some(&NSString::from_str("sweep")));
            }
            Motion::Pulsing => {
                self.band.setFrame(rect(0.0, 0.0, w, h));
                pulse(&self.band);
            }
            Motion::Still => self
                .band
                .setFrame(rect((w - band_w) * 0.35, 0.0, band_w, h)),
        }
    }

    pub fn set(&self, on: bool, view: &NSView) {
        if self.on.replace(on) == on {
            return;
        }
        let animated = widgets::on_screen(view);
        if on {
            let motion = Motion::of(view);
            self.motion.set(motion);
            let ink = theme::ink();
            let clear = ink.colorWithAlphaComponent(0.0);
            let lit =
                ink.colorWithAlphaComponent(if motion == Motion::Pulsing { 0.18 } else { 0.3 });
            let colors = [
                resolved(view, &clear),
                resolved(view, &lit),
                resolved(view, &clear),
            ];
            // SAFETY: a CGColor is a Core Foundation object, which Objective-C collections
            // hold as any other object; the array only retains them.
            let colors = unsafe {
                NSArray::from_slice(
                    &colors
                        .each_ref()
                        .map(|c| &*std::ptr::from_ref(&**c).cast::<AnyObject>()),
                )
            };
            // SAFETY: the array holds CGColors, which is what a gradient's colours are.
            unsafe { self.band.setColors(Some(&colors)) };
            self.lay_band();
            self.host.setHidden(false);
            fade(&self.host, true, animated, || {});
        } else {
            let (host, band, on) = (self.host.clone(), self.band.clone(), self.on.clone());
            fade(&self.host, false, animated, move || {
                if !on.get() {
                    band.removeAllAnimations();
                    host.setHidden(true);
                }
            });
        }
    }
}
