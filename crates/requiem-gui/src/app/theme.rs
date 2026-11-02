//! Blutarche Studio design tokens (dark and light), embedded Inter, and Lucide icons.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSColor, NSFont,
    NSFontDescriptor, NSFontFeatureSelectorIdentifierKey, NSFontFeatureSettingsAttribute,
    NSFontFeatureTypeIdentifierKey, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSImage,
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

/// A colour's 0xRRGGBB value in the dark appearance and in the light one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tone {
    pub dark: u32,
    pub light: u32,
}

const fn tone(dark: u32, light: u32) -> Tone {
    Tone { dark, light }
}

/// An opacity in the dark appearance and in the light one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alpha {
    pub dark: CGFloat,
    pub light: CGFloat,
}

impl Alpha {
    pub const NONE: Alpha = Alpha::both(0.0);

    pub const fn both(a: CGFloat) -> Alpha {
        Alpha { dark: a, light: a }
    }

    /// The opacity of `self` laid over `under`, as one layer.
    pub fn over(self, under: Alpha) -> Alpha {
        Alpha {
            dark: 1.0 - (1.0 - self.dark) * (1.0 - under.dark),
            light: 1.0 - (1.0 - self.light) * (1.0 - under.light),
        }
    }
}

/// Whether `appearance` is a dark one, high-contrast dark included.
pub fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: the appearance-name statics are immutable framework constants.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    appearance
        .bestMatchFromAppearancesWithNames(&NSArray::from_slice(&[aqua, dark]))
        .is_some_and(|name| &*name == dark)
}

/// A colour that is `dark` in a dark appearance and `light` otherwise, resolved wherever it
/// is drawn.
fn dynamic(dark: Retained<NSColor>, light: Retained<NSColor>) -> Retained<NSColor> {
    let provider = RcBlock::new(
        move |appearance: NonNull<NSAppearance>| -> NonNull<NSColor> {
            // SAFETY: AppKit passes an appearance that is valid for the duration of the call.
            let c = if is_dark(unsafe { appearance.as_ref() }) {
                &dark
            } else {
                &light
            };
            NonNull::from(&**c)
        },
    );
    // SAFETY: the provider returns, for any appearance, a colour the block itself keeps alive.
    unsafe { NSColor::colorWithName_dynamicProvider(None, &provider) }
}

fn shade(t: Tone, a: Alpha) -> Retained<NSColor> {
    dynamic(rgba(t.dark, a.dark), rgba(t.light, a.light))
}

/// `top` at `alpha` over the opaque `base`, as one opaque colour, for surfaces that can hold
/// only one background.
pub fn mix(base: Tone, top: Tone, alpha: Alpha) -> Retained<NSColor> {
    fn one(base: u32, top: u32, alpha: CGFloat) -> Retained<NSColor> {
        let c = |hex: u32, shift: u32| CGFloat::from(((hex >> shift) & 0xff) as u8) / 255.0;
        let m = |shift| c(base, shift) * (1.0 - alpha) + c(top, shift) * alpha;
        NSColor::colorWithSRGBRed_green_blue_alpha(m(16), m(8), m(0), 1.0)
    }
    dynamic(
        one(base.dark, top.dark, alpha.dark),
        one(base.light, top.light, alpha.light),
    )
}

