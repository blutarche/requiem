//! The mini player in the status item's popover.

use std::cell::Cell;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSButton, NSColor, NSControl, NSFont, NSFontWeightRegular,
    NSFontWeightSemibold, NSImage, NSImageScaling, NSImageSymbolConfiguration, NSImageView,
    NSLineBreakMode, NSPopover, NSPopoverBehavior, NSPopoverDelegate, NSResponder, NSSlider,
    NSTextAlignment, NSTextField, NSView, NSViewController, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSObject, NSPoint, NSRect, NSRectEdge, NSSize, NSString, ns_string,
};

use super::art::{Cover, Placeholder, hide_from_accessibility};
use super::is_active;
use super::status::menu_tag;
use crate::view::{clock, subtitle};
use crate::{PlayerView, UiAction};

const W: CGFloat = 300.0;
const PAD: CGFloat = 16.0;
const COVER: CGFloat = W - 2.0 * PAD;
/// `ACCENT` in crates/requiem-gui/src/app/theme.rs.
const ACCENT: u32 = 0x7C70F5;
/// How long a seek or volume change outranks the daemon's echoes, as `ECHO_HOLD` in
/// requiem-gui: without it, events sent before the daemon saw the change snap the slider back.
const ECHO_HOLD: Duration = Duration::from_millis(500);
/// A position this close to a seek's target confirms it.
const SEEK_AGREES: Duration = Duration::from_secs(1);

define_class!(
    /// Lays out top-down.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RequiemMiniFlipped"]
    struct Flipped;

    impl Flipped {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl Flipped {
    fn new(mtm: MainThreadMarker, size: NSSize) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, size)] }
    }
}

fn rect(x: CGFloat, y: CGFloat, w: CGFloat, h: CGFloat) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn accent() -> Retained<NSColor> {
    let c = |shift: u32| CGFloat::from(((ACCENT >> shift) & 0xff) as u8) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(c(16), c(8), c(0), 1.0)
}

fn symbol(name: &NSString, label: &NSString, points: CGFloat) -> Option<Retained<NSImage>> {
    // SAFETY: a framework constant.
    let weight = unsafe { NSFontWeightRegular };
    let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(points, weight);
    NSImage::imageWithSystemSymbolName_accessibilityDescription(name, Some(label))?
        .imageWithSymbolConfiguration(&config)
}

fn label(
    mtm: MainThreadMarker,
    font: &NSFont,
    color: &NSColor,
    frame: NSRect,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(ns_string!(""), mtm);
    l.setFont(Some(font));
    l.setTextColor(Some(color));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l.setMaximumNumberOfLines(1);
    l.setFrame(frame);
    l
}

fn set_text(l: &NSTextField, text: &str) {
    if l.stringValue().to_string() != text {
        l.setStringValue(&NSString::from_str(text));
    }
}

/// Labels the control and its cell, which is what VoiceOver reads for most controls.
fn name(control: &NSControl, name: &NSString) {
    control.setAccessibilityLabel(Some(name));
    if let Some(cell) = control.cell() {
        cell.setAccessibilityLabel(Some(name));
    }
}

/// A borderless symbol button sending `action` with `ui`'s tag.
fn transport(
    mtm: MainThreadMarker,
    image: Option<&NSImage>,
    accessible: &NSString,
    ui: UiAction,
    target: Option<&AnyObject>,
    action: Sel,
    frame: NSRect,
) -> Retained<NSButton> {
    let b = match image {
        // SAFETY: `target`, when set, implements `action`.
        Some(image) => unsafe {
            NSButton::buttonWithImage_target_action(image, target, Some(action), mtm)
        },
        // SAFETY: as above.
        None => unsafe {
            NSButton::buttonWithTitle_target_action(accessible, target, Some(action), mtm)
        },
    };
    b.setBordered(false);
    b.setContentTintColor(Some(&NSColor::labelColor()));
    b.setTag(menu_tag(ui));
    b.setFrame(frame);
    name(&b, accessible);
    b
}

