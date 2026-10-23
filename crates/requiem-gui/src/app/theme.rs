//! Blutarche Studio design tokens (dark), embedded Inter, and Lucide icons.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSColor, NSFont, NSFontDescriptor, NSFontFeatureSelectorIdentifierKey,
    NSFontFeatureSettingsAttribute, NSFontFeatureTypeIdentifierKey, NSFontWeightMedium,
    NSFontWeightRegular, NSFontWeightSemibold, NSImage,
};
use objc2_core_foundation::{CFData, CGFloat};
use objc2_core_graphics::{CGDataProvider, CGFont};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSSize, NSString};

fn rgb(hex: u32) -> Retained<NSColor> {
    rgba(hex, 1.0)
}

fn rgba(hex: u32, alpha: CGFloat) -> Retained<NSColor> {
    let c = |shift: u32| CGFloat::from(((hex >> shift) & 0xff) as u8) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(c(16), c(8), c(0), alpha)
}

/// `top` at `alpha` over the opaque `base`, as one opaque colour, for surfaces that can hold
/// only one background.
pub fn mix(base: u32, top: u32, alpha: CGFloat) -> Retained<NSColor> {
    let c = |hex: u32, shift: u32| CGFloat::from(((hex >> shift) & 0xff) as u8) / 255.0;
    let m = |shift| c(base, shift) * (1.0 - alpha) + c(top, shift) * alpha;
    NSColor::colorWithSRGBRed_green_blue_alpha(m(16), m(8), m(0), 1.0)
}

pub const BG: u32 = 0x0A0A0C;
pub const BG_2: u32 = 0x121216;
pub const BG_3: u32 = 0x1A1A20;
pub const LINE_2: u32 = 0x34343F;
/// A hovered table row: BG_3 30% toward LINE_2. BG_3 itself is 1.08:1 against a BG_2 stripe.
pub const ROW_HOVER: u32 = 0x22222A;
pub const ACCENT: u32 = 0x7C70F5;
const ACCENT_DEEP: u32 = 0x5A4FC0;
/// Alphas of `accent_soft` and `selection`.
pub const SOFT: CGFloat = 0.10;
pub const SELECTED: CGFloat = 0.32;

pub fn bg() -> Retained<NSColor> {
    rgb(BG)
}
pub fn bg_2() -> Retained<NSColor> {
    rgb(BG_2)
}
pub fn bg_3() -> Retained<NSColor> {
    rgb(BG_3)
}
/// The fill of a hovered row or card.
pub fn row_hover() -> Retained<NSColor> {
    rgb(ROW_HOVER)
}
pub fn bg_4() -> Retained<NSColor> {
    rgb(0x24242C)
}
pub fn line() -> Retained<NSColor> {
    rgb(0x24242C)
}
pub fn line_2() -> Retained<NSColor> {
    rgb(LINE_2)
}
pub fn line_hi() -> Retained<NSColor> {
    rgb(0x43434F)
}
pub fn ink() -> Retained<NSColor> {
    rgb(0xF2EFEA)
}
pub fn ink_2() -> Retained<NSColor> {
    rgb(0xC8C5BD)
}
pub fn muted() -> Retained<NSColor> {
    rgb(0x8C8C94)
}
pub fn muted_2() -> Retained<NSColor> {
    rgb(0x5A5A63)
}
pub fn chrome_bg() -> Retained<NSColor> {
    rgb(0x07070A)
}
pub fn chrome_bg_2() -> Retained<NSColor> {
    rgb(0x0E0E14)
}
pub fn chrome_bg_hover() -> Retained<NSColor> {
    rgb(0x14141A)
}
pub fn chrome_border() -> Retained<NSColor> {
    rgb(0x18181E)
}
pub fn chrome_ink() -> Retained<NSColor> {
    rgb(0xE8E5DE)
}
pub fn chrome_muted() -> Retained<NSColor> {
    rgb(0x8A8A92)
}
pub fn accent() -> Retained<NSColor> {
    rgb(ACCENT)
}
pub fn accent_hi() -> Retained<NSColor> {
    rgb(0x9088F8)
}
/// A primary button's fill: the accent darkened 40% toward accent-deep, so the white label
/// keeps 4.64:1, where on the accent itself it would have 3.82:1.
pub fn primary() -> Retained<NSColor> {
    mix(ACCENT, ACCENT_DEEP, 0.4)
}
/// A primary button's hover, darker still: 6.34:1 under the white label. Lightening it to
/// accent-hi would leave 2.97:1.
pub fn accent_deep() -> Retained<NSColor> {
    rgb(ACCENT_DEEP)
}
pub fn accent_soft() -> Retained<NSColor> {
    rgba(ACCENT, SOFT)
}
pub fn accent_ring() -> Retained<NSColor> {
    rgba(0x7C70F5, 0.25)
}
pub fn selection() -> Retained<NSColor> {
    rgba(ACCENT, SELECTED)
}
pub fn bad() -> Retained<NSColor> {
    rgb(0xE26F5C)
}
pub fn warn() -> Retained<NSColor> {
    rgb(0xE0B85D)
}
/// Palette backdrop: bg at 60%.
pub fn scrim() -> Retained<NSColor> {
    rgba(0x0A0A0C, 0.60)
}
/// Text on primary buttons.
pub fn white() -> Retained<NSColor> {
    rgb(0xFFFFFF)
}
/// The icon on the accent play button.
pub fn on_accent() -> Retained<NSColor> {
    rgb(0x0A0A0C)
}
/// The service badges' fills, the services' own colours.
pub fn spotify_green() -> Retained<NSColor> {
    rgb(0x1ED760)
}
pub fn youtube_red() -> Retained<NSColor> {
    rgb(0xFF0033)
}