/// Colour functions, each returning one shared colour, so that two calls give the same
/// object and compare equal.
macro_rules! colors {
    ($($(#[$doc:meta])* $name:ident = $make:expr;)*) => {
        $(
            $(#[$doc])*
            pub fn $name() -> Retained<NSColor> {
                thread_local! {
                    static COLOR: Retained<NSColor> = $make;
                }
                COLOR.with(Retained::clone)
            }
        )*
    };
}

pub const BG: Tone = tone(0x0A0A0C, 0xFAF9F6);
pub const BG_2: Tone = tone(0x121216, 0xF1EFEA);
pub const BG_3: Tone = tone(0x1A1A20, 0xE8E6E0);
pub const LINE_2: Tone = tone(0x34343F, 0xCFCBC2);
/// A hovered table row: BG_3 30% toward LINE_2. BG_3 itself is 1.08:1 against a BG_2 stripe.
pub const ROW_HOVER: Tone = tone(0x22222A, 0xE9E7E1);
pub const ACCENT: Tone = tone(0x7C70F5, 0x5A4DDB);
const ACCENT_DEEP: Tone = tone(0x5A4FC0, 0x4136A8);
/// Alphas of `accent_soft` and `selection`. The lighter selection in the light appearance
/// keeps muted text on it at 4.56:1.
pub const SOFT: Alpha = Alpha::both(0.10);
pub const SELECTED: Alpha = Alpha {
    dark: 0.32,
    light: 0.18,
};
const WHITE: Tone = tone(0xFFFFFF, 0xFFFFFF);

colors! {
    bg = shade(BG, Alpha::both(1.0));
    bg_2 = shade(BG_2, Alpha::both(1.0));
    bg_3 = shade(BG_3, Alpha::both(1.0));
    /// The fill of a hovered row or card.
    row_hover = shade(ROW_HOVER, Alpha::both(1.0));
    bg_4 = shade(tone(0x24242C, 0xE3E0D9), Alpha::both(1.0));
    line = shade(tone(0x24242C, 0xE3E0D9), Alpha::both(1.0));
    line_2 = shade(LINE_2, Alpha::both(1.0));
    line_hi = shade(tone(0x43434F, 0xB8B4AA), Alpha::both(1.0));
    ink = shade(tone(0xF2EFEA, 0x17171C), Alpha::both(1.0));
    ink_2 = shade(tone(0xC8C5BD, 0x45454D), Alpha::both(1.0));
    muted = shade(tone(0x8C8C94, 0x606068), Alpha::both(1.0));
    muted_2 = shade(tone(0x5A5A63, 0x8E8E96), Alpha::both(1.0));
    chrome_bg = shade(tone(0x07070A, 0xF0EEE9), Alpha::both(1.0));
    chrome_bg_2 = shade(tone(0x0E0E14, 0xE8E6E0), Alpha::both(1.0));
    chrome_bg_hover = shade(tone(0x14141A, 0xE0DDD6), Alpha::both(1.0));
    chrome_border = shade(tone(0x18181E, 0xDDDAD3), Alpha::both(1.0));
    chrome_ink = shade(tone(0xE8E5DE, 0x1A1A1F), Alpha::both(1.0));
    chrome_muted = shade(tone(0x8A8A92, 0x6A6A72), Alpha::both(1.0));
    accent = shade(ACCENT, Alpha::both(1.0));
    /// The accent with more contrast: lighter in the dark appearance, darker in the light.
    accent_hi = shade(tone(0x9088F8, 0x4B3FC4), Alpha::both(1.0));
    /// A primary button's fill: the accent darkened 40% toward accent-deep, so the white label
    /// keeps 4.64:1 (7.02:1 light), where on the dark accent itself it would have 3.82:1.
    primary = mix(ACCENT, ACCENT_DEEP, Alpha::both(0.4));
    /// A primary button's hover, darker still: 6.34:1 (9.06:1 light) under the white label.
    /// Lightening it to the dark accent-hi would leave 2.97:1.
    accent_deep = shade(ACCENT_DEEP, Alpha::both(1.0));
    accent_soft = shade(ACCENT, SOFT);
    accent_ring = shade(ACCENT, Alpha::both(0.25));
    selection = shade(ACCENT, SELECTED);
    bad = shade(tone(0xE26F5C, 0xB83A27), Alpha::both(1.0));
    warn = shade(tone(0xE0B85D, 0x8A6000), Alpha::both(1.0));
    /// Palette backdrop: bg at 60%, or black at 35% in the light appearance.
    scrim = shade(tone(0x0A0A0C, 0x000000), Alpha { dark: 0.60, light: 0.35 });
    /// Text on primary buttons.
    white = shade(WHITE, Alpha::both(1.0));
    /// The icon on the accent play button.
    on_accent = shade(tone(0x0A0A0C, 0xFFFFFF), Alpha::both(1.0));
    /// The service badges' fills, the services' own colours.
    spotify_green = rgb(0x1ED760);
    youtube_red = rgb(0xFF0033);
}

/// The volume bar's colour at `t` along its track, 0 to 1: accent, then pink, then orange;
/// in the light appearance each dark enough for 3:1 against the track.
pub fn heat(t: CGFloat) -> Retained<NSColor> {
    const STOPS: [Tone; 3] = [ACCENT, tone(0xE86FD8, 0xB8358F), tone(0xFF8A5B, 0xB84E1A)];
    let t = t.clamp(0.0, 1.0) * 2.0;
    let i = (t as usize).min(1);
    mix(STOPS[i], STOPS[i + 1], Alpha::both(t - i as CGFloat))
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
    use objc2_app_kit::NSAppearanceName;
    use objc2_core_graphics::CGColor;
    use objc2_foundation::NSObjectProtocol;

    /// `color` as the appearance `name` shows it, as 0xRRGGBB and alpha.
    fn shown(color: &NSColor, name: &NSAppearanceName) -> (u32, CGFloat) {
        let appearance = NSAppearance::appearanceNamed(name).unwrap();
        let out = RefCell::new(Vec::new());
        appearance.performAsCurrentDrawingAppearance(&block2::StackBlock::new(|| {
            let cg = color.CGColor();
            let n = CGColor::number_of_components(Some(&cg));
            let c = CGColor::components(Some(&cg));
            // SAFETY: a CGColor has as many components as it reports.
            *out.borrow_mut() = unsafe { std::slice::from_raw_parts(c, n) }.to_vec();
        }));
        let c = out.into_inner();
        let byte = |v: CGFloat| (v * 255.0).round() as u32;
        ((byte(c[0]) << 16) | (byte(c[1]) << 8) | byte(c[2]), c[3])
    }

    #[test]
    fn colours_resolve_for_the_appearance_they_are_shown_in() {
        // SAFETY: the appearance-name statics are immutable framework constants.
        let (dark, light) = unsafe { (NSAppearanceNameDarkAqua, NSAppearanceNameAqua) };
        assert_eq!(shown(&bg(), dark), (0x0A0A0C, 1.0));
        assert_eq!(shown(&bg(), light), (0xFAF9F6, 1.0));
        assert_eq!(shown(&ink(), light), (0x17171C, 1.0));
        assert_eq!(shown(&selection(), dark).1, 0.32);
        assert_eq!(shown(&selection(), light).1, 0.18);
        let selected = mix(BG, ACCENT, SELECTED);
        assert_eq!(shown(&selected, dark), (0x2E2B57, 1.0));
        assert_eq!(shown(&selected, light), (0xDDDAF1, 1.0));
        for fixed in [spotify_green(), youtube_red(), white()] {
            assert_eq!(shown(&fixed, dark), shown(&fixed, light));
        }
    }

    #[test]
    fn a_colour_is_one_shared_object() {
        assert!(ptr::eq(&*accent(), &*accent()));
        assert!(accent().isEqual(Some(&accent())));
        assert!(!accent().isEqual(Some(&accent_hi())));
    }

    #[test]
    fn layered_alphas_combine() {
        let both = SELECTED.over(SOFT);
        assert_eq!(both.dark, 1.0 - (1.0 - 0.32) * (1.0 - 0.10));
        assert_eq!(both.light, 1.0 - (1.0 - 0.18) * (1.0 - 0.10));
        let soft = Alpha::NONE.over(SOFT);
        assert!((soft.dark - SOFT.dark).abs() < 1e-12 && (soft.light - SOFT.light).abs() < 1e-12);
    }

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
