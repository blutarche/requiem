//! The window's palette as GTK CSS, dark as on macOS or light, laid over the default theme.

use std::cell::RefCell;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, glib};

use super::portal;
use crate::vm::Appearance;

pub const SPOTIFY_GREEN: u32 = 0x1ED760;
pub const YOUTUBE_RED: u32 = 0xFF0033;

/// How long the start waits for the portal's first answer, so a light desktop's window does
/// not open dark and flip.
const PORTAL_WAIT: Duration = Duration::from_millis(250);

/// The palette the window is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

/// A palette: each token's value as the stylesheet writes it.
type Tokens = &'static [(&'static str, &'static str)];

/// The macOS window's dark palette.
const DARK: Tokens = &[
    ("bg", "#0A0A0C"),
    ("ink", "#F2EFEA"),
    ("ink2", "#C8C5BD"),
    ("muted", "#8C8C94"),
    ("muted2", "#5A5A63"),
    ("focus", "rgba(124, 112, 245, 0.6)"),
    ("focus-glow", "rgba(124, 112, 245, 0.25)"),
    ("accent", "#7C70F5"),
    ("selection", "rgba(124, 112, 245, 0.32)"),
    ("selected-ink", "#FFFFFF"),
    ("sidebar", "#07070A"),
    ("sidebar-line", "#18181E"),
    ("sidebar-ink", "#E8E5DE"),
    ("sidebar-icon", "#C8C5BD"),
    ("sidebar-muted", "#8A8A92"),
    ("sidebar-hover", "#14141A"),
    ("nav-active", "#15132B"),
    ("switch", "#0E0E14"),
    ("on", "#1C1A33"),
    ("bar", "#0E0E12"),
    ("bar-line", "#18181E"),
    ("hover", "#14141A"),
    ("playpause-ink", "#0A0A0C"),
    ("playpause-hover", "#9088F8"),
    ("trough", "#24242C"),
    ("disabled", "#24242C"),
    ("grad2", "#E86FD8"),
    ("grad3", "#FF8A5B"),
    ("line", "#24242C"),
    ("line2", "#34343F"),
    ("line3", "#43434F"),
    ("bad", "#E26F5C"),
    ("primary", "#6E63E0"),
    ("primary-hover", "#5A4FC0"),
    ("field", "#121216"),
    ("field-line", "#1A1A20"),
    ("cover", "#1A1A20"),
    ("row-hover", "#22222A"),
    ("row-alt", "#0E0E11"),
    ("row-line", "#121216"),
    ("head", "#121216"),
    ("lead", "#121216"),
    ("lead-hover", "#16161B"),
    ("lead-selected", "#1E1B36"),
    ("divider", "#1A1A20"),
    ("panel", "#121216"),
    ("float", "#121216"),
    ("toast-shadow", "rgba(0,0,0,0.5)"),
    ("banner", "#1A1A20"),
    ("banner-error", "#2A1715"),
    ("scrim", "rgba(10, 10, 12, 0.60)"),
    ("palette-shadow", "rgba(0,0,0,0.6)"),
    ("palette-field", "#1A1A20"),
    ("action-selected", "#2A2550"),
    ("popover", "#1A1A20"),
];

