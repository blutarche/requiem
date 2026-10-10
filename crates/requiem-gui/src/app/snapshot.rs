//! `--snapshot`: renders a view to a PNG. The window is never shown, so this needs neither
//! screen-recording permission nor a visible display.

use std::path::Path;

use objc2_app_kit::{NSBitmapImageFileType, NSView};
use objc2_foundation::NSDictionary;

pub fn write(view: &NSView, path: &Path) -> Result<(), String> {
    view.layoutSubtreeIfNeeded();
    let bounds = view.bounds();
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or("cannot allocate a bitmap")?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // SAFETY: PNG encoding accepts an empty property dictionary.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("cannot encode the PNG")?;
    std::fs::write(path, png.to_vec()).map_err(|e| format!("{}: {e}", path.display()))
}
