//! The window's parts outside the lists: the player bar, the sidebar, the top search result,
//! Now Playing, Settings, the sign-in panel, the Library's toolbar, the palette, the toast and
//! the banner. Each runs its actions through `ui::with`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use requiem_core::{Collection, ProviderId, Track};

use super::art::Cover;
use super::icons::{self, Icon};
use super::songs::TrackRows;
use super::theme;
use super::ui;
use super::widgets;
use crate::view::{RowText, rows, unix_now};
use crate::vm::{
    self, AccountRow, AccountRowState, Banner, Controls, Intent, LibraryChip, LibrarySort, List,
    PaletteAction, Place, SearchChip, Service, ServiceFilter, SessionForm, Toast, TopResult,
};

/// A sidebar item: icon, name, and an optional count at its right.
pub struct NavItem {
    pub button: gtk::Button,
    count: gtk::Label,
}

impl NavItem {
    fn new(icon: Icon, name: &str) -> NavItem {
        let b = gtk::Button::new();
        b.add_css_class("nav");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&icons::icon(icon, 20));
        let l = theme::label(name, &[]);
        l.set_hexpand(true);
        row.append(&l);
        let count = theme::label("", &["nav-count"]);
        count.set_visible(false);
        row.append(&count);
        b.set_child(Some(&row));
        b.set_focus_on_click(false);
        NavItem { button: b, count }
    }

    pub fn set_active(&self, on: bool) {
        theme::toggle_class(&self.button, "active", on);
    }

    pub fn set_count(&self, count: Option<&str>) {
        theme::set_text(&self.count, count.unwrap_or(""));
        self.count.set_visible(count.is_some());
    }
}

/// The sidebar: the app's name, the nav, the service switch and Settings.
pub struct Sidebar {
    pub root: gtk::Box,
    pub home: NavItem,
    pub search: NavItem,
    pub library: NavItem,
    pub queue: NavItem,
    pub settings: NavItem,
    switch: gtk::Box,
    segments: Vec<(ServiceFilter, gtk::Button)>,
}

impl Sidebar {
    pub fn new() -> Sidebar {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("sidebar");
        root.set_size_request(240, -1);
        // Its labels would otherwise pass their stretch up to the sidebar.
        root.set_hexpand(false);
        root.append(&theme::label("Requiem", &["app-title"]));
        let home = NavItem::new(Icon::Home, "Home");
        let search = NavItem::new(Icon::Search, "Search");
        let library = NavItem::new(Icon::Library, "Library");
        let queue = NavItem::new(Icon::ListMusic, "Queue");
        let settings = NavItem::new(Icon::Settings, "Settings");
        home.button.set_tooltip_text(Some("Home (Ctrl+1)"));
        search.button.set_tooltip_text(Some("Search (Ctrl+2)"));
        library.button.set_tooltip_text(Some("Library (Ctrl+3)"));
        queue.button.set_tooltip_text(Some("Queue (Ctrl+4)"));
        settings.button.set_tooltip_text(Some("Settings (Ctrl+,)"));
        for n in [&home, &search, &library, &queue] {
            root.append(&n.button);
        }
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        root.append(&spacer);
        let switch = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        switch.add_css_class("switch");
        switch.set_homogeneous(true);
        switch.update_property(&[gtk::accessible::Property::Label("Service")]);
        let mut segments = Vec::new();
        for filter in [
            ServiceFilter::All,
            ServiceFilter::Only(Service::Spotify),
            ServiceFilter::Only(Service::YouTubeMusic),
        ] {
            let b = gtk::Button::new();
            b.add_css_class("segment");
            b.set_focus_on_click(false);
            let mark = match filter {
                ServiceFilter::All => {
                    let side = 14.0;
                    let a = gtk::DrawingArea::new();
                    a.set_content_width(icons::pair_width(side).ceil() as i32);
                    a.set_content_height(side as i32 + 3);
                    a.set_draw_func(move |_, cr, _, _| icons::draw_pair(cr, side));
                    a
                }
                ServiceFilter::Only(s) => icons::mark(s, 14),
            };
            mark.set_halign(gtk::Align::Center);
            mark.set_valign(gtk::Align::Center);
            b.set_child(Some(&mark));
            b.set_tooltip_text(Some(filter.label()));
            b.update_property(&[gtk::accessible::Property::Label(filter.label())]);
            b.connect_clicked(move |_| {
                ui::with(|ui| ui.choose_filter(filter));
            });
            switch.append(&b);
            segments.push((filter, b));
        }
        switch.set_visible(false);
        root.append(&switch);
        root.append(&settings.button);
        for (n, view) in [
            (&home, crate::view::AppView::Home),
            (&search, crate::view::AppView::Search),
            (&library, crate::view::AppView::Library),
            (&queue, crate::view::AppView::Queue),
            (&settings, crate::view::AppView::Settings),
        ] {
            n.button.connect_clicked(move |_| {
                ui::with(|ui| {
                    if view == crate::view::AppView::Search {
                        ui.focus_search();
                    } else {
                        ui.show(view);
                    }
                });
            });
        }
        Sidebar {
            root,
            home,
            search,
            library,
            queue,
            settings,
            switch,
            segments,
        }
    }

    /// Shows the switch when both services can be chosen, with `filter` on.
    pub fn set_filter(&self, shown: bool, filter: ServiceFilter) {
        self.switch.set_visible(shown);
        for (f, b) in &self.segments {
            theme::toggle_class(b, "on", *f == filter);
            theme::toggle_class(b, "off", *f != filter && *f != ServiceFilter::All);
        }
    }
}

/// The player bar along the window's foot.
pub struct Bar {
    pub root: gtk::Box,
    cover: Cover,
    title: gtk::Label,
    artists: gtk::Label,
    artists_link: gtk::Button,
    prev: gtk::Button,
    play: gtk::Button,
    play_icon: gtk::DrawingArea,
    next: gtk::Button,
    elapsed: gtk::Label,
    total: gtk::Label,
    pub seek: gtk::Scale,
    pub volume: gtk::Scale,
    /// Set while the code moves a slider, so its handler does not send it back.
    setting: Rc<Cell<bool>>,
    playing: Cell<Option<bool>>,
    dragging: Rc<Cell<bool>>,
}

