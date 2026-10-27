//! Artwork: fetched over HTTPS on worker threads, kept on disk, decoded at the size it shows
//! and held decoded in a small cache, as the macOS window does; and the cover that shows it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, glib};
use requiem_macos::art::{Book, CACHE_DIR_ENV, DiskCache, Lru, Queue, Want, pixel_cost, sized_url};

use super::icons::{self, Badge, Icon};
use crate::vm::Service;

/// Decoded covers kept for reuse, in bytes.
const CAPACITY: usize = 8 * 1024 * 1024;
const DISK_CAPACITY: u64 = 256 * 1024 * 1024;
/// Requests waiting for the network; older ones are dropped for newer.
const QUEUE_LIMIT: usize = 48;
const NETWORK_THREADS: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: u64 = 4 * 1024 * 1024;

type Waiter = Box<dyn FnOnce(&gdk::Texture)>;

struct Loader {
    cache: Lru<gdk::Texture>,
    book: Book<Waiter>,
    fetcher: Option<Arc<Fetcher>>,
    /// The demo's made-up covers by seed, drawn at whatever size is asked.
    demo: HashMap<String, usize>,
}

thread_local! {
    static LOADER: RefCell<Loader> = RefCell::new(Loader {
        cache: Lru::new(CAPACITY),
        book: Book::new(),
        fetcher: None,
        demo: HashMap::new(),
    });
    /// Requests sent and not yet answered, which a snapshot waits on.
    static IN_FLIGHT: Cell<usize> = const { Cell::new(0) };
}

struct Request {
    key: String,
    url: String,
    px: u32,
}

/// A request queue that blocks its readers while empty.
struct Lane {
    queue: Mutex<Queue<Request>>,
    ready: Condvar,
}

impl Lane {
    fn new() -> Lane {
        Lane {
            queue: Mutex::new(Queue::new(QUEUE_LIMIT)),
            ready: Condvar::new(),
        }
    }

    fn push(&self, r: Request) -> Option<Request> {
        let dropped = self
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(r);
        self.ready.notify_one();
        dropped
    }

    fn pop(&self) -> Request {
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(r) = q.pop() {
                return r;
            }
            q = self.ready.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct Fetcher {
    disk: Option<(DiskCache, Lane)>,
    network: Lane,
}

/// Pixels decoded on a worker, made a texture on the main thread.
struct Decoded {
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
    bytes: glib::Bytes,
}

/// The demo's covers, so they show without any fetch.
pub fn seed_demo(covers: impl IntoIterator<Item = (String, usize)>) {
    LOADER.with_borrow_mut(|l| l.demo.extend(covers));
}

/// Covers asked for and not yet in.
pub fn busy() -> bool {
    IN_FLIGHT.get() > 0
}

/// `url`'s artwork `px` pixels square: at once if cached; otherwise `None`, and `done` runs on
/// the main thread when it arrives. It never runs when the load fails or is dropped for newer
/// requests. Only https URLs are fetched.
pub fn load(
    url: &str,
    px: u32,
    done: impl FnOnce(&gdk::Texture) + 'static,
) -> Option<gdk::Texture> {
    let sized = sized_url(url, px);
    let key = format!("{px} {sized}");
    LOADER.with_borrow_mut(|l| {
        if let Some(t) = l.cache.get(&key) {
            return Some(t);
        }
        if let Some(&seed) = l.demo.get(url) {
            let t = icons::demo_cover(seed, px as i32)?;
            l.cache
                .insert(key, t.clone(), pixel_cost(px as usize, px as usize));
            return Some(t);
        }
        if sized.starts_with("https://") && l.book.want(&key, Box::new(done)) == Want::Fetch {
            IN_FLIGHT.set(IN_FLIGHT.get() + 1);
            let fetcher = l.fetcher.get_or_insert_with(start_fetcher).clone();
            let lane = match &fetcher.disk {
                Some((_, lane)) => lane,
                None => &fetcher.network,
            };
            if let Some(dropped) = lane.push(Request {
                key,
                url: sized,
                px,
            }) {
                l.book.dropped(&dropped.key);
                IN_FLIGHT.set(IN_FLIGHT.get().saturating_sub(1));
            }
        }
        None
    })
}

/// `$REQUIEM_ART_CACHE_DIR`, or `$XDG_CACHE_HOME/requiem/artwork`, or
/// `~/.cache/requiem/artwork`. An override that is not absolute keeps nothing on disk.
fn disk_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV).filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
        eprintln!("requiem: not keeping artwork on disk: {CACHE_DIR_ENV} is not absolute");
        return None;
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".cache"))
        })?;
    Some(base.join("requiem").join("artwork"))
}

