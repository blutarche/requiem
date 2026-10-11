use std::path::{Path, PathBuf};

use requiem_cli::remote::daemon_exe;

fn only(existing: &'static [&'static str]) -> impl Fn(&Path) -> bool {
    move |p| existing.iter().any(|e| Path::new(e) == p)
}

#[test]
fn prefers_the_daemon_next_to_the_cli() {
    let got = daemon_exe(
        Path::new("/opt/bin/requiem"),
        only(&[
            "/opt/bin/requiemd",
            "/opt/bin/../Helpers/requiemd.app/Contents/MacOS/requiemd",
        ]),
    );
    assert_eq!(got, PathBuf::from("/opt/bin/requiemd"));
}

#[test]
fn falls_back_to_the_bundle_helper() {
    let got = daemon_exe(
        Path::new("/Applications/requiem.app/Contents/MacOS/requiem"),
        only(&[
            "/Applications/requiem.app/Contents/MacOS/../Helpers/requiemd.app/Contents/MacOS/requiemd",
        ]),
    );
    assert_eq!(
        got,
        PathBuf::from(
            "/Applications/requiem.app/Contents/MacOS/../Helpers/requiemd.app/Contents/MacOS/requiemd"
        )
    );
}

#[test]
fn names_the_sibling_when_neither_exists() {
    let got = daemon_exe(Path::new("/opt/bin/requiem"), only(&[]));
    assert_eq!(got, PathBuf::from("/opt/bin/requiemd"));
}