impl Bar {
    pub fn new() -> Bar {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("playerbar");
        root.set_size_request(-1, 72);
        // The track: cover and text, which opens Now Playing.
        let left = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        left.set_size_request(280, -1);
        left.set_hexpand(false);
        left.set_valign(gtk::Align::Center);
        let cover = Cover::new(48, &["small"], true);
        cover.root.set_valign(gtk::Align::Center);
        let open = gtk::Button::new();
        open.add_css_class("flat");
        open.add_css_class("bar-artist");
        open.set_child(Some(&cover.root));
        open.set_tooltip_text(Some("Show now playing (Ctrl+L)"));
        open.update_property(&[gtk::accessible::Property::Label("Show now playing")]);
        open.set_focus_on_click(false);
        open.connect_clicked(|_| {
            ui::with(|ui| ui.show_now_playing());
        });
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let title = theme::label("Nothing playing", &["bar-title"]);
        title.set_max_width_chars(1);
        let artists = theme::label("", &[]);
        // Its natural width, which the link button takes; a narrower bar ellipsizes it.
        artists.set_max_width_chars(40);
        let artists_link = gtk::Button::new();
        artists_link.add_css_class("bar-artist");
        artists_link.set_child(Some(&artists));
        artists_link.set_halign(gtk::Align::Start);
        artists_link.set_focus_on_click(false);
        artists_link.connect_clicked(|_| {
            ui::with(|ui| ui.go_to_playing_artist());
        });
        let title_click = gtk::GestureClick::new();
        title_click.connect_released(|_, _, _, _| {
            ui::with(|ui| ui.show_now_playing());
        });
        title.add_controller(title_click);
        text.append(&title);
        text.append(&artists_link);
        left.append(&open);
        left.append(&text);
        // The transport and the seek bar.
        let mid = gtk::Box::new(gtk::Orientation::Vertical, 2);
        mid.set_hexpand(true);
        mid.set_valign(gtk::Align::Center);
        mid.set_margin_start(24);
        mid.set_margin_end(24);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        buttons.set_halign(gtk::Align::Center);
        let prev = icons::icon_button(Icon::SkipBack, 18, "Previous", &["transport"]);
        prev.set_tooltip_text(Some("Previous (Ctrl+Left)"));
        let play_icon = icons::icon(Icon::Play, 18);
        let play = gtk::Button::new();
        play.set_child(Some(&play_icon));
        theme::classes(&play, &["transport", "playpause"]);
        play.set_tooltip_text(Some("Play (Space)"));
        let next = icons::icon_button(Icon::SkipForward, 18, "Next", &["transport"]);
        next.set_tooltip_text(Some("Next (Ctrl+Right)"));
        for b in [&prev, &play, &next] {
            b.set_focus_on_click(false);
            b.set_valign(gtk::Align::Center);
        }
        prev.connect_clicked(|_| {
            ui::with(|ui| ui.intent(Intent::Prev));
        });
        play.connect_clicked(|_| {
            ui::with(|ui| ui.intent(Intent::Toggle));
        });
        next.connect_clicked(|_| {
            ui::with(|ui| ui.intent(Intent::Next));
        });
        buttons.append(&prev);
        buttons.append(&play);
        buttons.append(&next);
        let seek_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let elapsed = theme::label("0:00", &["time"]);
        elapsed.set_xalign(1.0);
        elapsed.set_size_request(72, -1);
        let total = theme::label("", &["time"]);
        total.set_size_request(72, -1);
        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
        seek.add_css_class("seek");
        seek.set_draw_value(false);
        seek.set_hexpand(true);
        seek.set_focusable(false);
        seek.update_property(&[gtk::accessible::Property::Label("Seek")]);
        seek_row.append(&elapsed);
        seek_row.append(&seek);
        seek_row.append(&total);
        mid.append(&buttons);
        mid.append(&seek_row);
        // Volume.
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        right.set_size_request(200, -1);
        right.set_hexpand(false);
        right.set_halign(gtk::Align::End);
        right.set_valign(gtk::Align::Center);
        let vol_icon = icons::icon(Icon::Volume, 18);
        vol_icon.add_css_class("muted");
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        volume.add_css_class("volume");
        volume.set_draw_value(false);
        volume.set_size_request(100, -1);
        volume.set_focusable(false);
        volume.update_property(&[gtk::accessible::Property::Label("Volume")]);
        volume.set_tooltip_text(Some("Volume (Ctrl+Up, Ctrl+Down)"));
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        right.append(&spacer);
        right.append(&vol_icon);
        right.append(&volume);
        root.append(&left);
        root.append(&mid);
        root.append(&right);

        let setting = Rc::new(Cell::new(false));
        let dragging = Rc::new(Cell::new(false));
        let s = setting.clone();
        let d = dragging.clone();
        seek.connect_change_value(move |_, _, value| {
            if !s.get() {
                d.set(true);
                ui::with(|ui| ui.preview_seek(value.clamp(0.0, 1.0)));
            }
            glib::Propagation::Proceed
        });
        // The seek lands when the pointer lets go, as on macOS.
        let release = gtk::GestureClick::new();
        release.set_propagation_phase(gtk::PropagationPhase::Capture);
        let d = dragging.clone();
        let sk = seek.downgrade();
        release.connect_released(move |_, _, _, _| {
            if let Some(sk) = sk.upgrade() {
                d.set(false);
                let v = sk.value();
                ui::with(|ui| ui.seek_fraction(v));
            }
        });
        let d = dragging.clone();
        let sk = seek.downgrade();
        release.connect_stopped(move |_| {
            if d.replace(false)
                && let Some(sk) = sk.upgrade()
            {
                let v = sk.value();
                ui::with(|ui| ui.seek_fraction(v));
            }
        });
        seek.add_controller(release);
        let drag_end = gtk::GestureDrag::new();
        drag_end.set_propagation_phase(gtk::PropagationPhase::Capture);
        let d = dragging.clone();
        let sk = seek.downgrade();
        drag_end.connect_drag_end(move |_, _, _| {
            if d.replace(false)
                && let Some(sk) = sk.upgrade()
            {
                let v = sk.value();
                ui::with(|ui| ui.seek_fraction(v));
            }
        });
        seek.add_controller(drag_end);
        let s = setting.clone();
        volume.connect_value_changed(move |v| {
            if !s.get() {
                let to = v.value().round().clamp(0.0, 100.0) as u8;
                ui::with(|ui| ui.set_volume(to));
            }
        });
        Bar {
            root,
            cover,
            title,
            artists,
            artists_link,
            prev,
            play,
            play_icon,
            next,
            elapsed,
            total,
            seek,
            volume,
            setting,
            playing: Cell::new(None),
            dragging,
        }
    }

    pub fn set_track(
        &self,
        track: Option<(&str, &str)>,
        link: Option<&str>,
        total: &str,
        artwork: Option<&str>,
        badge: Option<Service>,
    ) {
        self.cover.set_url(artwork);
        self.cover.set_badge(badge);
        let (title, artists) = track.unwrap_or(("Nothing playing", ""));
        theme::set_text(&self.title, title);
        theme::toggle_class(&self.title, "muted", track.is_none());
        theme::set_text(&self.artists, artists);
        self.title.set_tooltip_text(track.map(|t| t.0));
        self.artists_link.set_sensitive(link.is_some());
        self.artists_link.set_visible(!artists.is_empty());
        self.artists_link
            .set_tooltip_text(link.map(|l| format!("Go to {l}")).as_deref());
        theme::set_text(&self.total, total);
    }

    pub fn set_elapsed(&self, text: &str) {
        if !self.dragging.get() {
            theme::set_text(&self.elapsed, text);
        }
    }

    pub fn preview_seek(&self, text: &str) {
        theme::set_text(&self.elapsed, text);
    }

