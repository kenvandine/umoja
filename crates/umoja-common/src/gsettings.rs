//! GSettings helpers.
//!
//! Umoja reads GNOME/Ubuntu schemas (`org.gnome.desktop.*`,
//! `org.gnome.shell.extensions.dash-to-dock`, ...) directly so that changes
//! made in gnome-control-center apply instantly. Some of those schemas are
//! optional (dash-to-dock is only present when the Ubuntu Dock extension is
//! installed), and `gio::Settings::new` aborts the process on a missing
//! schema, so every lookup goes through [`settings`] which returns `None`
//! instead.

use gio::prelude::*;

pub const UMOJA_SHELL: &str = "org.umoja.shell";
pub const DESKTOP_INTERFACE: &str = "org.gnome.desktop.interface";
pub const WM_KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const MUTTER: &str = "org.gnome.mutter";
pub const MUTTER_KEYBINDINGS: &str = "org.gnome.mutter.keybindings";
pub const SHELL: &str = "org.gnome.shell";
pub const SHELL_KEYBINDINGS: &str = "org.gnome.shell.keybindings";
pub const DASH_TO_DOCK: &str = "org.gnome.shell.extensions.dash-to-dock";
pub const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
pub const NIGHT_LIGHT: &str = "org.gnome.settings-daemon.plugins.color";

/// Look up a schema without aborting when it is not installed.
pub fn settings(schema_id: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(schema_id, true)?;
    Some(gio::Settings::new_full(
        &schema,
        None::<&gio::SettingsBackend>,
        None,
    ))
}

/// True when `key` exists in `settings`' schema.
pub fn has_key(settings: &gio::Settings, key: &str) -> bool {
    settings
        .settings_schema()
        .map(|s| s.has_key(key))
        .unwrap_or(false)
}

pub fn get_bool(schema: &str, key: &str, default: bool) -> bool {
    match settings(schema) {
        Some(s) if has_key(&s, key) => s.boolean(key),
        _ => default,
    }
}

pub fn get_int(schema: &str, key: &str, default: i32) -> i32 {
    match settings(schema) {
        Some(s) if has_key(&s, key) => s.int(key),
        _ => default,
    }
}

pub fn get_string(schema: &str, key: &str, default: &str) -> String {
    match settings(schema) {
        Some(s) if has_key(&s, key) => s.string(key).to_string(),
        _ => default.to_string(),
    }
}

pub fn get_strv(schema: &str, key: &str) -> Vec<String> {
    match settings(schema) {
        Some(s) if has_key(&s, key) => s.strv(key).iter().map(|g| g.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Call `f` now and again whenever `key` changes. Returns the settings object,
/// which must be kept alive for the callback to keep firing.
pub fn watch<F>(schema: &str, key: &str, f: F) -> Option<gio::Settings>
where
    F: Fn(&gio::Settings, &str) + 'static,
{
    let s = settings(schema)?;
    if !has_key(&s, key) {
        return None;
    }
    f(&s, key);
    s.connect_changed(Some(key), move |s, k| f(s, k));
    Some(s)
}