/// The light palette: the Blutarche Studio design system's cool neutral light theme. The
/// accent #5C51D0 is 5.4:1 on the page; muted text keeps 4.5:1 on hovered and selected rows
/// (at least 6.2:1 on the selection, 7.3:1 on hover).
const LIGHT: Tokens = &[
    ("bg", "#F4F4F6"),
    ("ink", "#0A0A0C"),
    ("ink2", "#18181D"),
    ("muted", "#4A4A52"),
    ("muted2", "#6C6C74"),
    ("focus", "rgba(92, 81, 208, 0.6)"),
    ("focus-glow", "rgba(92, 81, 208, 0.25)"),
    ("accent", "#5C51D0"),
    ("selection", "rgba(92, 81, 208, 0.18)"),
    ("selected-ink", "#0A0A0C"),
    ("sidebar", "#EDEDF0"),
    ("sidebar-line", "#D4D4D9"),
    ("sidebar-ink", "#0A0A0C"),
    ("sidebar-icon", "#18181D"),
    ("sidebar-muted", "#5E5E66"),
    ("sidebar-hover", "#DDDDE2"),
    ("nav-active", "#DCDAEC"),
    ("switch", "#E4E4E8"),
    ("on", "#D1CFE5"),
    ("bar", "#FFFFFF"),
    ("bar-line", "#DDDDE2"),
    ("hover", "#EAEAED"),
    ("playpause-ink", "#FFFFFF"),
    ("playpause-hover", "#5047B8"),
    ("trough", "#DCDCE0"),
    ("disabled", "#EAEAED"),
    ("grad2", "#D85BC8"),
    ("grad3", "#F07A4A"),
    ("line", "#DDDDE2"),
    ("line2", "#BFBFC6"),
    ("line3", "#8E8E96"),
    ("bad", "#B83A27"),
    ("primary", "#5046A9"),
    ("primary-hover", "#3F356E"),
    ("field", "#FFFFFF"),
    ("field-line", "#DDDDE2"),
    ("cover", "#EAEAED"),
    ("row-hover", "#EAEAED"),
    ("row-alt", "#FAFAFC"),
    ("row-line", "#EDEDF0"),
    ("head", "#FFFFFF"),
    ("lead", "#FFFFFF"),
    ("lead-hover", "#EAEAED"),
    ("lead-selected", "#E8E7F8"),
    ("divider", "#DDDDE2"),
    ("panel", "#FFFFFF"),
    ("float", "#FFFFFF"),
    ("toast-shadow", "rgba(0,0,0,0.12)"),
    ("banner", "#EDEDF0"),
    ("banner-error", "#FCEBE7"),
    ("scrim", "rgba(0, 0, 0, 0.35)"),
    ("palette-shadow", "rgba(0,0,0,0.18)"),
    ("palette-field", "#F4F4F6"),
    ("action-selected", "#E5E3F7"),
    ("popover", "#FFFFFF"),
];

/// Every class the window uses, its colours as `$token`s of a palette. Primary buttons fill
/// with the accent darkened toward accent-deep, as on macOS, so the white label keeps 4.5:1.
const TEMPLATE: &str = r#"
/* Sizes are px throughout: KDE's gtk-xft-dpi already carries the display scale, so pt-based
   defaults would scale twice on top of Wayland's own scale. */
window.requiem, window.requiem > * {
  background-color: $bg;
  color: $ink;
  font-size: 13px;
}
.requiem .root { background-color: $bg; }
.requiem * { outline-color: $focus; }

/* Sidebar */
.sidebar {
  background-color: $sidebar;
  border-right: 1px solid $sidebar-line;
  padding: 0 12px 12px 12px;
}
.app-title { font-weight: 700; font-size: 15px; color: $sidebar-ink; margin: 44px 14px 26px 14px; }
button.nav {
  background: none; border: none; box-shadow: none;
  border-radius: 6px; padding: 0 14px; min-height: 36px; margin-bottom: 4px;
  color: $sidebar-ink; font-weight: 600; font-size: 14px;
}
button.nav:hover { background-color: $sidebar-hover; }
button.nav.active { background-color: $nav-active; }
button.nav.active .icon { color: $accent; }
button.nav .icon { color: $sidebar-icon; }
.nav-count { color: $sidebar-muted; font-size: 12px; font-weight: 600; }

/* The service switch */
.switch { background-color: $switch; border: 1px solid $sidebar-line; border-radius: 6px; padding: 2px; margin-bottom: 8px; }
button.segment { background: none; border: none; box-shadow: none; border-radius: 4px; min-height: 24px; padding: 0; }
button.segment:hover { background-color: $sidebar-hover; }
button.segment.on { background-color: $on; }
button.segment.off { opacity: 0.55; }

/* Player bar */
.playerbar { background-color: $bar; border-top: 1px solid $bar-line; padding: 0 20px; min-height: 72px; }
.bar-title { font-weight: 600; font-size: 14px; }
.bar-artist { color: $muted; font-size: 12px; }
button.bar-artist { background: none; border: none; box-shadow: none; padding: 0; min-height: 0; }
button.bar-artist:hover label { color: $ink; text-decoration-line: underline; }
.time { color: $muted; font-size: 12px; font-feature-settings: "tnum"; }
button.transport { background: none; border: none; box-shadow: none; border-radius: 999px; min-width: 32px; min-height: 32px; padding: 0; color: $ink2; }
button.transport:hover { color: $ink; background-color: $hover; }
button.transport:disabled { color: $muted2; }
button.playpause { background-color: $accent; color: $playpause-ink; min-width: 36px; min-height: 36px; }
button.playpause:hover { background-color: $playpause-hover; color: $playpause-ink; }
button.playpause:disabled { background-color: $disabled; color: $muted2; }