    pub fn set_progress(&self, f: f64) {
        if self.dragging.get() {
            return;
        }
        // Moves under half a pixel are not worth a redraw; before layout every move counts.
        let width = self.seek.width();
        if width > 0 && ((self.seek.value() - f) * f64::from(width)).abs() < 0.5 && f != 0.0 {
            return;
        }
        self.setting.set(true);
        self.seek.set_value(f);
        self.setting.set(false);
    }

    pub fn set_playing(&self, playing: bool) {
        if self.playing.replace(Some(playing)) != Some(playing) {
            let (icon, tip, name) = if playing {
                (Icon::Pause, "Pause (Space)", "Pause")
            } else {
                (Icon::Play, "Play (Space)", "Play")
            };
            icons::swap(&self.play_icon, icon);
            self.play.set_tooltip_text(Some(tip));
            self.play
                .update_property(&[gtk::accessible::Property::Label(name)]);
        }
    }

    pub fn set_volume(&self, volume: u8) {
        self.setting.set(true);
        self.volume.set_value(f64::from(volume));
        self.setting.set(false);
    }

    pub fn set_controls(&self, c: Controls) {
        for b in [&self.prev, &self.play, &self.next] {
            b.set_sensitive(c.transport);
        }
        self.seek.set_sensitive(c.seek);
        self.volume.set_sensitive(c.player);
    }

    pub fn covers_loading(&self) -> bool {
        self.cover.is_loading()
    }
}

/// Chips for the search kinds or the Library's.
pub struct Chips {
    pub root: gtk::Box,
}

impl Chips {
    pub fn new() -> Chips {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        Chips { root }
    }

    /// Shows a chip per `(label, on)`; a click runs `pick(i)`.
    pub fn set(&self, chips: &[(&'static str, bool)], pick: impl Fn(usize) + Clone + 'static) {
        while let Some(c) = self.root.first_child() {
            self.root.remove(&c);
        }
        for (i, (label, on)) in chips.iter().enumerate() {
            let b = gtk::Button::with_label(label);
            b.add_css_class("chip");
            theme::toggle_class(&b, "on", *on);
            b.set_focus_on_click(false);
            let f = pick.clone();
            b.connect_clicked(move |_| f(i));
            self.root.append(&b);
        }
    }

    pub fn set_search(&self, chips: &[(SearchChip, bool)]) {
        let shown: Vec<_> = chips.iter().map(|(c, on)| (c.label(), *on)).collect();
        let kinds: Vec<SearchChip> = chips.iter().map(|c| c.0).collect();
        self.set(&shown, move |i| {
            if let Some(&c) = kinds.get(i) {
                ui::with(|ui| ui.choose_search_chip(c));
            }
        });
    }

    pub fn set_library(&self, chips: &[(LibraryChip, bool)]) {
        let shown: Vec<_> = chips.iter().map(|(c, on)| (c.label(), *on)).collect();
        let kinds: Vec<LibraryChip> = chips.iter().map(|c| c.0).collect();
        self.set(&shown, move |i| {
            if let Some(&c) = kinds.get(i) {
                ui::with(|ui| ui.choose_library_chip(c));
            }
        });
    }
}

/// The Library's toolbar: its chips, the filter field and the sort.
pub struct LibraryBar {
    pub root: gtk::Box,
    pub chips: Chips,
    pub filter: gtk::Entry,
    sort: gtk::DropDown,
    setting: Rc<Cell<bool>>,
}

impl LibraryBar {
    pub fn new() -> LibraryBar {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let chips = Chips::new();
        chips.root.set_hexpand(true);
        chips.root.set_valign(gtk::Align::Center);
        let filter = widgets::field("Filter", "Filter", Some(Icon::Search));
        filter.set_size_request(200, -1);
        filter.set_valign(gtk::Align::Center);
        let labels: Vec<&str> = LibrarySort::CHOICES.iter().map(|s| s.label()).collect();
        let sort = gtk::DropDown::from_strings(&labels);
        sort.add_css_class("sort");
        sort.set_valign(gtk::Align::Center);
        sort.update_property(&[gtk::accessible::Property::Label("Sort")]);
        root.append(&chips.root);
        root.append(&filter);
        root.append(&sort);
        let setting = Rc::new(Cell::new(false));
        let s = setting.clone();
        sort.connect_selected_notify(move |d| {
            if s.get() {
                return;
            }
            if let Some(&sort) = LibrarySort::CHOICES.get(d.selected() as usize) {
                ui::with(|ui| ui.choose_library_sort(sort));
            }
        });
        let s = setting.clone();
        filter.connect_changed(move |_| {
            if !s.get() {
                ui::with(|ui| ui.library_filter_changed());
            }
        });
        widgets::on_field_key(&filter, |f, key, mods| {
            use gtk::gdk::{Key, ModifierType};
            if mods.intersects(ModifierType::CONTROL_MASK | ModifierType::ALT_MASK) {
                return false;
            }
            match key {
                Key::Escape => {
                    if f.text().is_empty() {
                        ui::with(|ui| ui.focus_library_grid());
                    } else {
                        // `changed` lists everything again.
                        f.set_text("");
                    }
                    true
                }
                Key::Return | Key::KP_Enter | Key::Down | Key::KP_Down => {
                    ui::with(|ui| ui.focus_library_grid());
                    true
                }
                _ => false,
            }
        });
        LibraryBar {
            root,
            chips,
            filter,
            sort,
            setting,
        }
    }

    pub fn set(&self, chips: &[(LibraryChip, bool)], sort: LibrarySort, text: &str) {
        self.chips.set_library(chips);
        self.setting.set(true);
        if let Some(i) = LibrarySort::CHOICES.iter().position(|&s| s == sort) {
            self.sort.set_selected(i as u32);
        }
        if self.filter.text() != text {
            self.filter.set_text(text);
        }
        self.setting.set(false);
    }
}

/// The All view's top result and the songs beside it.
pub struct SearchLead {
    pub root: gtk::Box,
    top: gtk::Box,
    play: gtk::Button,
    songs: TrackRows,
    /// The first song row's index among the results: 1 when the top result is a song.
    first: usize,
    cover: Cover,
}

impl SearchLead {
    pub fn new(top: &TopResult, songs: &[Track], mixed: bool) -> SearchLead {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 24);
        root.set_homogeneous(true);
        root.set_margin_start(24);
        root.set_margin_end(24);
        let left = gtk::Box::new(gtk::Orientation::Vertical, 10);
        left.append(&theme::label("Top result", &["section-title"]));
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("lead");
        card.set_vexpand(true);
        let (title, sub, art, provider, round, first) = match top {
            TopResult::Song(t) => (
                t.title.clone(),
                vm::artists(t),
                t.artwork_url.clone(),
                t.id.provider.clone(),
                false,
                1,
            ),
            TopResult::Artist { artist, .. } => (
                artist.title.clone(),
                artist
                    .subtitle
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Artist".into()),
                artist.artwork_url.clone(),
                artist.id.provider.clone(),
                true,
                0,
            ),
        };
        let cover = Cover::new(104, if round { &["round"] } else { &[] }, true);
        cover.set_url(art.as_deref());
        cover.set_badge(vm::badge(&provider, mixed));
        let name = theme::label(&title, &["lead-title"]);
        name.set_margin_top(16);
        let subtitle = theme::label(&sub, &["muted"]);
        theme::two_lines(&subtitle);
        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::End);
        text.append(&name);
        text.append(&subtitle);
        let play = icons::icon_button(Icon::Play, 20, &format!("Play {title}"), &["round-play"]);
        play.set_valign(gtk::Align::End);
        play.set_focusable(false);
        foot.append(&text);
        foot.append(&play);
        card.append(&cover.root);
        card.append(&foot);
        left.append(&card);
        let right = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let rows_ = TrackRows::new(44);
        if !songs.is_empty() {
            right.append(&theme::label("Songs", &["section-title"]));
            let shown: Vec<RowText> = rows_for(songs, mixed);
            rows_.set(&shown, move |i| {
                ui::with(|ui| {
                    ui.intent(Intent::PlayFrom {
                        list: List::Results,
                        from: first + i,
                    })
                });
            });
            right.append(&rows_.root);
        }
        root.append(&left);
        root.append(&right);
        let lead = SearchLead {
            root,
            top: card,
            play,
            songs: rows_,
            first,
            cover,
        };
        let (press, play_list): (Rc<dyn Fn()>, List) = match top {
            TopResult::Song(_) => (
                Rc::new(|| {
                    ui::with(|ui| {
                        ui.intent(Intent::PlayFrom {
                            list: List::Results,
                            from: 0,
                        })
                    });
                }),
                List::Results,
            ),
            TopResult::Artist { artist, playable } => {
                lead.play.set_sensitive(*playable);
                let a = artist.clone();
                (
                    Rc::new(move || {
                        let a = a.clone();
                        ui::with(|ui| ui.open_collection(a, Place::Search));
                    }),
                    List::TopArtist,
                )
            }
        };
        let p = press.clone();
        widgets::on_click(&lead.top, move |_| p());
        lead.play.connect_clicked(move |_| {
            ui::with(|ui| {
                ui.intent(Intent::PlayFrom {
                    list: play_list,
                    from: 0,
                })
            });
        });
        attach_press(&lead.top, press);
        lead
    }

