//! The head of the All view of search results: the top result, an artist or a song, as a card
//! with a Play button, and the first songs in a short list.

use std::cell::Cell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSColor, NSEvent, NSResponder, NSTextAlignment, NSTextField,
    NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect};
use requiem_core::Track;
use requiem_macos::artwork::Cover;

use super::badge::{self, Badge};
use super::cover_grid;
use super::shelves::{self, FlippedView, spoken};
use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{self, FLEX_W, rect};
use crate::search_keys::Spot;
use crate::vm::{self, Intent, List, Place, Service, TOP_SONGS, TopResult};

const PAD: CGFloat = 24.0;
/// A section title's height, and the space under it: those of a shelf's.
const TITLE_H: CGFloat = 24.0;
const TITLE_GAP: CGFloat = 10.0;
const HEAD_H: CGFloat = TITLE_H + TITLE_GAP;
/// A song's row, with the cover of Home's compact cards.
const ROW_H: CGFloat = 56.0;
const THUMB: CGFloat = 44.0;
const THUMB_GAP: CGFloat = 12.0;
const DURATION_W: CGFloat = 44.0;
const BODY_H: CGFloat = TOP_SONGS as CGFloat * ROW_H;
/// The space between the top result's card and the songs.
const GAP: CGFloat = 24.0;
const CARD_PAD: CGFloat = 16.0;
const CARD_COVER: CGFloat = 104.0;
const CARD_RADIUS: CGFloat = 12.0;
const PLAY: CGFloat = 44.0;
const PLAY_ICON: CGFloat = 20.0;
/// A button's opacity while it has nothing to do.
const DISABLED_ALPHA: CGFloat = 0.4;

pub struct PressIvars {
    press: Rc<dyn Fn()>,
    rest: Option<Retained<NSColor>>,
    hover: Retained<NSColor>,
    hovered: Cell<bool>,
    pressed: Cell<bool>,
    enabled: Cell<bool>,
    /// Selected with the keyboard, as a row of the song table is.
    selected: Cell<bool>,
}

define_class!(
    /// A button made of a view: it presses on a click that is let go inside it, and fills on
    /// hover.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PressIvars]
    pub struct PressView;

    impl PressView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.set_hovered(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().pressed.set(false);
            self.set_hovered(false);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.ivars().pressed.set(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let was = self.ivars().pressed.replace(false);
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            let b = self.bounds();
            let inside = at.x >= 0.0 && at.x < b.size.width && at.y >= 0.0 && at.y < b.size.height;
            if was && inside {
                self.activate();
            }
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Clicks on the cover or a label land on the view itself.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let f = self.frame();
            let inside = point.x >= f.origin.x
                && point.x < f.origin.x + f.size.width
                && point.y >= f.origin.y
                && point.y < f.origin.y + f.size.height;
            inside.then(|| Retained::into_super(self.retain()))
        }

        /// Also called when an ancestor is hidden or the view leaves the page; a hidden view
        /// gets no `mouseExited:`.
        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), viewDidHide] };
            self.set_hovered(false);
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            self.activate();
            true
        }

        /// A plain view reports itself disabled, which VoiceOver reads as dimmed.
        #[unsafe(method(isAccessibilityEnabled))]
        fn accessibility_enabled(&self) -> bool {
            self.ivars().enabled.get()
        }
    }
);

impl PressView {
    /// A button in `frame` that calls `press`, named `label` to VoiceOver. It is filled with
    /// `rest` and with `hover` under the pointer. Add its parts before `finish`.
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        radius: CGFloat,
        (rest, hover): (Option<Retained<NSColor>>, Retained<NSColor>),
        press: Rc<dyn Fn()>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PressIvars {
            press,
            rest,
            hover,
            hovered: Cell::new(false),
            pressed: Cell::new(false),
            enabled: Cell::new(true),
            selected: Cell::new(false),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        widgets::round(&this, radius);
        widgets::track_hover(&this, NSTrackingAreaOptions::empty());
        this.paint();
        this
    }

