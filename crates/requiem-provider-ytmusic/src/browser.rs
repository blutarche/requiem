//! A YouTube Music sign-in copied from a signed-in music.youtube.com tab, as ytmusicapi's
//! browser auth takes it: the request headers the browser's developer tools copy, and the
//! SAPISIDHASH each signed-in request carries.
//!
//! Errors never quote the paste: it holds the user's Google sign-in.

use std::fmt::{self, Write};

use crate::clients::MUSIC_ORIGIN;
use crate::google::Secret;

/// The most text a paste may hold: a request's headers are a few KiB.
pub(crate) const MAX_PASTE: usize = 64 * 1024;
const MAX_COOKIE: usize = 16 * 1024;
/// The cookie whose value the SAPISIDHASH hashes.
const SAPISID: &str = "__Secure-3PAPISID";

const TOO_BIG: &str = "That's over 64 KiB, more than a request's headers. Copy the headers of a music.youtube.com request named browse.";
const NO_COOKIE: &str =
    "That has no Cookie header. Copy the headers of a music.youtube.com request named browse.";
const COOKIE_TOO_BIG: &str = "That cookie is over 16 KiB, more than music.youtube.com sends.";
const NO_SAPISID: &str = "That cookie has no __Secure-3PAPISID, so it isn't from a signed-in tab.";

/// What a signed-in request sends from the paste. Every other header is dropped, a pasted
/// `authorization` too: it is recomputed for each request.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BrowserHeaders {
    pub cookie: Secret,
    pub authuser: String,
    /// A brand account's id, sent back as it came.
    pub page_id: Option<String>,
    pub visitor_id: Option<String>,
}

impl fmt::Debug for BrowserHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserHeaders")
            .field("cookie", &self.cookie)
            .field("authuser", &self.authuser)
            .field("page_id", &self.page_id)
            .field("visitor_id", &self.visitor_id)
            .finish()
    }
}

/// Reads the headers of a signed-in request from a raw header block (Firefox, Safari, Chrome's
/// "Copy request headers"), Chrome's name-then-value lines, a "Copy as cURL (bash)" command,
/// or a bare cookie string.
pub(crate) fn parse_paste(text: &str) -> Result<BrowserHeaders, String> {
    if text.len() > MAX_PASTE {
        return Err(TOO_BIG.to_owned());
    }
    let text = text.trim_start_matches('\u{feff}');
    let fields = if is_curl(text) {
        curl_fields(text)
    } else {
        [header_lines, separate_lines, bare_cookie]
            .into_iter()
            .map(|read| read(text))
            .find(has_cookie)
            .unwrap_or_default()
    };
    keep(&fields)
}

/// The value of the cookie the SAPISIDHASH hashes.
pub(crate) fn sapisid(cookie: &str) -> Option<&str> {
    cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SAPISID)
        .map(|(_, value)| value.trim())
        .filter(|v| !v.is_empty())
}

/// Checks headers read back from a saved session as a paste's are checked.
pub(crate) fn from_parts(
    cookie: &str,
    authuser: &str,
    page_id: Option<&str>,
    visitor_id: Option<&str>,
) -> Result<BrowserHeaders, String> {
    let fields = [
        ("cookie", Some(cookie)),
        ("x-goog-authuser", Some(authuser)),
        ("x-goog-pageid", page_id),
        ("x-goog-visitor-id", visitor_id),
    ]
    .into_iter()
    .filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned())))
    .collect();
    keep(&fields)
}

/// `SAPISIDHASH <unix>_<sha1 of "<unix> <sapisid> <origin>">`, as music.youtube.com computes it.
pub(crate) fn authorization(unix: u64, sapisid: &str) -> String {
    let input = format!("{unix} {sapisid} {MUSIC_ORIGIN}");
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, input.as_bytes());
    let mut out = format!("SAPISIDHASH {unix}_");
    for b in digest.as_ref() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

type Fields = Vec<(String, String)>;

/// The headers [`keep`] reads.
const KEPT: [&str; 4] = [
    "cookie",
    "x-goog-authuser",
    "x-goog-pageid",
    "x-goog-visitor-id",
];

fn has_cookie(fields: &Fields) -> bool {
    fields.iter().any(|(k, _)| k == "cookie")
}

/// RFC 9110's token characters, which a header name is made of.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `Name: value` lines; a request line or a pseudo-header (`:authority`) is skipped.
fn header_lines(text: &str) -> Fields {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.trim().split_once(':')?;
            let value = value.trim();
            (is_name(name) && !value.is_empty()).then(|| (name.to_ascii_lowercase(), value.into()))
        })
        .collect()
}