    pub fn songs(&self) -> usize {
        self.songs.len()
    }

    /// Marks the keyboard's spot.
    pub fn select(&self, spot: Option<crate::search_keys::Spot>) {
        use crate::search_keys::Spot;
        theme::toggle_class(&self.top, "selected", spot == Some(Spot::Top));
        self.songs.select(match spot {
            Some(Spot::Song(i)) => Some(i),
            _ => None,
        });
    }

    /// The result index of the song at `spot`.
    pub fn song_at(&self, spot: crate::search_keys::Spot) -> Option<usize> {
        match spot {
            crate::search_keys::Spot::Song(i) if i < self.songs() => Some(self.first + i),
            _ => None,
        }
    }

    /// What a click on `spot` does, to run once the lead is no longer borrowed.
    pub fn press_fn(&self, spot: crate::search_keys::Spot) -> Option<Rc<dyn Fn()>> {
        match spot {
            crate::search_keys::Spot::Top => {
                widgets::attached::<Rc<dyn Fn()>>(&self.top, "press").cloned()
            }
            crate::search_keys::Spot::Song(i) => {
                let from = self.first + i;
                Some(Rc::new(move || {
                    ui::with(|ui| {
                        ui.intent(Intent::PlayFrom {
                            list: List::Results,
                            from,
                        })
                    });
                }))
            }
            crate::search_keys::Spot::Card { .. } => None,
        }
    }

    pub fn set_play_enabled(&self, on: bool) {
        self.play.set_sensitive(on);
    }

    pub fn covers_loading(&self) -> bool {
        self.cover.is_loading() || self.songs.covers_loading()
    }
}

fn attach_press(w: &gtk::Box, press: Rc<dyn Fn()>) {
    widgets::attach(w, "press", press);
}

fn rows_for(tracks: &[Track], mixed: bool) -> Vec<RowText> {
    let mut r = rows(tracks);
    for (row, t) in r.iter_mut().zip(tracks) {
        row.badge = vm::badge(&t.id.provider, mixed);
    }
    r
}

/// Now Playing: the cover large, the track, and what plays next.
pub struct NowPlayingPage {
    pub root: gtk::Stack,
    cover: Cover,
    title: gtk::Label,
    artists: gtk::Label,
    album: gtk::Label,
    up_next_title: gtk::Label,
    up_next: TrackRows,
    cover_box: gtk::AspectFrame,
}

impl NowPlayingPage {
    pub fn new() -> NowPlayingPage {
        let root = gtk::Stack::new();
        root.add_css_class("page");
        let page = gtk::Box::new(gtk::Orientation::Horizontal, 34);
        page.set_margin_top(40);
        page.set_margin_start(40);
        page.set_margin_end(40);
        page.set_margin_bottom(40);
        let cover = Cover::new(387, &[], true);
        let cover_box = gtk::AspectFrame::new(0.0, 0.0, 1.0, false);
        cover_box.set_child(Some(&cover.root));
        cover_box.set_valign(gtk::Align::Start);
        let side = gtk::Box::new(gtk::Orientation::Vertical, 4);
        side.set_hexpand(true);
        let title = theme::label("", &["page-title"]);
        theme::two_lines(&title);
        let artists = gtk::Label::new(None);
        artists.set_xalign(0.0);
        artists.add_css_class("np-artists");
        artists.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let artists_link = gtk::Button::new();
        artists_link.add_css_class("bar-artist");
        artists_link.set_child(Some(&artists));
        artists_link.set_halign(gtk::Align::Start);
        artists_link.connect_clicked(|_| {
            ui::with(|ui| ui.go_to_playing_artist());
        });
        let album = theme::label("", &["muted"]);
        let up_next_title = theme::label("Up next", &["section-title"]);
        up_next_title.set_margin_top(24);
        up_next_title.set_margin_bottom(8);
        let up_next = TrackRows::new(44);
        let show_queue = gtk::Button::with_label("Show queue");
        show_queue.add_css_class("link-btn");
        show_queue.set_halign(gtk::Align::Start);
        show_queue.set_margin_top(16);
        show_queue.connect_clicked(|_| {
            ui::with(|ui| ui.show(crate::view::AppView::Queue));
        });
        side.append(&title);
        side.append(&artists_link);
        side.append(&album);
        side.append(&up_next_title);
        side.append(&up_next.root);
        side.append(&show_queue);
        page.append(&cover_box);
        page.append(&side);
        let empty = widgets::EmptyState::new();
        empty.set(
            Icon::Music,
            "Nothing playing",
            "Pick a song from Home or Search.",
            None,
        );
        root.add_named(&page, Some("page"));
        root.add_named(&empty.root, Some("empty"));
        NowPlayingPage {
            root,
            cover,
            title,
            artists,
            album,
            up_next_title,
            up_next,
            cover_box,
        }
    }

