//! Small widgets and helpers the pages share.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

use super::icons::{self, Icon};
use super::theme;
use crate::view::{ADD_W, DURATION_W, column_widths};

/// Attaches `value` to `obj` under `key`, for `attached` to find. Each key holds one type.
pub fn attach<T: 'static>(obj: &impl IsA<glib::Object>, key: &str, value: T) {
    // SAFETY: `attached` reads the key back as the same `T`; each key is used with one type.
    unsafe { obj.set_data(key, value) };
}

/// What `attach` put on `obj` under `key`.
pub fn attached<'a, T: 'static>(obj: &'a impl IsA<glib::Object>, key: &str) -> Option<&'a T> {
    // SAFETY: `attach` stored a `T` under this key, which lives as long as `obj`.
    unsafe { obj.data::<T>(key).map(|p| p.as_ref()) }
}

/// Runs `f` on a primary click released over `widget`, with the click count.
pub fn on_click(widget: &impl IsA<gtk::Widget>, f: impl Fn(i32) + 'static) {
    let g = gtk::GestureClick::new();
    g.set_button(gdk::BUTTON_PRIMARY);
    g.connect_released(move |g, n, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        f(n);
    });
    widget.add_controller(g);
}

/// Runs `f` on a secondary click, or a long press, at the point in `widget`.
pub fn on_menu(widget: &impl IsA<gtk::Widget>, f: impl Fn(f64, f64) + 'static) {
    let f = std::rc::Rc::new(f);
    let g = gtk::GestureClick::new();
    g.set_button(gdk::BUTTON_SECONDARY);
    let menu = f.clone();
    g.connect_pressed(move |g, _, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        menu(x, y);
    });
    widget.add_controller(g);
    let press = gtk::GestureLongPress::new();
    press.set_touch_only(true);
    press.connect_pressed(move |_, x, y| f(x, y));
    widget.add_controller(press);
}

/// A menu item for `popup_menu`: its label, whether it is enabled, and what it does.
pub type MenuItem = (String, bool, Box<dyn Fn()>);

/// Pops a menu of `items` up at (`x`, `y`) in `widget`; it goes away once one is chosen.
pub fn popup_menu(widget: &impl IsA<gtk::Widget>, x: f64, y: f64, items: Vec<MenuItem>) {
    let popover = gtk::Popover::new();
    popover.add_css_class("menu");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_halign(gtk::Align::Start);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, enabled, run) in items {
        let b = gtk::Button::with_label(&label);
        b.add_css_class("flat");
        b.add_css_class("menu-item");
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        b.set_sensitive(enabled);
        let p = popover.downgrade();
        b.connect_clicked(move |_| {
            if let Some(p) = p.upgrade() {
                p.popdown();
            }
            run();
        });
        list.append(&b);
    }
    popover.set_child(Some(&list));
    popover.set_parent(widget);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|p| {
        let p = p.clone();
        // Unparented once closed, after GTK is done with it.
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}

/// An empty state's button: its title and what it runs.
pub type Action<'a> = (&'a str, Box<dyn Fn()>);
/// An empty state's button: its title, a mark before it, and what it runs.
pub type MarkedAction = (String, Option<gtk::Widget>, Box<dyn Fn()>);

/// A page's empty state: an icon, a heading, a line under it, and buttons.
pub struct EmptyState {
    pub root: gtk::Box,
    icon: gtk::DrawingArea,
    heading: gtk::Label,
    sub: gtk::Label,
    buttons: gtk::Box,
}

impl EmptyState {
    pub fn new() -> EmptyState {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("empty");
        root.set_valign(gtk::Align::Center);
        root.set_halign(gtk::Align::Center);
        root.set_vexpand(true);
        root.set_hexpand(true);
        let icon = icons::icon(Icon::Home, 32);
        icon.set_margin_bottom(14);
        let heading = gtk::Label::new(None);
        heading.add_css_class("empty-heading");
        heading.set_justify(gtk::Justification::Center);
        heading.set_wrap(true);
        let sub = gtk::Label::new(None);
        sub.add_css_class("muted");
        sub.set_margin_top(6);
        sub.set_justify(gtk::Justification::Center);
        sub.set_wrap(true);
        sub.set_max_width_chars(60);
        sub.set_selectable(true);
        sub.set_can_focus(false);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::Center);
        buttons.set_margin_top(18);
        root.append(&icon);
        root.append(&heading);
        root.append(&sub);
        root.append(&buttons);
        EmptyState {
            root,
            icon,
            heading,
            sub,
            buttons,
        }
    }

    /// Shows `heading` and `sub`, with one button `action` that runs `on`.
    pub fn set(&self, icon: Icon, heading: &str, sub: &str, action: Option<Action>) {
        let actions: Vec<MarkedAction> = action
            .into_iter()
            .map(|(t, f)| (t.to_owned(), None, f))
            .collect();
        self.set_actions(icon, heading, sub, actions);
    }

    /// Shows `heading` and `sub` with a button for each action: its title, a mark before it,
    /// and what it runs. The first is the primary.
    pub fn set_actions(&self, icon: Icon, heading: &str, sub: &str, actions: Vec<MarkedAction>) {
        icons::swap(&self.icon, icon);
        theme::set_text(&self.heading, heading);
        theme::set_text(&self.sub, sub);
        self.sub.set_visible(!sub.is_empty());
        while let Some(c) = self.buttons.first_child() {
            self.buttons.remove(&c);
        }
        let primary = actions.len() == 1;
        for (title, mark, run) in actions {
            let b = gtk::Button::new();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            if let Some(m) = mark {
                content.append(&m);
            }
            content.append(&gtk::Label::new(Some(&title)));
            b.set_child(Some(&content));
            b.add_css_class("btn");
            if primary {
                b.add_css_class("primary");
            }
            b.connect_clicked(move |_| run());
            self.buttons.append(&b);
        }
        self.buttons
            .set_visible(self.buttons.first_child().is_some());
    }
}

