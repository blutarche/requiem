use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSCellImagePosition, NSEventMask, NSFont, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSStatusBar, NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, ns_string};
use requiem_core::PlayState;

use super::is_active;
use crate::view::{menu_title, status_title};
use crate::{PlayerView, UiAction};

/// Menu item tags index this.
const MENU_ACTIONS: [UiAction; 5] = [
    UiAction::Toggle,
    UiAction::Next,
    UiAction::Prev,
    UiAction::OpenGui,
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

struct Icons {
    playing: Option<Retained<NSImage>>,
    paused: Option<Retained<NSImage>>,
    idle: Option<Retained<NSImage>>,
}

fn symbol(name: &NSString, description: &NSString) -> Option<Retained<NSImage>> {
    let image =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(name, Some(description))?;
    image.setTemplate(true);
    Some(image)
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
    targeted: Vec<Retained<NSMenuItem>>,
    icons: Icons,
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
        let open = action_item(ns_string!("Open requiem"), UiAction::OpenGui);
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
            targeted: vec![toggle.clone(), next, prev, open, quit],
            toggle,
            icons: Icons {
                playing: symbol(ns_string!("waveform"), ns_string!("requiem: playing")),
                paused: symbol(ns_string!("pause.fill"), ns_string!("requiem: paused")),
                idle: symbol(ns_string!("music.note"), ns_string!("requiem")),
            },
        };
        status.show(None);
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

    pub(super) fn show(&self, view: Option<&PlayerView>) {
        let state = view.map_or(PlayState::Stopped, |v| v.state);
        let icon = match state {
            PlayState::Playing | PlayState::Buffering => &self.icons.playing,
            PlayState::Paused => &self.icons.paused,
            PlayState::Stopped => &self.icons.idle,
        };
        if let Some(button) = self.button() {
            button.setImage(icon.as_deref());
            let title = match view {
                Some(view) if state != PlayState::Stopped => status_title(view),
                _ if icon.is_none() => "requiem".to_owned(),
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

    pub(super) fn cancel_tracking(&self) {
        self.menu.cancelTrackingWithoutAnimation();
    }

    pub(super) fn remove(&self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_tags_round_trip() {
        for action in MENU_ACTIONS {
            assert_eq!(menu_action(menu_tag(action)), Some(action));
        }
        assert_eq!(menu_action(-1), None);
        assert_eq!(menu_action(MENU_ACTIONS.len() as isize), None);
    }
}
