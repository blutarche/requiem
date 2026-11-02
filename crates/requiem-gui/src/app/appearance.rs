//! Light, dark, or the system's: the app's appearance, and the layer colours that follow it.
//!
//! Text, tints and drawn colours are the theme's dynamic colours, which AppKit resolves as it
//! draws, and it redraws when the appearance changes. A layer's colour is a `CGColor`, fixed
//! when it is set, so every view whose layer colours are set here is remembered, and its
//! colours are set again when the app's appearance changes, from the system or from Settings.
//!
//! The window takes the app's appearance; no view sets one of its own.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use block2::StackBlock;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSColor, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_core_graphics::CGColor;
use objc2_foundation::{
    MainThreadMarker, NSDictionary, NSKeyValueObservingOptions,
    NSObjectNSKeyValueObserverRegistration, NSString, ns_string,
};

use crate::vm::Appearance;

thread_local! {
    static CHOSEN: Cell<Appearance> = const { Cell::new(Appearance::System) };
    static PAINTED: RefCell<Painted> = RefCell::new(Painted::default());
}

/// Gives the app `appearance`; every window follows at once.
pub fn apply(mtm: MainThreadMarker, appearance: Appearance) {
    CHOSEN.with(|c| c.set(appearance));
    // SAFETY: the appearance-name statics are immutable framework constants.
    let name = unsafe {
        match appearance {
            Appearance::System => None,
            Appearance::Light => Some(NSAppearanceNameAqua),
            Appearance::Dark => Some(NSAppearanceNameDarkAqua),
        }
    };
    let named = name.and_then(NSAppearance::appearanceNamed);
    NSApplication::sharedApplication(mtm).setAppearance(named.as_deref());
}

/// The appearance last applied.
pub fn chosen() -> Appearance {
    CHOSEN.with(Cell::get)
}

/// Sets `v`'s layer background to `color`, or clears it, and keeps it in step with the app's
/// appearance.
pub fn set_background(v: &NSView, color: Option<&NSColor>) {
    v.setWantsLayer(true);
    let Some(layer) = v.layer() else { return };
    let current = NSApplication::sharedApplication(v.mtm()).effectiveAppearance();
    layer.setBackgroundColor(color.map(|c| resolve(c, &current)).as_deref());
    PAINTED.with(|p| {
        let mut p = p.borrow_mut();
        match color {
            Some(c) => p.note(v).background = Some(c.retain()),
            None => {
                if let Some(e) = p.get(v) {
                    e.background = None;
                }
            }
        }
    });
}

/// Sets `v`'s layer border to `width` of `color`, and keeps the colour in step with the app's
/// appearance.
pub fn set_border(v: &NSView, color: &NSColor, width: CGFloat) {
    v.setWantsLayer(true);
    let Some(layer) = v.layer() else { return };
    let current = NSApplication::sharedApplication(v.mtm()).effectiveAppearance();
    layer.setBorderColor(Some(&resolve(color, &current)));
    layer.setBorderWidth(width);
    PAINTED.with(|p| p.borrow_mut().note(v).border = Some(color.retain()));
}

/// `color` as `appearance` shows it.
pub fn resolve(color: &NSColor, appearance: &NSAppearance) -> Retained<CGColor> {
    let out = RefCell::new(None);
    appearance.performAsCurrentDrawingAppearance(&StackBlock::new(|| {
        *out.borrow_mut() = Some(color.CGColor());
    }));
    out.into_inner().unwrap_or_else(|| color.CGColor())
}

/// Sets every remembered layer colour again for the app's appearance now.
pub fn refresh(mtm: MainThreadMarker) {
    let current = NSApplication::sharedApplication(mtm).effectiveAppearance();
    PAINTED.with(|p| {
        let mut p = p.borrow_mut();
        p.views.retain(|_, entry| {
            let Some(view) = entry.view.load() else {
                return false;
            };
            if let Some(layer) = view.layer() {
                if let Some(c) = &entry.background {
                    layer.setBackgroundColor(Some(&resolve(c, &current)));
                }
                if let Some(c) = &entry.border {
                    layer.setBorderColor(Some(&resolve(c, &current)));
                }
            }
            true
        });
        p.limit = PRUNE_AT.max(2 * p.views.len());
    });
}

/// How many remembered views, dead ones included, may pile up before the dead are dropped;
/// the limit then becomes twice the living.
const PRUNE_AT: usize = 1024;

struct Painted {
    /// By the view's address.
    views: HashMap<usize, Entry>,
    limit: usize,
}

impl Default for Painted {
    fn default() -> Painted {
        Painted {
            views: HashMap::new(),
            limit: PRUNE_AT,
        }
    }
}

struct Entry {
    view: Weak<NSView>,
    background: Option<Retained<NSColor>>,
    border: Option<Retained<NSColor>>,
}

impl Entry {
    fn new(v: &NSView) -> Entry {
        Entry {
            view: Weak::new(v),
            background: None,
            border: None,
        }
    }
}

fn key(v: &NSView) -> usize {
    ptr::from_ref(v) as usize
}

impl Painted {
    /// `v`'s entry, if it has one.
    fn get(&mut self, v: &NSView) -> Option<&mut Entry> {
        let e = self.views.get_mut(&key(v))?;
        // A dead view's address can come back as a new view's.
        e.view.load().is_some_and(|l| ptr::eq(&*l, v)).then_some(e)
    }

    /// `v`'s entry, made if it has none.
    fn note(&mut self, v: &NSView) -> &mut Entry {
        let fresh = self.get(v).is_none();
        if fresh && self.views.len() >= self.limit {
            self.views.retain(|_, e| e.view.load().is_some());
            self.limit = PRUNE_AT.max(2 * self.views.len());
        }
        let e = self.views.entry(key(v)).or_insert_with(|| Entry::new(v));
        if fresh {
            *e = Entry::new(v);
        }
        e
    }
}

define_class!(
    /// Hears the app's effective appearance change.
    // SAFETY: NSObject has no subclassing requirements; no ivars; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Observer;

    impl Observer {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            _path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSString, AnyObject>>,
            _context: *mut c_void,
        ) {
            refresh(self.mtm());
        }
    }
);

/// Keeps the remembered layer colours in step with the app's appearance from now on; once.
pub fn observe(mtm: MainThreadMarker) {
    // SAFETY: `init` is NSObject's designated initializer.
    let observer: Retained<Observer> = unsafe { msg_send![Observer::alloc(mtm), init] };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: the observer implements `observeValueForKeyPath:…` and, leaked below, outlives
    // the observation; the context is unused.
    unsafe {
        app.addObserver_forKeyPath_options_context(
            &observer,
            ns_string!("effectiveAppearance"),
            NSKeyValueObservingOptions::empty(),
            ptr::null_mut(),
        );
    }
    std::mem::forget(observer);
}