fn start_fetcher() -> Arc<Fetcher> {
    let fetcher = Arc::new(Fetcher {
        disk: disk_dir().map(|dir| (DiskCache::new(dir, DISK_CAPACITY), Lane::new())),
        network: Lane::new(),
    });
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(IO_TIMEOUT))
        .timeout_recv_response(Some(IO_TIMEOUT))
        .timeout_recv_body(Some(IO_TIMEOUT))
        .build()
        .into();
    let spawn = |name: &str, work: Box<dyn FnOnce() + Send>| {
        if let Err(e) = thread::Builder::new().name(name.into()).spawn(work) {
            eprintln!("requiem: cannot start the artwork thread: {e}");
        }
    };
    if fetcher.disk.is_some() {
        let shared = Arc::clone(&fetcher);
        spawn("requiem-artwork-disk", Box::new(move || disk_loop(&shared)));
    }
    for _ in 0..NETWORK_THREADS {
        let shared = Arc::clone(&fetcher);
        let agent = agent.clone();
        spawn(
            "requiem-artwork",
            Box::new(move || network_loop(&shared, &agent)),
        );
    }
    fetcher
}

fn disk_loop(fetcher: &Fetcher) {
    let Some((disk, lane)) = &fetcher.disk else {
        return;
    };
    disk.prune();
    loop {
        let request = lane.pop();
        if let Some(bytes) = disk.read(&request.url) {
            if let Some(image) = decode(&bytes, request.px) {
                let key = request.key;
                glib::MainContext::default().invoke(move || deliver(&key, Some(image)));
                continue;
            }
            disk.remove(&request.url);
        }
        if let Some(dropped) = fetcher.network.push(request) {
            glib::MainContext::default().invoke(move || {
                LOADER.with_borrow_mut(|l| l.book.dropped(&dropped.key));
                IN_FLIGHT.set(IN_FLIGHT.get().saturating_sub(1));
            });
        }
    }
}

fn network_loop(fetcher: &Fetcher, agent: &ureq::Agent) {
    loop {
        let request = fetcher.network.pop();
        let image = fetch(agent, &request.url).and_then(|bytes| {
            let image = decode(&bytes, request.px)?;
            if let Some((disk, _)) = &fetcher.disk
                && let Err(e) = disk.write(&request.url, &bytes)
            {
                eprintln!("requiem: cannot keep artwork on disk: {e}");
            }
            Some(image)
        });
        let key = request.key;
        glib::MainContext::default().invoke(move || deliver(&key, image));
    }
}

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

/// `bytes` decoded and scaled to fill `px` square, its middle kept, as covers crop.
fn decode(bytes: &[u8], px: u32) -> Option<Decoded> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(bytes).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    let (w, h) = (pixbuf.width(), pixbuf.height());
    if w <= 0 || h <= 0 {
        return None;
    }
    let px = px.max(1) as i32;
    // Scaled so the shorter side fills `px`, never up.
    let scale = (f64::from(px) / f64::from(w.min(h))).min(1.0);
    let (sw, sh) = (
        ((f64::from(w) * scale).round() as i32).max(1),
        ((f64::from(h) * scale).round() as i32).max(1),
    );
    let pixbuf = if (sw, sh) == (w, h) {
        pixbuf
    } else {
        pixbuf.scale_simple(sw, sh, gdk_pixbuf::InterpType::Bilinear)?
    };
    Some(Decoded {
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.has_alpha(),
        bytes: pixbuf.read_pixel_bytes(),
    })
}

fn deliver(key: &str, image: Option<Decoded>) {
    IN_FLIGHT.set(IN_FLIGHT.get().saturating_sub(1));
    let texture = image.map(|d| {
        let format = if d.alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        let cost = pixel_cost(d.width as usize, d.height as usize);
        let t: gdk::Texture =
            gdk::MemoryTexture::new(d.width, d.height, format, &d.bytes, d.stride).upcast();
        (t, cost)
    });
    let waiters = LOADER.with_borrow_mut(|l| {
        let waiters = l.book.finish(key, texture.is_some());
        if let Some((t, cost)) = &texture {
            l.cache.insert(key.to_owned(), t.clone(), *cost);
        }
        waiters
    });
    if let Some((t, _)) = texture {
        for w in waiters {
            w(&t);
        }
    }
}

/// The device pixels a cover `side` points across needs, at the display's scale.
fn pixels(side: i32) -> u32 {
    thread_local! {
        static SCALE: Cell<i32> = const { Cell::new(0) };
    }
    let mut scale = SCALE.get();
    if scale == 0 {
        scale = gdk::Display::default()
            .and_then(|d| {
                let monitors = d.monitors();
                (0..monitors.n_items())
                    .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
                    .map(|m| m.scale_factor())
                    .max()
            })
            .unwrap_or(1)
            .max(1);
        SCALE.set(scale);
    }
    (side * scale).max(1) as u32
}