    pub fn set(&self, track: Option<&Track>, next: &[Track], badge: Option<Service>) {
        let Some(t) = track else {
            self.root.set_visible_child_name("empty");
            self.cover.set_url(None);
            return;
        };
        self.root.set_visible_child_name("page");
        self.cover.set_url(t.artwork_url.as_deref());
        self.cover.set_badge(badge);
        theme::set_text(&self.title, &t.title);
        theme::set_text(&self.artists, &vm::artists(t));
        theme::set_text(&self.album, t.album.as_deref().unwrap_or(""));
        self.album.set_visible(t.album.is_some());
        theme::set_text(
            &self.up_next_title,
            if next.is_empty() {
                "Nothing up next"
            } else {
                "Up next"
            },
        );
        let mixed = vm::mixed(next.iter().map(|t| &t.id.provider));
        let shown = rows_for(next, mixed);
        self.up_next.set(&shown, |i| {
            ui::with(|ui| ui.play_up_next(i));
        });
    }

    /// Sizes the cover to the page: as large as fits beside the text.
    pub fn fit(&self, width: i32, height: i32) {
        let side = ((width - 80 - 34) / 2).min(height - 80).clamp(160, 640);
        self.cover.root.set_size_request(side, side);
        self.cover_box.set_size_request(side, side);
        self.cover.resize(side);
    }

    pub fn covers_loading(&self) -> bool {
        self.cover.is_loading() || self.up_next.covers_loading()
    }
}

/// The sign-in code and its buttons, in the Library's panel or a Settings row.
pub struct CodePanel {
    pub root: gtk::Box,
    heading: gtk::Label,
    hint: gtk::Label,
    code: gtk::Label,
    expiry: gtk::Label,
    notice: gtk::Label,
    expires_at: Cell<u64>,
}

impl CodePanel {
    /// `boxed` frames it as the Library's panel, with a heading.
    pub fn new(
        boxed: bool,
        copy: impl Fn() + 'static,
        open: impl Fn() + 'static,
        cancel: impl Fn() + 'static,
    ) -> CodePanel {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let heading = theme::label("", &["empty-heading"]);
        let hint = gtk::Label::new(None);
        hint.add_css_class("muted");
        hint.set_wrap(true);
        hint.set_max_width_chars(54);
        let code = gtk::Label::new(None);
        code.add_css_class("code");
        code.set_selectable(true);
        code.set_can_focus(false);
        let expiry = gtk::Label::new(None);
        expiry.add_css_class("muted");
        expiry.add_css_class("small");
        let notice = gtk::Label::new(None);
        notice.add_css_class("muted");
        notice.add_css_class("small");
        notice.set_wrap(true);
        notice.set_max_width_chars(54);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_margin_top(16);
        let b_copy = icons::button(Some(Icon::Copy), "Copy Code", &["btn", "primary"]);
        let b_open = icons::button(Some(Icon::ExternalLink), "Open in Browser", &["btn"]);
        let b_cancel = icons::button(None, "Cancel", &["btn"]);
        b_copy.connect_clicked(move |_| copy());
        b_open.connect_clicked(move |_| open());
        b_cancel.connect_clicked(move |_| cancel());
        buttons.append(&b_copy);
        buttons.append(&b_open);
        buttons.append(&b_cancel);
        if boxed {
            root.add_css_class("signin");
            root.set_halign(gtk::Align::Center);
            root.set_valign(gtk::Align::Center);
            root.set_vexpand(true);
            for l in [&heading, &hint, &code, &expiry, &notice] {
                l.set_xalign(0.5);
                l.set_justify(gtk::Justification::Center);
            }
            heading.set_margin_bottom(6);
            code.set_margin_top(14);
            buttons.set_halign(gtk::Align::Center);
            root.append(&heading);
        } else {
            for l in [&hint, &code, &expiry, &notice] {
                l.set_xalign(0.0);
            }
            hint.set_halign(gtk::Align::Start);
        }
        root.append(&hint);
        root.append(&code);
        root.append(&expiry);
        root.append(&notice);
        root.append(&buttons);
        CodePanel {
            root,
            heading,
            hint,
            code,
            expiry,
            notice,
            expires_at: Cell::new(0),
        }
    }

    pub fn show(&self, provider: &ProviderId, url: &str, code: &str, expires_at_unix: u64) {
        theme::set_text(
            &self.heading,
            &format!("Sign in to {}", vm::provider_name(&provider.0)),
        );
        theme::set_text(&self.hint, &vm::sign_in_hint(url));
        theme::set_text(&self.code, code);
        self.expires_at.set(expires_at_unix);
        self.tick(unix_now());
        let notice = vm::sign_in_notice(provider).unwrap_or("");
        theme::set_text(&self.notice, notice);
        self.notice.set_visible(!notice.is_empty());
    }

    /// Moves "Code expires in N min" on.
    pub fn tick(&self, now: u64) {
        theme::set_text(&self.expiry, &vm::expires_text(self.expires_at.get(), now));
    }
}

const SESSION_STEPS: [&str; 4] = [
    "Open a private window and sign in to music.youtube.com there.",
    "Open the browser's developer tools on the Network tab and filter for browse.",
    "Click Explore or Library in the sidebar (or scroll Home) so a browse request appears, then copy its request headers, or choose Copy as cURL.",
    "Close the private window without signing out. A browser that keeps YouTube open replaces the session, which ends the copy.",
];
const DISCLOSURE: &str = "This copies your Google sign-in from your browser. Anyone who has it \
can use your Google account, so Requiem keeps it only on this computer. Using it is against \
YouTube's terms and could get the account flagged.";

/// Settings' form for a session copied from a signed-in browser tab.
struct SessionFormView {
    root: gtk::Box,
    error: gtk::Label,
    paste: gtk::Button,
}

impl SessionFormView {
    fn new() -> SessionFormView {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_top(12);
        for (i, step) in SESSION_STEPS.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let n = gtk::Label::new(Some(&format!("{}.", i + 1)));
            n.set_valign(gtk::Align::Start);
            let t = gtk::Label::new(Some(step));
            t.set_wrap(true);
            t.set_xalign(0.0);
            t.set_max_width_chars(70);
            row.append(&n);
            row.append(&t);
            root.append(&row);
        }
        let disclosure = gtk::Label::new(Some(DISCLOSURE));
        disclosure.add_css_class("muted");
        disclosure.add_css_class("small");
        disclosure.set_wrap(true);
        disclosure.set_xalign(0.0);
        disclosure.set_max_width_chars(80);
        disclosure.set_halign(gtk::Align::Start);
        disclosure.set_margin_top(8);
        let error = gtk::Label::new(None);
        error.add_css_class("error-text");
        error.add_css_class("small");
        error.set_wrap(true);
        error.set_xalign(0.0);
        error.set_selectable(true);
        error.set_can_focus(false);
        error.set_halign(gtk::Align::Start);
        error.set_max_width_chars(80);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_margin_top(12);
        let paste = icons::button(
            Some(Icon::ClipboardPaste),
            "Paste from Clipboard",
            &["btn", "primary"],
        );
        let guide = icons::button(Some(Icon::BookOpen), "How to Copy", &["btn"]);
        let cancel = icons::button(None, "Cancel", &["btn"]);
        paste.connect_clicked(|_| {
            ui::with(|ui| ui.paste_session());
        });
        guide.connect_clicked(|_| {
            ui::with(|ui| ui.how_to_copy());
        });
        cancel.connect_clicked(|_| {
            ui::with(|ui| ui.cancel_setup());
        });
        buttons.append(&paste);
        buttons.append(&guide);
        buttons.append(&cancel);
        root.append(&disclosure);
        root.append(&error);
        root.append(&buttons);
        SessionFormView { root, error, paste }
    }

    fn show(&self, form: &SessionForm) {
        let error = form.error.as_deref().unwrap_or("");
        theme::set_text(&self.error, error);
        self.error.set_visible(!error.is_empty());
        self.paste.set_sensitive(!form.checking);
    }
}

