use std::sync::OnceLock;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("REQUIEM_LOG").is_ok_and(|v| v == "debug"))
}

/// Debug line on stderr. Never pass full stream URLs: they embed the client IP.
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            eprintln!("[ytmusic] {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use debug;

/// Host only, for logging stream URLs.
pub(crate) fn host_of(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or("?")
}
