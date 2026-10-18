//! Artwork, fetched on one background thread and decoded on the main thread into a cache shared
//! by every [`Cover`] in the process. Nothing is written to disk.

use std::cell::{Cell, OnceCell, RefCell};
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Bool};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSControl, NSImage,
    NSImageScaling, NSImageView, NSResponder, NSScreen, NSView, NSWindowOcclusionState,
    NSWorkspace,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    MainThreadMarker, NSData, NSNumber, NSObject, NSPoint, NSRect, NSSize, ns_string,
};
use objc2_quartz_core::{
    CABasicAnimation, CAMediaTiming, CAMediaTimingFunction, kCAGravityResizeAspectFill,
};

use crate::art::{Book, Lru, Queue, Want, pixel_cost, sized_url};

/// The decoded-image budget unless [`set_capacity`] changes it.
pub const DEFAULT_CAPACITY: usize = 8 * 1024 * 1024;
/// Waiting requests past this many drop the oldest, for rows long scrolled away.
const QUEUE_LIMIT: usize = 48;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Covers are tens of KB; anything near this is not one.
const MAX_BODY: u64 = 4 * 1024 * 1024;
const FADE: Duration = Duration::from_millis(120);

type Waiter = Box<dyn FnOnce(&NSImage)>;

struct Fetcher {
    queue: Mutex<Queue>,
    ready: Condvar,
}

struct Loader {
    cache: Lru<Retained<NSImage>>,
    book: Book<Waiter>,
    /// Started by the first fetch.
    fetcher: Option<Arc<Fetcher>>,
}

thread_local! {
    /// Only the main thread uses it: the public functions take a `MainThreadMarker`, and
    /// fetches deliver through the main queue.
    static LOADER: RefCell<Loader> = RefCell::new(Loader {
        cache: Lru::new(DEFAULT_CAPACITY),
        book: Book::new(),
        fetcher: None,
    });
}

/// Sets the budget for decoded images, evicting down to it.
pub fn set_capacity(_mtm: MainThreadMarker, bytes: usize) {
    LOADER.with_borrow_mut(|l| l.cache.set_cap(bytes));
}

/// Caches `image` as `url`'s artwork, as if fetched. Found at every size only for URLs that
/// take none, such as demo artwork.
pub fn insert(_mtm: MainThreadMarker, url: &str, image: &NSImage) {
    LOADER.with_borrow_mut(|l| {
        l.cache
            .insert(url.to_owned(), image.retain(), image_cost(image))
    });
}

/// `url`'s artwork drawn `px` pixels square: returned at once if cached. Otherwise `None`, and
/// `done` runs on the main thread once it arrives; it never runs if the load fails or the
/// request is dropped for newer ones. Only https URLs are fetched.
pub fn load(
    _mtm: MainThreadMarker,
    url: &str,
    px: u32,
    done: impl FnOnce(&NSImage) + 'static,
) -> Option<Retained<NSImage>> {
    let key = sized_url(url, px);
    LOADER.with_borrow_mut(|l| {
        if let Some(image) = l.cache.get(&key) {
            return Some(image);
        }
        if key.starts_with("https://") && l.book.want(&key, Box::new(done)) == Want::Fetch {
            l.enqueue(key);
        }
        None
    })
}

impl Loader {
    fn enqueue(&mut self, key: String) {
        let fetcher = self.fetcher.get_or_insert_with(start_fetcher);
        let dropped = fetcher
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key);
        fetcher.ready.notify_one();
        if let Some(key) = dropped {
            self.book.dropped(&key);
        }
    }
}

fn start_fetcher() -> Arc<Fetcher> {
    let fetcher = Arc::new(Fetcher {
        queue: Mutex::new(Queue::new(QUEUE_LIMIT)),
        ready: Condvar::new(),
    });
    let shared = Arc::clone(&fetcher);
    let spawned = thread::Builder::new()
        .name("requiem-artwork".into())
        .spawn(move || fetch_loop(&shared));
    if let Err(e) = spawned {
        eprintln!("requiem: cannot start the artwork thread: {e}");
    }
    fetcher
}

/// Waits on the condvar while idle, so it costs nothing between requests.
fn fetch_loop(fetcher: &Fetcher) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(IO_TIMEOUT))
        .timeout_recv_response(Some(IO_TIMEOUT))
        .timeout_recv_body(Some(IO_TIMEOUT))
        .build()
        .into();
    loop {
        let key = {
            let mut queue = fetcher.queue.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(key) = queue.pop() {
                    break key;
                }
                queue = fetcher
                    .ready
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let body = fetch(&agent, &key);
        DispatchQueue::main().exec_async(move || deliver(&key, body));
    }
}

/// The body of a successful response no larger than `MAX_BODY`.
fn fetch(agent: &ureq::Agent, url: &str) -> Option<Vec<u8>> {
    let response = agent.get(url).call().ok()?;
    let mut body = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() as u64 <= MAX_BODY).then_some(body)
}