    /// Makes the view one accessibility button, once its parts are in.
    fn finish(&self, label: &str) {
        widgets::as_button(self, label);
    }

    /// Dims the button, which then does nothing, or brings it back.
    fn set_enabled(&self, enabled: bool) {
        self.ivars().enabled.set(enabled);
        self.setAlphaValue(if enabled { 1.0 } else { DISABLED_ALPHA });
        self.paint();
    }

    fn set_hovered(&self, hovered: bool) {
        if self.ivars().hovered.replace(hovered) != hovered {
            self.paint();
        }
    }

    /// Shows the row as the keyboard's choice, in the song table's selected row's fill.
    fn set_row_selected(&self, selected: bool) {
        self.setAccessibilitySelected(selected);
        if self.ivars().selected.replace(selected) != selected {
            self.paint();
        }
    }

    fn paint(&self) {
        let iv = self.ivars();
        let hovered = iv.hovered.get() && iv.enabled.get();
        if iv.selected.get() {
            let base = if hovered { theme::ROW_HOVER } else { theme::BG };
            let fill = theme::mix(base, theme::ACCENT, theme::SELECTED);
            widgets::fill(self, Some(&fill));
            return;
        }
        let color = if hovered {
            Some(&iv.hover)
        } else {
            iv.rest.as_ref()
        };
        widgets::fill(self, color.map(|c| &**c));
    }

    pub(super) fn activate(&self) {
        if !self.ivars().enabled.get() {
            return;
        }
        // The press can bring the page down with this view, and the closure with it.
        let press = self.ivars().press.clone();
        let _alive = self.retain();
        press();
    }
}

/// Plays `list` from item `from`, as a row of the song table does.
fn play(list: List, from: usize) -> Rc<dyn Fn()> {
    Rc::new(move || {
        ui::with(|ui| ui.intent(Intent::PlayFrom { list, from }));
    })
}

/// What the top result's card shows.
struct TopFace<'a> {
    title: &'a str,
    sub: &'a str,
    art: Option<&'a str>,
    service: Option<Service>,
    /// The cover's corner radius: half its side for an artist's round photo.
    radius: CGFloat,
}

struct TopCard {
    title_label: Retained<NSTextField>,
    card: Retained<PressView>,
    play: Retained<PressView>,
    cover: Retained<Cover>,
    badge: Option<Retained<Badge>>,
    title: Retained<NSTextField>,
    artists: Retained<NSTextField>,
}

struct SongRow {
    row: Retained<PressView>,
    title: Retained<NSTextField>,
    artists: Retained<NSTextField>,
    duration: Retained<NSTextField>,
    /// The song's place in the results.
    from: usize,
}

pub struct SearchLead {
    pub view: Retained<FlippedView>,
    top: TopCard,
    /// Whether the top result is a song, the results' first.
    top_song: bool,
    songs_title: Option<Retained<NSTextField>>,
    songs: Vec<SongRow>,
}

impl SearchLead {
    /// The lead's height: a title over the card, or over the songs when there are four.
    pub const H: CGFloat = HEAD_H + BODY_H;

    /// `top` as a card with a Play button, and `songs` beside it, in a view `width` wide.
    /// `songs` are the first of the shown results, after the top one when it is a song, so a
    /// row presses to play from its own place in them; `mixed` says whether the results span
    /// services.
    pub fn new(
        mtm: MainThreadMarker,
        top: &TopResult,
        songs: &[Track],
        mixed: bool,
        width: CGFloat,
    ) -> SearchLead {
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<FlippedView> = unsafe {
            msg_send![FlippedView::alloc(mtm), initWithFrame: rect(0.0, 0.0, width, Self::H)]
        };
        view.setAutoresizingMask(FLEX_W);
        let heading = |text: &str| {
            let l = widgets::label(
                mtm,
                text,
                &theme::font(17.0, Weight::SemiBold),
                &theme::ink(),
            );
            view.addSubview(&l);
            l
        };
        let title = heading("Top result");
        let (top, first) = match top {
            TopResult::Song(track) => (Self::top_song(mtm, &view, title, track, mixed), 1),
            TopResult::Artist { artist, playable } => {
                let top = Self::top_artist(mtm, &view, title, artist, mixed);
                top.play.set_enabled(*playable);
                (top, 0)
            }
        };
        let songs_title = (!songs.is_empty()).then(|| heading("Songs"));
        let songs = songs
            .iter()
            .enumerate()
            .map(|(i, t)| Self::song_row(mtm, &view, t, mixed, first + i))
            .collect();
        let lead = SearchLead {
            view,
            top,
            top_song: first == 1,
            songs_title,
            songs,
        };
        lead.layout();
        lead
    }

