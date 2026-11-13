//! Agent-owned network secrets in the user's keyring, over the Secret
//! Service D-Bus API (gnome-keyring), stored exactly as gnome-shell's
//! agent stores them with libsecret: schema
//! `org.freedesktop.NetworkManager.Connection`, attributes
//! `connection-uuid`, `setting-name`, `setting-key`. So passwords saved
//! "for this user only" under GNOME keep working under Umoja and back.
//!
//! Every function blocks: call them from a worker thread.

use gio::prelude::*;
use gtk4::glib;
use std::collections::HashMap;

const SECRETS: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const ITEM: &str = "org.freedesktop.Secret.Item";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";
const SCHEMA: &str = "org.freedesktop.NetworkManager.Connection";

pub const UUID_TAG: &str = "connection-uuid";
pub const SN_TAG: &str = "setting-name";
pub const SK_TAG: &str = "setting-key";

const TIMEOUT: i32 = 5000;

fn op(p: &str) -> glib::variant::ObjectPath {
    crate::bus::object_path(p)
}

struct Keyring {
    conn: gio::DBusConnection,
    session: String,
}

impl Keyring {
    fn open() -> Option<Self> {
        let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
        // "plain": the secret travels unencrypted over the private session
        // bus, as libsecret does when it can't negotiate (the bus is ours).
        let reply = conn
            .call_sync(
                Some(SECRETS),
                SERVICE_PATH,
                SERVICE,
                "OpenSession",
                Some(&("plain", "".to_variant()).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                TIMEOUT,
                gio::Cancellable::NONE,
            )
            .inspect_err(|e| tracing::warn!(error = %e, "no Secret Service"))
            .ok()?;
        let session = reply.child_value(1).str()?.to_string();
        Some(Self { conn, session })
    }

    fn call(&self, path: &str, iface: &str, method: &str, args: Option<glib::Variant>) -> Option<glib::Variant> {
        self.conn
            .call_sync(
                Some(SECRETS),
                path,
                iface,
                method,
                args.as_ref(),
                None,
                gio::DBusCallFlags::NONE,
                TIMEOUT,
                gio::Cancellable::NONE,
            )
            .inspect_err(|e| tracing::warn!(method, error = %e, "Secret Service call failed"))
            .ok()
    }

    /// Items matching `attrs`, unlocking the locked ones when the keyring
    /// lets us without a prompt (the login keyring is unlocked at login).
    fn search(&self, attrs: &HashMap<String, String>) -> Vec<String> {
        let Some(reply) = self.call(SERVICE_PATH, SERVICE, "SearchItems", Some((attrs.clone(),).to_variant())) else {
            return vec![];
        };
        let paths = |v: glib::Variant| v.iter().filter_map(|p| p.str().map(String::from)).collect::<Vec<_>>();
        let mut items = paths(reply.child_value(0));
        let locked = paths(reply.child_value(1));
        if !locked.is_empty() {
            let list: Vec<_> = locked.iter().map(|p| op(p)).collect();
            if let Some(r) = self.call(SERVICE_PATH, SERVICE, "Unlock", Some((list,).to_variant())) {
                items.extend(paths(r.child_value(0)));
                if r.child_value(1).str().is_some_and(|p| p != "/") {
                    tracing::info!("keyring is locked; skipping its network secrets");
                }
            }
        }
        items
    }

    fn attributes(&self, item: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Some(reply) = self.call(
            item,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some((ITEM, "Attributes").to_variant()),
        ) else {
            return out;
        };
        let v = reply.child_value(0);
        let v = v.as_variant().unwrap_or(v);
        for e in v.iter() {
            if let (Some(k), Some(val)) = (e.child_value(0).str(), e.child_value(1).str()) {
                out.insert(k.to_string(), val.to_string());
            }
        }
        out
    }
}

impl Drop for Keyring {
    fn drop(&mut self) {
        let _ = self.conn.call_sync(
            Some(SECRETS),
            &self.session,
            "org.freedesktop.Secret.Session",
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            TIMEOUT,
            gio::Cancellable::NONE,
        );
    }
}

/// `setting-key` -> secret for one connection's setting.
pub fn lookup(uuid: &str, setting: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(k) = Keyring::open() else { return out };
    let attrs = HashMap::from([
        (UUID_TAG.to_string(), uuid.to_string()),
        (SN_TAG.to_string(), setting.to_string()),
    ]);
    let items = k.search(&attrs);
    if items.is_empty() {
        return out;
    }
    let list: Vec<_> = items.iter().map(|p| op(p)).collect();
    let Some(reply) = k.call(
        SERVICE_PATH,
        SERVICE,
        "GetSecrets",
        Some((list, op(&k.session)).to_variant()),
    ) else {
        return out;
    };
    // a{o(oayays)}
    for e in reply.child_value(0).iter() {
        let Some(item) = e.child_value(0).str().map(String::from) else {
            continue;
        };
        let Some(value) = e.child_value(1).child_value(2).get::<Vec<u8>>() else {
            continue;
        };
        if let Some(key) = k.attributes(&item).remove(SK_TAG) {
            out.insert(key, String::from_utf8_lossy(&value).into_owned());
        }
    }
    out
}

/// One secret, replacing any older copy (libsecret's secret_password_store).
pub fn store(uuid: &str, setting: &str, key: &str, label: &str, value: &str) {
    let Some(k) = Keyring::open() else { return };
    let attrs = HashMap::from([
        (UUID_TAG.to_string(), uuid.to_string()),
        (SN_TAG.to_string(), setting.to_string()),
        (SK_TAG.to_string(), key.to_string()),
        ("xdg:schema".to_string(), SCHEMA.to_string()),
    ]);
    let props: HashMap<String, glib::Variant> = HashMap::from([
        (format!("{ITEM}.Label"), label.to_variant()),
        (format!("{ITEM}.Attributes"), attrs.to_variant()),
    ]);
    let secret = (
        op(&k.session),
        Vec::<u8>::new(),
        value.as_bytes().to_vec(),
        "text/plain",
    );
    if let Some(r) = k.call(
        DEFAULT_COLLECTION,
        COLLECTION,
        "CreateItem",
        Some((props, secret, true).to_variant()),
    ) && r.child_value(1).str().is_some_and(|p| p != "/")
    {
        tracing::info!("keyring is locked; network secret not saved");
    }
}

/// Every secret saved for a connection.
pub fn delete(uuid: &str) {
    let Some(k) = Keyring::open() else { return };
    let attrs = HashMap::from([(UUID_TAG.to_string(), uuid.to_string())]);
    for item in k.search(&attrs) {
        k.call(&item, ITEM, "Delete", None);
    }
}
