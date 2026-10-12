//! Now Playing: the playing track's cover, large, beside its details and what plays next.

use std::cell::{Cell, RefCell};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{NSImage, NSResponder, NSScreen, NSTextField, NSView};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSRect, NSSize};
use objc2_quartz_core::kCAGravityResizeAspectFill;
use requiem_core::Track;
use requiem_macos::artwork::{self, Cover};

use super::shelves::FlippedView;
use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{self, EmptyState, FLEX_H, FLEX_W, HoverButton, rect};
use crate::vm;

/// The cover's distance from the page's top and left edges.
const MARGIN: CGFloat = 40.0;
const COVER_MIN: CGFloat = 200.0;
const COVER_MAX: CGFloat = 420.0;
/// Home's card corners.
const COVER_RADIUS: CGFloat = 8.0;
/// Between the cover and the details.
const DETAILS_GAP: CGFloat = 32.0;
const UP_NEXT_GAP: CGFloat = 24.0;
const UP_NEXT_H: CGFloat = 24.0;
const UP_NEXT_TITLE_GAP: CGFloat = 10.0;
/// Up-next rows: Home's Quick picks rows.
const ROW_H: CGFloat = 56.0;
const ROW_COVER: CGFloat = 44.0;
const ROW_PAD: CGFloat = (ROW_H - ROW_COVER) / 2.0;
/// "Show queue", below the last row.
const BUTTON_GAP: CGFloat = 12.0;
const BUTTON_H: CGFloat = 32.0;
/// Larger artwork looks no sharper.
const MAX_PX: u32 = 1080;
/// How far the cover's side may drift from the side its image was loaded for before it loads
/// again, as a fraction of that side.
const RELOAD_DRIFT: CGFloat = 0.2;

/// The cover's side on a page `w` by `h`.
fn cover_side(w: CGFloat, h: CGFloat) -> CGFloat {
    (h - 80.0).min(0.45 * w).clamp(COVER_MIN, COVER_MAX)
}

/// The pixels of artwork a cover `side` points square needs at `scale`.
fn pixels(side: CGFloat, scale: CGFloat) -> u32 {
    ((side * scale).ceil() as u32).min(MAX_PX)
}

/// How many of `wanted` up-next rows starting at `top` fit on a page `height` tall with
/// "Show queue" whole below them.
fn rows_that_fit(top: CGFloat, height: CGFloat, wanted: usize) -> usize {
    let room = height - top - BUTTON_GAP - BUTTON_H;
    ((room / ROW_H).floor().max(0.0) as usize).min(wanted)
}

/// Whether the cover's image must load again: for another URL, or a side that drifted from
/// the one it was loaded for.
fn needs_load(loaded: Option<&(String, CGFloat)>, url: &str, side: CGFloat) -> bool {
    loaded.is_none_or(|(u, s)| u != url || (side - s).abs() > RELOAD_DRIFT * s)
}

define_class!(
    /// The page, laid out from its top left, again whenever its size changes.
    // SAFETY: NSView has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct NowPlayingView;

    impl NowPlayingView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, old: NSSize) {
            // SAFETY: forwards to the superclass implementation.
            let _: () = unsafe { msg_send![super(self), resizeSubviewsWithOldSize: old] };
            ui::with(|ui| ui.now_playing.layout());
        }
    }
);

pub struct ArtIvars {
    image: RefCell<Option<Retained<NSImage>>>,
    /// Bumped by every load, so an image that arrives for an earlier one is dropped.
    generation: Cell<u64>,
}

define_class!(
    /// The cover's image over its placeholder, scaled to fill and cropped.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ArtIvars]
    struct NowArt;

    impl NowArt {
        /// The layer's contents are the image, so AppKit gives it no backing store to draw.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            self.paint();
        }
    }
);

impl NowArt {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ArtIvars {
            image: RefCell::new(None),
            generation: Cell::new(0),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        widgets::round(&this, COVER_RADIUS);
        this
    }

    fn set_image(&self, image: Option<&NSImage>) {
        self.ivars().image.replace(image.map(|i| i.retain()));
        self.paint();
    }

