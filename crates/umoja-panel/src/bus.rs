//! Small helpers for talking to system services (NetworkManager, UPower)
//! over GDBus without pulling in a second D-Bus stack.

use gio::prelude::*;
use gtk4::glib;
use std::collections::HashMap;

const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// The result of `org.freedesktop.DBus.Properties.GetAll`, with the `v`
/// wrappers already removed.
#[derive(Default, Debug, Clone)]
pub struct Props(HashMap<String, glib::Variant>);

impl Props {
    pub fn from_dict(v: &glib::Variant) -> Self {
        let mut map = HashMap::new();
        for entry in v.iter() {
            let (Some(key), value) = (
                entry.child_value(0).str().map(String::from),
                entry.child_value(1),
            ) else {
                continue;
            };
            map.insert(key, value.as_variant().unwrap_or(value));
        }
        Self(map)
    }

    pub fn get(&self, key: &str) -> Option<&glib::Variant> {
        self.0.get(key)
    }

    /// Strings and object paths.
    pub fn str(&self, key: &str) -> Option<String> {
        self.get(key)?.str().map(String::from)
    }

    /// An object path, `None` for the "/" placeholder.
    pub fn obj(&self, key: &str) -> Option<String> {
        self.str(key).filter(|p| p != "/" && !p.is_empty())
    }

    pub fn objs(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|v| v.iter().filter_map(|c| c.str().map(String::from)).collect())
            .unwrap_or_default()
    }

    pub fn u32(&self, key: &str) -> Option<u32> {
        self.get(key)?.get()
    }

    pub fn u8(&self, key: &str) -> Option<u8> {
        self.get(key)?.get()
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.get()
    }

    pub fn bytes(&self, key: &str) -> Vec<u8> {
        self.get(key)
            .and_then(|v| v.get::<Vec<u8>>())
            .unwrap_or_default()
    }
}

/// Synchronous `GetAll`. Only used against local system daemons, which
/// answer in well under a millisecond; the short timeout keeps a wedged
/// daemon from freezing the panel.
pub fn get_all(conn: &gio::DBusConnection, dest: &str, path: &str, iface: &str) -> Option<Props> {
    let reply = conn
        .call_sync(
            Some(dest),
            path,
            PROPERTIES,
            "GetAll",
            Some(&(iface,).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            500,
            gio::Cancellable::NONE,
        )
        .ok()?;
    Some(Props::from_dict(&reply.child_value(0)))
}

/// Fire-and-forget method call that logs failures.
pub fn call(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    args: Option<glib::Variant>,
) {
    let what = format!("{iface}.{method}");
    conn.call(
        Some(dest),
        path,
        iface,
        method,
        args.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
        move |r| {
            if let Err(e) = r {
                tracing::warn!(call = what, error = %e, "D-Bus call failed");
            }
        },
    );
}

/// `org.freedesktop.DBus.Properties.Set`.
pub fn set_property(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    iface: &str,
    prop: &str,
    value: glib::Variant,
) {
    // A glib::Variant already serialises as `v`; wrapping it again with
    // from_variant would send <<value>> and the service rejects the Set.
    let args = (iface, prop, value).to_variant();
    call(conn, dest, path, PROPERTIES, "Set", Some(args));
}

pub fn object_path(p: &str) -> glib::variant::ObjectPath {
    glib::variant::ObjectPath::try_from(p.to_string()).unwrap_or_else(|_| {
        glib::variant::ObjectPath::try_from("/".to_string()).expect("root path")
    })
}

/// Whether a method call came from the current owner of `name` (a system
/// daemon), so only that daemon can drive one of our agents.
pub fn sent_by_owner(conn: &gio::DBusConnection, sender: Option<&str>, name: &str) -> bool {
    let Some(sender) = sender else {
        return false;
    };
    conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetNameOwner",
        Some(&(name,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|r| r.child_value(0).str().map(|o| o == sender))
    .unwrap_or(false)
}

pub fn system_bus() -> Option<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
        .inspect_err(|e| tracing::warn!(error = %e, "no system bus"))
        .ok()
}