/// Chrome's copy of the headers pane: each name on its own line, with or without a colon, and
/// its value on the next. Only a header that is kept starts a pair: headings such as
/// "General", the other headers and their values are each skipped alone, so a stray line
/// can't shift the pairing. A kept name followed by another has no value.
fn separate_lines(text: &str) -> Fields {
    let kept = |line: &str| {
        let n = line.strip_suffix(':').unwrap_or(line);
        KEPT.into_iter().find(|k| k.eq_ignore_ascii_case(n))
    };
    let mut fields = Vec::new();
    let mut name: Option<&str> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match (name.take(), kept(line)) {
            (_, Some(k)) => name = Some(k),
            (Some(n), None) => fields.push((n.to_owned(), line.to_owned())),
            (None, None) => {}
        }
    }
    fields
}

/// `a=b; c=d` on one line, with no header around it.
fn bare_cookie(text: &str) -> Fields {
    let text = text.trim();
    if text.contains('\n') || !text.contains('=') {
        return Vec::new();
    }
    vec![("cookie".to_owned(), text.to_owned())]
}

fn is_curl(text: &str) -> bool {
    text.trim_start()
        .strip_prefix("curl")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '\\'))
}

/// The `-H` headers and the `-b` cookie of a bash cURL command.
fn curl_fields(text: &str) -> Fields {
    let words = shell_words(text);
    let mut fields = Vec::new();
    let mut words = words.iter();
    while let Some(w) = words.next() {
        let header = match w.as_str() {
            "-H" | "--header" => words.next().map(String::as_str),
            "-b" | "--cookie" => {
                // `-b` also takes a cookie file's name, which has no `=`.
                if let Some(c) = words.next().filter(|c| c.contains('=')) {
                    fields.push(("cookie".to_owned(), c.trim().to_owned()));
                }
                continue;
            }
            w => w
                .strip_prefix("--header=")
                .or_else(|| w.strip_prefix("-H").filter(|h| !h.is_empty())),
        };
        if let Some((name, value)) = header.and_then(|h| h.split_once(':')) {
            let name = name.trim();
            if is_name(name) {
                fields.push((name.to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
    }
    fields
}

/// Splits a bash command line into words: single, double and `$'…'` quotes, backslash
/// escapes, and backslash-newline continuations.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if let Some(w) = word.take() {
                    words.push(w);
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some('\r') if chars.peek() == Some(&'\n') => {
                    chars.next();
                }
                Some(e) => word.get_or_insert_default().push(e),
                None => {}
            },
            '\'' => {
                let w = word.get_or_insert_default();
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    w.push(q);
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('\n') => {}
                            Some(e @ ('$' | '`' | '"' | '\\')) => w.push(e),
                            Some(e) => {
                                w.push('\\');
                                w.push(e);
                            }
                            None => {}
                        },
                        q => w.push(q),
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                let w = word.get_or_insert_default();
                ansi_c(&mut chars, w);
            }
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// The rest of a `$'…'` string, up to its closing quote, with its escapes undone.
fn ansi_c(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, w: &mut String) {
    while let Some(q) = chars.next() {
        match q {
            '\'' => return,
            '\\' => {
                let Some(e) = chars.next() else { return };
                let hex = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>, max: usize| {
                    let mut n = 0u32;
                    for _ in 0..max {
                        match chars.peek().and_then(|c| c.to_digit(16)) {
                            Some(d) => {
                                n = n * 16 + d;
                                chars.next();
                            }
                            None => break,
                        }
                    }
                    char::from_u32(n)
                };
                match e {
                    'n' => w.push('\n'),
                    'r' => w.push('\r'),
                    't' => w.push('\t'),
                    'x' => w.extend(hex(chars, 2)),
                    'u' => w.extend(hex(chars, 4)),
                    'U' => w.extend(hex(chars, 8)),
                    e => w.push(e),
                }
            }
            q => w.push(q),
        }
    }
}

/// A header's name as an error names it.
fn shown(name: &str) -> &str {
    match name {
        "cookie" => "Cookie",
        "x-goog-authuser" => "X-Goog-AuthUser",
        "x-goog-pageid" => "X-Goog-PageId",
        "x-goog-visitor-id" => "X-Goog-Visitor-Id",
        other => other,
    }
}

/// Printable ASCII only: anything else is no header value a browser sent.
fn checked(name: &str, value: &str) -> Result<String, String> {
    if value.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "That {} header has characters no header can hold. Copy the headers again.",
            shown(name)
        ))
    }
}

