//! The desktop's light or dark preference, from the freedesktop settings portal's
//! `org.freedesktop.appearance` `color-scheme`.

use std::sync::mpsc;
use std::time::Duration;

use gtk::glib;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

use super::theme::{self, Mode};

const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// The mode a `color-scheme` value asks for: 1 dark, 2 light. 0 is no preference, and the
/// portal says to read unknown values as 0.
fn scheme(value: u32) -> Option<Mode> {
    match value {
        1 => Some(Mode::Dark),
        2 => Some(Mode::Light),
        _ => None,
    }
}

/// `value`'s number inside any layers of variant: the deprecated `Read` wraps it in two.
fn number(value: &Value<'_>) -> Option<u32> {
    match value {
        Value::U32(n) => Some(*n),
        Value::Value(inner) => number(inner),
        _ => None,
    }
}

/// Follows the portal on a thread of its own, posting each answer to `theme::set_portal` on
/// the main loop. Returns the first answer if it comes within `wait`; nothing when there is no
/// portal or it has no preference.
pub fn watch(wait: Duration) -> Option<Mode> {
    let (first, answer) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("requiem-portal".into())
        .spawn(move || {
            // No session bus or no portal: System falls back to GTK's setting.
            let _ = follow(&first);
        });
    if spawned.is_err() {
        return None;
    }
    answer.recv_timeout(wait).ok().flatten()
}

fn follow(first: &mpsc::Sender<Option<Mode>>) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let proxy = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )?;
    // Before the read, so a change while it is answered is not lost.
    let changes = proxy.receive_signal("SettingChanged")?;
    let value: OwnedValue = proxy
        .call("ReadOne", &(NAMESPACE, KEY))
        .or_else(|_| proxy.call("Read", &(NAMESPACE, KEY)))?;
    let mode = number(&value).and_then(scheme);
    let _ = first.send(mode);
    post(mode);
    for msg in changes {
        let Ok((namespace, key, value)) = msg.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == NAMESPACE && key == KEY {
            post(number(&value).and_then(scheme));
        }
    }
    Ok(())
}

fn post(mode: Option<Mode>) {
    glib::idle_add_once(move || theme::set_portal(mode));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_dark_two_light_and_anything_else_no_preference() {
        assert_eq!(scheme(1), Some(Mode::Dark));
        assert_eq!(scheme(2), Some(Mode::Light));
        assert_eq!(scheme(0), None);
        assert_eq!(scheme(7), None);
    }

    #[test]
    fn the_number_is_read_through_read_ones_variant_and_reads_two() {
        assert_eq!(number(&Value::U32(2)), Some(2));
        let twice = Value::Value(Box::new(Value::Value(Box::new(Value::U32(1)))));
        assert_eq!(number(&twice), Some(1));
        assert_eq!(number(&Value::Str("dark".into())), None);
    }
}
