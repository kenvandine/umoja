//! Named setups: a set of apps, each on its workspace and in its place
//! (left half, a quarter, maximized...), saved under a name ("Coding",
//! "Writing") and brought back in one go.
//!
//! `save_setup` records the windows open now; `define_setup` builds one
//! from a description the model turns into a list; `open_setup` opens
//! the apps that aren't running, waits for their windows and puts every
//! window back. Stored in `~/.config/umoja/setups.json`. Tile positions
//! come from umoja-mir's `GetWindowTiles`.

use crate::desktop::{self, FULLSCREEN, MAXIMIZED, Win};
use crate::tool::{Outcome, Tool, arg_str};
use gio::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// Desktop file id ("org.gnome.Ptyxis.desktop"), or the window's app id
    /// when it has no launcher.
    pub app: String,
    /// What the user calls it ("Terminal").
    pub name: String,
    /// 1-based.
    pub workspace: u32,
    /// An `arrange_window` position, or "" to leave it where the app puts it.
    #[serde(default)]
    pub position: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    pub name: String,
    pub windows: Vec<Place>,
}

/// `tiling::Action` numbers → `arrange_window` positions.
pub fn tile_position(action: u32) -> Option<&'static str> {
    Some(match action {
        1 => "left",
        2 => "right",
        7 => "top_left",
        8 => "top_right",
        9 => "bottom_left",
        10 => "bottom_right",
        28 => "top",
        29 => "bottom",
        _ => return None,
    })
}

/// The position to save for a window: its tile, else maximized/fullscreen.
pub fn position_for(tile: Option<u32>, flags: u32) -> String {
    if let Some(p) = tile.and_then(tile_position) {
        return p.into();
    }
    if flags & FULLSCREEN != 0 {
        "fullscreen".into()
    } else if flags & MAXIMIZED != 0 {
        "maximize".into()
    } else {
        String::new()
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("umoja").join("setups.json")
}

pub fn load() -> Vec<Setup> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store(setups: &[Setup]) -> Result<(), String> {
    let p = path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(setups).map_err(|e| e.to_string())?;
    std::fs::write(&p, text).map_err(|e| e.to_string())
}

/// The setup called `name` (case and spacing don't matter).
pub fn find<'a>(setups: &'a [Setup], name: &str) -> Option<&'a Setup> {
    let key = |s: &str| s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let want = key(name);
    setups
        .iter()
        .find(|s| key(&s.name) == want)
        .or_else(|| setups.iter().find(|s| key(&s.name).contains(&want) && !want.is_empty()))
}

/// Add or replace `setup`.
pub fn upsert(mut setups: Vec<Setup>, setup: Setup) -> Vec<Setup> {
    let lower = setup.name.to_lowercase();
    setups.retain(|s| s.name.to_lowercase() != lower);
    setups.push(setup);
    setups
}

/// The desktop id for a window's app id.
fn desktop_id(app_id: &str) -> Option<String> {
    umoja_common::apps::app_for_window(app_id)
        .and_then(|a| a.id())
        .map(|s| s.to_string())
}

/// Whether window `w` belongs to the app `place` names.
fn is_app(w: &Win, app: &str) -> bool {
    w.app_id == app
        || desktop_id(&w.app_id).as_deref() == Some(app)
        || app.trim_end_matches(".desktop").eq_ignore_ascii_case(&w.app_id)
}

async fn tiles() -> Vec<(u64, u32)> {
    let Ok(c) = desktop::session_bus().await else { return Vec::new() };
    c.call_future(
        Some(umoja_common::dbus::COMPOSITOR_BUS_NAME),
        umoja_common::dbus::COMPOSITOR_OBJECT_PATH,
        umoja_common::dbus::COMPOSITOR_INTERFACE,
        "GetWindowTiles",
        None,
        Some(glib::VariantTy::new("(a(tu))").unwrap()),
        gio::DBusCallFlags::NONE,
        2000,
    )
    .await
    .ok()
    .and_then(|v| v.get::<(Vec<(u64, u32)>,)>())
    .map(|(t,)| t)
    .unwrap_or_default()
}

async fn save_setup(a: Value) -> Outcome {
    let Some(name) = arg_str(&a, "name") else { return Outcome::err("name is required") };
    let only_current = arg_str(&a, "workspaces") == Some("current");
    let wins = match desktop::windows().await {
        Ok(w) => w,
        Err(e) => return Outcome::err(e),
    };
    let (cur, _) = desktop::workspaces().await.unwrap_or((0, 1));
    let tiles = tiles().await;
    let mut places = Vec::new();
    // Oldest first, so they open in the order they were used.
    for w in wins.iter().rev() {
        let Some(ws) = desktop::window_workspace(w.id).await else { continue };
        if only_current && ws != cur {
            continue;
        }
        let tile = tiles.iter().find(|(id, _)| *id == w.id).map(|(_, t)| *t);
        let app = desktop_id(&w.app_id).unwrap_or_else(|| w.app_id.clone());
        let label = umoja_common::apps::app_for_window(&w.app_id)
            .map(|a| a.display_name().to_string())
            .unwrap_or_else(|| w.app_id.clone());
        places.push(Place {
            app,
            name: label,
            workspace: ws + 1,
            position: position_for(tile, w.flags),
        });
    }
    if places.is_empty() {
        return Outcome::err("no windows are open to save");
    }
    let n = places.len();
    let setup = Setup { name: name.to_string(), windows: places };
    match store(&upsert(load(), setup)) {
        Ok(()) => Outcome::ok(format!("Saved “{name}” with {n} window(s)")),
        Err(e) => Outcome::err(e),
    }
}