pub const RADIUS: CGFloat = 6.0;
pub const RADIUS_MODAL: CGFloat = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    SemiBold,
}

const FONTS: [(&str, &[u8]); 3] = [
    (
        "Inter-Regular",
        include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
    ),
    (
        "Inter-Medium",
        include_bytes!("../../assets/fonts/Inter-Medium.ttf"),
    ),
    (
        "Inter-SemiBold",
        include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
    ),
];

static INTER: AtomicBool = AtomicBool::new(false);

/// Registers the embedded Inter faces for this process. On failure the UI falls back to the
/// system font.
pub fn register_fonts() {
    let mut ok = true;
    for (name, bytes) in FONTS {
        let data = CFData::from_static_bytes(bytes);
        let font =
            CGDataProvider::with_cf_data(Some(&data)).and_then(|p| CGFont::with_data_provider(&p));
        let Some(font) = font else {
            eprintln!("requiem-gui: cannot load font {name}; using the system font");
            ok = false;
            continue;
        };
        // Deprecated in favour of descriptor registration, which needs a URL or an async
        // handler; this is the synchronous in-memory path and still works.
        // SAFETY: a null error pointer is allowed.
        #[allow(deprecated)]
        let registered =
            unsafe { objc2_core_text::CTFontManagerRegisterGraphicsFont(&font, ptr::null_mut()) };
        if !registered {
            eprintln!("requiem-gui: cannot register font {name}; using the system font");
            ok = false;
        }
    }
    INTER.store(ok, Ordering::Relaxed);
}

pub fn font(size: CGFloat, weight: Weight) -> Retained<NSFont> {
    let name = match weight {
        Weight::Regular => "Inter-Regular",
        Weight::Medium => "Inter-Medium",
        Weight::SemiBold => "Inter-SemiBold",
    };
    if INTER.load(Ordering::Relaxed)
        && let Some(f) = NSFont::fontWithName_size(&NSString::from_str(name), size)
    {
        return f;
    }
    // SAFETY: the weight statics are immutable framework constants.
    let w = unsafe {
        match weight {
            Weight::Regular => NSFontWeightRegular,
            Weight::Medium => NSFontWeightMedium,
            Weight::SemiBold => NSFontWeightSemibold,
        }
    };
    NSFont::systemFontOfSize_weight(size, w)
}

/// `font` with tabular (fixed-width) figures, for times and durations.
pub fn tabular(size: CGFloat, weight: Weight) -> Retained<NSFont> {
    let base = font(size, weight);
    // kNumberSpacingType / kMonospacedNumbersSelector from SFNTLayoutTypes.h.
    // SAFETY: the key statics are immutable framework constants.
    let (type_key, selector_key, settings_key) = unsafe {
        (
            NSFontFeatureTypeIdentifierKey,
            NSFontFeatureSelectorIdentifierKey,
            NSFontFeatureSettingsAttribute,
        )
    };
    let setting: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_retained_objects(
        &[type_key, selector_key],
        &[
            Retained::into_super(Retained::into_super(NSNumber::new_i32(6))).into(),
            Retained::into_super(Retained::into_super(NSNumber::new_i32(0))).into(),
        ],
    );
    let settings = NSArray::from_retained_slice(&[Retained::into_super(setting)]);
    let attrs: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_retained_objects(
        &[settings_key],
        &[Retained::into_super(Retained::into_super(settings))],
    );
    // SAFETY: the dictionary maps the feature-settings key to an array of feature
    // dictionaries holding NSNumbers, as the attribute requires.
    let desc: Retained<NSFontDescriptor> = unsafe {
        base.fontDescriptor()
            .fontDescriptorByAddingAttributes(&attrs)
    };
    NSFont::fontWithDescriptor_size(&desc, size).unwrap_or(base)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    SkipBack,
    SkipForward,
    Search,
    ListMusic,
    Volume,
    Plus,
    X,
    Music,
    Check,
    Home,
    Library,
    ChevronLeft,
    ChevronRight,
    Settings,
    LogIn,
    LogOut,
    Copy,
    ExternalLink,
    BookOpen,
    ClipboardPaste,
}

