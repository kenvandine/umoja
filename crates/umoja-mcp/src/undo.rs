//! Undo for changes made through these tools.
//!
//! Before a tool that changes something runs, [`capture`] notes how to put
//! it back: the old values of the GSettings keys it touches, or tool calls
//! that restore the old state (Wi-Fi on, the volume at 40%, the window back
//! on workspace 2). After the call succeeds the server [`record`]s it, and
//! the result carries `_meta["umoja/undo"]` so a client can offer an Undo
//! button; `umoja/undo` (or the `undo` tool, for "undo that") runs it.
//!
//! The journal lives in `$XDG_RUNTIME_DIR/umoja/undo.json`, so it outlasts
//! the server process (the overlay stops an idle one) and every client on
//! the desktop shares it. It is gone at log out, and only the last
//! [`KEEP`] changes of the last [`MAX_AGE`] are kept.

use crate::tool::{Outcome, Tool, arg_str, arg_u64};
use gio::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use umoja_common::gsettings as gs;

const KEEP: usize = 20;
const MAX_AGE: u64 = 24 * 3600;

/// One GSettings key as it was: its value in GVariant text form, or `None`
/// when it was at its default (so undo resets it rather than pinning the
/// default value).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedKey {
    pub schema: String,
    pub key: String,
    pub value: Option<String>,
}

/// How to put a change back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inverse {
    Settings { keys: Vec<SavedKey> },
    /// Tool calls (name, arguments), run in order without asking.
    Calls { calls: Vec<(String, Value)> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    /// What was done ("Turn on Dark Style").
    pub label: String,
    pub inverse: Inverse,
    /// Unix seconds.
    pub at: u64,
    #[serde(default)]
    pub undone: bool,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn journal_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::env::var_os("UMOJA_UNDO_JOURNAL")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("umoja").join("undo.json"))
}

