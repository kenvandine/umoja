//! Settings in plain language: a catalogue of the GNOME/Ubuntu GSettings
//! keys people ask about ("make text bigger", "stop the screen dimming so
//! fast"), so the model picks a friendly name instead of guessing schema
//! paths. Value types come from the installed schema, so the catalogue only
//! needs names and descriptions. Anything outside it goes through
//! `set_gsetting`, which asks the user first.

use crate::tool::{Outcome, Tool, arg_str, show};
use gio::prelude::*;
use serde_json::{Value, json};
use umoja_common::gsettings as gs;

pub struct Entry {
    pub name: &'static str,
    pub schema: &'static str,
    pub key: &'static str,
    pub about: &'static str,
}

const IFACE: &str = "org.gnome.desktop.interface";
const POWER: &str = "org.gnome.settings-daemon.plugins.power";
const A11Y_APPS: &str = "org.gnome.desktop.a11y.applications";
const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
const KEYBOARD: &str = "org.gnome.desktop.peripherals.keyboard";
const NOTIFY: &str = "org.gnome.desktop.notifications";
const PRIVACY: &str = "org.gnome.desktop.privacy";

macro_rules! e {
    ($name:literal, $schema:expr, $key:literal, $about:literal) => {
        Entry {
            name: $name,
            schema: $schema,
            key: $key,
            about: $about,
        }
    };
}

