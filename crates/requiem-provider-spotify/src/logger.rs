//! The helper's `log` sink. Fixed at `warn` and blind to `RUST_LOG`: librespot logs URIs and
//! account details at `info` and below.

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Warn;

impl Log for Warn {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        m.level() <= Level::Warn
    }

    fn log(&self, r: &Record<'_>) {
        if self.enabled(r.metadata()) {
            let msg = redact(&r.args().to_string());
            eprintln!("[spotify] {} {}: {msg}", r.level(), r.target());
        }
    }

    fn flush(&self) {}
}

pub(crate) fn init() {
    if log::set_logger(&Warn).is_ok() {
        log::set_max_level(LevelFilter::Warn);
    }
}

/// Drops URL query strings: librespot warns with CDN URLs, whose queries carry signed tokens.
fn redact(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(i) = rest.find("://") {
        let end = rest[i..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
            .map_or(rest.len(), |n| i + n);
        let url = &rest[..end];
        match url[i..].find('?') {
            Some(q) => {
                out.push_str(&url[..i + q]);
                out.push_str("?<redacted>");
            }
            None => out.push_str(url),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn url_queries_are_redacted() {
        assert_eq!(
            redact("Fetching https://a.scdn.co/audio/ab?__token__=exp=1~hmac=2 failed"),
            "Fetching https://a.scdn.co/audio/ab?<redacted> failed"
        );
        assert_eq!(
            redact("x \"http://h/p?a=1\" and (https://h/q?b) end"),
            "x \"http://h/p?<redacted>\" and (https://h/q?<redacted>) end"
        );
        assert_eq!(redact("https://h/p plain"), "https://h/p plain");
        assert_eq!(redact("no urls? none"), "no urls? none");
        assert_eq!(redact("tail https://h/?t"), "tail https://h/?<redacted>");
    }
}