impl Icon {
    fn svg(self) -> &'static [u8] {
        match self {
            Icon::Play => include_bytes!("../../assets/icons/play.svg"),
            Icon::Pause => include_bytes!("../../assets/icons/pause.svg"),
            Icon::SkipBack => include_bytes!("../../assets/icons/skip-back.svg"),
            Icon::SkipForward => include_bytes!("../../assets/icons/skip-forward.svg"),
            Icon::Search => include_bytes!("../../assets/icons/search.svg"),
            Icon::ListMusic => include_bytes!("../../assets/icons/list-music.svg"),
            Icon::Volume => include_bytes!("../../assets/icons/volume-2.svg"),
            Icon::Plus => include_bytes!("../../assets/icons/plus.svg"),
            Icon::X => include_bytes!("../../assets/icons/x.svg"),
            Icon::Music => include_bytes!("../../assets/icons/music.svg"),
            Icon::Check => include_bytes!("../../assets/icons/check.svg"),
            Icon::Home => include_bytes!("../../assets/icons/house.svg"),
            Icon::Library => include_bytes!("../../assets/icons/library.svg"),
            Icon::ChevronLeft => include_bytes!("../../assets/icons/chevron-left.svg"),
            Icon::ChevronRight => include_bytes!("../../assets/icons/chevron-right.svg"),
            Icon::Settings => include_bytes!("../../assets/icons/settings.svg"),
            Icon::LogIn => include_bytes!("../../assets/icons/log-in.svg"),
            Icon::LogOut => include_bytes!("../../assets/icons/log-out.svg"),
            Icon::Copy => include_bytes!("../../assets/icons/copy.svg"),
            Icon::ExternalLink => include_bytes!("../../assets/icons/external-link.svg"),
            Icon::BookOpen => include_bytes!("../../assets/icons/book-open.svg"),
            Icon::ClipboardPaste => include_bytes!("../../assets/icons/clipboard-paste.svg"),
        }
    }

    /// A template image at `size` points; tint it with `contentTintColor`. `None`, logged,
    /// if the SVG does not decode: callers run inside AppKit callbacks, where a panic would
    /// abort, and a missing icon is the lesser failure. Shared per icon and size, so callers
    /// must not change it.
    pub fn image(self, size: CGFloat) -> Option<Retained<NSImage>> {
        /// Keyed by icon and the size's bits.
        type Images = HashMap<(Icon, u64), Option<Retained<NSImage>>>;
        thread_local! {
            static IMAGES: RefCell<Images> = RefCell::new(HashMap::new());
        }
        IMAGES.with(|images| {
            images
                .borrow_mut()
                .entry((self, size.to_bits()))
                .or_insert_with(|| self.decode(size))
                .clone()
        })
    }

    fn decode(self, size: CGFloat) -> Option<Retained<NSImage>> {
        let data = NSData::with_bytes(self.svg());
        let Some(img) = NSImage::initWithData(NSImage::alloc(), &data) else {
            eprintln!("requiem-gui: embedded icon {self:?} does not decode");
            return None;
        };
        img.setTemplate(true);
        img.setSize(NSSize::new(size, size));
        Some(img)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_decodes() {
        for icon in [
            Icon::Play,
            Icon::Pause,
            Icon::SkipBack,
            Icon::SkipForward,
            Icon::Search,
            Icon::ListMusic,
            Icon::Volume,
            Icon::Plus,
            Icon::X,
            Icon::Music,
            Icon::Check,
            Icon::Home,
            Icon::Library,
            Icon::ChevronLeft,
            Icon::ChevronRight,
            Icon::Settings,
            Icon::LogIn,
            Icon::LogOut,
            Icon::Copy,
            Icon::ExternalLink,
            Icon::BookOpen,
            Icon::ClipboardPaste,
        ] {
            assert!(icon.image(16.0).is_some(), "{icon:?}");
        }
    }

    #[test]
    fn icons_are_decoded_once_per_size() {
        let a = Icon::Plus.image(16.0).unwrap();
        let b = Icon::Plus.image(16.0).unwrap();
        let big = Icon::Plus.image(20.0).unwrap();
        assert!(ptr::eq(&*a, &*b));
        assert!(!ptr::eq(&*a, &*big));
        assert_eq!(big.size(), NSSize::new(20.0, 20.0));
    }
}