pub const CATALOG: &[Entry] = &[
    // Appearance and text
    e!("text_size", IFACE, "text-scaling-factor", "Text (font) size multiplier: 1.0 normal, 1.25 large, 1.5 larger (0.5–3.0)"),
    e!("cursor_size", IFACE, "cursor-size", "Mouse pointer size in pixels: 24 default, 32, 48, 64, 96"),
    e!("accent_color", IFACE, "accent-color", "Accent colour: blue, teal, green, yellow, orange, red, pink, purple, slate"),
    e!("animations", IFACE, "enable-animations", "Window and interface animations (false = reduce motion)"),
    e!("overlay_scrollbars", IFACE, "overlay-scrolling", "Scrollbars that hide until used (false = always show scrollbars)"),
    e!("show_battery_percentage", IFACE, "show-battery-percentage", "Show battery percentage in the top bar"),
    e!("clock_format", IFACE, "clock-format", "Clock format: \"24h\" or \"12h\""),
    e!("clock_show_seconds", IFACE, "clock-show-seconds", "Show seconds in the top bar clock"),
    e!("clock_show_weekday", IFACE, "clock-show-weekday", "Show the weekday in the top bar clock"),
    e!("clock_show_date", IFACE, "clock-show-date", "Show the date in the top bar clock"),
    e!("calendar_week_numbers", "org.gnome.desktop.calendar", "show-weekdate", "Show week numbers in the top bar's dropdown calendar"),
    e!("hot_corner", IFACE, "enable-hot-corners", "Open the overview by pushing the pointer into the top-left corner"),
    e!("locate_pointer", IFACE, "locate-pointer", "Highlight the pointer when Ctrl is pressed"),
    // Accessibility
    e!("high_contrast", "org.gnome.desktop.a11y.interface", "high-contrast", "High contrast interface"),
    e!("screen_reader", A11Y_APPS, "screen-reader-enabled", "Screen reader (Orca)"),
    e!("screen_keyboard", A11Y_APPS, "screen-keyboard-enabled", "On-screen keyboard"),
    e!("zoom", A11Y_APPS, "screen-magnifier-enabled", "Screen magnifier (zoom)"),
    e!("sticky_keys", "org.gnome.desktop.a11y.keyboard", "stickykeys-enable", "Sticky keys: press shortcuts one key at a time"),
    e!("visual_alerts", "org.gnome.desktop.wm.preferences", "visual-bell", "Flash the screen instead of the alert sound"),
    // Screen, lock and power
    e!("screen_blank_delay", "org.gnome.desktop.session", "idle-delay", "Seconds of inactivity before the screen dims and turns off (0 = never). Lengthen this when the screen dims or blanks too fast. GNOME offers 60, 120, 180, 240, 300, 480, 600, 720, 900"),
    e!("dim_screen", POWER, "idle-dim", "Dim the screen shortly before it turns off. For \"dims too fast\" lengthen screen_blank_delay instead"),
    e!("auto_brightness", POWER, "ambient-enabled", "Adjust brightness automatically from the light sensor"),
    e!("auto_lock", "org.gnome.desktop.screensaver", "lock-enabled", "Lock the screen when it turns off"),
    e!("lock_delay", "org.gnome.desktop.screensaver", "lock-delay", "Seconds after the screen turns off before it locks (0 = immediately)"),
    e!("suspend_on_battery", POWER, "sleep-inactive-battery-type", "On battery when idle: \"suspend\" or \"nothing\""),
    e!("suspend_on_battery_delay", POWER, "sleep-inactive-battery-timeout", "Seconds idle on battery before suspending (default 900)"),
    e!("suspend_plugged_in", POWER, "sleep-inactive-ac-type", "Plugged in when idle: \"suspend\" or \"nothing\""),
    e!("suspend_plugged_in_delay", POWER, "sleep-inactive-ac-timeout", "Seconds idle while plugged in before suspending (default 900)"),
    e!("power_button_action", POWER, "power-button-action", "Power button: \"suspend\", \"hibernate\", \"interactive\" (ask) or \"nothing\""),
    e!("power_saver_on_low_battery", POWER, "power-saver-profile-on-low-battery", "Switch to Power Saver automatically when the battery is low"),
    e!("night_light", "org.gnome.settings-daemon.plugins.color", "night-light-enabled", "Night Light: warmer colours at night"),
    e!("night_light_temperature", "org.gnome.settings-daemon.plugins.color", "night-light-temperature", "Night Light colour temperature in kelvin: 1700 (warmest) to 4700, default 2700"),
    e!("night_light_sunset_to_sunrise", "org.gnome.settings-daemon.plugins.color", "night-light-schedule-automatic", "Night Light follows sunset and sunrise"),
    // Notifications and sound
    e!("notification_banners", NOTIFY, "show-banners", "Show notification pop-ups (false = Do Not Disturb)"),
    e!("notifications_on_lock_screen", NOTIFY, "show-in-lock-screen", "Show notifications on the lock screen"),
    e!("event_sounds", "org.gnome.desktop.sound", "event-sounds", "Play sounds for alerts and events"),
    e!("over_amplification", "org.gnome.desktop.sound", "allow-volume-above-100-percent", "Allow volume above 100%"),
    // Mouse, touchpad and keyboard
    e!("tap_to_click", TOUCHPAD, "tap-to-click", "Tap the touchpad to click"),
    e!("touchpad_natural_scroll", TOUCHPAD, "natural-scroll", "Touchpad natural (reversed) scrolling"),
    e!("touchpad_speed", TOUCHPAD, "speed", "Touchpad pointer speed, -1.0 (slowest) to 1.0 (fastest), 0 default"),
    e!("touchpad_while_typing", TOUCHPAD, "disable-while-typing", "Disable the touchpad while typing"),
    e!("touchpad_enabled", TOUCHPAD, "send-events", "Touchpad: \"enabled\", \"disabled\" or \"disabled-on-external-mouse\""),
    e!("mouse_natural_scroll", MOUSE, "natural-scroll", "Mouse natural (reversed) scrolling"),
    e!("mouse_speed", MOUSE, "speed", "Mouse pointer speed, -1.0 (slowest) to 1.0 (fastest), 0 default"),
    e!("left_handed_mouse", MOUSE, "left-handed", "Swap the mouse buttons"),
    e!("key_repeat", KEYBOARD, "repeat", "Repeat keys when held down"),
    e!("key_repeat_delay", KEYBOARD, "delay", "Milliseconds before a held key repeats (default 500)"),
    e!("key_repeat_interval", KEYBOARD, "repeat-interval", "Milliseconds between repeats (default 30; lower is faster)"),
    // Dock, windows and workspaces
    e!("dock_always_visible", "org.gnome.shell.extensions.dash-to-dock", "dock-fixed", "Keep the dock visible (false = auto-hide)"),
    e!("dock_position", "org.gnome.shell.extensions.dash-to-dock", "dock-position", "Dock position: \"LEFT\", \"BOTTOM\" or \"RIGHT\""),
    e!("dock_icon_size", "org.gnome.shell.extensions.dash-to-dock", "dash-max-icon-size", "Largest dock icon size in pixels (16–64, default 48)"),
    e!("dock_panel_mode", "org.gnome.shell.extensions.dash-to-dock", "extend-height", "Dock fills the whole edge of the screen"),
    e!("dock_show_trash", "org.gnome.shell.extensions.dash-to-dock", "show-trash", "Show the trash in the dock"),
    e!("dock_show_volumes", "org.gnome.shell.extensions.dash-to-dock", "show-mounts", "Show drives and volumes in the dock"),
    e!("dynamic_workspaces", "org.gnome.mutter", "dynamic-workspaces", "Add and remove workspaces automatically"),
    e!("number_of_workspaces", "org.gnome.desktop.wm.preferences", "num-workspaces", "Number of workspaces when they are not dynamic"),
    e!("workspaces_on_primary_only", "org.gnome.mutter", "workspaces-only-on-primary", "Workspaces only on the primary display"),
    e!("edge_tiling", "org.gnome.mutter", "edge-tiling", "Tile windows by dragging them to screen edges"),
    e!("center_new_windows", "org.gnome.mutter", "center-new-windows", "Open new windows in the centre of the screen"),
    e!("window_buttons", "org.gnome.desktop.wm.preferences", "button-layout", "Title bar buttons, e.g. \"appmenu:minimize,maximize,close\" (Ubuntu) or \"close,minimize,maximize:\" (left side)"),
    // Privacy and system
    e!("location_services", "org.gnome.system.location", "enabled", "Let apps use your location"),
    e!("remember_recent_files", PRIVACY, "remember-recent-files", "Remember recently used files"),
    e!("auto_empty_trash", PRIVACY, "remove-old-trash-files", "Delete old files from the trash automatically"),
    e!("auto_delete_temp_files", PRIVACY, "remove-old-temp-files", "Delete old temporary files automatically"),
    e!("old_files_days", PRIVACY, "old-files-age", "Days before trash and temporary files count as old (default 30)"),
    e!("automatic_time_zone", "org.gnome.desktop.datetime", "automatic-timezone", "Set the time zone automatically"),
    e!("wallpaper", "org.gnome.desktop.background", "picture-uri", "Wallpaper image as a file:// URI (light style)"),
    e!("wallpaper_dark", "org.gnome.desktop.background", "picture-uri-dark", "Wallpaper image as a file:// URI used with Dark Style"),
    e!("show_desktop_icons", "org.gnome.shell.extensions.ding", "show-home", "Show the Home folder icon on the desktop"),
    // Umoja's own
    e!("file_search", UMOJA, "file-search-enabled", "Index the home folder so files can be found by what they're about"),
    e!("clipboard_history", UMOJA, "clipboard-history", "Keep a clipboard history this session (in memory only)"),
    e!("dictation_shortcut", UMOJA, "dictation-hotkey", "Keyboard shortcut for voice dictation, e.g. [\"<Super>d\"]"),
    e!("ask_about_screen_shortcut", UMOJA, "ask-about-screen-hotkey", "Keyboard shortcut to ask the assistant about the screen"),
    e!("clipboard_history_shortcut", UMOJA, "clipboard-history-hotkey", "Keyboard shortcut to show the clipboard history"),
    e!("copy_text_shortcut", UMOJA, "copy-text-hotkey", "Keyboard shortcut to copy the text in a screen area (OCR)"),
    e!("crash_notifications", UMOJA, "crash-notifications", "Show a notification when an app crashes, with a button to ask the assistant why"),
];