/// An account's row in Settings.
struct AccountView {
    root: gtk::Box,
    badge: gtk::Box,
    name: gtk::Label,
    status: gtk::Label,
    action: gtk::Button,
    session: gtk::Button,
    code: CodePanel,
    form: SessionFormView,
}

impl AccountView {
    fn new(i: usize) -> AccountView {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("account");
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        let badge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        badge.set_size_request(18, 18);
        badge.set_valign(gtk::Align::Center);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        let name = theme::label("", &["title"]);
        let status = theme::label("", &["subtitle"]);
        text.append(&name);
        text.append(&status);
        let session = icons::button(
            Some(Icon::ClipboardPaste),
            "Use Browser Session\u{2026}",
            &["btn"],
        );
        // Under the name, as on macOS.
        session.set_halign(gtk::Align::Start);
        session.set_margin_start(32);
        session.set_margin_top(12);
        let action = gtk::Button::new();
        action.add_css_class("btn");
        action.set_valign(gtk::Align::Center);
        session.connect_clicked(move |_| {
            ui::with(|ui| ui.use_browser_session(i));
        });
        action.connect_clicked(move |_| {
            ui::with(|ui| ui.settings_action(i));
        });
        top.append(&badge);
        top.append(&text);
        top.append(&action);
        let code = CodePanel::new(
            false,
            move || {
                ui::with(|ui| ui.settings_copy_code(i));
            },
            move || {
                ui::with(|ui| ui.settings_open_host(i));
            },
            move || {
                ui::with(|ui| ui.settings_cancel_sign_in(i));
            },
        );
        code.root.set_margin_start(32);
        code.root.set_margin_top(12);
        let form = SessionFormView::new();
        form.root.set_margin_start(32);
        root.append(&top);
        root.append(&session);
        root.append(&code.root);
        root.append(&form.root);
        AccountView {
            root,
            badge,
            name,
            status,
            action,
            session,
            code,
            form,
        }
    }

    fn show(&self, row: &AccountRow) {
        while let Some(c) = self.badge.first_child() {
            self.badge.remove(&c);
        }
        if let Some(s) = Service::of(&row.provider) {
            self.badge.append(&icons::mark(s, 18));
        }
        theme::set_text(&self.name, vm::provider_name(&row.provider.0));
        theme::set_text(&self.status, &row.status);
        let action = match &row.state {
            AccountRowState::SignedIn { .. } => Some(("Sign Out\u{2026}", Icon::LogOut)),
            AccountRowState::SignedOut => Some(("Sign In\u{2026}", Icon::LogIn)),
            _ => None,
        };
        if let Some((title, icon)) = action {
            self.action
                .set_child(Some(&icons::labelled(Some(icon), title)));
        }
        self.action.set_visible(action.is_some());
        self.session.set_visible(row.methods);
        match &row.state {
            AccountRowState::Pending {
                verification_url,
                user_code,
                expires_at_unix,
            } => {
                self.code
                    .show(&row.provider, verification_url, user_code, *expires_at_unix);
                self.code.root.set_visible(true);
            }
            _ => self.code.root.set_visible(false),
        }
        match &row.state {
            AccountRowState::Session(form) => {
                self.form.show(form);
                self.form.root.set_visible(true);
            }
            _ => self.form.root.set_visible(false),
        }
    }
}

/// Settings: the accounts.
pub struct SettingsPage {
    pub root: gtk::ScrolledWindow,
    list: gtk::Box,
    views: RefCell<Vec<AccountView>>,
    pub rows: RefCell<Vec<AccountRow>>,
}

impl SettingsPage {
    pub fn new() -> SettingsPage {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.set_margin_start(24);
        page.set_margin_end(24);
        page.set_margin_top(28);
        page.set_margin_bottom(24);
        let title = widgets::page_title("Settings");
        let accounts = theme::label("Accounts", &["section-title"]);
        accounts.set_margin_top(24);
        accounts.set_margin_bottom(12);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&title);
        page.append(&accounts);
        page.append(&list);
        let root = widgets::vscroll(&page);
        root.add_css_class("page");
        SettingsPage {
            root,
            list,
            views: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
        }
    }

    /// Shows `rows`; true when one shows a sign-in code, whose expiry needs ticking.
    pub fn set(&self, rows: Vec<AccountRow>) -> bool {
        let mut views = self.views.borrow_mut();
        while views.len() < rows.len() {
            let v = AccountView::new(views.len());
            self.list.append(&v.root);
            views.push(v);
        }
        for (i, v) in views.iter().enumerate() {
            match rows.get(i) {
                Some(r) => {
                    v.show(r);
                    v.root.set_visible(true);
                }
                None => v.root.set_visible(false),
            }
        }
        let pending = rows
            .iter()
            .any(|r| matches!(r.state, AccountRowState::Pending { .. }));
        *self.rows.borrow_mut() = rows;
        pending
    }

    pub fn row(&self, i: usize) -> Option<AccountRow> {
        self.rows.borrow().get(i).cloned()
    }

    /// Whether the session form shows.
    pub fn session_form(&self) -> bool {
        self.rows
            .borrow()
            .iter()
            .any(|r| matches!(r.state, AccountRowState::Session(_)))
    }

    /// Moves the codes' expiry on; true when one shows.
    pub fn refresh_expiry(&self, now: u64) -> bool {
        let rows = self.rows.borrow();
        let mut showing = false;
        for (v, r) in self.views.borrow().iter().zip(rows.iter()) {
            if matches!(r.state, AccountRowState::Pending { .. }) {
                v.code.tick(now);
                showing = true;
            }
        }
        showing
    }
}

/// The open collection's head: Back, cover, title, subtitle, Play and Add to Queue.
pub struct CollectionHead {
    pub root: gtk::Box,
    pub back: gtk::Button,
    back_label: gtk::Label,
    pub cover: Cover,
    title: gtk::Label,
    sub: gtk::Label,
    pub play: gtk::Button,
    pub add: gtk::Button,
}

