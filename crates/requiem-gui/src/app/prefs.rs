//! What the window keeps between launches, in the app's user defaults.

use objc2_foundation::{NSString, NSUserDefaults};

use crate::vm::ServiceFilter;

const SERVICE_FILTER: &str = "serviceFilter";

/// The saved service filter; All when none is saved or the value is unknown.
pub fn service_filter() -> ServiceFilter {
    NSUserDefaults::standardUserDefaults()
        .stringForKey(&NSString::from_str(SERVICE_FILTER))
        .map_or(ServiceFilter::All, |v| {
            ServiceFilter::from_key(&v.to_string())
        })
}

pub fn save_service_filter(filter: ServiceFilter) {
    let value = NSString::from_str(filter.key());
    // SAFETY: the value is an NSString, which user defaults can hold.
    unsafe {
        NSUserDefaults::standardUserDefaults()
            .setObject_forKey(Some(&value), &NSString::from_str(SERVICE_FILTER));
    }
}