const UMOJA: &str = "org.umoja.shell";

/// Schemas searched for settings the catalogue doesn't name.
const SEARCHED_PREFIXES: &[&str] = &[
    "org.gnome.desktop.",
    "org.gnome.settings-daemon.",
    "org.gnome.mutter",
    "org.gnome.shell.extensions.dash-to-dock",
    "org.gnome.system.",
    "org.umoja.",
];

/// Keys the assistant shouldn't be steered to (internal state, keyring-ish).
const HIDDEN_KEYS: &[&str] = &["cloud-base-url", "cloud-model", "lemonade-model", "assistant-backend"];

fn names() -> Vec<&'static str> {
    CATALOG.iter().map(|e| e.name).collect()
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::read_only(
            "list_settings",
            "Find desktop settings and their current values. Optional query words narrow the list (e.g. \"text\", \"screen dim\", \"touchpad\"). Named settings come first; other matching GSettings keys are listed with their schema and key for set_gsetting.",
            json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            |a| Box::pin(async move { Outcome::text(list_settings(&a)) }),
        )
        .with_summary(|_| "Look up settings".into()),
        Tool::new(
            "change_setting",
            "Change a desktop setting by name (from list_settings). Booleans take true/false, numbers take numbers, choices take the listed string.",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "enum": names()},
                "value": {"description": "The new value"}
            }, "required": ["name", "value"]}),
            |a| Box::pin(async move { Outcome::text(change_setting(&a)) }),
        )
        .with_summary(|a| {
            format!(
                "Set {} to {}",
                arg_str(a, "name").unwrap_or("?").replace('_', " "),
                show(a.get("value").unwrap_or(&Value::Null))
            )
        }),
        Tool::new(
            "reset_setting",
            "Put a desktop setting (from list_settings) back to its default.",
            json!({"type": "object", "properties": {"name": {"type": "string", "enum": names()}}, "required": ["name"]}),
            |a| Box::pin(async move { Outcome::text(reset_setting(&a)) }),
        )
        .with_summary(|a| format!("Reset {}", arg_str(a, "name").unwrap_or("?").replace('_', " "))),
        Tool::read_only(
            "describe_setting",
            "Explain a setting: what it does, its type, allowed values, default and current value. Takes a name from list_settings, or a schema and key.",
            json!({"type": "object", "properties": {
                "name": {"type": "string"}, "schema": {"type": "string"}, "key": {"type": "string"}
            }}),
            |a| Box::pin(async move { Outcome::text(describe_setting(&a)) }),
        )
        .with_summary(|a| format!("Look up {}", arg_str(a, "name").or(arg_str(a, "key")).unwrap_or("a setting").replace('_', " "))),
        Tool::new(
            "set_gsetting",
            "Set any org.gnome.* GSettings key not covered by change_setting. The user is asked first.",
            json!({"type": "object", "properties": {
                "schema": {"type": "string"}, "key": {"type": "string"}, "value": {}
            }, "required": ["schema", "key", "value"]}),
            |a| Box::pin(async move { Outcome::text(set_gsetting(&a)) }),
        )
        .confirm()
        .with_summary(|a| {
            format!(
                "Set {} {} to {}",
                arg_str(a, "schema").unwrap_or("?"),
                arg_str(a, "key").unwrap_or("?"),
                show(a.get("value").unwrap_or(&Value::Null))
            )
        }),
    ]
}