async fn define_setup(a: Value) -> Outcome {
    let Some(name) = arg_str(&a, "name") else { return Outcome::err("name is required") };
    let Some(list) = a.get("windows").and_then(Value::as_array) else {
        return Outcome::err("windows is required");
    };
    let mut places = Vec::new();
    for w in list {
        let Some(app) = arg_str(w, "app") else { continue };
        let Some(info) = crate::apps::find_launcher(app) else {
            return Outcome::err(format!("no installed app matches \"{app}\""));
        };
        let position = arg_str(w, "position").unwrap_or("").to_string();
        if !position.is_empty() && !desktop::is_position(&position) {
            return Outcome::err(format!("unknown position \"{position}\""));
        }
        places.push(Place {
            app: info.id().map(|s| s.to_string()).unwrap_or_else(|| app.to_string()),
            name: info.display_name().to_string(),
            workspace: crate::tool::arg_u64(w, "workspace").unwrap_or(1).clamp(1, 16) as u32,
            position,
        });
    }
    if places.is_empty() {
        return Outcome::err("windows is empty");
    }
    let summary = describe(&places);
    match store(&upsert(load(), Setup { name: name.to_string(), windows: places })) {
        Ok(()) => Outcome::ok(format!("Saved “{name}”: {summary}")),
        Err(e) => Outcome::err(e),
    }
}