/// The "Open Requiem" button, centred at `y`.
fn open_button(
    mtm: MainThreadMarker,
    target: Option<&AnyObject>,
    action: Sel,
    y: CGFloat,
) -> Retained<NSButton> {
    // SAFETY: `target`, when set, implements `action`.
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            ns_string!("Open Requiem"),
            target,
            Some(action),
            mtm,
        )
    };
    b.setTag(menu_tag(UiAction::OpenGui));
    b.sizeToFit();
    let size = b.frame().size;
    b.setFrameOrigin(NSPoint::new(((W - size.width) / 2.0).round(), y));
    name(&b, ns_string!("Open Requiem"));
    b
}

fn decoration(
    mtm: MainThreadMarker,
    image: Option<&NSImage>,
    frame: NSRect,
) -> Retained<NSImageView> {
    let v = NSImageView::initWithFrame(NSImageView::alloc(mtm), frame);
    v.setImage(image);
    v.setImageScaling(NSImageScaling::ScaleNone);
    v.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
    hide_from_accessibility(&v);
    v
}

/// The mini player, built on first open. Controls send their selectors to the target it was
/// built with: `miniAction:` with a `MENU_ACTIONS` tag, `miniSeek:` and `miniVolume:`.
pub(super) struct MiniPlayer {
    popover: Retained<NSPopover>,
    root: Retained<NSVisualEffectView>,
    player: Retained<Flipped>,
    empty: Retained<Flipped>,
    cover: Retained<Cover>,
    title: Retained<NSTextField>,
    subtitle: Retained<NSTextField>,
    seek: Retained<NSSlider>,
    elapsed: Retained<NSTextField>,
    total: Retained<NSTextField>,
    toggle: Retained<NSButton>,
    volume: Retained<NSSlider>,
    play: Option<Retained<NSImage>>,
    pause: Option<Retained<NSImage>>,
    duration: Cell<Option<Duration>>,
    seek_dragging: Cell<bool>,
    /// A seek's target, and until when positions far from it are stale.
    seek_hold: Cell<Option<(Duration, Instant)>>,
    volume_dragging: Cell<bool>,
    volume_hold: Cell<Option<Instant>>,
    /// The volume last sent or shown, so moving the slider within one step sends nothing.
    volume_value: Cell<Option<u8>>,
}