    /// The top song: a press on the card or its Play plays the results from it.
    fn top_song(
        mtm: MainThreadMarker,
        view: &NSView,
        title_label: Retained<NSTextField>,
        track: &Track,
        mixed: bool,
    ) -> TopCard {
        let card = TopFace {
            title: &track.title,
            sub: &vm::artists(track),
            art: track.artwork_url.as_deref(),
            service: vm::badge(&track.id.provider, mixed),
            radius: 8.0,
        };
        let play = play(List::Results, 0);
        Self::top_card(mtm, view, title_label, card, play.clone(), play)
    }

    /// The top artist, with a round photo: a press on the card opens its page, and its Play
    /// plays its top songs.
    fn top_artist(
        mtm: MainThreadMarker,
        view: &NSView,
        title_label: Retained<NSTextField>,
        artist: &requiem_core::Collection,
        mixed: bool,
    ) -> TopCard {
        let card = TopFace {
            title: &artist.title,
            // With its audience, when the service gave it.
            sub: artist
                .subtitle
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Artist"),
            art: artist.artwork_url.as_deref(),
            service: vm::badge(&artist.id.provider, mixed),
            radius: CARD_COVER / 2.0,
        };
        let shown = artist.clone();
        let open: Rc<dyn Fn()> = Rc::new(move || {
            let artist = shown.clone();
            ui::with(|ui| ui.open_collection(artist, Place::Search));
        });
        Self::top_card(mtm, view, title_label, card, open, play(List::TopArtist, 0))
    }

    fn top_card(
        mtm: MainThreadMarker,
        view: &NSView,
        title_label: Retained<NSTextField>,
        face: TopFace<'_>,
        press: Rc<dyn Fn()>,
        play: Rc<dyn Fn()>,
    ) -> TopCard {
        let TopFace {
            title: name,
            sub: subtitle,
            art,
            service,
            radius,
        } = face;
        let card = PressView::new(
            mtm,
            NSRect::ZERO,
            CARD_RADIUS,
            (Some(theme::bg_2()), theme::row_hover()),
            press,
        );
        let cover = widgets::cover(
            mtm,
            rect(CARD_PAD, CARD_PAD, CARD_COVER, CARD_COVER),
            radius,
            36.0,
        );
        cover.set_url(art);
        card.addSubview(&cover);
        let title = widgets::label(
            mtm,
            name,
            &theme::font(20.0, Weight::SemiBold),
            &theme::ink(),
        );
        let sub = widgets::label(
            mtm,
            subtitle,
            &theme::font(13.0, Weight::Regular),
            &theme::muted(),
        );
        shelves::two_lines(&sub);
        card.addSubview(&title);
        card.addSubview(&sub);
        let mark = service.map(|s| Self::badge(mtm, &card, s, rect(0.0, 0.0, 0.0, 0.0)));
        card.finish(&spoken(name, subtitle, service));

        let disc = PressView::new(
            mtm,
            rect(0.0, 0.0, PLAY, PLAY),
            PLAY / 2.0,
            (Some(theme::accent()), theme::accent_hi()),
            play,
        );
        let glyph = widgets::icon_view(
            mtm,
            Icon::Play,
            PLAY_ICON,
            &theme::on_accent(),
            rect(
                (PLAY - PLAY_ICON) / 2.0,
                (PLAY - PLAY_ICON) / 2.0,
                PLAY_ICON,
                PLAY_ICON,
            ),
        );
        disc.addSubview(&glyph);
        disc.finish(&format!("Play {name}"));
        view.addSubview(&card);
        view.addSubview(&disc);
        TopCard {
            title_label,
            card,
            play: disc,
            cover,
            badge: mark,
            title,
            artists: sub,
        }
    }