fn describe(places: &[Place]) -> String {
    places
        .iter()
        .map(|p| {
            let at = if p.position.is_empty() { String::new() } else { format!(" {}", p.position.replace('_', " ")) };
            format!("{}{at} on workspace {}", p.name, p.workspace)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn launch(app: &str) -> Result<(), String> {
    let ctx = umoja_common::app_scope::launch_context();
    if let Some(info) = gio::DesktopAppInfo::new(app) {
        return info.launch(&[], Some(&ctx)).map_err(|e| e.to_string());
    }
    match crate::apps::find_launcher(app.trim_end_matches(".desktop")) {
        Some(info) => info.launch(&[], Some(&ctx)).map_err(|e| e.to_string()),
        None => Err(format!("{app} isn't installed")),
    }
}

async fn open_setup(a: Value) -> Outcome {
    let Some(name) = arg_str(&a, "name") else { return Outcome::err("name is required") };
    let setups = load();
    let Some(setup) = find(&setups, name).cloned() else {
        let names: Vec<&str> = setups.iter().map(|s| s.name.as_str()).collect();
        return Outcome::err(if names.is_empty() {
            "there are no saved setups yet".to_string()
        } else {
            format!("no setup called “{name}”; saved: {}", names.join(", "))
        });
    };
    // Match running windows first; launch the rest and wait for them.
    let mut used: Vec<u64> = Vec::new();
    let mut matched: Vec<Option<u64>> = vec![None; setup.windows.len()];
    let mut notes = Vec::new();
    let assign = |wins: &[Win], used: &mut Vec<u64>, matched: &mut Vec<Option<u64>>| {
        for (i, p) in setup.windows.iter().enumerate() {
            if matched[i].is_some() {
                continue;
            }
            if let Some(w) = wins.iter().find(|w| !used.contains(&w.id) && is_app(w, &p.app)) {
                used.push(w.id);
                matched[i] = Some(w.id);
            }
        }
    };
    let wins = desktop::windows().await.unwrap_or_default();
    assign(&wins, &mut used, &mut matched);
    for (i, p) in setup.windows.iter().enumerate() {
        if matched[i].is_none()
            && let Err(e) = launch(&p.app)
        {
            notes.push(format!("{}: {e}", p.name));
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while matched.iter().any(Option::is_none) && std::time::Instant::now() < deadline {
        glib::timeout_future(Duration::from_millis(300)).await;
        let wins = desktop::windows().await.unwrap_or_default();
        assign(&wins, &mut used, &mut matched);
    }
    // Give new windows a moment to map before moving them.
    glib::timeout_future(Duration::from_millis(400)).await;
    for (i, p) in setup.windows.iter().enumerate() {
        let Some(id) = matched[i] else {
            notes.push(format!("{} didn't open", p.name));
            continue;
        };
        let count = desktop::workspaces().await.map(|(_, n)| n).unwrap_or(1);
        let ws = (p.workspace - 1).min(count.saturating_sub(1));
        let _ = desktop::send_to_workspace(id, ws).await;
        if !p.position.is_empty() {
            let _ = desktop::tile(id, &p.position).await;
        }
    }
    if let Some(first) = setup.windows.first() {
        let count = desktop::workspaces().await.map(|(_, n)| n).unwrap_or(1);
        let _ = desktop::show_workspace((first.workspace - 1).min(count.saturating_sub(1))).await;
    }
    let done = format!("Opened “{}”", setup.name);
    Outcome::ok(if notes.is_empty() { done } else { format!("{done} ({})", notes.join("; ")) }).hide()
}

fn list_setups() -> Outcome {
    let setups = load();
    if setups.is_empty() {
        return Outcome::ok("No setups saved yet.");
    }
    Outcome::ok(
        setups
            .iter()
            .map(|s| format!("- {}: {}", s.name, describe(&s.windows)))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn delete_setup(a: &Value) -> Outcome {
    let Some(name) = arg_str(a, "name") else { return Outcome::err("name is required") };
    let setups = load();
    let Some(found) = find(&setups, name).map(|s| s.name.clone()) else {
        return Outcome::err(format!("no setup called “{name}”"));
    };
    let rest: Vec<Setup> = setups.into_iter().filter(|s| s.name != found).collect();
    match store(&rest) {
        Ok(()) => Outcome::ok(format!("Deleted “{found}”")),
        Err(e) => Outcome::err(e),
    }
}

pub fn tools() -> Vec<Tool> {
    let name = json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]});
    vec![
        Tool::new(
            "save_setup",
            "Save the windows open now (each app, its workspace and where it is: left half, quarter, maximized) as a named setup the user can bring back later, e.g. \"save this as Coding\". workspaces: \"all\" (default) or \"current\".",
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "workspaces": {"type": "string", "enum": ["all", "current"]}
            }, "required": ["name"]}),
            |a| Box::pin(save_setup(a)),
        )
        .with_summary(|a| format!("Save this setup as “{}”", arg_str(a, "name").unwrap_or("?"))),
        Tool::new(
            "define_setup",
            "Create a named setup from a description without arranging anything now, e.g. \"a Writing setup with the text editor on the left and Firefox on the right\". windows lists the apps with workspace (1 = first) and position (left, right, top_left, top_right, bottom_left, bottom_right, top, bottom, maximize, or empty).",
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "windows": {"type": "array", "items": {"type": "object", "properties": {
                    "app": {"type": "string"},
                    "workspace": {"type": "integer", "minimum": 1},
                    "position": {"type": "string"}
                }, "required": ["app"]}}
            }, "required": ["name", "windows"]}),
            |a| Box::pin(define_setup(a)),
        )
        .with_summary(|a| format!("Create the setup “{}”", arg_str(a, "name").unwrap_or("?"))),
        Tool::new(
            "open_setup",
            "Bring back a saved setup by name: opens its apps that aren't running and puts every window on its workspace and in its place.",
            name.clone(),
            |a| Box::pin(open_setup(a)),
        )
        .with_summary(|a| format!("Set up “{}”", arg_str(a, "name").unwrap_or("?"))),
        Tool::read_only("list_setups", "List the saved setups and what is in each.", json!({"type": "object", "properties": {}}), |_| {
            Box::pin(async { list_setups() })
        })
        .with_summary(|_| "Look at the saved setups".into()),
        Tool::new("delete_setup", "Delete a saved setup.", name, |a| Box::pin(async move { delete_setup(&a) }))
            .confirm()
            .with_summary(|a| format!("Delete the setup “{}”", arg_str(a, "name").unwrap_or("?"))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> Setup {
        Setup { name: name.into(), windows: vec![] }
    }

    #[test]
    fn positions() {
        assert_eq!(position_for(Some(1), 0), "left");
        assert_eq!(position_for(Some(8), MAXIMIZED), "top_right");
        assert_eq!(position_for(None, MAXIMIZED), "maximize");
        assert_eq!(position_for(Some(3), 0), "");
    }

    #[test]
    fn find_and_upsert() {
        let v = upsert(vec![setup("Coding"), setup("Writing")], setup("coding"));
        assert_eq!(v.len(), 2);
        assert_eq!(v.last().unwrap().name, "coding");
        assert_eq!(find(&v, "  WRITING ").unwrap().name, "Writing");
        assert_eq!(find(&v, "writ").unwrap().name, "Writing");
        assert!(find(&v, "gaming").is_none());
    }

    #[test]
    fn json_shape() {
        let s = Setup {
            name: "Coding".into(),
            windows: vec![Place { app: "a.desktop".into(), name: "A".into(), workspace: 2, position: "left".into() }],
        };
        let back: Setup = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
