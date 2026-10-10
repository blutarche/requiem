//! The player bar along the bottom of the window.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSTextAlignment, NSTextField, NSView};
use objc2_core_foundation::CGFloat;
use objc2_foundation::MainThreadMarker;

use super::theme::{self, Icon, Weight};
use super::widgets::{
    self, FLEX_W, HoverButton, PIN_BOTTOM, PIN_LEFT, PIN_RIGHT, PIN_TOP, Slider, SliderKind, rect,
};
use crate::vm::Controls;

pub struct PlayerBar {
    pub view: Retained<NSView>,
    title: Retained<NSTextField>,
    artists: Retained<NSTextField>,
    prev: Retained<HoverButton>,
    play: Retained<HoverButton>,
    next: Retained<HoverButton>,
    seek: Retained<Slider>,
    elapsed: Retained<NSTextField>,
    total: Retained<NSTextField>,
    volume: Retained<Slider>,
    volume_icon: Retained<NSView>,
    playing: Cell<bool>,
}

impl PlayerBar {
    pub const H: CGFloat = 72.0;
    const CENTER_W: CGFloat = 520.0;
    /// Fits "Buffering…".
    const TIME_W: CGFloat = 72.0;
    const PLAY: CGFloat = 36.0;
    const SKIP: CGFloat = 32.0;
    /// The title's centre with an artist line below it, and alone.
    const TITLE_Y: CGFloat = 45.0;
    const TITLE_ALONE_Y: CGFloat = Self::H / 2.0;

    pub fn new(mtm: MainThreadMarker, width: CGFloat, target: &AnyObject) -> PlayerBar {
        let h = Self::H;
        let view = widgets::panel(
            mtm,
            rect(0.0, 0.0, width, h),
            FLEX_W | PIN_BOTTOM,
            &theme::chrome_bg_2(),
        );
        let rule = widgets::panel(
            mtm,
            rect(0.0, h - 1.0, width, 1.0),
            FLEX_W | PIN_TOP,
            &theme::chrome_border(),
        );
        view.addSubview(&rule);

        // Width and the free space on each side shrink in proportion, so the left block and
        // the centred transport never overlap down to the window's minimum width.
        let left_w = (width - Self::CENTER_W) / 2.0 - 40.0;
        let title = widgets::placed_label(
            mtm,
            "",
            &theme::font(14.0, Weight::SemiBold),
            &theme::ink(),
            20.0,
            Self::TITLE_Y,
            left_w,
            FLEX_W | PIN_LEFT,
        );
        let artists = widgets::placed_label(
            mtm,
            "",
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
            20.0,
            26.0,
            left_w,
            FLEX_W | PIN_LEFT,
        );
        view.addSubview(&title);
        view.addSubview(&artists);

        let cw = Self::CENTER_W;
        let center = widgets::view(
            mtm,
            rect((width - cw) / 2.0, 0.0, cw, h),
            FLEX_W | PIN_LEFT | PIN_RIGHT,
        );
        let mid = cw / 2.0;
        let row_y = 46.0;
        let centred = PIN_LEFT | PIN_RIGHT;

        let seek_y = 19.0;
        let tw = Self::TIME_W;
        let time_font = theme::tabular(12.0, Weight::Regular);
        let elapsed = widgets::placed_label(
            mtm,
            "",
            &time_font,
            &theme::muted(),
            0.0,
            seek_y,
            tw,
            PIN_LEFT,
        );
        elapsed.setAlignment(NSTextAlignment::Right);
        let total = widgets::placed_label(
            mtm,
            "",
            &time_font,
            &theme::muted(),
            cw - tw,
            seek_y,
            tw,
            PIN_RIGHT,
        );
        // 24 pt tall for an easy grab; it reaches under the transport buttons, which sit
        // above it and keep their clicks.
        let seek_h = 24.0;
        let seek = Slider::new(
            mtm,
            rect(
                tw + 6.0,
                seek_y - seek_h / 2.0,
                cw - 2.0 * (tw + 6.0),
                seek_h,
            ),
            FLEX_W,
            SliderKind::Seek,
        );
        center.addSubview(&elapsed);
        center.addSubview(&seek);
        center.addSubview(&total);

        let ghost = |icon, x| {
            let b = HoverButton::new(
                mtm,
                rect(x, row_y - Self::SKIP / 2.0, Self::SKIP, Self::SKIP),
                icon,
                18.0,
                &theme::muted(),
                None,
                &theme::bg_3(),
                theme::RADIUS,
            );
            b.setAutoresizingMask(centred);
            b
        };
        let prev = ghost(Icon::SkipBack, mid - Self::PLAY / 2.0 - 12.0 - Self::SKIP);
        let next = ghost(Icon::SkipForward, mid + Self::PLAY / 2.0 + 12.0);
        let play = HoverButton::new(
            mtm,
            rect(
                mid - Self::PLAY / 2.0,
                row_y - Self::PLAY / 2.0,
                Self::PLAY,
                Self::PLAY,
            ),
            Icon::Play,
            18.0,
            &theme::on_accent(),
            Some(&theme::accent()),
            &theme::accent_hi(),
            Self::PLAY / 2.0,
        );
        play.setAutoresizingMask(centred);
        prev.set_action(target, sel!(prev:));
        play.set_action(target, sel!(toggle:));
        next.set_action(target, sel!(next:));
        center.addSubview(&prev);
        center.addSubview(&play);
        center.addSubview(&next);
        view.addSubview(&center);

        let vw = 112.0;
        let vx = width - 24.0 - vw;
        let vicon = widgets::icon_view(
            mtm,
            Icon::Volume,
            16.0,
            &theme::muted(),
            rect(vx - 26.0, (h - 16.0) / 2.0, 16.0, 16.0),
        );
        vicon.setAutoresizingMask(PIN_RIGHT);
        let vicon: Retained<NSView> = Retained::into_super(Retained::into_super(vicon));
        let volume = Slider::new(
            mtm,
            rect(vx, h / 2.0 - 8.0, vw, 16.0),
            PIN_RIGHT,
            SliderKind::Volume,
        );
        view.addSubview(&vicon);
        view.addSubview(&volume);

        PlayerBar {
            view,
            title,
            artists,
            prev,
            play,
            next,
            seek,
            elapsed,
            total,
            volume,
            volume_icon: vicon,
            playing: Cell::new(false),
        }
    }