impl CollectionHead {
    pub fn new() -> CollectionHead {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_start(24);
        root.set_margin_end(24);
        root.set_margin_top(20);
        let back = gtk::Button::new();
        back.add_css_class("btn");
        back.add_css_class("small");
        back.set_halign(gtk::Align::Start);
        let back_label = gtk::Label::new(None);
        back.set_child(Some(&back_label));
        back.set_focus_on_click(false);
        back.set_tooltip_text(Some("Back (Alt+Left)"));
        back.connect_clicked(|_| {
            ui::with(|ui| ui.go_back());
        });
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 22);
        let cover = Cover::new(120, &[], false);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let title = theme::label("", &["page-title"]);
        let sub = theme::label("", &["muted"]);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_margin_top(14);
        let play = icons::button(None, "Play", &["btn", "primary"]);
        let add = icons::button(None, "Add to Queue", &["btn"]);
        play.connect_clicked(|_| {
            ui::with(|ui| ui.play_collection());
        });
        add.connect_clicked(|_| {
            ui::with(|ui| ui.add_collection());
        });
        buttons.append(&play);
        buttons.append(&add);
        text.append(&title);
        text.append(&sub);
        text.append(&buttons);
        row.append(&cover.root);
        row.append(&text);
        root.append(&back);
        root.append(&row);
        CollectionHead {
            root,
            back,
            back_label,
            cover,
            title,
            sub,
            play,
            add,
        }
    }

    pub fn set(&self, c: &Collection, back: &str) {
        theme::set_text(&self.title, &c.title);
        theme::set_text(&self.sub, c.subtitle.as_deref().unwrap_or(""));
        self.cover.set_url(c.artwork_url.as_deref());
        self.cover
            .set_round(c.kind == requiem_core::CollectionKind::Artist);
        theme::set_text(&self.back_label, &format!("\u{2039} {back}"));
        self.back
            .update_property(&[gtk::accessible::Property::Label(&format!("Back to {back}"))]);
    }
}

/// The command palette: a field over the list of actions.
pub struct Palette {
    pub root: gtk::Overlay,
    pub field: gtk::Entry,
    scroll: gtk::ScrolledWindow,
    rows: Vec<(PaletteAction, gtk::Box)>,
    enabled: RefCell<Vec<bool>>,
    shown: RefCell<Vec<usize>>,
    selected: Cell<Option<usize>>,
    open: Cell<bool>,
    empty: gtk::Label,
}

const PALETTE_ROWS: i32 = 11;
const PALETTE_ROW_H: i32 = 36;

