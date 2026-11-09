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
pub const DESKTOP_CALENDAR: &str = "org.gnome.desktop.calendar";
pub const WM_KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const MUTTER: &str = "org.gnome.mutter";
pub const MUTTER_KEYBINDINGS: &str = "org.gnome.mutter.keybindings";
pub const SHELL: &str = "org.gnome.shell";
pub const SHELL_KEYBINDINGS: &str = "org.gnome.shell.keybindings";
pub const DASH_TO_DOCK: &str = "org.gnome.shell.extensions.dash-to-dock";
pub const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
/// One GNOME custom shortcut (relocatable, under media-keys'
/// `custom-keybindings` paths): `name`, `command`, `binding`.
pub const CUSTOM_KEYBINDING: &str =
    "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
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

/// A relocatable schema at `path`, without aborting when it is missing.
pub fn settings_at(schema_id: &str, path: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(schema_id, true)?;
    Some(gio::Settings::new_full(
        &schema,
        None::<&gio::SettingsBackend>,
        Some(path),
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

pub const BACKGROUND: &str = "org.gnome.desktop.background";

/// Ubuntu's default wallpaper, used when the configured one is missing.
pub const FALLBACK_WALLPAPER: &str = "/usr/share/backgrounds/warty-final-ubuntu.png";

/// The wallpaper image GNOME would show: `picture-uri-dark` when the
/// colour scheme prefers dark, `picture-uri` otherwise, falling back to the
/// other one and then to Ubuntu's default. Slideshow XMLs and missing files
/// are skipped.
pub fn wallpaper_path(prefer_dark: bool) -> Option<std::path::PathBuf> {
    let dark = get_string(BACKGROUND, "picture-uri-dark", "");
    let light = get_string(BACKGROUND, "picture-uri", "");
    let order = if prefer_dark { [dark, light] } else { [light, dark] };
    order
        .into_iter()
        .filter_map(|uri| {
            let path = glib::filename_from_uri(&uri).ok()?.0;
            let is_image = path.extension().is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().to_str(),
                    Some("png" | "jpg" | "jpeg" | "webp" | "svg")
                )
            });
            (is_image && path.exists()).then_some(path)
        })
        .next()
        .or_else(|| {
            let p = std::path::PathBuf::from(FALLBACK_WALLPAPER);
            p.exists().then_some(p)
        })
}

/// Whether `org.gnome.desktop.interface color-scheme` asks for dark.
pub fn prefers_dark() -> bool {
    get_string(DESKTOP_INTERFACE, "color-scheme", "default") == "prefer-dark"
}