    /// `None` when nothing is loaded.
    pub fn set_track(&self, track: Option<(&str, &str)>, total: &str) {
        let artists = match track {
            Some((title, artists)) => {
                widgets::set_text(&self.title, title);
                self.title.setTextColor(Some(&theme::ink()));
                artists
            }
            None => {
                widgets::set_text(&self.title, "Nothing playing");
                self.title.setTextColor(Some(&theme::muted()));
                ""
            }
        };
        widgets::set_text(&self.artists, artists);
        let mid = if artists.is_empty() {
            Self::TITLE_ALONE_Y
        } else {
            Self::TITLE_Y
        };
        let f = self.title.frame();
        let y = (mid - f.size.height / 2.0).round();
        if f.origin.y != y {
            self.title
                .setFrameOrigin(objc2_foundation::NSPoint::new(f.origin.x, y));
        }
        widgets::set_text(&self.total, total);
    }

    pub fn set_controls(&self, c: Controls) {
        for b in [&self.prev, &self.play, &self.next] {
            b.set_enabled(c.transport);
        }
        self.seek.set_enabled(c.seek);
        self.volume.set_enabled(c.player);
        self.volume_icon
            .setAlphaValue(if c.player { 1.0 } else { widgets::DIMMED });
    }

    /// Ignored while the seek bar is dragged, which shows its target time instead.
    pub fn set_elapsed(&self, text: &str) {
        if !self.seek.is_dragging() {
            widgets::set_text(&self.elapsed, text);
        }
    }

    pub fn preview_seek(&self, text: &str) {
        widgets::set_text(&self.elapsed, text);
    }

    pub fn set_progress(&self, fraction: f64) {
        self.seek.set_value(fraction);
    }

    pub fn set_playing(&self, playing: bool) {
        if self.playing.replace(playing) != playing {
            let icon = if playing { Icon::Pause } else { Icon::Play };
            self.play.set_icon(icon, 18.0);
        }
    }

    pub fn set_volume(&self, volume: u8) {
        self.volume.set_value(f64::from(volume) / 100.0);
    }
}
