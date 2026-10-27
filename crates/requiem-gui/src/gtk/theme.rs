//! The macOS window's dark palette as GTK CSS, forced dark whatever the desktop's theme.

use gtk::prelude::*;
use gtk::{gdk, glib};

pub const BG: u32 = 0x0A0A0C;
pub const MUTED: u32 = 0x8C8C94;
pub const SPOTIFY_GREEN: u32 = 0x1ED760;
pub const YOUTUBE_RED: u32 = 0xFF0033;

/// `hex` as cairo's red, green and blue, 0 to 1.
pub fn rgb(hex: u32) -> (f64, f64, f64) {
    let c = |shift: u32| f64::from(((hex >> shift) & 0xff) as u8) / 255.0;
    (c(16), c(8), c(0))
}

/// The palette's tokens and every class the window uses. Primary buttons fill with the accent
/// darkened 40% toward accent-deep (#5A4FC0), as on macOS, so the white label keeps 4.6:1.
const CSS: &str = r#"
/* Sizes are px throughout: KDE's gtk-xft-dpi already carries the display scale, so pt-based
   defaults would scale twice on top of Wayland's own scale. */
window.requiem, window.requiem > * {
  background-color: #0A0A0C;
  color: #F2EFEA;
  font-size: 13px;
}
.requiem .root { background-color: #0A0A0C; }
.requiem * { outline-color: rgba(124, 112, 245, 0.6); }

/* Sidebar */
.sidebar {
  background-color: #07070A;
  border-right: 1px solid #18181E;
  padding: 0 12px 12px 12px;
}
.app-title { font-weight: 700; font-size: 15px; color: #E8E5DE; margin: 44px 14px 26px 14px; }
button.nav {
  background: none; border: none; box-shadow: none;
  border-radius: 6px; padding: 0 14px; min-height: 36px; margin-bottom: 4px;
  color: #E8E5DE; font-weight: 600; font-size: 14px;
}
button.nav:hover { background-color: #14141A; }
button.nav.active { background-color: #15132B; }
button.nav.active .icon { color: #7C70F5; }
button.nav .icon { color: #C8C5BD; }
.nav-count { color: #8A8A92; font-size: 12px; font-weight: 600; }

/* The service switch */
.switch { background-color: #0E0E14; border: 1px solid #18181E; border-radius: 6px; padding: 2px; margin-bottom: 8px; }
button.segment { background: none; border: none; box-shadow: none; border-radius: 4px; min-height: 24px; padding: 0; }
button.segment:hover { background-color: #14141A; }
button.segment.on { background-color: #1C1A33; }
button.segment.off { opacity: 0.55; }

/* Player bar */
.playerbar { background-color: #0E0E12; border-top: 1px solid #18181E; padding: 0 20px; min-height: 72px; }
.bar-title { font-weight: 600; font-size: 14px; }
.bar-artist { color: #8C8C94; font-size: 12px; }
button.bar-artist { background: none; border: none; box-shadow: none; padding: 0; min-height: 0; }
button.bar-artist:hover label { color: #F2EFEA; text-decoration-line: underline; }
.time { color: #8C8C94; font-size: 12px; font-feature-settings: "tnum"; }
button.transport { background: none; border: none; box-shadow: none; border-radius: 999px; min-width: 32px; min-height: 32px; padding: 0; color: #C8C5BD; }
button.transport:hover { color: #F2EFEA; background-color: #14141A; }
button.transport:disabled { color: #5A5A63; }
button.playpause { background-color: #7C70F5; color: #0A0A0C; min-width: 36px; min-height: 36px; }
button.playpause:hover { background-color: #9088F8; color: #0A0A0C; }
button.playpause:disabled { background-color: #24242C; color: #5A5A63; }

scale.seek trough, scale.volume trough { background-color: #24242C; min-height: 4px; border-radius: 2px; border: none; }
scale.seek highlight { background-color: #7C70F5; border-radius: 2px; border: none; }
scale.volume highlight { background-image: linear-gradient(to right, #7C70F5, #E86FD8, #FF8A5B); border-radius: 2px; border: none; }
scale.seek slider, scale.volume slider { min-width: 10px; min-height: 10px; margin: -4px; background-color: #F2EFEA; border: none; box-shadow: none; opacity: 0; }
scale.seek:hover slider, scale.volume:hover slider { opacity: 1; }
scale { padding: 6px 0; }
/* The default theme's negative margins leave a sliver of fill at zero. */
scale.seek highlight, scale.volume highlight { margin: 0; min-width: 0; min-height: 0; }

/* Pages */
.page { background-color: #0A0A0C; }
.page-title { font-size: 22px; font-weight: 700; }
.shelf-title { font-size: 17px; font-weight: 700; }
.section-title { font-size: 17px; font-weight: 700; }
.muted { color: #8C8C94; }
.muted-2 { color: #5A5A63; }
.small { font-size: 12px; }
.title { font-weight: 600; }
.subtitle { color: #8C8C94; font-size: 12px; }
.error-text { color: #E26F5C; }

button.btn {
  background-color: #0A0A0C; background-image: none; box-shadow: none;
  border: 1px solid #34343F; border-radius: 6px; padding: 0 12px; min-height: 34px;
  color: #F2EFEA; font-weight: 600;
}
button.btn:hover { background-color: #14141A; border-color: #43434F; }
button.btn:disabled { color: #5A5A63; border-color: #24242C; }
button.btn.primary { background-color: #6E63E0; border-color: #6E63E0; color: #FFFFFF; }
button.btn.primary:hover { background-color: #5A4FC0; border-color: #5A4FC0; }
button.btn.primary:disabled { background-color: #24242C; border-color: #24242C; color: #5A5A63; }
button.btn.small { min-height: 28px; padding: 0 10px; }
button.icon-btn { background: none; border: none; box-shadow: none; border-radius: 6px; min-width: 28px; min-height: 28px; padding: 0; color: #8C8C94; }
button.icon-btn:hover { background-color: #14141A; color: #F2EFEA; }
button.icon-btn:disabled { color: #34343F; background: none; }
button.round-play { background-color: #6E63E0; border-radius: 999px; min-width: 44px; min-height: 44px; padding: 0; color: #FFFFFF; border: none; box-shadow: none; }
button.round-play:hover { background-color: #5A4FC0; }
button.round-play:disabled { background-color: #24242C; color: #5A5A63; }

/* Chips */
button.chip { background-color: transparent; background-image: none; box-shadow: none; border: 1px solid #34343F; border-radius: 999px; padding: 0 12px; min-height: 26px; color: #C8C5BD; font-weight: 600; }
button.chip:hover { background-color: #14141A; }
button.chip.on { background-color: #1C1A33; border-color: #1C1A33; color: #F2EFEA; }

/* Fields */
entry.field {
  background-color: #121216; background-image: none; box-shadow: none;
  border: 1px solid #1A1A20; border-radius: 6px; min-height: 34px; padding: 0 10px; color: #F2EFEA;
}
entry.field:focus-within { border-color: #7C70F5; box-shadow: 0 0 0 2px rgba(124, 112, 245, 0.25); }
entry.field image { color: #8C8C94; }
dropdown.sort > button { background-color: #121216; background-image: none; box-shadow: none; border: 1px solid #1A1A20; border-radius: 6px; min-height: 30px; color: #F2EFEA; }

/* Covers and cards */
.cover { background-color: #1A1A20; border-radius: 8px; }
.cover.round { border-radius: 999px; }
.cover.small { border-radius: 4px; }
.cover .icon { color: #5A5A63; }
.card { border-radius: 10px; padding: 6px; }
.card:hover { background-color: #22222A; }
.card-title { font-weight: 600; font-size: 13px; }
.card-sub { color: #8C8C94; font-size: 12px; }
.tile { border-radius: 8px; padding: 4px 6px; }
.tile:hover { background-color: #22222A; }

listview, gridview { background-color: transparent; }
listview.cards > row, gridview.cards > child { padding: 0; background: none; border-radius: 10px; }
gridview.cards { padding-top: 2px; }
gridview.cards > child { padding-bottom: 8px; }
/* The default theme's spacing for icon grids, which would shrink every card. */
gridview.cards > child box { margin: 0; border-spacing: 0; }
gridview.cards > child > box > box { padding: 6px; }
listview.cards > row:hover, gridview.cards > child:hover { background-color: #22222A; }
.card.selected, listview.cards > row:selected, gridview.cards > child:selected { box-shadow: inset 0 0 0 2px #7C70F5; }
listview.cards > row:focus, gridview.cards > child:focus { outline: none; }

/* Song tables */
.table-head { background-color: #121216; border-radius: 6px 6px 0 0; min-height: 32px; padding: 0 12px; }
.table-head label { color: #8C8C94; font-size: 11px; font-weight: 600; letter-spacing: 1px; }
listview.songs > row { padding: 0; min-height: 36px; border-bottom: 1px solid #121216; }
listview.songs > row:nth-child(odd) { background-color: #0A0A0C; }
listview.songs > row:nth-child(even) { background-color: #0E0E11; }
listview.songs > row:hover { background-color: #22222A; }
listview.songs > row:selected { background-color: rgba(124, 112, 245, 0.32); }
listview.songs > row:selected:focus { outline: none; }
.song { padding: 0 12px; min-height: 36px; }
.song.played { color: #8C8C94; }
.song.current { background-color: rgba(124, 112, 245, 0.32); box-shadow: inset 2px 0 0 #7C70F5; }
.song .artists, .song .album { color: #C8C5BD; }
.song.played .artists, .song.played .album, .song .num { color: #8C8C94; }
.song .time { color: #C8C5BD; }
/* A row's "+" shows on hover and on the keyboard's row. */
.song button.add { opacity: 0; }
.song:hover button.add, row:selected .song button.add { opacity: 1; }

/* Lists of tracks beside covers */
.track-row { border-radius: 8px; padding: 4px 6px; }
.track-row:hover { background-color: #22222A; }
.track-row.selected { background-color: rgba(124, 112, 245, 0.32); }

/* The top result */
.lead { background-color: #121216; border-radius: 10px; padding: 16px; }
.lead:hover { background-color: #16161B; }
.lead.selected { background-color: #1E1B36; }
.lead-title { font-size: 20px; font-weight: 700; }

/* Empty states */
.empty-heading { font-size: 17px; font-weight: 700; }
.empty .icon { color: #5A5A63; }

/* Settings */
.account { border-top: 1px solid #1A1A20; padding: 14px 0; }
.account:last-child { border-bottom: 1px solid #1A1A20; }
.code { font-size: 28px; font-weight: 700; letter-spacing: 4px; font-family: monospace; }
.signin { background-color: #121216; border: 1px solid #24242C; border-radius: 8px; padding: 24px; }

/* Toast and banner */
.toast { background-color: #121216; border: 1px solid #24242C; border-radius: 6px; padding: 8px 8px 8px 16px; box-shadow: 0 6px 20px rgba(0,0,0,0.5); }
.toast.error { border-left: 3px solid #E26F5C; }
.banner { background-color: #1A1A20; padding: 6px 24px; border-bottom: 1px solid #24242C; }
.banner.error { background-color: #2A1715; }

/* The command palette */
.scrim { background-color: rgba(10, 10, 12, 0.60); }
.palette { background-color: #121216; border: 1px solid #34343F; border-radius: 8px; padding: 8px; box-shadow: 0 12px 40px rgba(0,0,0,0.6); }
.palette entry.field { background-color: #1A1A20; border-color: #7C70F5; }
.action { padding: 0 14px; border-radius: 6px; }
.action label { font-size: 14px; font-weight: 500; }
.action.selected { background-color: #2A2550; }
.action.disabled label { color: #5A5A63; }
.shortcut { color: #8C8C94; font-size: 12px; font-weight: 600; }

scrollbar { background: none; border: none; }
scrollbar slider { background-color: #34343F; min-width: 6px; min-height: 6px; border-radius: 3px; }
popover.menu contents { background-color: #1A1A20; }
"#;

/// Forces the dark variant of the default theme, then lays the palette over it.
pub fn install() {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
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