/// On the main thread. Waiters run after the loader is released, so they may load again.
fn deliver(key: &str, body: Option<Vec<u8>>) {
    let image = body.and_then(|bytes| {
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&bytes))?;
        image.isValid().then_some(image)
    });
    let waiters = LOADER.with_borrow_mut(|l| {
        if let Some(image) = &image {
            l.cache
                .insert(key.to_owned(), image.clone(), image_cost(image));
        }
        l.book.finish(key, image.is_some())
    });
    if let Some(image) = image {
        for done in waiters {
            done(&image);
        }
    }
}

/// The largest representation's pixels; for an image drawn on demand, its size at 2x.
fn image_cost(image: &NSImage) -> usize {
    let largest = image
        .representations()
        .iter()
        .map(|r| (r.pixelsWide(), r.pixelsHigh()))
        .filter(|&(w, h)| w > 0 && h > 0)
        .max_by_key(|&(w, h)| w.saturating_mul(h));
    match largest {
        Some((w, h)) => pixel_cost(w as usize, h as usize),
        None => {
            let s = image.size();
            pixel_cost(
                (s.width * 2.0).ceil() as usize,
                (s.height * 2.0).ceil() as usize,
            )
        }
    }
}

/// How a [`Cover`] looks without an image.
pub struct Placeholder {
    pub fill: Retained<NSColor>,
    /// A template image, centred and tinted `tint`.
    pub glyph: Option<Retained<NSImage>>,
    pub tint: Retained<NSColor>,
}

pub struct CoverIvars {
    fill: Retained<NSColor>,
    art: OnceCell<Retained<CoverArt>>,
    url: RefCell<Option<String>>,
    /// Bumped by every `set_url`, so an image for an earlier URL is dropped.
    generation: Cell<u64>,
}

define_class!(
    /// Square artwork with rounded corners over its placeholder. Decorative: it is not an
    /// accessibility element, and neither is anything in it.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RequiemCover"]
    #[ivars = CoverIvars]
    pub struct Cover;

    impl Cover {
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        /// Resolves the fill for the view's appearance, which may have changed.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            self.paint();
        }
    }
);

pub struct ArtIvars {
    image: RefCell<Option<Retained<NSImage>>>,
}

