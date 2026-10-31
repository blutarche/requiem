//! The region the system is set to, as an ISO 3166 alpha-2 code.

/// The country of the system's locale: macOS's current locale, elsewhere `LC_ALL`,
/// `LC_MESSAGES` or `LANG`, the first of them that is set and not empty.
pub fn region() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::NSLocale;
        #[allow(deprecated)] // `regionCode` needs macOS 13
        let code = NSLocale::currentLocale().countryCode()?;
        valid(&code.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|v| !v.is_empty())
            .and_then(|v| region_of_locale(&v))
    }
}

/// `TH` from a POSIX locale name such as `th_TH.UTF-8` or `en_US@euro`; `None` for `C`,
/// `POSIX` and names without a country.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn region_of_locale(locale: &str) -> Option<String> {
    let name = locale.split(['.', '@']).next()?;
    let (_, country) = name.split_once('_')?;
    valid(country)
}

/// `code` in uppercase when it is exactly two ASCII letters.
fn valid(code: &str) -> Option<String> {
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| code.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locale_name_gives_its_country() {
        for (locale, region) in [
            ("th_TH.UTF-8", Some("TH")),
            ("en_US", Some("US")),
            ("de_AT@euro", Some("AT")),
            ("pt_br.utf8", Some("BR")),
            ("C", None),
            ("POSIX", None),
            ("C.UTF-8", None),
            ("en", None),
            ("en_USA", None),
            ("en_1A", None),
            ("", None),
        ] {
            assert_eq!(region_of_locale(locale).as_deref(), region, "{locale}");
        }
    }
}