pub fn entry(name: &str) -> Option<&'static Entry> {
    let n = name.trim().to_lowercase().replace([' ', '-'], "_");
    CATALOG.iter().find(|e| e.name == n)
}

fn open(schema: &str, key: &str) -> Result<(gio::Settings, gio::SettingsSchemaKey), String> {
    let s = gs::settings(schema).ok_or_else(|| format!("{schema} is not installed"))?;
    let k = s
        .settings_schema()
        .filter(|sc| sc.has_key(key))
        .map(|sc| sc.key(key))
        .ok_or_else(|| format!("{schema} has no key {key}"))?;
    Ok((s, k))
}

/// GVariant → JSON for showing the model current values.
fn to_json(v: &glib::Variant) -> Value {
    if let Some(b) = v.get::<bool>() {
        json!(b)
    } else if let Some(i) = v.get::<i32>() {
        json!(i)
    } else if let Some(u) = v.get::<u32>() {
        json!(u)
    } else if let Some(d) = v.get::<f64>() {
        json!((d * 1000.0).round() / 1000.0)
    } else if let Some(s) = v.str() {
        json!(s)
    } else if let Some(a) = v.get::<Vec<String>>() {
        json!(a)
    } else {
        json!(v.print(false).to_string())
    }
}

/// JSON → a GVariant of type `ty`, with the loose input models produce
/// ("true", "1.25", 300.0 for an integer key).
pub fn from_json(ty: &glib::VariantTy, v: &Value) -> Option<glib::Variant> {
    let as_f64 = || match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches('%').parse().ok(),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    };
    Some(match ty.as_str() {
        "b" => match v {
            Value::Bool(b) => b.to_variant(),
            Value::String(s) => match s.to_lowercase().as_str() {
                "true" | "on" | "yes" | "1" => true.to_variant(),
                "false" | "off" | "no" | "0" => false.to_variant(),
                _ => return None,
            },
            Value::Number(n) => (n.as_f64()? != 0.0).to_variant(),
            _ => return None,
        },
        "i" => (as_f64()?.round() as i32).to_variant(),
        "u" => {
            let f = as_f64()?;
            if f < 0.0 {
                return None;
            }
            (f.round() as u32).to_variant()
        }
        "d" => as_f64()?.to_variant(),
        "s" => match v {
            Value::String(s) => s.to_variant(),
            Value::Bool(_) | Value::Number(_) => v.to_string().to_variant(),
            _ => return None,
        },
        "as" => match v {
            Value::Array(a) => a
                .iter()
                .map(|i| i.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()?
                .to_variant(),
            Value::String(s) => vec![s.clone()].to_variant(),
            _ => return None,
        },
        _ => glib::Variant::parse(Some(ty), &show(v)).ok()?,
    })
}

