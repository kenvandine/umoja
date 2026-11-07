//! `org.freedesktop.impl.portal.Settings`.
//!
//! Serves the `org.freedesktop.appearance` namespace (color-scheme,
//! accent-color, contrast) from the same GNOME keys gnome-control-center
//! writes, plus read-through of `org.gnome.desktop.interface`, so Flatpak and
//! Qt/Electron apps follow Dark Style instantly.

use gio::prelude::*;
use std::collections::HashMap;
use umoja_common::gsettings as gs;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{fdo, interface};

const APPEARANCE: &str = "org.freedesktop.appearance";

/// GNOME schemas exposed read-through, like xdg-desktop-portal-gnome does.
/// GTK4 reads these over the portal on Wayland: fonts, cursor, text scaling,
/// and `button-layout`, which is what makes headerbar buttons match GNOME.
const PASSTHROUGH: &[&str] = &[
    "org.gnome.desktop.interface",
    "org.gnome.desktop.wm.preferences",
    "org.gnome.desktop.a11y",
    "org.gnome.desktop.a11y.interface",
    "org.gnome.desktop.privacy",
    "org.gnome.desktop.sound",
    "org.gnome.settings-daemon.plugins.xsettings",
];

#[derive(Clone)]
pub struct Settings;

impl Settings {
    pub fn new() -> Self {
        Self
    }
}

/// gnome `color-scheme` → portal enum (0 no preference, 1 dark, 2 light).
pub fn color_scheme_value(gnome: &str) -> u32 {
    match gnome {
        "prefer-dark" => 1,
        "prefer-light" => 2,
        _ => 0,
    }
}

/// Yaru accent names → sRGB triples (gnome `accent-color` key).
pub fn accent_rgb(name: &str) -> (f64, f64, f64) {
    let hex = match name {
        "blue" => 0x3584e4,
        "teal" => 0x2190a4,
        "green" => 0x3a944a,
        "yellow" => 0xc88800,
        "red" => 0xe62d42,
        "pink" => 0xd56199,
        "purple" => 0x9141ac,
        "slate" => 0x6f8396,
        _ => 0xe95420, // Ubuntu orange
    };
    let c = |shift: u32| ((hex >> shift) & 0xff) as f64 / 255.0;
    (c(16), c(8), c(0))
}

fn appearance(key: &str) -> Option<OwnedValue> {
    let v: Value<'static> = match key {
        "color-scheme" => color_scheme_value(&gs::get_string(
            gs::DESKTOP_INTERFACE,
            "color-scheme",
            "default",
        ))
        .into(),
        "accent-color" => {
            let (r, g, b) = accent_rgb(&gs::get_string(
                gs::DESKTOP_INTERFACE,
                "accent-color",
                "orange",
            ));
            Value::from((r, g, b))
        }
        "contrast" => {
            let high = gs::get_bool("org.gnome.desktop.a11y.interface", "high-contrast", false);
            (high as u32).into()
        }
        _ => return None,
    };
    OwnedValue::try_from(v).ok()
}

fn schema_key(schema: &str, key: &str) -> Option<OwnedValue> {
    let s = gs::settings(schema)?;
    if !gs::has_key(&s, key) {
        return None;
    }
    let v = s.value(key);
    let value: Value<'static> = if let Some(b) = v.get::<bool>() {
        b.into()
    } else if let Some(i) = v.get::<i32>() {
        i.into()
    } else if let Some(u) = v.get::<u32>() {
        u.into()
    } else if let Some(d) = v.get::<f64>() {
        d.into()
    } else if let Some(st) = v.get::<String>() {
        st.into()
    } else {
        let list = v.get::<Vec<String>>()?;
        list.into()
    };
    OwnedValue::try_from(value).ok()
}

fn read_one(namespace: &str, key: &str) -> Option<OwnedValue> {
    match namespace {
        APPEARANCE => appearance(key),
        ns if PASSTHROUGH.contains(&ns) => schema_key(ns, key),
        _ => None,
    }
}

/// `ReadAll` namespace patterns may end in `*`.
fn matches(patterns: &[String], ns: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|p| {
            p.is_empty() || p == ns || p.strip_suffix('*').is_some_and(|pre| ns.starts_with(pre))
        })
}

#[interface(name = "org.freedesktop.impl.portal.Settings")]
impl Settings {
    async fn read_all(
        &self,
        namespaces: Vec<String>,
    ) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut out = HashMap::new();
        if matches(&namespaces, APPEARANCE) {
            let m: HashMap<_, _> = ["color-scheme", "accent-color", "contrast"]
                .iter()
                .filter_map(|k| appearance(k).map(|v| (k.to_string(), v)))
                .collect();
            out.insert(APPEARANCE.to_string(), m);
        }
        for ns in PASSTHROUGH.iter().filter(|ns| matches(&namespaces, ns)) {
            if let Some(s) = gs::settings(ns) {
                let keys = s
                    .settings_schema()
                    .map(|sc| sc.list_keys())
                    .unwrap_or_default();
                let m: HashMap<_, _> = keys
                    .iter()
                    .filter_map(|k| schema_key(ns, k).map(|v| (k.to_string(), v)))
                    .collect();
                out.insert(ns.to_string(), m);
            }
        }
        out
    }

    async fn read(&self, namespace: String, key: String) -> fdo::Result<OwnedValue> {
        read_one(&namespace, &key).ok_or_else(|| {
            fdo::Error::Failed(format!("Requested setting {namespace}.{key} not found"))
        })
    }

    #[zbus(signal)]
    pub async fn setting_changed(
        emitter: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Value<'_>,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// Watch GSettings on a GLib thread and emit `SettingChanged`.
pub fn watch(conn: zbus::Connection, _settings: Settings) {
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let ctx = glib::MainContext::new();
        let _ = ctx.with_thread_default(|| {
            let mut keep = Vec::new();
            for &schema in PASSTHROUGH {
                let Some(s) = gs::settings(schema) else {
                    continue;
                };
                let conn = conn.clone();
                let handle = handle.clone();
                s.connect_changed(None, move |_, key| {
                    let mut changes = vec![(schema, key.to_string())];
                    if schema == gs::DESKTOP_INTERFACE
                        && matches!(key, "color-scheme" | "accent-color")
                    {
                        changes.push((APPEARANCE, key.to_string()));
                    }
                    for (ns, k) in changes {
                        let Some(value) = read_one(ns, &k) else {
                            continue;
                        };
                        let conn = conn.clone();
                        handle.spawn(async move {
                            let Ok(emitter) = SignalEmitter::new(&conn, crate::OBJECT_PATH) else {
                                return;
                            };
                            let _ = Settings::setting_changed(&emitter, ns, &k, Value::from(value))
                                .await;
                        });
                    }
                });
                keep.push(s);
            }
            glib::MainLoop::new(Some(&ctx), false).run();
            drop(keep);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping() {
        assert_eq!(color_scheme_value("prefer-dark"), 1);
        assert_eq!(color_scheme_value("default"), 0);
        let (r, _, _) = accent_rgb("orange");
        assert!((r - 233.0 / 255.0).abs() < 1e-9);
        assert!(matches(&["org.freedesktop.*".into()], APPEARANCE));
        assert!(!matches(&["org.kde.*".into()], APPEARANCE));
    }
}