/// A square cover with a placeholder until its artwork arrives, and a service badge at its
/// corner. `root` is what to add to a parent.
#[derive(Clone)]
pub struct Cover {
    pub root: gtk::Overlay,
    frame: gtk::Box,
    picture: gtk::Picture,
    placeholder: gtk::DrawingArea,
    badge: Rc<Badge>,
    side: Rc<Cell<i32>>,
    /// The URL shown or asked for.
    url: Rc<RefCell<Option<String>>>,
    loading: Rc<Cell<bool>>,
}

impl Cover {
    /// A cover `side` points square; `classes` are added to its frame (`round`, `small`).
    pub fn new(side: i32, classes: &[&str], badge: bool) -> Cover {
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("cover");
        for c in classes {
            frame.add_css_class(c);
        }
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(side, side);
        frame.set_halign(gtk::Align::Start);
        frame.set_valign(gtk::Align::Start);
        // The placeholder's stretch stays inside the frame.
        frame.set_hexpand(false);
        frame.set_vexpand(false);
        let stack = gtk::Overlay::new();
        let placeholder = icons::icon(Icon::Music, (side / 3).clamp(12, 64));
        placeholder.set_hexpand(true);
        placeholder.set_vexpand(true);
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_can_shrink(true);
        picture.set_size_request(side, side);
        picture.set_visible(false);
        stack.set_child(Some(&placeholder));
        stack.add_overlay(&picture);
        frame.append(&stack);
        let root = gtk::Overlay::new();
        root.set_child(Some(&frame));
        root.set_halign(gtk::Align::Start);
        root.set_valign(gtk::Align::Start);
        let side_badge = icons::badge_side(f64::from(side));
        let b = Rc::new(Badge::new(side_badge, true));
        if badge {
            // Set off the corner by a fifth of its side, outside the cover's clip.
            let off = side_badge / 5;
            b.area.set_margin_end(-off);
            b.area.set_margin_bottom(-off);
            root.add_overlay(&b.area);
        }
        Cover {
            root,
            frame,
            picture,
            placeholder,
            badge: b,
            side: Rc::new(Cell::new(side)),
            url: Rc::new(RefCell::new(None)),
            loading: Rc::new(Cell::new(false)),
        }
    }

    pub fn set_badge(&self, service: Option<Service>) {
        self.badge.set(service);
    }

    /// Rounds the cover into a circle, for artists, or squares it back.
    pub fn set_round(&self, round: bool) {
        super::theme::toggle_class(&self.frame, "round", round);
    }

    /// Dims the cover, as a played row's.
    pub fn set_dim(&self, dim: bool) {
        self.root.set_opacity(if dim { 0.6 } else { 1.0 });
    }

    /// Shows `url`'s artwork, or the placeholder for `None`. Loads only what `near` allows: a
    /// cover far off screen keeps its placeholder until `set_url` comes again.
    pub fn set_url(&self, url: Option<&str>) {
        if self.url.borrow().as_deref() == url && (self.picture.is_visible() || self.loading.get())
        {
            return;
        }
        *self.url.borrow_mut() = url.map(str::to_owned);
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.picture.set_visible(false);
        self.placeholder.set_visible(true);
        self.loading.set(false);
        let Some(url) = url else { return };
        let want = self.url.clone();
        let picture = self.picture.downgrade();
        let placeholder = self.placeholder.downgrade();
        let loading = self.loading.clone();
        let asked = url.to_owned();
        let shown = load(url, pixels(self.side.get()), move |t| {
            if want.borrow().as_deref() != Some(asked.as_str()) {
                return;
            }
            loading.set(false);
            if let (Some(p), Some(ph)) = (picture.upgrade(), placeholder.upgrade()) {
                p.set_paintable(Some(t));
                p.set_visible(true);
                ph.set_visible(false);
            }
        });
        match shown {
            Some(t) => {
                self.picture.set_paintable(Some(&t));
                self.picture.set_visible(true);
                self.placeholder.set_visible(false);
            }
            None => self.loading.set(url.starts_with("https://")),
        }
    }

    /// Makes the cover `side` points square, loading its artwork again at that size.
    pub fn resize(&self, side: i32) {
        if self.side.replace(side) == side {
            return;
        }
        self.frame.set_size_request(side, side);
        self.picture.set_size_request(side, side);
        let url = self.url.borrow_mut().take();
        self.set_url(url.as_deref());
    }

    /// Its artwork was asked for and has not arrived.
    pub fn is_loading(&self) -> bool {
        self.loading.get()
    }
}
