use std::cell::RefCell;
use std::f64::consts::TAU;
use std::ptr::NonNull;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBezierPath, NSCellImagePosition, NSColor, NSEventMask, NSFont, NSImage, NSMenu,
    NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusBarButton, NSStatusItem,
    NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer, ns_string,
};
use requiem_core::PlayState;

use super::is_active;
use crate::view::{menu_title, status_title};
use crate::{PlayerView, UiAction};

/// Menu item tags index this.
const MENU_ACTIONS: [UiAction; 7] = [
    UiAction::Toggle,
    UiAction::Next,
    UiAction::Prev,
    UiAction::OpenGui,
    UiAction::CheckForUpdates,
    UiAction::RestartToUpdate,
    UiAction::Quit,
];

pub(super) fn menu_tag(action: UiAction) -> isize {
    let index = MENU_ACTIONS.iter().position(|a| *a == action);
    index.expect("action is in MENU_ACTIONS") as isize
}

pub(super) fn menu_action(tag: isize) -> Option<UiAction> {
    usize::try_from(tag)
        .ok()
        .and_then(|i| MENU_ACTIONS.get(i))
        .copied()
}

/// The images that do not animate; the playing one is for buffering and for Reduce Motion.
struct Icons {
    idle: Retained<NSImage>,
    playing: Retained<NSImage>,
    paused: Retained<NSImage>,
}

/// The four bars of `docs/media/logo.svg` as `(x, height)` in its 256-unit square, each 24 wide.
const LOGO_BARS: [(f64, f64); 4] = [(56.0, 56.0), (96.0, 100.0), (136.0, 124.0), (176.0, 76.0)];

/// The shortest an animated bar gets, as a share of its logo height.
const BAR_FLOOR: f64 = 0.35;
/// Seconds per cycle and starting angle of each bar, apart enough to read as an equalizer.
const BAR_PERIODS: [f64; 4] = [1.7, 1.3, 2.1, 1.5];
const BAR_PHASES: [f64; 4] = [0.0, 1.9, 3.7, 5.1];
const FRAME_INTERVAL: f64 = 0.1;
/// Opacity of the bars while paused.
const PAUSED_ALPHA: f64 = 0.45;

/// The bar heights, in logo units, `elapsed` seconds into the animation.
fn bar_heights(elapsed: f64) -> [f64; 4] {
    std::array::from_fn(|i| {
        let wave = (elapsed / BAR_PERIODS[i] * TAU + BAR_PHASES[i]).sin();
        let level = BAR_FLOOR + (1.0 - BAR_FLOOR) * (wave + 1.0) / 2.0;
        LOGO_BARS[i].1 * level
    })
}

fn rest_heights() -> [f64; 4] {
    LOGO_BARS.map(|(_, height)| height)
}