scale.seek trough, scale.volume trough { background-color: $trough; min-height: 4px; border-radius: 2px; border: none; }
scale.seek highlight { background-color: $accent; border-radius: 2px; border: none; }
scale.volume highlight { background-image: linear-gradient(to right, $accent, $grad2, $grad3); border-radius: 2px; border: none; }
scale.seek slider, scale.volume slider { min-width: 10px; min-height: 10px; margin: -4px; background-color: $ink; border: none; box-shadow: none; opacity: 0; }
scale.seek:hover slider, scale.volume:hover slider { opacity: 1; }
scale { padding: 6px 0; }
/* The default theme's negative margins leave a sliver of fill at zero. */
scale.seek highlight, scale.volume highlight { margin: 0; min-width: 0; min-height: 0; }

/* Pages */
.page { background-color: $bg; }
.page-title { font-size: 22px; font-weight: 700; }
.shelf-title { font-size: 17px; font-weight: 700; }
.section-title { font-size: 17px; font-weight: 700; }
.muted { color: $muted; }
.muted-2 { color: $muted2; }
.small { font-size: 12px; }
.title { font-weight: 600; }
.subtitle { color: $muted; font-size: 12px; }
.error-text { color: $bad; }

button.btn {
  background-color: $bg; background-image: none; box-shadow: none;
  border: 1px solid $line2; border-radius: 6px; padding: 0 12px; min-height: 34px;
  color: $ink; font-weight: 600;
}
button.btn:hover { background-color: $hover; border-color: $line3; }
button.btn:disabled { color: $muted2; border-color: $disabled; }
button.btn.primary { background-color: $primary; border-color: $primary; color: #FFFFFF; }
button.btn.primary:hover { background-color: $primary-hover; border-color: $primary-hover; }
button.btn.primary:disabled { background-color: $disabled; border-color: $disabled; color: $muted2; }
button.btn.small { min-height: 28px; padding: 0 10px; }
button.icon-btn { background: none; border: none; box-shadow: none; border-radius: 6px; min-width: 28px; min-height: 28px; padding: 0; color: $muted; }
button.icon-btn:hover { background-color: $hover; color: $ink; }
button.icon-btn:disabled { color: $line2; background: none; }
button.round-play { background-color: $primary; border-radius: 999px; min-width: 44px; min-height: 44px; padding: 0; color: #FFFFFF; border: none; box-shadow: none; }
button.round-play:hover { background-color: $primary-hover; }
button.round-play:disabled { background-color: $disabled; color: $muted2; }

/* Chips */
button.chip { background-color: transparent; background-image: none; box-shadow: none; border: 1px solid $line2; border-radius: 999px; padding: 0 12px; min-height: 26px; color: $ink2; font-weight: 600; }
button.chip:hover { background-color: $hover; }
button.chip.on { background-color: $on; border-color: $on; color: $ink; }

/* Fields */
entry.field {
  background-color: $field; background-image: none; box-shadow: none;
  border: 1px solid $field-line; border-radius: 6px; min-height: 34px; padding: 0 10px; color: $ink;
}
entry.field:focus-within { border-color: $accent; box-shadow: 0 0 0 2px $focus-glow; }
entry.field image { color: $muted; }
dropdown.sort > button { background-color: $field; background-image: none; box-shadow: none; border: 1px solid $field-line; border-radius: 6px; min-height: 30px; color: $ink; }

/* Covers and cards */
.cover { background-color: $cover; border-radius: 8px; }
.cover.round { border-radius: 999px; }
.cover.small { border-radius: 4px; }
.cover .icon { color: $muted2; }
.card { border-radius: 10px; padding: 6px; }
.card:hover { background-color: $row-hover; }
.card-title { font-weight: 600; font-size: 13px; }
.card-sub { color: $muted; font-size: 12px; }
.tile { border-radius: 8px; padding: 4px 6px; }
.tile:hover { background-color: $row-hover; }

listview, gridview { background-color: transparent; }
listview.cards > row, gridview.cards > child { padding: 0; background: none; border-radius: 10px; }
gridview.cards { padding-top: 2px; }
gridview.cards > child { padding-bottom: 8px; }
/* The default theme's spacing for icon grids, which would shrink every card. */
gridview.cards > child box { margin: 0; border-spacing: 0; }
gridview.cards > child > box > box { padding: 6px; }
listview.cards > row:hover, gridview.cards > child:hover { background-color: $row-hover; }
.card.selected, listview.cards > row:selected, gridview.cards > child:selected { box-shadow: inset 0 0 0 2px $accent; }
listview.cards > row:focus, gridview.cards > child:focus { outline: none; }

/* Song tables */
.table-head { background-color: $head; border-radius: 6px 6px 0 0; min-height: 32px; padding: 0 12px; }
.table-head label { color: $muted; font-size: 11px; font-weight: 600; letter-spacing: 1px; }
listview.songs > row { padding: 0; min-height: 36px; border-bottom: 1px solid $row-line; }
listview.songs > row:nth-child(odd) { background-color: $bg; }
listview.songs > row:nth-child(even) { background-color: $row-alt; }
listview.songs > row:hover { background-color: $row-hover; }
/* The default theme inks a selected row white, which the light selection would not show. */
listview.songs > row:selected { background-color: $selection; color: $selected-ink; }
listview.songs > row:selected:focus { outline: none; }
.song { padding: 0 12px; min-height: 36px; }
.song.played { color: $muted; }
.song.current { background-color: $selection; box-shadow: inset 2px 0 0 $accent; }
.song .artists, .song .album { color: $ink2; }
.song.played .artists, .song.played .album, .song .num { color: $muted; }
.song .time { color: $ink2; }
/* A row's "+" shows on hover and on the keyboard's row. */
.song button.add { opacity: 0; }
.song:hover button.add, row:selected .song button.add { opacity: 1; }

/* Lists of tracks beside covers */
.track-row { border-radius: 8px; padding: 4px 6px; }
.track-row:hover { background-color: $row-hover; }
.track-row.selected { background-color: $selection; }

/* The top result */
.lead { background-color: $lead; border-radius: 10px; padding: 16px; }
.lead:hover { background-color: $lead-hover; }
.lead.selected { background-color: $lead-selected; }
.lead-title { font-size: 20px; font-weight: 700; }

/* Empty states */
.empty-heading { font-size: 17px; font-weight: 700; }
.empty .icon { color: $muted2; }

/* Settings */
.account { border-top: 1px solid $divider; padding: 14px 0; }
.account:last-child { border-bottom: 1px solid $divider; }
.code { font-size: 28px; font-weight: 700; letter-spacing: 4px; font-family: monospace; }
.signin { background-color: $panel; border: 1px solid $line; border-radius: 8px; padding: 24px; }

/* Toast and banner */
.toast { background-color: $float; border: 1px solid $line; border-radius: 6px; padding: 8px 8px 8px 16px; box-shadow: 0 6px 20px $toast-shadow; }
.toast.error { border-left: 3px solid $bad; }
.banner { background-color: $banner; padding: 6px 24px; border-bottom: 1px solid $line; }
.banner.error { background-color: $banner-error; }

/* The command palette */
.scrim { background-color: $scrim; }
.palette { background-color: $float; border: 1px solid $line2; border-radius: 8px; padding: 8px; box-shadow: 0 12px 40px $palette-shadow; }
.palette entry.field { background-color: $palette-field; border-color: $accent; }
.action { padding: 0 14px; border-radius: 6px; }
.action label { font-size: 14px; font-weight: 500; }
.action.selected { background-color: $action-selected; }
.action.disabled label { color: $muted2; }
.shortcut { color: $muted; font-size: 12px; font-weight: 600; }

scrollbar { background: none; border: none; }
scrollbar slider { background-color: $line2; min-width: 6px; min-height: 6px; border-radius: 3px; }
popover.menu contents { background-color: $popover; }
"#;

fn tokens(mode: Mode) -> Tokens {
    match mode {
        Mode::Dark => DARK,
        Mode::Light => LIGHT,
    }
}

fn value(mode: Mode, name: &str) -> Option<&'static str> {
    tokens(mode)
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| *v)
}