impl Palette {
    pub fn new() -> Palette {
        let root = gtk::Overlay::new();
        root.add_css_class("scrim");
        root.set_hexpand(true);
        root.set_vexpand(true);
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
        panel.add_css_class("palette");
        panel.set_halign(gtk::Align::Center);
        panel.set_valign(gtk::Align::Start);
        panel.set_margin_top(80);
        panel.set_size_request(520, -1);
        // The panel's top is a fifth of the way down the window, as on macOS.
        let sizer = gtk::DrawingArea::new();
        sizer.set_hexpand(true);
        sizer.set_vexpand(true);
        let p = panel.downgrade();
        sizer.connect_resize(move |_, _, h| {
            let Some(p) = p.upgrade() else { return };
            let ph = p.height().max(PALETTE_ROW_H * 3);
            let top = (f64::from(h) * 0.2)
                .round()
                .min(f64::from(h - ph - 16))
                .max(16.0) as i32;
            theme::later(move || p.set_margin_top(top));
        });
        root.set_child(Some(&sizer));
        let field = widgets::field("Type a command\u{2026}", "Command", Some(Icon::Search));
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_child(Some(&list));
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(PALETTE_ROWS * PALETTE_ROW_H);
        let empty = theme::label("No matching actions", &["muted"]);
        empty.set_margin_start(12);
        empty.set_size_request(-1, PALETTE_ROW_H);
        empty.set_visible(false);
        let mut rows = Vec::new();
        for (i, action) in PaletteAction::ALL.into_iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("action");
            row.set_size_request(-1, PALETTE_ROW_H);
            let label = theme::label(action.label(), &[]);
            label.set_hexpand(true);
            let keys = theme::label(&shortcut(action), &["shortcut"]);
            keys.set_xalign(1.0);
            row.append(&label);
            row.append(&keys);
            widgets::on_click(&row, move |_| {
                ui::with(|ui| ui.palette_run(i));
            });
            let motion = gtk::EventControllerMotion::new();
            motion.connect_enter(move |_, _, _| {
                ui::with(|ui| ui.palette.hover(i));
            });
            row.add_controller(motion);
            list.append(&row);
            rows.push((action, row));
        }
        panel.append(&field);
        panel.append(&scroll);
        panel.append(&empty);
        root.add_overlay(&panel);
        // A click on the scrim, outside the panel, closes the palette.
        let outside = gtk::GestureClick::new();
        let p = panel.downgrade();
        outside.connect_pressed(move |g, _, x, y| {
            let Some(panel) = p.upgrade() else { return };
            let Some(root) = g.widget() else { return };
            let inside = panel
                .compute_bounds(&root)
                .is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
            if !inside {
                ui::with(|ui| ui.close_palette());
            }
        });
        root.add_controller(outside);
        field.connect_changed(|_| {
            ui::with(|ui| ui.palette.filter());
        });
        field.connect_activate(|_| {
            ui::with(|ui| {
                if let Some(a) = ui.palette.selected_action() {
                    ui.run_action(a);
                }
            });
        });
        widgets::on_field_key(&field, |_, key, _| {
            use gtk::gdk::Key;
            ui::with(|ui| match key {
                Key::Up | Key::KP_Up => {
                    ui.palette.move_selection(-1);
                    true
                }
                Key::Down | Key::KP_Down => {
                    ui.palette.move_selection(1);
                    true
                }
                Key::Escape => {
                    ui.close_palette();
                    true
                }
                // The palette has nothing else to focus.
                Key::Tab | Key::ISO_Left_Tab => true,
                _ => false,
            })
            .unwrap_or(false)
        });
        root.set_visible(false);
        Palette {
            root,
            field,
            scroll,
            enabled: RefCell::new(vec![true; rows.len()]),
            rows,
            shown: RefCell::new(Vec::new()),
            selected: Cell::new(None),
            open: Cell::new(false),
            empty,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    pub fn open(&self, enabled: impl Fn(PaletteAction) -> bool) {
        *self.enabled.borrow_mut() = self.rows.iter().map(|(a, _)| enabled(*a)).collect();
        self.field.set_text("");
        self.open.set(true);
        self.filter();
        self.root.set_visible(true);
        self.field.grab_focus();
    }

    pub fn close(&self) {
        self.open.set(false);
        self.root.set_visible(false);
    }

    pub fn set_enabled(&self, enabled: impl Fn(PaletteAction) -> bool) {
        let now: Vec<bool> = self.rows.iter().map(|(a, _)| enabled(*a)).collect();
        if *self.enabled.borrow() == now {
            return;
        }
        *self.enabled.borrow_mut() = now;
        if self.listed() != *self.shown.borrow() {
            self.filter();
            return;
        }
        let flags = self.shown_enabled();
        let kept = self.selected.get().filter(|&i| flags[i]);
        self.selected.set(kept.or_else(|| step(&flags, None, 1)));
        self.repaint();
    }

    fn listed(&self) -> Vec<usize> {
        let matches = PaletteAction::filter(&self.field.text());
        let enabled = self.enabled.borrow();
        self.rows
            .iter()
            .enumerate()
            .filter(|(i, (a, _))| {
                matches.contains(a) && (!a.listed_only_when_runnable() || enabled[*i])
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn shown_enabled(&self) -> Vec<bool> {
        let enabled = self.enabled.borrow();
        self.shown.borrow().iter().map(|&i| enabled[i]).collect()
    }

    pub fn filter(&self) {
        let listed = self.listed();
        for (i, (_, row)) in self.rows.iter().enumerate() {
            row.set_visible(listed.contains(&i));
        }
        self.empty.set_visible(listed.is_empty());
        self.scroll.set_visible(!listed.is_empty());
        *self.shown.borrow_mut() = listed;
        self.selected.set(step(&self.shown_enabled(), None, 1));
        self.scroll.vadjustment().set_value(0.0);
        self.repaint();
    }

    fn repaint(&self) {
        let enabled = self.enabled.borrow();
        let shown = self.shown.borrow();
        let sel = self.selected.get().and_then(|s| shown.get(s).copied());
        for (i, (_, row)) in self.rows.iter().enumerate() {
            theme::toggle_class(row, "selected", Some(i) == sel);
            theme::toggle_class(row, "disabled", !enabled[i]);
        }
        if let Some(s) = self.selected.get() {
            let y = f64::from(s as i32 * PALETTE_ROW_H);
            let adj = self.scroll.vadjustment();
            let page = f64::from(PALETTE_ROWS * PALETTE_ROW_H);
            if y < adj.value() {
                adj.set_value(y);
            } else if y + f64::from(PALETTE_ROW_H) > adj.value() + page {
                adj.set_value(y + f64::from(PALETTE_ROW_H) - page);
            }
        }
    }

    pub fn move_selection(&self, delta: isize) {
        let next = step(&self.shown_enabled(), self.selected.get(), delta);
        if next.is_some() && self.selected.replace(next) != next {
            self.repaint();
        }
    }

    /// The pointer rests on row `row` of `ALL`.
    pub fn hover(&self, row: usize) {
        if !self.enabled.borrow().get(row).copied().unwrap_or(false) {
            return;
        }
        let slot = self.shown.borrow().iter().position(|&i| i == row);
        if slot.is_some() && self.selected.replace(slot) != slot {
            self.repaint();
        }
    }

    pub fn selected_action(&self) -> Option<PaletteAction> {
        let i = *self.shown.borrow().get(self.selected.get()?)?;
        Some(self.rows[i].0)
    }

    pub fn action(&self, row: usize) -> Option<PaletteAction> {
        let enabled = self.enabled.borrow().get(row).copied().unwrap_or(false);
        self.rows.get(row).filter(|_| enabled).map(|r| r.0)
    }
}

/// `action`'s key as Linux spells it: Ctrl for ⌘, and the arrows and Return by name.
pub fn shortcut(action: PaletteAction) -> String {
    action
        .shortcut()
        .replace('⌘', "Ctrl+")
        .replace('→', "Right")
        .replace('←', "Left")
        .replace('↑', "Up")
        .replace('↓', "Down")
        .replace('↩', "Enter")
}

/// The next enabled row from `from`, `delta` rows on, wrapping; the first enabled for `None`.
fn step(enabled: &[bool], from: Option<usize>, delta: isize) -> Option<usize> {
    let Some(from) = from else {
        return enabled.iter().position(|&e| e);
    };
    let n = enabled.len() as isize;
    if n == 0 {
        return None;
    }
    let mut i = from as isize;
    for _ in 0..n {
        i = (i + delta).rem_euclid(n);
        if enabled[i as usize] {
            return Some(i as usize);
        }
    }
    None
}

/// The toast at the window's bottom right.
pub struct ToastView {
    pub root: gtk::Box,
    text: gtk::Label,
    action: gtk::Button,
}

impl ToastView {
    pub fn new() -> ToastView {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        root.add_css_class("toast");
        root.set_halign(gtk::Align::End);
        root.set_valign(gtk::Align::End);
        root.set_margin_end(24);
        root.set_margin_bottom(24);
        let text = gtk::Label::new(None);
        text.set_wrap(true);
        text.set_max_width_chars(48);
        text.set_xalign(0.0);
        text.set_valign(gtk::Align::Center);
        let action = gtk::Button::with_label("Undo");
        action.add_css_class("btn");
        action.add_css_class("small");
        action.set_valign(gtk::Align::Center);
        action.connect_clicked(|_| {
            ui::with(|ui| ui.toast_action());
        });
        let close = icons::icon_button(Icon::X, 14, "Dismiss", &["icon-btn"]);
        close.set_valign(gtk::Align::Center);
        close.connect_clicked(|_| {
            ui::with(|ui| ui.close_toast());
        });
        root.append(&text);
        root.append(&action);
        root.append(&close);
        let hover = gtk::EventControllerMotion::new();
        hover.connect_enter(|_, _, _| {
            ui::with(|ui| ui.hover_toast(true));
        });
        hover.connect_leave(|_| {
            ui::with(|ui| ui.hover_toast(false));
        });
        root.add_controller(hover);
        root.set_visible(false);
        ToastView { root, text, action }
    }

    pub fn show(&self, t: &Toast) {
        theme::set_text(&self.text, &t.text);
        theme::toggle_class(&self.root, "error", t.error);
        if let Some(a) = t.action {
            self.action.set_label(a);
            self.action
                .set_tooltip_text((a == "Undo").then_some("Undo (Ctrl+Z)"));
        }
        self.action.set_visible(t.action.is_some());
        self.root.set_visible(true);
    }

    pub fn hide(&self) {
        self.root.set_visible(false);
    }
}

/// The banner across the top of the pages, when the player can't be reached or an update
/// waits.
pub struct BannerView {
    pub root: gtk::Box,
    text: gtk::Label,
    action: gtk::Button,
}

impl BannerView {
    pub fn new() -> BannerView {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        root.add_css_class("banner");
        let text = gtk::Label::new(None);
        text.set_wrap(true);
        text.set_lines(2);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.set_hexpand(true);
        text.set_justify(gtk::Justification::Center);
        let action = gtk::Button::new();
        action.add_css_class("btn");
        action.add_css_class("small");
        action.set_valign(gtk::Align::Center);
        action.connect_clicked(|_| {
            ui::with(|ui| ui.restart_to_update());
        });
        root.append(&text);
        root.append(&action);
        root.set_visible(false);
        BannerView { root, text, action }
    }

    pub fn show(&self, banner: Option<&Banner>) {
        let Some(b) = banner else {
            self.root.set_visible(false);
            return;
        };
        theme::set_text(&self.text, &b.text);
        self.text.set_tooltip_text(Some(&b.text));
        theme::toggle_class(&self.root, "error", b.error);
        theme::toggle_class(&self.text, "error-text", b.error);
        if let Some(a) = b.action {
            self.action.set_label(a);
        }
        self.action.set_visible(b.action.is_some());
        self.root.set_visible(true);
    }
}