/// The bars, vertically centered, without the logo's tile, as a template image so the menu bar
/// tints it. The size does not depend on `heights`, so the title beside it stays put.
fn bars(heights: [f64; 4], alpha: f64, description: &NSString) -> Retained<NSImage> {
    const HEIGHT: f64 = 16.0;
    const SPAN: f64 = 144.0;
    let scale = HEIGHT / 124.0;
    let size = NSSize::new(SPAN * scale, HEIGHT);
    let draw = RcBlock::new(move |_: NSRect| -> Bool {
        NSColor::blackColor()
            .colorWithAlphaComponent(alpha)
            .setFill();
        for ((x, _), height) in LOGO_BARS.into_iter().zip(heights) {
            let (w, h) = (24.0 * scale, height * scale);
            let bar = NSRect::new(
                NSPoint::new((x - 56.0) * scale, (HEIGHT - h) / 2.0),
                NSSize::new(w, h),
            );
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bar, w / 2.0, w / 2.0).fill();
        }
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
    image.setTemplate(true);
    image.setAccessibilityDescription(Some(description));
    image
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IconMode {
    Animated,
    Static,
    Dim,
}

fn icon_mode(state: PlayState, reduce_motion: bool) -> IconMode {
    match state {
        PlayState::Playing if !reduce_motion => IconMode::Animated,
        PlayState::Playing | PlayState::Buffering | PlayState::Stopped => IconMode::Static,
        PlayState::Paused => IconMode::Dim,
    }
}

fn animated_frame(elapsed: f64) -> Retained<NSImage> {
    bars(bar_heights(elapsed), 1.0, ns_string!("Requiem: playing"))
}

/// The status item, its menu and the items that change with the view. A click sends the
/// button's action; the menu is the item's only while it is open, since an item with a menu
/// opens it on every click.
pub(super) struct StatusMenu {
    mtm: MainThreadMarker,
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    title: Retained<NSMenuItem>,
    title_separator: Retained<NSMenuItem>,
    toggle: Retained<NSMenuItem>,
    check: Retained<NSMenuItem>,
    restart: Retained<NSMenuItem>,
    targeted: Vec<Retained<NSMenuItem>>,
    icons: Icons,
    /// Ticks the bars while playing; the block holds the button, never the menu.
    animation: RefCell<Option<Retained<NSTimer>>>,
}

impl StatusMenu {
    /// Builds the menu with every action item sending `action`, tagged per `MENU_ACTIONS`;
    /// the button sends `click`.
    pub(super) fn new(mtm: MainThreadMarker, action: Sel, click: Sel) -> Self {
        let action_item = |title: &NSString, ui: UiAction| {
            // SAFETY: `action` is a selector the controller set by `set_target` implements.
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    title,
                    Some(action),
                    ns_string!(""),
                )
            };
            item.setTag(menu_tag(ui));
            item
        };

        let title = NSMenuItem::new(mtm);
        title.setEnabled(false);
        let title_separator = NSMenuItem::separatorItem(mtm);
        let toggle = action_item(ns_string!("Play"), UiAction::Toggle);
        let next = action_item(ns_string!("Next"), UiAction::Next);
        let prev = action_item(ns_string!("Previous"), UiAction::Prev);
        let open = action_item(ns_string!("Open Requiem"), UiAction::OpenGui);
        let check = action_item(ns_string!("Check for Updates…"), UiAction::CheckForUpdates);
        let restart = action_item(ns_string!("Restart to Update"), UiAction::RestartToUpdate);
        let quit = action_item(ns_string!("Quit and Stop Music"), UiAction::Quit);

        let menu = NSMenu::new(mtm);
        // Enabled state is set explicitly; autoenabling would consult the responder chain.
        menu.setAutoenablesItems(false);
        for item in [
            &title,
            &title_separator,
            &toggle,
            &next,
            &prev,
            &NSMenuItem::separatorItem(mtm),
            &open,
            &check,
            &restart,
            &quit,
        ] {
            menu.addItem(item);
        }

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            button.setFont(Some(&NSFont::menuBarFontOfSize(0.0)));
            // SAFETY: `click` is a selector the controller set by `set_target` implements.
            unsafe { button.setAction(Some(click)) };
            button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
        }

        let status = Self {
            mtm,
            item,
            menu,
            title,
            title_separator,
            targeted: vec![
                toggle.clone(),
                next,
                prev,
                open,
                check.clone(),
                restart.clone(),
                quit,
            ],
            toggle,
            check,
            restart,
            icons: Icons {
                idle: bars(rest_heights(), 1.0, ns_string!("Requiem")),
                playing: bars(rest_heights(), 1.0, ns_string!("Requiem: playing")),
                paused: bars(rest_heights(), PAUSED_ALPHA, ns_string!("Requiem: paused")),
            },
            animation: RefCell::new(None),
        };
        status.show(None, false);
        status.show_updates(false, None);
        status
    }

    /// Menu items, the button and the menu hold these weakly; the caller keeps them alive.
    pub(super) fn set_target(
        &self,
        target: &AnyObject,
        delegate: &ProtocolObject<dyn NSMenuDelegate>,
    ) {
        for item in &self.targeted {
            // SAFETY: `target` implements the action selector these items were built with.
            unsafe { item.setTarget(Some(target)) };
        }
        if let Some(button) = self.button() {
            // SAFETY: `target` implements the button's selector.
            unsafe { button.setTarget(Some(target)) };
        }
        self.menu.setDelegate(Some(delegate));
    }

    pub(super) fn button(&self) -> Option<Retained<NSStatusBarButton>> {
        self.item.button(self.mtm)
    }

    /// Opens the menu as a click on an item with one would; [`menu_closed`](Self::menu_closed)
    /// takes it back.
    pub(super) fn open_menu(&self) {
        self.item.setMenu(Some(&self.menu));
        if let Some(button) = self.button() {
            // SAFETY: `performClick:` takes any sender, or none.
            unsafe { button.performClick(None) };
        }
    }

    pub(super) fn menu_closed(&self) {
        self.item.setMenu(None);
    }

    /// `idle` makes the button read as nothing playing; the menu still shows `view`.
    pub(super) fn show(&self, view: Option<&PlayerView>, idle: bool) {
        let state = view.map_or(PlayState::Stopped, |v| v.state);
        let button_state = if idle { PlayState::Stopped } else { state };
        let mode = icon_mode(
            button_state,
            NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion(),
        );
        let button = self.button();
        match (mode, &button) {
            (IconMode::Animated, Some(button)) => self.animate(button),
            _ => self.stop_animation(),
        }
        if let Some(button) = button {
            match mode {
                IconMode::Animated => {}
                IconMode::Static => button.setImage(Some(match button_state {
                    PlayState::Stopped => &self.icons.idle,
                    _ => &self.icons.playing,
                })),
                IconMode::Dim => button.setImage(Some(&self.icons.paused)),
            }
            let title = match view {
                Some(view) if button_state != PlayState::Stopped => status_title(view),
                _ => String::new(),
            };
            button.setImagePosition(if title.is_empty() {
                NSCellImagePosition::ImageOnly
            } else {
                NSCellImagePosition::ImageLeading
            });
            button.setTitle(&NSString::from_str(&title));
        }

        match view {
            Some(view) => self.title.setTitle(&NSString::from_str(&menu_title(view))),
            None => self.title.setTitle(ns_string!("")),
        }
        self.title.setHidden(view.is_none());
        self.title_separator.setHidden(view.is_none());
        self.toggle.setTitle(if is_active(state) {
            ns_string!("Pause")
        } else {
            ns_string!("Play")
        });
    }

    pub(super) fn show_updates(&self, check: bool, ready: Option<&str>) {
        self.check.setHidden(!check);
        if let Some(version) = ready {
            self.restart.setTitle(&NSString::from_str(&format!(
                "Restart to Update ({version})"
            )));
        }
        self.restart.setHidden(ready.is_none());
    }

    pub(super) fn cancel_tracking(&self) {
        self.menu.cancelTrackingWithoutAnimation();
    }

    /// Starts the animation unless it is running, so repeated views do not restart it.
    fn animate(&self, button: &Retained<NSStatusBarButton>) {
        let mut animation = self.animation.borrow_mut();
        if animation.is_some() {
            return;
        }
        button.setImage(Some(&animated_frame(0.0)));
        let (button, start) = (button.clone(), Instant::now());
        let tick = RcBlock::new(move |_: NonNull<NSTimer>| {
            button.setImage(Some(&animated_frame(start.elapsed().as_secs_f64())));
        });
        // SAFETY: the timer runs on the main run loop, the thread the block was made on.
        let timer =
            unsafe { NSTimer::timerWithTimeInterval_repeats_block(FRAME_INTERVAL, true, &tick) };
        // SAFETY: `NSRunLoopCommonModes` is a valid mode; it keeps the timer firing while the
        // menu tracks events.
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        *animation = Some(timer);
    }

    fn stop_animation(&self) {
        if let Some(timer) = self.animation.borrow_mut().take() {
            timer.invalidate();
        }
    }

    pub(super) fn remove(&self) {
        self.stop_animation();
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [PlayState; 4] = [
        PlayState::Playing,
        PlayState::Buffering,
        PlayState::Paused,
        PlayState::Stopped,
    ];

    #[test]
    fn menu_tags_round_trip() {
        for action in MENU_ACTIONS {
            assert_eq!(menu_action(menu_tag(action)), Some(action));
        }
        assert_eq!(menu_action(-1), None);
        assert_eq!(menu_action(MENU_ACTIONS.len() as isize), None);
    }

    fn sweep() -> impl Iterator<Item = f64> {
        (0..=1200).map(|i| f64::from(i) * 0.05)
    }

    #[test]
    fn bar_heights_stay_within_the_floor_and_the_logo() {
        for t in sweep() {
            for (i, height) in bar_heights(t).into_iter().enumerate() {
                let logo = LOGO_BARS[i].1;
                assert!(
                    height >= logo * BAR_FLOOR - 1e-9,
                    "bar {i} at {t}: {height}"
                );
                assert!(height <= logo + 1e-9, "bar {i} at {t}: {height}");
            }
        }
    }

    #[test]
    fn bars_move_out_of_step() {
        let heights = bar_heights(0.7);
        let levels: [f64; 4] = std::array::from_fn(|i| heights[i] / LOGO_BARS[i].1);
        for (i, a) in levels.iter().enumerate() {
            for b in &levels[i + 1..] {
                assert!((a - b).abs() > 1e-3, "{levels:?}");
            }
        }
    }

    #[test]
    fn bar_heights_change_gradually_between_frames() {
        let mut t = 0.0;
        while t < 60.0 {
            let (now, next) = (bar_heights(t), bar_heights(t + FRAME_INTERVAL));
            for i in 0..4 {
                let step = (next[i] - now[i]).abs() / LOGO_BARS[i].1;
                assert!(step < 0.2, "bar {i} at {t}: {step}");
            }
            t += FRAME_INTERVAL;
        }
    }

    #[test]
    fn each_bar_repeats_every_period() {
        for (i, period) in BAR_PERIODS.into_iter().enumerate() {
            for t in [0.0, 0.4, 1.1] {
                let (a, b) = (bar_heights(t)[i], bar_heights(t + period)[i]);
                assert!((a - b).abs() < 1e-9, "bar {i} at {t}");
            }
        }
    }

    #[test]
    fn icon_mode_follows_state_and_reduce_motion() {
        let modes = |reduce| ALL_STATES.map(|state| icon_mode(state, reduce));
        assert_eq!(
            modes(false),
            [
                IconMode::Animated,
                IconMode::Static,
                IconMode::Dim,
                IconMode::Static
            ]
        );
        assert_eq!(
            modes(true),
            [
                IconMode::Static,
                IconMode::Static,
                IconMode::Dim,
                IconMode::Static
            ]
        );
    }
}
