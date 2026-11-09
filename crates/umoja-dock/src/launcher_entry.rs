//! Unity LauncherEntry API: unread counts, progress and urgency on dock
//! icons, as Ubuntu Dock shows them.
//!
//! Apps (Telegram, Thunderbird, Qt's `setBadgeNumber`, libunity users,
//! Chromium and Electron) broadcast
//! `com.canonical.Unity.LauncherEntry.Update(s app_uri, a{sv} properties)`
//! with `app_uri` like `application://org.telegram.desktop.desktop` and
//! properties `count` (x), `count-visible` (b), `progress` (d),
//! `progress-visible` (b) and `urgent` (b). Each signal may carry only the
//! keys that changed, so entries are merged, and an app's entries are
//! dropped when it leaves the bus.

use gtk::glib;
use gtk4 as gtk;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
/// Ubuntu Dock owns this name too: some clients only send updates once a
/// Unity-compatible launcher is on the bus.
const UNITY_BUS_NAME: &str = "com.canonical.Unity";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub count: i64,
    pub count_visible: bool,
    pub progress: f64,
    pub progress_visible: bool,
    pub urgent: bool,
}

impl Entry {
    /// The badge text, or `None` when no count should show.
    pub fn badge(&self) -> Option<String> {
        (self.count_visible && self.count > 0).then(|| format_count(self.count))
    }

    pub fn progress(&self) -> Option<f64> {
        self.progress_visible
            .then_some(self.progress.clamp(0.0, 1.0))
    }

    fn merge(&mut self, props: &glib::Variant) {
        for item in props.iter() {
            let (Some(key), value) = (
                item.child_value(0).str().map(String::from),
                item.child_value(1),
            ) else {
                continue;
            };
            let value = value.as_variant().unwrap_or(value);
            match key.as_str() {
                // libunity sends x, but accept any integer type.
                "count" => {
                    if let Some(n) = int(&value) {
                        self.count = n;
                    }
                }
                "count-visible" => self.count_visible = value.get().unwrap_or(false),
                "progress" => self.progress = value.get().unwrap_or(0.0),
                "progress-visible" => self.progress_visible = value.get().unwrap_or(false),
                "urgent" => self.urgent = value.get().unwrap_or(false),
                _ => {}
            }
        }
    }
}

fn int(v: &glib::Variant) -> Option<i64> {
    v.get::<i64>()
        .or_else(|| v.get::<i32>().map(i64::from))
        .or_else(|| v.get::<u32>().map(i64::from))
        .or_else(|| v.get::<u64>().and_then(|n| i64::try_from(n).ok()))
}

/// Short enough for the badge: 7, 42, 999, 1k, 12k.
pub fn format_count(n: i64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{}k", n / 1000)
    }
}

/// `application://foo.desktop` → `foo`, the dock's key for an app.
pub fn key_for_uri(uri: &str) -> String {
    let id = uri.strip_prefix("application://").unwrap_or(uri);
    id.strip_suffix(".desktop").unwrap_or(id).to_string()
}

#[derive(Default)]
pub struct Entries {
    /// Dock key → (sender's unique name, merged entry).
    map: RefCell<HashMap<String, (String, Entry)>>,
}

impl Entries {
    pub fn get(&self, key: &str) -> Option<Entry> {
        self.map.borrow().get(key).map(|(_, e)| e.clone())
    }

    /// Returns the key that changed.
    fn update(&self, sender: &str, uri: &str, props: &glib::Variant) -> Option<String> {
        let key = key_for_uri(uri);
        if key.is_empty() {
            return None;
        }
        let mut map = self.map.borrow_mut();
        let slot = map.entry(key.clone()).or_default();
        let before = slot.1.clone();
        slot.0 = sender.to_string();
        slot.1.merge(props);
        (slot.1 != before).then_some(key)
    }

    /// Drops everything `sender` published; returns the keys it had.
    fn forget(&self, sender: &str) -> Vec<String> {
        let mut map = self.map.borrow_mut();
        let gone: Vec<String> = map
            .iter()
            .filter(|(_, (s, _))| s == sender)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &gone {
            map.remove(k);
        }
        gone
    }
}