/// `template` with each `$token` replaced by its value in `mode`'s palette; `Err` names the
/// first token the palette lacks.
fn substitute(template: &str, mode: Mode) -> Result<String, String> {
    let mut out = String::with_capacity(template.len() + 1024);
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(after.len());
        let name = &after[..len];
        out.push_str(value(mode, name).ok_or_else(|| name.to_owned())?);
        rest = &after[len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The stylesheet in `mode`'s palette.
fn css(mode: Mode) -> String {
    substitute(TEMPLATE, mode).expect("every token of the stylesheet is in both palettes")
}

/// `mode`'s `#RRGGBB` token `name` as `0xRRGGBB`.
fn hex(mode: Mode, name: &str) -> u32 {
    value(mode, name)
        .and_then(|v| v.strip_prefix('#'))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .expect("the token is a #RRGGBB colour")
}

/// What light or dark the window is in for `chosen`: System follows the portal's answer, then
/// GTK's, and is dark, as the window always was, when the desktop says nothing.
pub fn resolve(chosen: Appearance, portal: Option<Mode>, gtk: Option<Mode>) -> Mode {
    match chosen {
        Appearance::Light => Mode::Light,
        Appearance::Dark => Mode::Dark,
        Appearance::System => portal.or(gtk).unwrap_or(Mode::Dark),
    }
}

struct State {
    chosen: Appearance,
    portal: Option<Mode>,
    gtk: Option<Mode>,
    mode: Mode,
    provider: Option<gtk::CssProvider>,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State {
            chosen: Appearance::System,
            portal: None,
            gtk: None,
            mode: Mode::Dark,
            provider: None,
        })
    };
}