/// A text field with `placeholder`, named `name` for screen readers, and `icon` at its start.
pub fn field(placeholder: &str, name: &str, icon: Option<Icon>) -> gtk::Entry {
    let e = gtk::Entry::new();
    e.add_css_class("field");
    e.set_placeholder_text(Some(placeholder));
    e.update_property(&[gtk::accessible::Property::Label(name)]);
    if let Some(i) = icon {
        e.set_primary_icon_paintable(icons::paintable(i, 16, theme::MUTED).as_ref());
    }
    e
}

/// Watches `entry`'s keys before its text does; `f` returns true for a key it handled.
pub fn on_field_key(
    entry: &gtk::Entry,
    f: impl Fn(&gtk::Entry, gdk::Key, gdk::ModifierType) -> bool + 'static,
) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = entry.downgrade();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let Some(e) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if f(&e, key, mods) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    entry.add_controller(keys);
}

/// A page title, as each page has at its top left.
pub fn page_title(text: &str) -> gtk::Label {
    theme::label(text, &["page-title"])
}

/// A scrolled window with only a vertical scroll bar.
pub fn vscroll(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_hscrollbar_policy(gtk::PolicyType::Never);
    s.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    s.set_child(Some(child));
    s.set_vexpand(true);
    s.set_hexpand(true);
    s
}

/// Scrolls `scroll` the least that shows `y..y + h` of its content.
pub fn reveal(scroll: &gtk::ScrolledWindow, y: f64, h: f64) {
    let adj = scroll.vadjustment();
    let (top, page) = (adj.value(), adj.page_size());
    if y < top {
        adj.set_value(y);
    } else if y + h > top + page {
        adj.set_value(y + h - page);
    }
}

/// `widget`'s top edge and height in `ancestor`'s coordinates.
pub fn span_in(
    widget: &impl IsA<gtk::Widget>,
    ancestor: &impl IsA<gtk::Widget>,
) -> Option<(f64, f64)> {
    let b = widget.compute_bounds(ancestor)?;
    Some((f64::from(b.y()), f64::from(b.height())))
}

mod imp {
    use super::*;

    /// Lays its children out as a song table's columns: title, artists, album, "+" and
    /// time, at the widths `column_widths` gives for its own width.
    #[derive(Default)]
    pub struct Columns {
        pub add: Cell<bool>,
        pub album: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Columns {
        const NAME: &'static str = "RequiemColumns";
        type Type = super::Columns;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Columns {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Columns {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                return (0, 600, -1, -1);
            }
            let mut h = (0, 0);
            let mut child = self.obj().first_child();
            while let Some(c) = child {
                let (min, nat, _, _) = c.measure(orientation, -1);
                h = (h.0.max(min), h.1.max(nat));
                child = c.next_sibling();
            }
            (h.0, h.1, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let w = column_widths(f64::from(width), self.add.get(), self.album.get());
            let spans = [
                Some(w.title),
                Some(w.artists),
                w.album,
                self.add.get().then_some(ADD_W),
                Some(DURATION_W),
            ];
            let mut x = 0.0;
            let mut child = self.obj().first_child();
            let mut i = 0;
            while let Some(c) = child {
                let span = spans.get(i).copied().flatten();
                c.set_child_visible(span.is_some());
                if let Some(cw) = span {
                    // A little room between text columns, so ellipses do not touch.
                    let gap = if i < 3 { 12.0 } else { 0.0 };
                    let alloc =
                        gtk::Allocation::new(x as i32, 0, ((cw - gap).max(0.0)) as i32, height);
                    c.size_allocate(&alloc, baseline);
                    x += cw;
                }
                child = c.next_sibling();
                i += 1;
            }
        }
    }
}

glib::wrapper! {
    pub struct Columns(ObjectSubclass<imp::Columns>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Columns {
    /// Columns for `cells`: title, artists, album, "+" (when `add`) and time.
    pub fn new(cells: &[&gtk::Widget], add: bool, album: bool) -> Columns {
        let c: Columns = glib::Object::new();
        c.imp().add.set(add);
        c.imp().album.set(album);
        for w in cells {
            w.set_parent(&c);
        }
        c
    }

    pub fn set_album(&self, album: bool) {
        if self.imp().album.replace(album) != album {
            self.queue_allocate();
        }
    }
}