    fn song_row(
        mtm: MainThreadMarker,
        view: &NSView,
        track: &Track,
        mixed: bool,
        from: usize,
    ) -> SongRow {
        let artists = vm::artists(track);
        let service = vm::badge(&track.id.provider, mixed);
        let row = PressView::new(
            mtm,
            NSRect::ZERO,
            8.0,
            (None, theme::row_hover()),
            play(List::Results, from),
        );
        let pad = (ROW_H - THUMB) / 2.0;
        let cover = widgets::cover(mtm, rect(pad, pad, THUMB, THUMB), 4.0, 16.0);
        cover.set_url(track.artwork_url.as_deref());
        row.addSubview(&cover);
        let title = widgets::label(
            mtm,
            &track.title,
            &theme::font(14.0, Weight::Medium),
            &theme::ink(),
        );
        let sub = widgets::label(
            mtm,
            &artists,
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
        );
        let duration = widgets::label(
            mtm,
            &track.duration.map(vm::fmt_time).unwrap_or_default(),
            &theme::tabular(12.0, Weight::Regular),
            &theme::muted(),
        );
        duration.setAlignment(NSTextAlignment::Right);
        row.addSubview(&title);
        row.addSubview(&sub);
        row.addSubview(&duration);
        if let Some(s) = service {
            Self::badge(mtm, &row, s, rect(pad, pad, THUMB, THUMB));
        }
        row.finish(&spoken(&track.title, &artists, service));
        view.addSubview(&row);
        SongRow {
            row,
            title,
            artists: sub,
            duration,
            from,
        }
    }

    /// `service`'s badge on a cover at `cover`, added to `parent`.
    fn badge(
        mtm: MainThreadMarker,
        parent: &NSView,
        service: Service,
        cover: NSRect,
    ) -> Retained<Badge> {
        let b = Badge::new(mtm, badge::frame(cover, 6.0, true));
        b.set(Some(service));
        parent.addSubview(&b);
        b
    }

    /// The top result card's width at a lead `width` wide: a share of it, within bounds.
    fn card_w(width: CGFloat) -> CGFloat {
        ((width - 2.0 * PAD - GAP) * 0.4).clamp(240.0, 380.0)
    }

    /// Fits the parts to the lead's width, after the window resized.
    pub fn layout(&self) {
        let width = self.view.frame().size.width;
        let cw = Self::card_w(width);
        let t = &self.top;
        t.title_label.setFrame(rect(
            PAD,
            0.0,
            cw,
            widgets::line_height(&theme::font(17.0, Weight::SemiBold)),
        ));
        t.card.setFrame(rect(PAD, HEAD_H, cw, BODY_H));
        let inner = cw - 2.0 * CARD_PAD;
        let title_y = CARD_PAD + CARD_COVER + 12.0;
        let title_h = widgets::line_height(&theme::font(20.0, Weight::SemiBold));
        t.title.setFrame(rect(CARD_PAD, title_y, inner, title_h));
        // The artists stop short of the Play button.
        t.artists.setFrame(rect(
            CARD_PAD,
            title_y + title_h + 2.0,
            inner - PLAY - 8.0,
            2.0 * widgets::line_height(&theme::font(13.0, Weight::Regular)),
        ));
        let cover = rect(CARD_PAD, CARD_PAD, CARD_COVER, CARD_COVER);
        t.cover.setFrame(cover);
        if let Some(b) = &t.badge {
            b.setFrame(badge::frame(cover, 6.0, true));
        }
        t.play.setFrame(rect(
            PAD + cw - CARD_PAD - PLAY,
            HEAD_H + BODY_H - CARD_PAD - PLAY,
            PLAY,
            PLAY,
        ));

        let x = PAD + cw + GAP;
        let w = (width - x - PAD).max(0.0);
        if let Some(l) = &self.songs_title {
            l.setFrame(rect(
                x,
                0.0,
                w,
                widgets::line_height(&theme::font(17.0, Weight::SemiBold)),
            ));
        }
        let pad = (ROW_H - THUMB) / 2.0;
        let text_x = pad + THUMB + THUMB_GAP;
        let text_w = (w - text_x - DURATION_W - 2.0 * pad).max(0.0);
        let (title_h, sub_h) = (
            widgets::line_height(&theme::font(14.0, Weight::Medium)),
            widgets::line_height(&theme::font(12.0, Weight::Regular)),
        );
        let y = ((ROW_H - title_h - sub_h) / 2.0).round();
        for (i, s) in self.songs.iter().enumerate() {
            s.row
                .setFrame(rect(x, HEAD_H + i as CGFloat * ROW_H, w, ROW_H));
            s.title.setFrame(rect(text_x, y, text_w, title_h));
            s.artists.setFrame(rect(text_x, y + title_h, text_w, sub_h));
            s.duration.setFrame(rect(
                w - pad - DURATION_W,
                ((ROW_H - sub_h) / 2.0).round(),
                DURATION_W,
                sub_h,
            ));
        }
    }