/// Allowed choices for enum keys and the bounds of ranged ones.
fn constraint(k: &gio::SettingsSchemaKey) -> Option<String> {
    let range = k.range();
    let kind = range.child_value(0).str()?.to_string();
    let detail = range.child_value(1).as_variant()?;
    match kind.as_str() {
        "enum" => Some(format!(
            "one of {}",
            detail
                .get::<Vec<String>>()
                .unwrap_or_default()
                .join(", ")
        )),
        "range" => Some(format!(
            "{} to {}",
            to_json(&detail.child_value(0)),
            to_json(&detail.child_value(1))
        )),
        _ => None,
    }
}

fn query_words(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.trim_end_matches('s').to_string())
        .collect()
}

fn list_settings(a: &Value) -> String {
    let words = query_words(arg_str(a, "query").unwrap_or(""));
    let matches = |e: &Entry| {
        let hay = format!("{} {} {}", e.name.replace('_', " "), e.key, e.about).to_lowercase();
        words.iter().any(|w| hay.contains(w.as_str()))
    };
    let mut picked: Vec<&Entry> = CATALOG.iter().filter(|e| matches(e)).collect();
    let extra = if words.is_empty() { Vec::new() } else { search_schemas(&words, 12) };
    if picked.is_empty() && extra.is_empty() {
        picked = CATALOG.iter().collect();
    }
    let mut lines: Vec<String> = picked
        .into_iter()
        .filter_map(|e| {
            let (s, _) = open(e.schema, e.key).ok()?;
            Some(format!(
                "{}: {} = {}",
                e.name,
                e.about,
                show(&to_json(&s.value(e.key)))
            ))
        })
        .collect();
    if !extra.is_empty() {
        lines.push("Other settings (change with set_gsetting):".into());
        lines.extend(extra);
    }
    if lines.is_empty() {
        "No matching settings are installed.".into()
    } else {
        lines.join("\n")
    }
}