    fn paint(&self) {
        let Some(layer) = self.layer() else { return };
        let image = self.ivars().image.borrow();
        let contents: Option<&AnyObject> = image.as_deref().map(|i| i.as_ref());
        // SAFETY: on macOS a layer's contents may be an NSImage, and the gravity static is an
        // immutable framework constant.
        unsafe {
            layer.setContents(contents);
            layer.setContentsGravity(kCAGravityResizeAspectFill);
        }
    }
}

/// One upcoming track, shown and not clickable.
struct NextRow {
    view: Retained<NSView>,
    cover: Retained<Cover>,
    title: Retained<NSTextField>,
    artists: Retained<NSTextField>,
}

impl NextRow {
    fn new(mtm: MainThreadMarker) -> NextRow {
        let view = widgets::view(mtm, rect(0.0, 0.0, 300.0, ROW_H), FLEX_W);
        let pad = ROW_PAD;
        let cover = widgets::cover(mtm, rect(pad, pad, ROW_COVER, ROW_COVER), 4.0, 16.0);
        let title_font = theme::font(14.0, Weight::Medium);
        let sub_font = theme::font(12.0, Weight::Regular);
        let (th, sh) = (
            widgets::line_height(&title_font),
            widgets::line_height(&sub_font),
        );
        // `view` is not flipped: the title sits above the artists.
        let y = ((ROW_H - th - sh) / 2.0).round();
        let x = pad + ROW_COVER + 12.0;
        let w = 300.0 - x - 8.0;
        let title = widgets::label(mtm, "", &title_font, &theme::ink());
        title.setFrame(rect(x, y + sh, w, th));
        title.setAutoresizingMask(FLEX_W);
        let artists = widgets::label(mtm, "", &sub_font, &theme::muted());
        artists.setFrame(rect(x, y, w, sh));
        artists.setAutoresizingMask(FLEX_W);
        view.addSubview(&cover);
        view.addSubview(&title);
        view.addSubview(&artists);
        NextRow {
            view,
            cover,
            title,
            artists,
        }
    }

    fn set(&self, track: &Track) {
        self.cover.set_url(track.artwork_url.as_deref());
        widgets::set_text(&self.title, &track.title);
        widgets::set_text(&self.artists, &vm::artists(track));
    }
}

pub struct NowPlayingPage {
    mtm: MainThreadMarker,
    pub view: Retained<NowPlayingView>,
    /// Everything but the empty state.
    content: Retained<FlippedView>,
    placeholder: Retained<Cover>,
    art: Retained<NowArt>,
    title: Retained<NSTextField>,
    artists: Retained<NSTextField>,
    album: Retained<NSTextField>,
    up_next: Retained<NSTextField>,
    rows: Vec<NextRow>,
    nothing_next: Retained<NSTextField>,
    show_queue: Retained<HoverButton>,
    empty: EmptyState,
    track: RefCell<Option<Track>>,
    next_count: Cell<usize>,
    /// The URL and side the cover's image was last asked for.
    loaded: RefCell<Option<(String, CGFloat)>>,
}

