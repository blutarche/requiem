use semver::Version;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    UpToDate,
    Update,
}

/// The version table of section 3.1: the first matching row decides. `highest_seen` is the
/// highest version of any valid manifest before this one.
pub fn decide(
    offered: &Version,
    running: &Version,
    highest_seen: Option<&Version>,
) -> Result<Decision, String> {
    if let Some(highest) = highest_seen
        && offered < highest
    {
        return Err(format!(
            "the release page went backwards: it offers {offered} after {highest}"
        ));
    }
    if offered <= running {
        Ok(Decision::UpToDate)
    } else {
        Ok(Decision::Update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn lower_than_the_highest_seen_is_a_failed_check() {
        let err = decide(&v("0.2.0"), &v("0.1.0"), Some(&v("0.3.0"))).unwrap_err();
        assert!(err.contains("went backwards"), "{err}");
        // The first row wins even when the second would also match.
        assert!(decide(&v("0.1.0"), &v("0.2.0"), Some(&v("0.2.0"))).is_err());
    }

    #[test]
    fn at_most_the_running_version_is_up_to_date() {
        assert_eq!(
            decide(&v("0.2.0"), &v("0.2.0"), None),
            Ok(Decision::UpToDate)
        );
        assert_eq!(
            decide(&v("0.1.9"), &v("0.2.0"), None),
            Ok(Decision::UpToDate)
        );
        assert_eq!(
            decide(&v("0.2.0"), &v("0.2.0"), Some(&v("0.2.0"))),
            Ok(Decision::UpToDate)
        );
    }

    #[test]
    fn greater_than_the_running_version_is_an_update() {
        assert_eq!(decide(&v("0.2.1"), &v("0.2.0"), None), Ok(Decision::Update));
        assert_eq!(
            decide(&v("0.3.0"), &v("0.2.0"), Some(&v("0.3.0"))),
            Ok(Decision::Update)
        );
        assert_eq!(
            decide(&v("1.0.0"), &v("0.9.9"), Some(&v("0.9.9"))),
            Ok(Decision::Update)
        );
    }

    #[test]
    fn prereleases_order_below_their_release() {
        assert_eq!(
            decide(&v("0.2.0-rc.1"), &v("0.2.0"), None),
            Ok(Decision::UpToDate)
        );
    }
}