    /// How many songs are listed beside the top result.
    pub fn songs(&self) -> usize {
        self.songs.len()
    }

    /// Shows `spot` as the keyboard's choice, if it is the top result or a song: the card as
    /// a cover grid's selected card, a song as the song table's selected row.
    pub fn select(&self, spot: Option<Spot>) {
        cover_grid::show_selected(&self.top.card, spot == Some(Spot::Top));
        for (i, s) in self.songs.iter().enumerate() {
            let on = spot == Some(Spot::Song(i));
            s.row.set_row_selected(on);
            // Raised for contrast against the selection, as the table's are.
            let c = if on { theme::ink_2() } else { theme::muted() };
            s.artists.setTextColor(Some(&c));
            s.duration.setTextColor(Some(&c));
        }
        if let Some(r) = spot.and_then(|s| self.frame_of(s)) {
            self.view.scrollRectToVisible(r);
        }
    }

    /// Where `spot` is in the lead: the top result with its heading, or a song's row.
    fn frame_of(&self, spot: Spot) -> Option<NSRect> {
        match spot {
            Spot::Top => {
                let f = self.top.card.frame();
                Some(rect(f.origin.x, 0.0, f.size.width, HEAD_H + f.size.height))
            }
            Spot::Song(i) => self.songs.get(i).map(|s| s.row.frame()),
            Spot::Card { .. } => None,
        }
    }

    /// What a click on `spot` presses: the top result's card, or a song's row.
    pub fn press_view(&self, spot: Spot) -> Option<Retained<PressView>> {
        match spot {
            Spot::Top => Some(self.top.card.clone()),
            Spot::Song(i) => self.songs.get(i).map(|s| s.row.clone()),
            Spot::Card { .. } => None,
        }
    }

    /// The place in the results of the song at `spot`, which can join the queue.
    pub fn song_at(&self, spot: Spot) -> Option<usize> {
        match spot {
            Spot::Top => self.top_song.then_some(0),
            Spot::Song(i) => self.songs.get(i).map(|s| s.from),
            Spot::Card { .. } => None,
        }
    }

    /// Turns the top result's Play on once there is something for it to play.
    pub fn set_play_enabled(&self, enabled: bool) {
        self.top.play.set_enabled(enabled);
    }

    /// A cover has not arrived.
    pub fn covers_loading(&self) -> bool {
        let loading = |v: &NSView| {
            v.subviews()
                .iter()
                .any(|s| s.downcast_ref::<Cover>().is_some_and(Cover::is_loading))
        };
        self.top.cover.is_loading() || self.songs.iter().any(|s| loading(&s.row))
    }
}