impl NowPlayingPage {
    /// "Show queue" sends `show_queue` to `target`.
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        target: &AnyObject,
        show_queue: Sel,
    ) -> NowPlayingPage {
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<NowPlayingView> =
            unsafe { msg_send![NowPlayingView::alloc(mtm), initWithFrame: frame] };
        view.setAutoresizingMask(FLEX_W | FLEX_H);
        view.setWantsLayer(true);
        let bounds = rect(0.0, 0.0, frame.size.width, frame.size.height);
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let content: Retained<FlippedView> =
            unsafe { msg_send![FlippedView::alloc(mtm), initWithFrame: bounds] };
        content.setAutoresizingMask(FLEX_W | FLEX_H);
        let placeholder = widgets::cover(mtm, rect(0.0, 0.0, 200.0, 200.0), COVER_RADIUS, 48.0);
        let art = NowArt::new(mtm);
        let title = widgets::label(mtm, "", &theme::font(20.0, Weight::SemiBold), &theme::ink());
        let artists = widgets::label(
            mtm,
            "",
            &theme::font(15.0, Weight::Regular),
            &theme::ink_2(),
        );
        let album = widgets::label(
            mtm,
            "",
            &theme::font(14.0, Weight::Regular),
            &theme::muted(),
        );
        let up_next = widgets::label(
            mtm,
            "Up next",
            &theme::font(17.0, Weight::SemiBold),
            &theme::ink(),
        );
        let nothing_next = widgets::label(
            mtm,
            "Nothing up next",
            &theme::font(14.0, Weight::Regular),
            &theme::muted(),
        );
        let rows: Vec<NextRow> = (0..vm::UP_NEXT).map(|_| NextRow::new(mtm)).collect();
        let button = HoverButton::plain(mtm, rect(0.0, 0.0, 0.0, BUTTON_H));
        button.set_action(target, show_queue);
        let w = button.set_label("Show queue");
        button.setFrameSize(NSSize::new(w, BUTTON_H));
        for v in [
            &*placeholder as &NSView,
            &art,
            &title,
            &artists,
            &album,
            &up_next,
            &nothing_next,
        ] {
            content.addSubview(v);
        }
        for row in &rows {
            content.addSubview(&row.view);
        }
        content.addSubview(&button);
        let empty = EmptyState::new(mtm, bounds, Icon::Music, target, sel!(showHome:));
        empty.set(
            Icon::Music,
            "Nothing playing",
            "Pick a song from Home or Search.",
            None,
        );
        view.addSubview(&content);
        view.addSubview(&empty.view);
        let page = NowPlayingPage {
            mtm,
            view,
            content,
            placeholder,
            art,
            title,
            artists,
            album,
            up_next,
            rows,
            nothing_next,
            show_queue: button,
            empty,
            track: RefCell::new(None),
            next_count: Cell::new(0),
            loaded: RefCell::new(None),
        };
        page.set(None, &[]);
        page
    }

    /// Shows `track` and the `next` tracks after it; the empty state for no track.
    pub fn set(&self, track: Option<Track>, next: &[Track]) {
        let playing = track.is_some();
        self.content.setHidden(!playing);
        self.empty.view.setHidden(playing);
        if let Some(t) = &track {
            widgets::set_text(&self.title, &t.title);
            widgets::set_text(&self.artists, &vm::artists(t));
            let album = t.album.as_deref().unwrap_or_default();
            widgets::set_text(&self.album, album);
            self.album.setHidden(album.is_empty());
        }
        let next = &next[..next.len().min(self.rows.len())];
        for (row, t) in self.rows.iter().zip(next) {
            row.set(t);
        }
        self.nothing_next.setHidden(!next.is_empty());
        self.next_count.set(next.len());
        self.track.replace(track);
        self.layout();
    }

    /// Places the cover for the page's size, and the details beside it.
    pub fn layout(&self) {
        let b = self.view.bounds();
        let side = cover_side(b.size.width, b.size.height).round();
        let cover = rect(MARGIN, MARGIN, side, side);
        self.placeholder.setFrame(cover);
        self.art.setFrame(cover);
        let x = MARGIN + side + DETAILS_GAP;
        let w = (b.size.width - x - MARGIN).max(0.0);
        let mut y = MARGIN;
        let mut place = |l: &NSTextField, gap: CGFloat| {
            let h = l.font().map_or(20.0, |f| widgets::line_height(&f));
            y += gap;
            l.setFrame(rect(x, y, w, h));
            y += h;
        };
        place(&self.title, 0.0);
        place(&self.artists, 4.0);
        if !self.album.isHidden() {
            place(&self.album, 2.0);
        }
        y += UP_NEXT_GAP;
        self.up_next.setFrame(rect(x, y, w, UP_NEXT_H));
        y += UP_NEXT_H + UP_NEXT_TITLE_GAP;
        let n = self.next_count.get();
        if n == 0 {
            let h = self
                .nothing_next
                .font()
                .map_or(20.0, |f| widgets::line_height(&f));
            self.nothing_next.setFrame(rect(x, y, w, h));
            y += h;
        }
        let shown = rows_that_fit(y, b.size.height, n);
        for (i, row) in self.rows.iter().enumerate() {
            row.view.setHidden(i >= shown);
        }
        // Covers and titles line up with the details above them.
        for row in &self.rows[..shown] {
            row.view.setFrame(rect(x - ROW_PAD, y, w + ROW_PAD, ROW_H));
            y += ROW_H;
        }
        // Its title lines up with the labels' text, inset 2 pt; its hover fill reaches past.
        let bw = self.show_queue.frame().size.width;
        self.show_queue
            .setFrame(rect(x - 10.0, y + BUTTON_GAP, bw, BUTTON_H));
        self.load_art(side);
    }

    fn load_art(&self, side: CGFloat) {
        let url = self
            .track
            .borrow()
            .as_ref()
            .and_then(|t| t.artwork_url.clone());
        let iv = self.art.ivars();
        let Some(url) = url else {
            iv.generation.set(iv.generation.get().wrapping_add(1));
            self.art.set_image(None);
            self.loaded.replace(None);
            return;
        };
        if !needs_load(self.loaded.borrow().as_ref(), &url, side) {
            return;
        }
        let same_art = self
            .loaded
            .borrow()
            .as_ref()
            .is_some_and(|(u, _)| *u == url);
        // A resize keeps the image it has until the sharper one arrives.
        if !same_art {
            self.art.set_image(None);
        }
        self.loaded.replace(Some((url.clone(), side)));
        let generation = iv.generation.get().wrapping_add(1);
        iv.generation.set(generation);
        let scale = self
            .view
            .window()
            .map(|w| w.backingScaleFactor())
            .or_else(|| NSScreen::mainScreen(self.mtm).map(|s| s.backingScaleFactor()))
            .unwrap_or(2.0);
        let art = Weak::new(&*self.art);
        let cached = artwork::load(self.mtm, &url, pixels(side, scale), move |image| {
            if let Some(art) = art.load()
                && art.ivars().generation.get() == generation
            {
                art.set_image(Some(image));
            }
        });
        if let Some(image) = cached {
            self.art.set_image(Some(&image));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cover_takes_45_percent_of_the_width_within_its_bounds() {
        // 860 by 648, the default window's page: the width decides.
        assert_eq!(cover_side(860.0, 648.0), 387.0);
        // A short page: its height less 80 decides.
        assert_eq!(cover_side(1200.0, 380.0), 300.0);
        assert_eq!(cover_side(560.0, 260.0), 200.0);
        assert_eq!(cover_side(2000.0, 1200.0), 420.0);
    }

    #[test]
    fn artwork_loads_at_the_screen_scale_up_to_1080_pixels() {
        assert_eq!(pixels(387.0, 2.0), 774);
        assert_eq!(pixels(200.5, 1.0), 201);
        assert_eq!(pixels(420.0, 3.0), 1080);
    }

    #[test]
    fn as_many_up_next_rows_show_as_leave_show_queue_whole() {
        // Room for all 5, and no more than there are.
        assert_eq!(rows_that_fit(166.0, 648.0, 5), 5);
        assert_eq!(rows_that_fit(166.0, 648.0, 3), 3);
        // 166 + 2 rows + 12 + 32 = 322; a third row needs 378.
        assert_eq!(rows_that_fit(166.0, 322.0, 5), 2);
        assert_eq!(rows_that_fit(166.0, 377.0, 5), 2);
        // No room for a row, or for the button alone.
        assert_eq!(rows_that_fit(166.0, 265.0, 5), 0);
        assert_eq!(rows_that_fit(166.0, 180.0, 5), 0);
    }

    #[test]
    fn the_cover_loads_again_for_another_track_or_a_20_percent_resize() {
        let loaded = ("a".to_owned(), 300.0);
        assert!(needs_load(None, "a", 300.0));
        assert!(!needs_load(Some(&loaded), "a", 300.0));
        assert!(!needs_load(Some(&loaded), "a", 360.0));
        assert!(needs_load(Some(&loaded), "a", 361.0));
        assert!(needs_load(Some(&loaded), "a", 239.0));
        assert!(needs_load(Some(&loaded), "b", 300.0));
    }
}