/// Keys in the installed schemas whose name or summary has all of
/// `words`, as "schema key = value — summary".
fn search_schemas(words: &[String], limit: usize) -> Vec<String> {
    let Some(source) = gio::SettingsSchemaSource::default() else { return Vec::new() };
    let (non_reloc, _) = source.list_schemas(true);
    let in_catalog = |schema: &str, key: &str| CATALOG.iter().any(|e| e.schema == schema && e.key == key);
    let mut hits: Vec<(usize, String)> = Vec::new();
    for id in non_reloc {
        let id = id.to_string();
        if !SEARCHED_PREFIXES.iter().any(|p| id.starts_with(p)) {
            continue;
        }
        let Some(schema) = source.lookup(&id, true) else { continue };
        for key in schema.list_keys() {
            let key = key.to_string();
            if in_catalog(&id, &key) || HIDDEN_KEYS.contains(&key.as_str()) {
                continue;
            }
            let k = schema.key(&key);
            let summary = k.summary().map(|s| s.to_string()).unwrap_or_default();
            let hay = format!("{} {} {}", id.rsplit('.').next().unwrap_or(""), key.replace('-', " "), summary).to_lowercase();
            let n = words.iter().filter(|w| hay.contains(w.as_str())).count();
            if n < words.len() {
                continue;
            }
            let value = gs::settings(&id).map(|s| show(&to_json(&s.value(&key)))).unwrap_or_default();
            let summary = if summary.is_empty() { String::new() } else { format!(" — {}", summary.trim()) };
            hits.push((n, format!("{id} {key} = {value}{summary}")));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter().take(limit).map(|(_, s)| s).collect()
}

fn describe_setting(a: &Value) -> String {
    let (schema, key, about) = match arg_str(a, "name").and_then(entry) {
        Some(e) => (e.schema.to_string(), e.key.to_string(), Some(e.about)),
        None => match (arg_str(a, "schema"), arg_str(a, "key")) {
            (Some(s), Some(k)) => (s.to_string(), k.to_string(), None),
            _ => return "error: give a name from list_settings, or schema and key".into(),
        },
    };
    let (s, k) = match open(&schema, &key) {
        Ok(v) => v,
        Err(m) => return format!("error: {m}"),
    };
    let mut lines = vec![format!("{schema} {key}")];
    if let Some(about) = about {
        lines.push(about.to_string());
    }
    if let Some(sum) = k.summary() {
        lines.push(format!("Summary: {}", sum.trim()));
    }
    if let Some(d) = k.description() {
        lines.push(format!("Description: {}", d.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    lines.push(format!("Type: {}", k.value_type().as_str()));
    if let Some(c) = constraint(&k) {
        lines.push(format!("Allowed: {c}"));
    }
    lines.push(format!("Default: {}", show(&to_json(&k.default_value()))));
    lines.push(format!("Current: {}", show(&to_json(&s.value(&key)))));
    if !s.is_writable(&key) {
        lines.push("Locked by the administrator.".into());
    }
    lines.join("\n")
}

fn write(schema: &str, key: &str, value: &Value) -> Result<String, String> {
    let (s, k) = open(schema, key)?;
    let Some(v) = from_json(&k.value_type(), value) else {
        return Err(format!(
            "{key} needs a value of type {}",
            k.value_type().as_str()
        ));
    };
    if !k.range_check(&v) {
        return Err(format!(
            "{} is out of range for {key}{}",
            show(value),
            constraint(&k).map(|c| format!(" ({c})")).unwrap_or_default()
        ));
    }
    if !s.is_writable(key) {
        return Err(format!("{key} is locked by the administrator"));
    }
    let before = to_json(&s.value(key));
    s.set_value(key, &v).map_err(|e| e.to_string())?;
    gio::Settings::sync();
    Ok(format!(
        "{key} changed from {} to {}",
        show(&before),
        show(&to_json(&s.value(key)))
    ))
}

fn change_setting(a: &Value) -> String {
    let Some(e) = arg_str(a, "name").and_then(entry) else {
        return "error: unknown setting; call list_settings".into();
    };
    let Some(value) = a.get("value") else {
        return "error: value is required".into();
    };
    write(e.schema, e.key, value).unwrap_or_else(|m| format!("error: {m}"))
}

fn reset_setting(a: &Value) -> String {
    let Some(e) = arg_str(a, "name").and_then(entry) else {
        return "error: unknown setting; call list_settings".into();
    };
    match open(e.schema, e.key) {
        Ok((s, _)) => {
            s.reset(e.key);
            gio::Settings::sync();
            format!("{} is back to its default, {}", e.name, show(&to_json(&s.value(e.key))))
        }
        Err(m) => format!("error: {m}"),
    }
}

fn set_gsetting(a: &Value) -> String {
    let (Some(schema), Some(key), Some(value)) =
        (arg_str(a, "schema"), arg_str(a, "key"), a.get("value"))
    else {
        return "error: schema, key and value are required".into();
    };
    // Only desktop preferences, not other apps' or the shell's own state.
    if !(schema.starts_with("org.gnome.") || schema.starts_with("org.umoja.")) {
        return "error: only org.gnome.* and org.umoja.* settings can be changed".into();
    }
    write(schema, key, value).unwrap_or_else(|m| format!("error: {m}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_names_are_unique_snake_case() {
        let mut n = names();
        n.sort();
        let len = n.len();
        n.dedup();
        assert_eq!(len, n.len());
        assert!(n.iter().all(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '_')));
        assert!(entry("Text size").is_some());
        assert!(entry("screen-blank-delay").is_some());
    }

    #[test]
    fn loose_values_convert() {
        fn t(s: &str) -> &glib::VariantTy {
            glib::VariantTy::new(s).unwrap()
        }
        assert_eq!(from_json(t("b"), &json!("on")).unwrap().get::<bool>(), Some(true));
        assert_eq!(from_json(t("d"), &json!("1.25")).unwrap().get::<f64>(), Some(1.25));
        assert_eq!(from_json(t("u"), &json!(300.0)).unwrap().get::<u32>(), Some(300));
        assert!(from_json(t("u"), &json!(-5)).is_none());
        assert_eq!(from_json(t("i"), &json!(32)).unwrap().get::<i32>(), Some(32));
        assert_eq!(from_json(t("s"), &json!("12h")).unwrap().str(), Some("12h"));
        assert_eq!(
            from_json(t("as"), &json!(["a", "b"])).unwrap().get::<Vec<String>>(),
            Some(vec!["a".into(), "b".into()])
        );
    }
}