impl MiniPlayer {
    pub(super) fn new(mtm: MainThreadMarker, target: Option<&AnyObject>) -> Self {
        let player = Flipped::new(mtm, NSSize::new(W, 0.0));
        let mut y = PAD;

        let placeholder = Placeholder {
            fill: NSColor::quaternaryLabelColor(),
            glyph: symbol(ns_string!("music.note"), ns_string!(""), 48.0),
            tint: NSColor::tertiaryLabelColor(),
        };
        let cover = Cover::new(mtm, rect(PAD, y, COVER, COVER), 8.0, &placeholder);
        player.addSubview(&cover);
        y += COVER + 12.0;

        // SAFETY: framework constants.
        let (regular, semibold) = unsafe { (NSFontWeightRegular, NSFontWeightSemibold) };
        let title = label(
            mtm,
            &NSFont::systemFontOfSize_weight(13.0, semibold),
            &NSColor::labelColor(),
            rect(PAD, y, COVER, 18.0),
        );
        player.addSubview(&title);
        y += 19.0;
        let subtitle = label(
            mtm,
            &NSFont::systemFontOfSize_weight(12.0, regular),
            &NSColor::secondaryLabelColor(),
            rect(PAD, y, COVER, 16.0),
        );
        player.addSubview(&subtitle);
        y += 16.0 + 10.0;

        // SAFETY: `target`, when set, implements `miniSeek:`.
        let seek = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                0.0,
                0.0,
                1.0,
                target,
                Some(sel!(miniSeek:)),
                mtm,
            )
        };
        seek.setFrame(rect(PAD, y, COVER, 20.0));
        seek.setContinuous(true);
        seek.setTrackFillColor(Some(&accent()));
        name(&seek, ns_string!("Seek"));
        player.addSubview(&seek);
        y += 20.0 + 2.0;

        let digits = NSFont::monospacedDigitSystemFontOfSize_weight(11.0, regular);
        let elapsed = label(
            mtm,
            &digits,
            &NSColor::secondaryLabelColor(),
            rect(PAD, y, COVER / 2.0, 14.0),
        );
        let total = label(
            mtm,
            &digits,
            &NSColor::secondaryLabelColor(),
            rect(PAD + COVER / 2.0, y, COVER / 2.0, 14.0),
        );
        total.setAlignment(NSTextAlignment::Right);
        player.addSubview(&elapsed);
        player.addSubview(&total);
        y += 14.0 + 6.0;

        let action = sel!(miniAction:);
        let (button, gap) = (40.0, 64.0);
        let row = |x: CGFloat| rect((x - button / 2.0).round(), y, button, button);
        let play = symbol(ns_string!("play.fill"), ns_string!("Play"), 26.0);
        let pause = symbol(ns_string!("pause.fill"), ns_string!("Pause"), 26.0);
        let prev = transport(
            mtm,
            symbol(ns_string!("backward.fill"), ns_string!("Previous"), 18.0).as_deref(),
            ns_string!("Previous"),
            UiAction::Prev,
            target,
            action,
            row(W / 2.0 - gap),
        );
        let toggle = transport(
            mtm,
            play.as_deref(),
            ns_string!("Play"),
            UiAction::Toggle,
            target,
            action,
            row(W / 2.0),
        );
        let next = transport(
            mtm,
            symbol(ns_string!("forward.fill"), ns_string!("Next"), 18.0).as_deref(),
            ns_string!("Next"),
            UiAction::Next,
            target,
            action,
            row(W / 2.0 + gap),
        );
        for b in [&prev, &toggle, &next] {
            player.addSubview(b);
        }
        y += button + 8.0;

        let icon = 20.0;
        player.addSubview(&decoration(
            mtm,
            symbol(ns_string!("speaker.fill"), ns_string!(""), 11.0).as_deref(),
            rect(PAD, y, icon, 20.0),
        ));
        player.addSubview(&decoration(
            mtm,
            symbol(ns_string!("speaker.wave.3.fill"), ns_string!(""), 11.0).as_deref(),
            rect(W - PAD - icon, y, icon, 20.0),
        ));
        // SAFETY: `target`, when set, implements `miniVolume:`.
        let volume = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                0.0,
                0.0,
                100.0,
                target,
                Some(sel!(miniVolume:)),
                mtm,
            )
        };
        volume.setFrame(rect(PAD + icon + 6.0, y, COVER - 2.0 * (icon + 6.0), 20.0));
        volume.setContinuous(true);
        volume.setTrackFillColor(Some(&accent()));
        name(&volume, ns_string!("Volume"));
        player.addSubview(&volume);
        y += 20.0 + 14.0;

        let open = open_button(mtm, target, action, y);
        player.addSubview(&open);
        y += open.frame().size.height + PAD;
        player.setFrameSize(NSSize::new(W, y.ceil()));

        let empty = Flipped::new(mtm, NSSize::new(W, 0.0));
        let nothing = label(
            mtm,
            &NSFont::systemFontOfSize_weight(13.0, semibold),
            &NSColor::secondaryLabelColor(),
            rect(PAD, 24.0, COVER, 18.0),
        );
        nothing.setAlignment(NSTextAlignment::Center);
        nothing.setStringValue(ns_string!("Nothing playing"));
        empty.addSubview(&nothing);
        let open = open_button(mtm, target, action, 24.0 + 18.0 + 14.0);
        empty.addSubview(&open);
        empty.setFrameSize(NSSize::new(
            W,
            (open.frame().origin.y + open.frame().size.height + 20.0).ceil(),
        ));

        let root = NSVisualEffectView::new(mtm);
        root.setMaterial(NSVisualEffectMaterial::Popover);
        root.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        root.setState(NSVisualEffectState::Active);
        root.addSubview(&player);
        root.addSubview(&empty);

        let controller = NSViewController::new(mtm);
        controller.setView(&root);
        let popover = NSPopover::new(mtm);
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(true);
        popover.setContentViewController(Some(&controller));

        let mini = Self {
            popover,
            root,
            player,
            empty,
            cover,
            title,
            subtitle,
            seek,
            elapsed,
            total,
            toggle,
            volume,
            play,
            pause,
            duration: Cell::new(None),
            seek_dragging: Cell::new(false),
            seek_hold: Cell::new(None),
            volume_dragging: Cell::new(false),
            volume_hold: Cell::new(None),
            volume_value: Cell::new(None),
        };
        mini.show(None, Instant::now());
        mini
    }

    /// The popover holds it weakly.
    pub(super) fn set_delegate(&self, delegate: &ProtocolObject<dyn NSPopoverDelegate>) {
        self.popover.setDelegate(Some(delegate));
    }

    pub(super) fn is_shown(&self) -> bool {
        self.popover.isShown()
    }

    /// Below `anchor`, a status item's button.
    pub(super) fn open(&self, anchor: &NSView) {
        self.popover.showRelativeToRect_ofView_preferredEdge(
            anchor.bounds(),
            anchor,
            NSRectEdge::MinY,
        );
    }

    pub(super) fn close(&self) {
        self.popover.close();
    }

    /// Ends any drag the closing cut short.
    pub(super) fn closed(&self) {
        self.seek_dragging.set(false);
        self.volume_dragging.set(false);
    }

    pub(super) fn root(&self) -> &NSView {
        &self.root
    }

    /// Shows `view` as of `now`, or the empty state for `None`.
    pub(super) fn show(&self, view: Option<&PlayerView>, now: Instant) {
        let shown = if view.is_some() {
            &self.player
        } else {
            &self.empty
        };
        self.player.setHidden(view.is_none());
        self.empty.setHidden(view.is_some());
        let size = shown.frame().size;
        self.root.setFrameSize(size);
        self.popover.setContentSize(size);

        let Some(view) = view else {
            self.cover.set_url(None);
            self.duration.set(None);
            return;
        };
        set_text(&self.title, &view.title);
        set_text(&self.subtitle, &subtitle(view));
        self.cover.set_url(view.artwork_url.as_deref());
        self.duration.set(view.duration);
        set_text(&self.total, &view.duration.map(clock).unwrap_or_default());
        self.seek.setEnabled(view.duration.is_some());
        let (image, label) = if is_active(view.state) {
            (&self.pause, ns_string!("Pause"))
        } else {
            (&self.play, ns_string!("Play"))
        };
        self.toggle.setImage(image.as_deref());
        name(&self.toggle, label);
        self.set_position(view.position, now);
    }

    pub(super) fn set_position(&self, at: Duration, now: Instant) {
        if self.seek_dragging.get() {
            return;
        }
        if let Some((target, until)) = self.seek_hold.get() {
            if now < until && at.abs_diff(target) > SEEK_AGREES {
                return;
            }
            self.seek_hold.set(None);
        }
        set_text(&self.elapsed, &clock(at));
        let fraction = match self.duration.get() {
            Some(d) if !d.is_zero() => (at.as_secs_f64() / d.as_secs_f64()).min(1.0),
            _ => 0.0,
        };
        self.seek.setDoubleValue(fraction);
    }

    pub(super) fn set_volume(&self, volume: u8, now: Instant) {
        if self.volume_dragging.get() || self.volume_hold.get().is_some_and(|until| now < until) {
            return;
        }
        self.volume_value.set(Some(volume));
        self.volume.setDoubleValue(f64::from(volume));
    }

    /// The seek slider moved: previews while `dragging`, and seeks once it ends.
    pub(super) fn seek_moved(&self, dragging: bool, now: Instant) -> Option<UiAction> {
        let duration = self.duration.get()?;
        let to = duration.mul_f64(self.seek.doubleValue().clamp(0.0, 1.0));
        set_text(&self.elapsed, &clock(to));
        self.seek_dragging.set(dragging);
        if dragging {
            return None;
        }
        self.seek_hold.set(Some((to, now + ECHO_HOLD)));
        Some(UiAction::Seek(to))
    }

    /// The volume slider moved: sets the volume as it goes.
    pub(super) fn volume_moved(&self, dragging: bool, now: Instant) -> Option<UiAction> {
        self.volume_dragging.set(dragging);
        if !dragging {
            self.volume_hold.set(Some(now + ECHO_HOLD));
        }
        let volume = self.volume.doubleValue().round().clamp(0.0, 100.0) as u8;
        (self.volume_value.replace(Some(volume)) != Some(volume))
            .then_some(UiAction::SetVolume(volume))
    }
}