fn load() -> Vec<Entry> {
    std::fs::read_to_string(journal_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(entries: &[Entry]) {
    let path = journal_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(s) = serde_json::to_string(entries)
        && std::fs::write(&tmp, s).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Keep the newest [`KEEP`] entries younger than [`MAX_AGE`].
pub fn prune(mut entries: Vec<Entry>, now: u64) -> Vec<Entry> {
    entries.retain(|e| now.saturating_sub(e.at) <= MAX_AGE);
    let extra = entries.len().saturating_sub(KEEP);
    entries.drain(..extra);
    entries
}

/// Note a change that just succeeded; returns its id.
pub fn record(label: &str, inverse: Inverse) -> u64 {
    let t = now();
    let mut entries = prune(load(), t);
    let id = entries.iter().map(|e| e.id).max().unwrap_or(0).max(t * 1000) + 1;
    entries.push(Entry {
        id,
        label: label.to_string(),
        inverse,
        at: t,
        undone: false,
    });
    save(&entries);
    id
}

/// The newest change not yet undone.
pub fn last_open(entries: &[Entry]) -> Option<&Entry> {
    entries.iter().rev().find(|e| !e.undone)
}

/// Undo change `id`, or the newest one with `None`.
pub async fn apply(id: Option<u64>) -> Outcome {
    let entries = prune(load(), now());
    let entry = match id {
        Some(id) => entries.iter().find(|e| e.id == id),
        None => last_open(&entries),
    };
    let Some(entry) = entry.cloned() else {
        return Outcome::err("there is nothing to undo");
    };
    if entry.undone {
        return Outcome::err(format!("“{}” was already undone", entry.label));
    }
    let r = restore(&entry.inverse).await;
    if r.is_ok() {
        let mut entries = load();
        if let Some(e) = entries.iter_mut().find(|e| e.id == entry.id) {
            e.undone = true;
        }
        save(&entries);
    }
    match r {
        Ok(()) => Outcome::ok(format!("Undid “{}”", entry.label)),
        Err(e) => Outcome::err(format!("couldn't undo “{}”: {e}", entry.label)),
    }
}

async fn restore(inv: &Inverse) -> Result<(), String> {
    match inv {
        Inverse::Settings { keys } => {
            for k in keys {
                restore_key(k)?;
            }
            gio::Settings::sync();
            Ok(())
        }
        Inverse::Calls { calls } => {
            let tools = crate::registry();
            for (name, args) in calls {
                let tool = tools
                    .iter()
                    .find(|t| t.name == name)
                    .ok_or_else(|| format!("no tool {name}"))?;
                let o = (tool.run)(args.clone()).await;
                if o.is_error {
                    return Err(o.text.trim_start_matches("error: ").to_string());
                }
            }
            Ok(())
        }
    }
}

fn restore_key(k: &SavedKey) -> Result<(), String> {
    let s = gs::settings(&k.schema).ok_or_else(|| format!("{} is not installed", k.schema))?;
    match &k.value {
        None => {
            s.reset(&k.key);
            Ok(())
        }
        Some(text) => {
            let ty = s
                .settings_schema()
                .filter(|sc| sc.has_key(&k.key))
                .map(|sc| sc.key(&k.key).value_type())
                .ok_or_else(|| format!("{} has no key {}", k.schema, k.key))?;
            let v = glib::Variant::parse(Some(&ty), text).map_err(|e| e.to_string())?;
            s.set_value(&k.key, &v).map_err(|e| e.to_string())
        }
    }
}

fn saved(schema: &str, keys: &[&str]) -> Vec<SavedKey> {
    let Some(s) = gs::settings(schema) else { return Vec::new() };
    keys.iter()
        .filter(|k| gs::has_key(&s, k))
        .map(|k| SavedKey {
            schema: schema.to_string(),
            key: k.to_string(),
            value: s.user_value(k).map(|v| v.print(false).to_string()),
        })
        .collect()
}

fn settings(keys: Vec<SavedKey>) -> Option<Inverse> {
    (!keys.is_empty()).then_some(Inverse::Settings { keys })
}

fn calls(c: Vec<(&str, Value)>) -> Option<Inverse> {
    (!c.is_empty()).then(|| Inverse::Calls {
        calls: c.into_iter().map(|(n, a)| (n.to_string(), a)).collect(),
    })
}

/// How to undo `tool` with `args`, worked out before it runs. `None` for
/// tools that can't be undone (opening an app, emptying the Trash) or
/// whose old state can't be read.
pub async fn capture(tool: &str, args: &Value) -> Option<Inverse> {
    use crate::desktop as d;
    match tool {
        "set_dark_style" => settings(saved(gs::DESKTOP_INTERFACE, &["color-scheme", "gtk-theme"])),
        "set_do_not_disturb" => settings(saved("org.gnome.desktop.notifications", &["show-banners"])),
        "set_night_light" => settings(saved("org.gnome.settings-daemon.plugins.color", &["night-light-enabled"])),
        "set_wallpaper" => settings(saved(
            "org.gnome.desktop.background",
            &["picture-uri", "picture-uri-dark", "picture-options"],
        )),
        "change_setting" | "reset_setting" => {
            let e = arg_str(args, "name").and_then(crate::settings::entry)?;
            settings(saved(e.schema, &[e.key]))
        }
        "set_gsetting" => {
            let (schema, key) = (arg_str(args, "schema")?, arg_str(args, "key")?);
            settings(saved(schema, &[key]))
        }
        "set_wifi" => calls(vec![("set_wifi", json!({"enabled": d::wifi_enabled().await?}))]),
        "set_bluetooth" => calls(vec![("set_bluetooth", json!({"enabled": d::bluetooth_powered().await?}))]),
        "set_airplane_mode" => {
            let mut c = Vec::new();
            if let Some(on) = d::wifi_enabled().await {
                c.push(("set_wifi", json!({"enabled": on})));
            }
            if let Some(on) = d::bluetooth_powered().await {
                c.push(("set_bluetooth", json!({"enabled": on})));
            }
            calls(c)
        }
        "set_power_mode" => calls(vec![("set_power_mode", json!({"mode": d::power_profile().await?}))]),
        "set_volume" => {
            if args.get("level").is_none() && args.get("change").is_none() && args.get("mute").is_none() {
                return None;
            }
            let mic = crate::actions::is_mic(args);
            let (pct, muted) = crate::actions::volume_now(mic).await?;
            let mut a = json!({"level": pct, "mute": muted});
            if mic {
                a["device"] = json!("microphone");
            }
            calls(vec![("set_volume", a)])
        }
        "set_brightness" => {
            if args.get("level").is_none() && args.get("change").is_none() {
                return None;
            }
            calls(vec![("set_brightness", json!({"level": crate::actions::brightness_now()?.max(1)}))])
        }
        "set_audio_device" => {
            arg_str(args, "device")?;
            let input = matches!(arg_str(args, "kind"), Some("input"));
            let name = crate::actions::default_device_name(input).await?;
            calls(vec![(
                "set_audio_device",
                json!({"device": name, "kind": if input { "input" } else { "output" }}),
            )])
        }
        "switch_workspace" => {
            let (cur, _) = d::workspaces().await?;
            calls(vec![("switch_workspace", json!({"number": cur + 1}))])
        }
        "move_window_to_workspace" => {
            let (cur, _) = d::workspaces().await?;
            let w = d::with_window(args).await.ok()?;
            let ws = d::window_workspace(w.id).await?;
            calls(vec![
                ("move_window_to_workspace", json!({"window": format!("id:{}", w.id), "number": ws + 1})),
                ("switch_workspace", json!({"number": cur + 1})),
            ])
        }
        "arrange_window" => {
            let w = d::with_window(args).await.ok()?;
            let back = arrange_inverse(arg_str(args, "position")?, w.flags)?;
            calls(vec![("arrange_window", json!({"window": format!("id:{}", w.id), "position": back}))])
        }
        _ => None,
    }
}

/// The `arrange_window` position that undoes moving a window with state
/// `flags` to `position`.
pub fn arrange_inverse(position: &str, flags: u32) -> Option<&'static str> {
    use crate::desktop::{FULLSCREEN, MAXIMIZED, MINIMIZED, TILED};
    let opposite = match position {
        "monitor_left" => Some("monitor_right"),
        "monitor_right" => Some("monitor_left"),
        "monitor_up" => Some("monitor_down"),
        "monitor_down" => Some("monitor_up"),
        _ => None,
    };
    if opposite.is_some() {
        return opposite;
    }
    if flags & MINIMIZED != 0 || flags & TILED != 0 {
        // Which half it was in isn't known.
        None
    } else if flags & FULLSCREEN != 0 {
        Some("fullscreen")
    } else if flags & MAXIMIZED != 0 {
        Some("maximize")
    } else if position == "restore" {
        None
    } else {
        Some("restore")
    }
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "undo",
            "Undo the last change made with these tools (a setting, a switch like Wi-Fi or Dark Style, the volume or brightness, the wallpaper, a window or workspace move) when the user says undo, undo that, put it back or change it back. Pass nothing for the last change.",
            json!({"type": "object", "properties": {
                "id": {"type": "integer", "description": "A specific change to undo (from umoja/undo metadata); omit for the last one"}
            }}),
            |a| Box::pin(async move { apply(arg_u64(&a, "id")).await }),
        )
        .with_summary(|_| {
            match last_open(&load()) {
                Some(e) => format!("Undo “{}”", e.label),
                None => "Undo the last change".into(),
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{MAXIMIZED, TILED};

    fn entry(id: u64, at: u64) -> Entry {
        Entry {
            id,
            label: format!("change {id}"),
            inverse: Inverse::Calls { calls: vec![] },
            at,
            undone: false,
        }
    }

    #[test]
    fn prune_keeps_recent_and_few() {
        let now = 100_000;
        let mut v: Vec<Entry> = (0..30).map(|i| entry(i, now - 10)).collect();
        v.insert(0, entry(99, now - MAX_AGE - 1));
        let p = prune(v, now);
        assert_eq!(p.len(), KEEP);
        assert_eq!(p.first().unwrap().id, 10);
        assert_eq!(p.last().unwrap().id, 29);
    }

    #[test]
    fn last_open_skips_undone() {
        let mut v = vec![entry(1, 0), entry(2, 0)];
        v[1].undone = true;
        assert_eq!(last_open(&v).unwrap().id, 1);
    }

    #[test]
    fn inverse_round_trips_as_json() {
        let i = Inverse::Settings {
            keys: vec![SavedKey { schema: "a.b".into(), key: "k".into(), value: Some("'x'".into()) }],
        };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<Inverse>(&s).unwrap(), i);
        let c = Inverse::Calls { calls: vec![("set_wifi".into(), json!({"enabled": true}))] };
        assert_eq!(serde_json::from_str::<Inverse>(&serde_json::to_string(&c).unwrap()).unwrap(), c);
    }

    #[test]
    fn arrange_inverses() {
        assert_eq!(arrange_inverse("left", 0), Some("restore"));
        assert_eq!(arrange_inverse("left", MAXIMIZED), Some("maximize"));
        assert_eq!(arrange_inverse("maximize", TILED), None);
        assert_eq!(arrange_inverse("monitor_left", TILED), Some("monitor_right"));
        assert_eq!(arrange_inverse("restore", 0), None);
    }
}