/// The window's background, for cairo drawing and snapshots.
pub fn bg() -> u32 {
    hex(mode(), "bg")
}

/// Muted text's colour, for icons drawn into textures.
pub fn muted() -> u32 {
    hex(mode(), "muted")
}

pub fn mode() -> Mode {
    STATE.with_borrow(|s| s.mode)
}

/// The appearance Settings shows as chosen.
pub fn appearance() -> Appearance {
    STATE.with_borrow(|s| s.chosen)
}

/// Lays the palette `chosen` resolves to over the default theme, and starts following the
/// desktop's preference for when System is chosen.
pub fn install(chosen: Appearance) {
    let first = portal::watch(if chosen == Appearance::System {
        PORTAL_WAIT
    } else {
        Duration::ZERO
    });
    let gtk = gtk::Settings::default().and_then(|s| {
        let scheme = gtk_scheme(&s);
        s.connect_notify_local(Some(GTK_SCHEME), |s, _| {
            let scheme = gtk_scheme(s);
            STATE.with_borrow_mut(|st| st.gtk = scheme);
            apply();
        });
        scheme
    });
    STATE.with_borrow_mut(|s| {
        s.chosen = chosen;
        s.portal = first;
        s.gtk = gtk;
    });
    apply();
}

/// Shows the window in `chosen` at once.
pub fn set_appearance(chosen: Appearance) {
    STATE.with_borrow_mut(|s| s.chosen = chosen);
    apply();
}

/// The portal's answer, on the main loop.
pub fn set_portal(mode: Option<Mode>) {
    STATE.with_borrow_mut(|s| s.portal = mode);
    apply();
}

/// GTK 4.20's system-wide colour scheme; older GTK lacks the setting.
const GTK_SCHEME: &str = "gtk-interface-color-scheme";

/// What `gtk-interface-color-scheme` says, where GTK has it: 2 dark, 3 light.
fn gtk_scheme(settings: &gtk::Settings) -> Option<Mode> {
    settings.find_property(GTK_SCHEME)?;
    let v = settings.property_value(GTK_SCHEME);
    match glib::EnumValue::from_value(&v)?.1.value() {
        2 => Some(Mode::Dark),
        3 => Some(Mode::Light),
        _ => None,
    }
}

