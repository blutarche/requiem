//! What the window keeps between launches: `key=value` lines in
//! `$XDG_CONFIG_HOME/requiem/window.conf` (`~/.config` without it).

use std::path::PathBuf;

use crate::vm::{LibrarySort, ServiceFilter};

const SERVICE_FILTER: &str = "serviceFilter";
const LIBRARY_SORT: &str = "librarySort";

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".config"))
        })?;
    Some(base.join("requiem").join("window.conf"))
}

fn read() -> Vec<(String, String)> {
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    parse(&text)
}

fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn get(key: &str) -> Option<String> {
    read().into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn set(key: &str, value: &str) {
    let Some(path) = path() else { return };
    let mut pairs = read();
    match pairs.iter_mut().find(|(k, _)| k == key) {
        Some(pair) => pair.1 = value.to_owned(),
        None => pairs.push((key.to_owned(), value.to_owned())),
    }
    let text: String = pairs.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, text));
    if let Err(e) = saved {
        eprintln!("requiem-gui: cannot save {}: {e}", path.display());
    }
}

/// The saved service filter; All when none is saved or the value is unknown.
pub fn service_filter() -> ServiceFilter {
    get(SERVICE_FILTER).map_or(ServiceFilter::All, |v| ServiceFilter::from_key(&v))
}

pub fn save_service_filter(filter: ServiceFilter) {
    set(SERVICE_FILTER, filter.key());
}

/// The saved Library sort; Recent when none is saved or the value is unknown.
pub fn library_sort() -> LibrarySort {
    get(LIBRARY_SORT).map_or(LibrarySort::Recent, |v| LibrarySort::from_key(&v))
}

pub fn save_library_sort(sort: LibrarySort) {
    set(LIBRARY_SORT, sort.key());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_without_an_equals_sign_are_skipped() {
        assert_eq!(
            parse("a=1\nnoise\n b = two \n"),
            vec![("a".into(), "1".into()), ("b".into(), "two".into())]
        );
    }
}