fn keep(fields: &Fields) -> Result<BrowserHeaders, String> {
    let cookies: Vec<&str> = fields
        .iter()
        .filter(|(k, _)| k == "cookie")
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .collect();
    if cookies.is_empty() {
        return Err(NO_COOKIE.to_owned());
    }
    // HTTP/2 may send the cookie in pieces, which copies can keep apart.
    let cookie = cookies.join("; ");
    if cookie.len() > MAX_COOKIE {
        return Err(COOKIE_TOO_BIG.to_owned());
    }
    let cookie = checked("cookie", &cookie)?;
    if sapisid(&cookie).is_none() {
        return Err(NO_SAPISID.to_owned());
    }
    let first = |name: &str| -> Result<Option<String>, String> {
        fields
            .iter()
            .find(|(k, v)| k == name && !v.trim().is_empty())
            .map(|(_, v)| checked(name, v.trim()))
            .transpose()
    };
    Ok(BrowserHeaders {
        cookie: Secret::new(cookie),
        authuser: first("x-goog-authuser")?.unwrap_or_else(|| "0".to_owned()),
        page_id: first("x-goog-pageid")?,
        visitor_id: first("x-goog-visitor-id")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake cookie: no value in it was ever issued by Google.
    const COOKIE: &str = "VISITOR_INFO1_LIVE=fakeVisitor; SID=fakeSID-MARKER; __Secure-3PAPISID=FAKEsapisid-0123/AbCdEfGh; SAPISID=FAKEsapisid-0123/AbCdEfGh";
    const MARKER: &str = "fakeSID-MARKER";

    fn ok(text: &str) -> BrowserHeaders {
        parse_paste(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn sapisidhash_known_answer() {
        // printf '%s' "1800000000 FAKEsapisid-0123/AbCdEfGh https://music.youtube.com" | shasum
        assert_eq!(
            authorization(1_800_000_000, "FAKEsapisid-0123/AbCdEfGh"),
            "SAPISIDHASH 1800000000_6fb024d283b93f06ca3887a42fdcd15ad6bedc60"
        );
        assert_eq!(sapisid(COOKIE), Some("FAKEsapisid-0123/AbCdEfGh"));
        assert_eq!(sapisid("SAPISID=x; a=b"), None);
        assert_eq!(sapisid("__Secure-3PAPISID=; a=b"), None);
    }

    #[test]
    fn firefox_and_safari_header_blocks() {
        let firefox = format!(
            "POST /youtubei/v1/browse?prettyPrint=false HTTP/2\n\
             Host: music.youtube.com\n\
             User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:144.0) Gecko/20100101 Firefox/144.0\n\
             Accept: */*\n\
             Content-Type: application/json\n\
             X-Goog-Visitor-Id: CgtmYWtlVmlzaXRvcg%3D%3D\n\
             X-Goog-AuthUser: 1\n\
             X-Goog-PageId: 100000000000000000001\n\
             Authorization: SAPISIDHASH 1_fake\n\
             Origin: https://music.youtube.com\n\
             Sec-Fetch-Site: same-origin\n\
             Cookie: {COOKIE}\n"
        );
        let h = ok(&firefox);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.authuser, "1");
        assert_eq!(h.page_id.as_deref(), Some("100000000000000000001"));
        assert_eq!(h.visitor_id.as_deref(), Some("CgtmYWtlVmlzaXRvcg%3D%3D"));
        // Safari's lines end in CRLF and carry no page id.
        let safari = format!("Accept: */*\r\nCookie: {COOKIE}\r\nX-Goog-AuthUser: 0\r\n");
        let h = ok(&safari);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!((h.authuser.as_str(), h.page_id), ("0", None));
    }

    #[test]
    fn chrome_copy_request_headers_skips_pseudo_headers() {
        let chrome = format!(
            ":authority: music.youtube.com\n:method: POST\n:path: /youtubei/v1/browse\n\
             :scheme: https\naccept: */*\ncookie: {COOKIE}\nx-goog-authuser: 2\n"
        );
        let h = ok(&chrome);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.authuser, "2");
    }

    #[test]
    fn chrome_separate_lines_with_and_without_colons() {
        let bare = format!(
            "Request Headers\n:authority\nmusic.youtube.com\n:method\nPOST\naccept\n*/*\n\
             cookie\n{COOKIE}\nreferer\nhttps://music.youtube.com/\nx-goog-authuser\n3\n"
        );
        let h = ok(&bare);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.authuser, "3");
        let colons = format!("accept:\n*/*\ncookie:\n{COOKIE}\nx-goog-visitor-id:\nCgtWSVNJVE9S\n");
        let h = ok(&colons);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.visitor_id.as_deref(), Some("CgtWSVNJVE9S"));
        assert_eq!(h.authuser, "0");
    }

    #[test]
    fn chrome_separate_lines_pair_only_the_headers_kept() {
        // A heading, then headers whose values look like names, as a pane copy can run.
        let pane = format!(
            "General\n:method\nPOST\nx-goog-authuser\n1\nsec-fetch-mode\ncors\n\
             Cookie:\n{COOKIE}\npriority\nu=1, i\nX-Goog-PageId\n42\nx-goog-visitor-id\n"
        );
        let h = ok(&pane);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.authuser, "1");
        assert_eq!(h.page_id.as_deref(), Some("42"));
        // A kept name on the last line has no value.
        assert_eq!(h.visitor_id, None);
    }

    #[test]
    fn chrome_separate_lines_with_an_empty_value_take_no_name_for_it() {
        let pane = format!("x-goog-pageid\nx-goog-visitor-id\nCgtWSVNJVE9S\ncookie\n{COOKIE}\n");
        let h = ok(&pane);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.page_id, None);
        assert_eq!(h.visitor_id.as_deref(), Some("CgtWSVNJVE9S"));
    }

    #[test]
    fn curl_commands_in_every_quoting() {
        let single = format!(
            "curl 'https://music.youtube.com/youtubei/v1/browse?prettyPrint=false' \\\n  \
             -H 'accept: */*' \\\n  -H 'authorization: SAPISIDHASH 1_fake' \\\n  \
             -b '{COOKIE}' \\\n  -H 'x-goog-authuser: 1' \\\n  \
             --data-raw '{{\"context\":{{}},\"browseId\":\"FEmusic_home\"}}'"
        );
        let h = ok(&single);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.authuser, "1");

        let double = format!(
            "curl \"https://music.youtube.com/youtubei/v1/browse\" \\\r\n -H \"Cookie: {COOKIE}\" \
             -H \"X-Goog-PageId: 42\" --data-raw \"{{\\\"browseId\\\":\\\"FEmusic_home\\\"}}\""
        );
        let h = ok(&double);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.page_id.as_deref(), Some("42"));

        // Chrome quotes a string with a single quote in it as $'…'.
        let ansi = format!(
            "curl $'https://music.youtube.com/' -H $'cookie: {COOKIE}' -H $'x-goog-authuser: \\x31'"
        );
        let h = ok(&ansi);
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(h.authuser, "1");

        let file = "curl 'https://music.youtube.com/' -b cookies.txt";
        assert!(parse_paste(file).unwrap_err().contains("no Cookie header"));
    }

    #[test]
    fn a_bare_cookie() {
        let h = ok(&format!("  {COOKIE}\n"));
        assert_eq!(h.cookie.expose(), COOKIE);
        assert_eq!(
            (h.authuser.as_str(), h.page_id, h.visitor_id),
            ("0", None, None)
        );
    }

    #[test]
    fn rejections_say_why_and_never_quote_the_paste() {
        let cases = [
            (
                format!("Cookie: {COOKIE}\n{}", "x".repeat(MAX_PASTE)),
                "over 64 KiB",
            ),
            (
                format!("Cookie: {COOKIE}; big={}", "y".repeat(MAX_COOKIE)),
                "over 16 KiB",
            ),
            (
                format!("Cookie: {COOKIE}; name=caf\u{e9}"),
                "Cookie header has characters",
            ),
            (
                format!("Cookie: {COOKIE}\nX-Goog-AuthUser: 1\u{7}"),
                "X-Goog-AuthUser header",
            ),
            (
                format!("Accept: */*\nX-Goog-AuthUser: {MARKER}"),
                "no Cookie header",
            ),
            (
                "Cookie: SID=fakeSID-MARKER; SAPISID=abc".to_owned(),
                "no __Secure-3PAPISID",
            ),
            (String::new(), "no Cookie header"),
            (format!("just some text with {MARKER}"), "no Cookie header"),
        ];
        for (paste, why) in cases {
            let e = parse_paste(&paste).unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
            assert!(!e.contains(MARKER) && !e.contains("FAKEsapisid"), "{e}");
        }
        assert_eq!(
            parse_paste("Cookie: SID=a").unwrap_err(),
            "That cookie has no __Secure-3PAPISID, so it isn't from a signed-in tab."
        );
        assert_eq!(
            parse_paste("Accept: */*").unwrap_err(),
            "That has no Cookie header. Copy the headers of a music.youtube.com request named browse."
        );
    }

    #[test]
    fn debug_never_shows_the_cookie() {
        let s = format!("{:?}", ok(&format!("Cookie: {COOKIE}")));
        assert!(!s.contains(MARKER) && !s.contains("FAKEsapisid"), "{s}");
        assert!(s.contains("authuser"), "{s}");
    }
}