/// Swaps the stylesheet when the resolved palette changes, and redraws what draws its own
/// colours.
fn apply() {
    let (mode, old) = STATE.with_borrow(|s| {
        (
            resolve(s.chosen, s.portal, s.gtk),
            s.provider.as_ref().map(|p| (p.clone(), s.mode)),
        )
    });
    if matches!(old, Some((_, m)) if m == mode) {
        return;
    }
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(mode == Mode::Dark);
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&css(mode));
    STATE.with_borrow_mut(|s| {
        s.mode = mode;
        s.provider = Some(provider.clone());
    });
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    if let Some((old, _)) = old {
        gtk::style_context_remove_provider_for_display(&display, &old);
        for w in gtk::Window::list_toplevels() {
            redraw_drawn(&w);
        }
    }
}

/// Redraws what under `widget` takes its colours from Rust rather than CSS: drawing areas
/// (the badges' rings) and the fields' icons keep their old picture otherwise.
fn redraw_drawn(widget: &gtk::Widget) {
    if widget.is::<gtk::DrawingArea>() {
        widget.queue_draw();
    }
    super::widgets::recolor_field(widget);
    let mut child = widget.first_child();
    while let Some(c) = child {
        redraw_drawn(&c);
        child = c.next_sibling();
    }
}

/// Adds `classes` to `widget`, for builders.
pub fn classes<W: IsA<gtk::Widget>>(widget: &W, classes: &[&str]) {
    for c in classes {
        widget.add_css_class(c);
    }
}

/// A label with `classes`, left-aligned, ellipsized at its end.
pub fn label(text: &str, classes_: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    classes(&l, classes_);
    l
}

/// Lets `label` wrap to a second line, ellipsizing only past that.
pub fn two_lines(label: &gtk::Label) {
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_lines(2);
}

/// Sets `label`'s text unless it already shows it, so an unchanged label is not relaid out.
pub fn set_text(label: &gtk::Label, text: &str) {
    if label.text() != text {
        label.set_text(text);
    }
}

/// Toggles `class` on `widget`.
pub fn toggle_class<W: IsA<gtk::Widget>>(widget: &W, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// Runs `f` soon, for work that must not run inside a size allocation. Ahead of the next
/// layout and frame: at default idle priority, slow frames would put it off indefinitely.
pub fn later(f: impl FnOnce() + 'static) {
    let mut f = Some(f);
    glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
        if let Some(f) = f.take() {
            f();
        }
        glib::ControlFlow::Break
    });
}

/// `hex` as cairo's red, green and blue, 0 to 1.
pub fn rgb(hex: u32) -> (f64, f64, f64) {
    let c = |shift: u32| f64::from(((hex >> shift) & 0xff) as u8) / 255.0;
    (c(16), c(8), c(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_palettes_fill_every_token_and_name_the_same_ones() {
        for mode in [Mode::Dark, Mode::Light] {
            let css = substitute(TEMPLATE, mode).unwrap();
            assert!(!css.contains('$'), "{mode:?}");
        }
        let names = |t: Tokens| t.iter().map(|(n, _)| *n).collect::<Vec<_>>();
        assert_eq!(names(DARK), names(LIGHT));
    }

    #[test]
    fn tokens_end_at_the_first_character_outside_a_name() {
        assert_eq!(
            substitute("a { color: $ink2; b: $ink }", Mode::Dark).unwrap(),
            "a { color: #C8C5BD; b: #F2EFEA }"
        );
        assert_eq!(substitute("$nope;", Mode::Dark), Err("nope".into()));
    }

    #[test]
    fn the_rust_side_colours_are_the_stylesheets() {
        assert_eq!(hex(Mode::Dark, "bg"), 0x0A0A0C);
        assert_eq!(hex(Mode::Dark, "muted"), 0x8C8C94);
        assert_eq!(hex(Mode::Light, "bg"), 0xF4F4F6);
    }

    #[test]
    fn system_follows_the_portal_then_gtk_and_is_dark_without_either() {
        use Mode::{Dark, Light};
        let system = Appearance::System;
        assert_eq!(resolve(Appearance::Light, Some(Dark), Some(Dark)), Light);
        assert_eq!(resolve(Appearance::Dark, Some(Light), None), Dark);
        assert_eq!(resolve(system, Some(Light), Some(Dark)), Light);
        assert_eq!(resolve(system, None, Some(Light)), Light);
        assert_eq!(resolve(system, None, None), Dark);
    }
}