/// Listen for LauncherEntry updates; `changed` gets each dock key whose
/// entry changed.
pub fn start(changed: impl Fn(&str) + 'static) -> Rc<Entries> {
    let entries = Rc::new(Entries::default());
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return entries;
    };
    let changed = Rc::new(changed);

    let (weak, cb) = (Rc::downgrade(&entries), changed.clone());
    #[allow(deprecated)]
    conn.signal_subscribe(
        None,
        Some(INTERFACE),
        Some("Update"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |_, sender, _, _, _, params| {
            let Some(entries) = weak.upgrade() else {
                return;
            };
            if params.type_().as_str() != "(sa{sv})" {
                return;
            }
            let uri = params.child_value(0);
            let Some(uri) = uri.str() else { return };
            if let Some(key) = entries.update(sender, uri, &params.child_value(1)) {
                tracing::debug!(key, entry = ?entries.get(&key), "launcher entry");
                cb(&key);
            }
        },
    );

    // Clear badges of apps that quit (or crashed) without resetting them.
    let (weak, cb) = (Rc::downgrade(&entries), changed);
    #[allow(deprecated)]
    conn.signal_subscribe(
        Some("org.freedesktop.DBus"),
        Some("org.freedesktop.DBus"),
        Some("NameOwnerChanged"),
        Some("/org/freedesktop/DBus"),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            let Some(entries) = weak.upgrade() else {
                return;
            };
            let Some((name, _old, new)) = params.get::<(String, String, String)>() else {
                return;
            };
            if new.is_empty() && name.starts_with(':') {
                for key in entries.forget(&name) {
                    cb(&key);
                }
            }
        },
    );

    // Never released; the dock owns it for its lifetime.
    let _ = gio::bus_own_name_on_connection(
        &conn,
        UNITY_BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        |_, _| {},
        |_, _| {},
    );
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::prelude::*;

    fn props(pairs: &[(&str, glib::Variant)]) -> glib::Variant {
        let dict = glib::VariantDict::new(None);
        for (k, v) in pairs {
            dict.insert_value(k, v);
        }
        dict.end()
    }

    #[test]
    fn uri_to_key() {
        assert_eq!(
            key_for_uri("application://org.telegram.desktop.desktop"),
            "org.telegram.desktop"
        );
        assert_eq!(key_for_uri("thunderbird.desktop"), "thunderbird");
    }

    #[test]
    fn counts() {
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12k");
    }

    #[test]
    fn partial_updates_merge_and_senders_clear() {
        let e = Entries::default();
        let uri = "application://foo.desktop";
        let k = e.update(
            ":1.7",
            uri,
            &props(&[
                ("count", 3i64.to_variant()),
                ("count-visible", true.to_variant()),
            ]),
        );
        assert_eq!(k.as_deref(), Some("foo"));
        assert_eq!(e.get("foo").unwrap().badge().as_deref(), Some("3"));

        // Only progress changes; the count stays.
        e.update(
            ":1.7",
            uri,
            &props(&[
                ("progress", 0.5f64.to_variant()),
                ("progress-visible", true.to_variant()),
            ]),
        );
        let entry = e.get("foo").unwrap();
        assert_eq!(entry.badge().as_deref(), Some("3"));
        assert_eq!(entry.progress(), Some(0.5));

        // An identical update is not a change.
        assert_eq!(
            e.update(":1.7", uri, &props(&[("count", 3i64.to_variant())])),
            None
        );

        e.update(
            ":1.7",
            uri,
            &props(&[("count-visible", false.to_variant())]),
        );
        assert_eq!(e.get("foo").unwrap().badge(), None);

        assert_eq!(e.forget(":1.9"), Vec::<String>::new());
        assert_eq!(e.forget(":1.7"), vec!["foo".to_string()]);
        assert_eq!(e.get("foo"), None);
    }
}