define_class!(
    /// The image layer of a `Cover`, scaled to fill it and cropped.
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RequiemCoverArt"]
    #[ivars = ArtIvars]
    struct CoverArt;

    impl CoverArt {
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

impl CoverArt {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ArtIvars {
            image: RefCell::new(None),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setWantsLayer(true);
        this
    }

    fn set_image(&self, image: Option<&NSImage>) {
        self.ivars().image.replace(image.map(NSImage::retain));
        self.paint();
    }

    /// The image goes in as a CGImage: `cacheDisplayInRect:`, which snapshots draw with, leaves
    /// an NSImage of a JPEG with subsampled chroma blank, as Spotify's mix covers are, though
    /// the screen shows it.
    fn paint(&self) {
        let Some(layer) = self.layer() else { return };
        let image = self.ivars().image.borrow();
        let mut rect = self.bounds();
        // SAFETY: `rect` outlives the call, and no hints are passed.
        let cg = image
            .as_deref()
            .and_then(|i| unsafe { i.CGImageForProposedRect_context_hints(&mut rect, None, None) });
        let contents: Option<&AnyObject> = cg.as_deref().map(|c| c.as_ref());
        // SAFETY: a layer's contents may be a CGImage, and the gravity static is an immutable
        // framework constant.
        unsafe {
            layer.setContents(contents);
            layer.setContentsGravity(kCAGravityResizeAspectFill);
        }
    }
}

impl Cover {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        radius: CGFloat,
        placeholder: &Placeholder,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CoverIvars {
            fill: placeholder.fill.clone(),
            art: OnceCell::new(),
            url: RefCell::new(None),
            generation: Cell::new(0),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setWantsLayer(true);
        if let Some(layer) = this.layer() {
            layer.setCornerRadius(radius);
            layer.setMasksToBounds(true);
        }
        this.paint();
        this.setAccessibilityElement(false);

        let (w, h) = (frame.size.width, frame.size.height);
        if let Some(glyph) = &placeholder.glyph {
            let s = glyph.size();
            let g = NSImageView::initWithFrame(
                NSImageView::alloc(mtm),
                NSRect::new(
                    NSPoint::new(
                        ((w - s.width) / 2.0).round(),
                        ((h - s.height) / 2.0).round(),
                    ),
                    s,
                ),
            );
            g.setImage(Some(glyph));
            g.setImageScaling(NSImageScaling::ScaleProportionallyDown);
            g.setContentTintColor(Some(&placeholder.tint));
            g.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin
                    | NSAutoresizingMaskOptions::ViewMaxXMargin
                    | NSAutoresizingMaskOptions::ViewMinYMargin
                    | NSAutoresizingMaskOptions::ViewMaxYMargin,
            );
            hide_from_accessibility(&g);
            this.addSubview(&g);
        }
        let art = CoverArt::new(mtm, NSRect::new(NSPoint::ZERO, NSSize::new(w, h)));
        art.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        this.addSubview(&art);
        let _ = this.ivars().art.set(art);
        this
    }

    /// Shows `url`'s artwork; the placeholder while it loads, if it fails, or for `None`. An
    /// image already cached shows at once; one that arrives later fades in, unless the window
    /// is not visible or Reduce Motion is on.
    pub fn set_url(&self, url: Option<&str>) {
        let iv = self.ivars();
        if iv.url.borrow().as_deref() == url {
            return;
        }
        iv.url.replace(url.map(str::to_owned));
        let generation = iv.generation.get().wrapping_add(1);
        iv.generation.set(generation);
        let image = url.and_then(|url| {
            let cover = Weak::new(self);
            load(self.mtm(), url, self.pixel_size(), move |image| {
                if let Some(cover) = cover.load()
                    && cover.ivars().generation.get() == generation
                {
                    cover.show(Some(image), true);
                }
            })
        });
        self.show(image.as_deref(), false);
    }

    /// A URL is set whose image has not arrived: still loading, or failed.
    pub fn is_loading(&self) -> bool {
        let iv = self.ivars();
        iv.url.borrow().is_some()
            && iv
                .art
                .get()
                .is_some_and(|art| art.ivars().image.borrow().is_none())
    }

    fn show(&self, image: Option<&NSImage>, arrived: bool) {
        let Some(art) = self.ivars().art.get() else {
            return;
        };
        art.set_image(image);
        let Some(layer) = art.layer() else { return };
        layer.removeAllAnimations();
        if !(arrived && self.visible() && !reduce_motion()) {
            return;
        }
        let fade = CABasicAnimation::animationWithKeyPath(Some(ns_string!("opacity")));
        let (from, to) = (NSNumber::new_f32(0.0), NSNumber::new_f32(1.0));
        let (from, to): (&AnyObject, &AnyObject) = (&from, &to);
        // SAFETY: opacity animates between NSNumbers.
        unsafe {
            fade.setFromValue(Some(from));
            fade.setToValue(Some(to));
        }
        fade.setDuration(FADE.as_secs_f64());
        // requiem-gui's EASE_OUT curve.
        fade.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
            0.23, 1.0, 0.32, 1.0,
        )));
        layer.addAnimation_forKey(&fade, Some(ns_string!("fade")));
    }

    fn paint(&self) {
        if let Some(layer) = self.layer() {
            layer.setBackgroundColor(Some(&self.ivars().fill.CGColor()));
        }
    }

    fn visible(&self) -> bool {
        self.window().is_some_and(|w| {
            w.isVisible() && w.occlusionState().contains(NSWindowOcclusionState::Visible)
        })
    }

    /// The pixels its longer side covers. A view not yet in a window, such as a table cell
    /// being set up, takes the main screen's scale.
    fn pixel_size(&self) -> u32 {
        let scale = self
            .window()
            .map(|w| w.backingScaleFactor())
            .or_else(|| NSScreen::mainScreen(self.mtm()).map(|s| s.backingScaleFactor()))
            .unwrap_or(2.0);
        let size = self.bounds().size;
        (size.width.max(size.height) * scale).ceil().max(1.0) as u32
    }
}

fn reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// Keeps `v` out of the accessibility tree, and its cell, which stands in for a control.
pub(crate) fn hide_from_accessibility(v: &NSView) {
    v.setAccessibilityElement(false);
    if let Some(cell) = v.downcast_ref::<NSControl>().and_then(|c| c.cell()) {
        cell.setAccessibilityElement(false);
    }
}

/// A made-up cover for demos and snapshots, `points` square: a dark tone with a lighter disc,
/// varied by `seed`, drawn at whatever size it is shown.
#[doc(hidden)]
pub fn demo_cover(seed: usize, points: CGFloat) -> Retained<NSImage> {
    let hue = (seed as CGFloat * 0.161).fract();
    let base = NSColor::colorWithHue_saturation_brightness_alpha(hue, 0.55, 0.42, 1.0);
    let disc =
        NSColor::colorWithHue_saturation_brightness_alpha((hue + 0.09).fract(), 0.45, 0.88, 1.0);
    let (dx, dy) = [(0.62, 0.38), (0.35, 0.6), (0.5, 0.5), (0.7, 0.68)][seed % 4];
    let draw = RcBlock::new(move |r: NSRect| -> Bool {
        base.setFill();
        NSBezierPath::fillRect(r);
        disc.setFill();
        let d = r.size.width * 0.58;
        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
            NSPoint::new(r.size.width * dx - d / 2.0, r.size.height * dy - d / 2.0),
            NSSize::new(d, d),
        ))
        .fill();
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(points, points), false, &draw)
}
