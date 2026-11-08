//! Desktop actions for the assistant: apps, Settings panels, Dark Style,
//! Power Mode, Wi-Fi, Bluetooth, windows and workspaces, and (always
//! confirmed) shell commands.

use super::tools::{Risk, Run, ToolSpec, arg_bool, arg_str, arg_u64};
use gio::prelude::*;
use gtk4::prelude::DisplayExt;
use gtk4::glib;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use umoja_common::dbus;
use umoja_common::gsettings as gs;
use umoja_common::types::AssistantAction;

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "launch_app",
            "Open an installed application by name (e.g. \"Firefox\", \"Files\", \"Terminal\", \"text editor\").",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "App name, or what it's for"}
            }, "required": ["name"]}),
            Run::Main(|a| Box::pin(launch_app(a))),
        )
        .with_summary(|a| format!("Open {}", arg_str(a, "name").unwrap_or("app"))),
        ToolSpec::new(
            "open_settings",
            "Open a page of GNOME Settings. Panels: applications, background, bluetooth, color, datetime, display, keyboard, mouse, multitasking, network, notifications, online-accounts, power, printers, privacy, region, search, sharing, sound, system, ubuntu, universal-access, users, wacom, wifi, wwan.",
            json!({"type": "object", "properties": {
                "panel": {"type": "string"},
                "subpage": {"type": "string", "description": "Optional, e.g. \"vpn\" for network"}
            }, "required": ["panel"]}),
            Run::Main(|a| Box::pin(open_settings(a))),
        )
        .with_summary(|a| format!("Open Settings › {}", arg_str(a, "panel").unwrap_or(""))),
        ToolSpec::new(
            "desktop_status",
            "Read the current Dark Style, Power Mode, Wi-Fi, Bluetooth and workspace state.",
            json!({"type": "object", "properties": {}}),
            Run::Main(|_| Box::pin(desktop_status())),
        )
        .with_summary(|_| "Check the desktop's status".into()),
        ToolSpec::new(
            "set_dark_style",
            "Turn Dark Style (dark mode) on or off.",
            json!({"type": "object", "properties": {"enabled": {"type": "boolean"}}, "required": ["enabled"]}),
            Run::Main(|a| Box::pin(async move { set_dark_style(&a) })),
        )
        .with_summary(|a| on_off("Dark Style", a)),
        ToolSpec::new(
            "set_power_mode",
            "Set the Power Mode: \"power-saver\", \"balanced\" or \"performance\".",
            json!({"type": "object", "properties": {
                "mode": {"type": "string", "enum": ["power-saver", "balanced", "performance"]}
            }, "required": ["mode"]}),
            Run::Main(|a| Box::pin(set_power_mode(a))),
        )
        .with_summary(|a| {
            format!(
                "Set Power Mode to {}",
                profile_label(arg_str(a, "mode").unwrap_or(""))
            )
        }),
        ToolSpec::new(
            "set_wifi",
            "Turn Wi-Fi on or off.",
            json!({"type": "object", "properties": {"enabled": {"type": "boolean"}}, "required": ["enabled"]}),
            Run::Main(|a| Box::pin(set_wifi(a))),
        )
        .with_summary(|a| on_off("Wi-Fi", a)),
        ToolSpec::new(
            "set_bluetooth",
            "Turn Bluetooth on or off.",
            json!({"type": "object", "properties": {"enabled": {"type": "boolean"}}, "required": ["enabled"]}),
            Run::Main(|a| Box::pin(set_bluetooth(a))),
        )
        .with_summary(|a| on_off("Bluetooth", a)),
        ToolSpec::new(
            "list_windows",
            "List open windows (most recently used first) with their ids, and the workspaces.",
            json!({"type": "object", "properties": {}}),
            Run::Main(|_| Box::pin(list_windows())),
        )
        .with_summary(|_| "Look at the open windows".into()),
        ToolSpec::new(
            "focus_window",
            "Bring a window to the front.",
            window_schema(json!({})),
            Run::Main(|a| Box::pin(focus_window(a))),
        )
        .with_summary(|a| format!("Switch to {}", window_name(a))),
        ToolSpec::new(
            "arrange_window",
            "Put a window somewhere: the left or right half of the screen, a quarter, maximized, restored, minimized, fullscreen, or another monitor. Use this directly for requests like \"put the terminal on the left\" (pass the app name as window); no need to focus it first.",
            window_schema(json!({"position": {"type": "string", "enum": POSITIONS.iter().map(|(n, _)| *n).collect::<Vec<_>>()}})),
            Run::Main(|a| Box::pin(arrange_window(a))),
        )
        .with_summary(|a| {
            format!(
                "Move {} to {}",
                window_name(a),
                arg_str(a, "position").unwrap_or("?").replace('_', " ")
            )
        }),
        ToolSpec::new(
            "switch_workspace",
            "Switch to a workspace by number (1 is the first).",
            json!({"type": "object", "properties": {"number": {"type": "integer", "minimum": 1}}, "required": ["number"]}),
            Run::Main(|a| Box::pin(switch_workspace(a))),
        )
        .with_summary(|a| format!("Switch to workspace {}", arg_u64(a, "number").unwrap_or(1))),
        ToolSpec::new(
            "move_window_to_workspace",
            "Move a window to a workspace by number (1 is the first) and follow it.",
            window_schema(json!({"number": {"type": "integer", "minimum": 1}})),
            Run::Main(|a| Box::pin(move_to_workspace(a))),
        )
        .with_summary(|a| {
            format!(
                "Move {} to workspace {}",
                window_name(a),
                arg_u64(a, "number").unwrap_or(1)
            )
        }),
        ToolSpec::new(
            "close_window",
            "Close a window. Unsaved work may be lost, so the user is asked first.",
            window_schema(json!({})),
            Run::Main(|a| Box::pin(close_window(a))),
        )
        .confirm()
        .with_summary(|a| format!("Close {}", window_name(a))),
        ToolSpec::new(
            "run_command",
            "Run a shell command as the user and get its output. Only when no other tool fits. The user is always asked first; commands using sudo or package managers open in a terminal.",
            json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
            Run::Main(|a| Box::pin(run_command(a))),
        )
        .with_risk(|a| {
            if crate::view::needs_terminal(arg_str(a, "command").unwrap_or("")) {
                Risk::High
            } else {
                Risk::Medium
            }
        })
        .with_summary(|a| format!("Run “{}”", arg_str(a, "command").unwrap_or(""))),
    ]
}

fn on_off(what: &str, a: &Value) -> String {
    match arg_bool(a, "enabled") {
        Some(false) => format!("Turn off {what}"),
        _ => format!("Turn on {what}"),
    }
}

fn err(msg: impl std::fmt::Display) -> String {
    format!("error: {msg}")
}

// ---- apps -------------------------------------------------------------------

/// Score how well an app matches `q` (lower is better).
fn app_score(q: &str, name: &str, id: &str, extra: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let id = id.to_lowercase();
    if name == q || id.trim_end_matches(".desktop") == q {
        Some(0)
    } else if name.starts_with(q) {
        Some(1)
    } else if name.contains(q) || id.contains(q) {
        Some(2)
    } else if q.split_whitespace().all(|w| extra.contains(w)) {
        Some(3)
    } else {
        None
    }
}

fn find_app(query: &str) -> Option<gio::AppInfo> {
    let q = query.trim().to_lowercase();
    let mut best: Option<(u8, gio::AppInfo)> = None;
    for app in gio::AppInfo::all().into_iter().filter(|a| a.should_show()) {
        let id = app.id().map(|s| s.to_string()).unwrap_or_default();
        let mut extra = app
            .description()
            .map(|d| d.to_lowercase())
            .unwrap_or_default();
        if let Some(d) = app.downcast_ref::<gio::DesktopAppInfo>() {
            for k in d.keywords() {
                extra.push(' ');
                extra.push_str(&k.to_lowercase());
            }
            if let Some(g) = d.generic_name() {
                extra.push(' ');
                extra.push_str(&g.to_lowercase());
            }
        }
        if let Some(s) = app_score(&q, &app.display_name(), &id, &extra)
            && best.as_ref().is_none_or(|(b, _)| s < *b)
        {
            best = Some((s, app));
        }
    }
    best.map(|(_, a)| a)
}

async fn launch_app(a: Value) -> String {
    let Some(name) = arg_str(&a, "name") else {
        return err("name is required");
    };
    let Some(app) = find_app(name) else {
        return err(format!("no installed app matches \"{name}\""));
    };
    let ctx = gtk4::gdk::Display::default().map(|d| d.app_launch_context());
    match app.launch(&[], ctx.as_ref()) {
        Ok(()) => {
            super::hide_when_done();
            format!("Opened {}", app.display_name())
        }
        Err(e) => err(e),
    }
}

async fn open_settings(a: Value) -> String {
    let panel = arg_str(&a, "panel").unwrap_or("").to_string();
    let subpage = arg_str(&a, "subpage").map(String::from);
    let action = AssistantAction::OpenSettings {
        panel: panel.clone(),
        subpage: subpage.clone(),
    };
    if !action.is_valid() {
        return err(format!("unknown Settings panel \"{panel}\""));
    }
    let mut argv = vec!["gnome-control-center".to_string(), panel.clone()];
    argv.extend(subpage);
    match std::process::Command::new(&argv[0]).args(&argv[1..]).spawn() {
        Ok(_) => {
            super::hide_when_done();
            format!("Opened Settings › {panel}")
        }
        Err(e) => err(e),
    }
}

// ---- Dark Style -------------------------------------------------------------

/// Yaru theme name for `dark`, keeping the accent colour
/// ("Yaru-blue" ⇄ "Yaru-blue-dark"). Non-Yaru themes are left alone.
fn yaru_theme(current: &str, dark: bool) -> Option<String> {
    if !current.starts_with("Yaru") {
        return None;
    }
    let base = current.strip_suffix("-dark").unwrap_or(current);
    Some(if dark {
        format!("{base}-dark")
    } else {
        base.to_string()
    })
}

fn set_dark_style(a: &Value) -> String {
    let Some(on) = arg_bool(a, "enabled") else {
        return err("enabled must be true or false");
    };
    let Some(s) = gs::settings(gs::DESKTOP_INTERFACE) else {
        return err("org.gnome.desktop.interface is not installed");
    };
    let _ = s.set_string("color-scheme", if on { "prefer-dark" } else { "default" });
    // Like gnome-shell's tile (and Ubuntu's Appearance panel), switch Yaru too.
    if let Some(theme) = yaru_theme(&s.string("gtk-theme"), on) {
        let _ = s.set_string("gtk-theme", &theme);
    }
    gio::Settings::sync();
    format!("Dark Style is now {}", if on { "on" } else { "off" })
}

// ---- system bus helpers ------------------------------------------------------

async fn system_bus() -> Result<gio::DBusConnection, String> {
    gio::bus_get_future(gio::BusType::System)
        .await
        .map_err(|e| e.to_string())
}

async fn session_bus() -> Result<gio::DBusConnection, String> {
    gio::bus_get_future(gio::BusType::Session)
        .await
        .map_err(|e| e.to_string())
}

async fn get_prop(
    c: &gio::DBusConnection,
    bus: &str,
    path: &str,
    iface: &str,
    prop: &str,
) -> Result<glib::Variant, String> {
    let v = c
        .call_future(
            Some(bus),
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(iface, prop).to_variant()),
            Some(glib::VariantTy::new("(v)").unwrap()),
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(v.child_value(0).as_variant().unwrap_or(v))
}

async fn set_prop(
    c: &gio::DBusConnection,
    bus: &str,
    path: &str,
    iface: &str,
    prop: &str,
    value: glib::Variant,
) -> Result<(), String> {
    c.call_future(
        Some(bus),
        path,
        "org.freedesktop.DBus.Properties",
        "Set",
        Some(&(iface, prop, value).to_variant()),
        None,
        gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
        10_000,
    )
    .await
    .map(|_| ())
    .map_err(|e| {
        let e = e.to_string();
        // "GDBus.Error:org.bluez.Error.Blocked: ..." → the readable part.
        e.rsplit(": ").next().unwrap_or(&e).to_string()
    })
}

// ---- Power Mode ----------------------------------------------------------------

/// power-profiles-daemon renamed its bus name in 0.20; try the new one first.
const POWER_PROFILES: [(&str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];

fn profile_label(p: &str) -> &str {
    match p {
        "power-saver" => "Power Saver",
        "performance" => "Performance",
        "balanced" => "Balanced",
        other => other,
    }
}

async fn power_mode(c: &gio::DBusConnection) -> Result<(usize, String), String> {
    let mut last = "power-profiles-daemon is not running".to_string();
    for (i, (bus, path)) in POWER_PROFILES.iter().enumerate() {
        match get_prop(c, bus, path, bus, "ActiveProfile").await {
            Ok(v) => return Ok((i, v.str().unwrap_or("").to_string())),
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn set_power_mode(a: Value) -> String {
    let mode = arg_str(&a, "mode").unwrap_or("").to_lowercase().replace([' ', '_'], "-");
    let mode = match mode.as_str() {
        "power-saver" | "powersaver" | "saver" | "battery" | "low-power" => "power-saver",
        "balanced" | "normal" | "default" => "balanced",
        "performance" | "high" | "fast" => "performance",
        _ => return err("mode must be power-saver, balanced or performance"),
    };
    let c = match system_bus().await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let (i, _) = match power_mode(&c).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let (bus, path) = POWER_PROFILES[i];
    match set_prop(&c, bus, path, bus, "ActiveProfile", mode.to_variant()).await {
        Ok(()) => format!("Power Mode is now {}", profile_label(mode)),
        Err(e) => err(e),
    }
}

// ---- Wi-Fi ----------------------------------------------------------------------

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";

async fn set_wifi(a: Value) -> String {
    let Some(on) = arg_bool(&a, "enabled") else {
        return err("enabled must be true or false");
    };
    let c = match system_bus().await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    match set_prop(&c, NM, NM_PATH, NM, "WirelessEnabled", on.to_variant()).await {
        Ok(()) => format!("Wi-Fi is now {}", if on { "on" } else { "off" }),
        Err(e) => err(e),
    }
}

// ---- Bluetooth --------------------------------------------------------------------

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";

async fn bt_adapter(c: &gio::DBusConnection) -> Result<(String, bool), String> {
    let v = c
        .call_future(
            Some(BLUEZ),
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            None,
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await
        .map_err(|_| "Bluetooth is not available (bluetoothd isn't running)".to_string())?;
    for obj in v.child_value(0).iter() {
        let path = obj.child_value(0).str().unwrap_or("").to_string();
        for iface in obj.child_value(1).iter() {
            if iface.child_value(0).str() != Some(ADAPTER) {
                continue;
            }
            let props = iface.child_value(1);
            let powered = glib::VariantDict::new(Some(&props))
                .lookup_value("Powered", None)
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            return Ok((path, powered));
        }
    }
    Err("this computer has no Bluetooth adapter".into())
}

async fn set_bluetooth(a: Value) -> String {
    let Some(on) = arg_bool(&a, "enabled") else {
        return err("enabled must be true or false");
    };
    let c = match system_bus().await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let (path, _) = match bt_adapter(&c).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let mut r = set_prop(&c, BLUEZ, &path, ADAPTER, "Powered", on.to_variant()).await;
    // Soft-blocked by rfkill (airplane mode): unblock, then power on.
    if on && r.as_ref().is_err_and(|e| e.contains("Blocked") || e.contains("rfkill")) {
        let _ = gio::Subprocess::newv(
            &["rfkill".as_ref(), "unblock".as_ref(), "bluetooth".as_ref()],
            gio::SubprocessFlags::NONE,
        )
        .map(|p| p.wait_future());
        glib::timeout_future(std::time::Duration::from_millis(800)).await;
        r = set_prop(&c, BLUEZ, &path, ADAPTER, "Powered", on.to_variant()).await;
    }
    match r {
        Ok(()) => format!("Bluetooth is now {}", if on { "on" } else { "off" }),
        Err(e) => err(e),
    }
}

// ---- status -----------------------------------------------------------------------

async fn desktop_status() -> String {
    let mut lines = vec![format!(
        "Dark Style: {}",
        if gs::prefers_dark() { "on" } else { "off" }
    )];
    if let Ok(c) = system_bus().await {
        lines.push(match power_mode(&c).await {
            Ok((_, p)) => format!("Power Mode: {}", profile_label(&p)),
            Err(e) => format!("Power Mode: unavailable ({e})"),
        });
        lines.push(
            match get_prop(&c, NM, NM_PATH, NM, "WirelessEnabled").await {
                Ok(v) => format!(
                    "Wi-Fi: {}",
                    if v.get::<bool>().unwrap_or(false) { "on" } else { "off" }
                ),
                Err(_) => "Wi-Fi: unavailable".into(),
            },
        );
        lines.push(match bt_adapter(&c).await {
            Ok((_, p)) => format!("Bluetooth: {}", if p { "on" } else { "off" }),
            Err(e) => format!("Bluetooth: {e}"),
        });
    }
    if let Some((cur, n)) = workspaces().await {
        lines.push(format!("Workspace: {} of {n}", cur + 1));
    }
    lines.join("\n")
}

// ---- windows ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Win {
    id: u64,
    app_id: String,
    title: String,
    flags: u32,
}

const FOCUSED: u32 = 1;
const MINIMIZED: u32 = 2;
const CURRENT_WORKSPACE: u32 = 4;
const MAXIMIZED: u32 = 8;
const FULLSCREEN: u32 = 16;
const TILED: u32 = 32;

thread_local! {
    /// Short numbers handed to the model by list_windows → compositor ids.
    /// Compositor ids are pointers, too long for a small model to copy.
    static SHORT_IDS: RefCell<HashMap<u64, u64>> = RefCell::new(HashMap::new());
}

fn short_id(id: u64) -> u64 {
    SHORT_IDS.with(|m| {
        let mut m = m.borrow_mut();
        if let Some((k, _)) = m.iter().find(|(_, v)| **v == id) {
            return *k;
        }
        let k = m.keys().max().copied().unwrap_or(0) + 1;
        m.insert(k, id);
        k
    })
}

fn window_schema(extra: Value) -> Value {
    let mut props = json!({
        "window": {"type": "string", "description": "Window number from list_windows, or part of the app name or title (e.g. \"firefox\"). Omit for the focused window."}
    });
    if let (Some(p), Value::Object(e)) = (props.as_object_mut(), extra) {
        p.extend(e);
    }
    let required: Vec<&str> = props
        .as_object()
        .map(|p| p.keys().filter(|k| *k != "window").map(String::as_str).collect())
        .unwrap_or_default();
    json!({"type": "object", "properties": props, "required": required})
}

fn window_name(a: &Value) -> String {
    match a.get("window") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(Value::Number(n)) => format!("window {n}"),
        _ => "the focused window".into(),
    }
}

async fn compositor_call(method: &str, args: Option<glib::Variant>, reply: Option<&str>) -> Result<glib::Variant, String> {
    let c = session_bus().await?;
    c.call_future(
        Some(dbus::COMPOSITOR_BUS_NAME),
        dbus::COMPOSITOR_OBJECT_PATH,
        dbus::COMPOSITOR_INTERFACE,
        method,
        args.as_ref(),
        reply.map(|r| glib::VariantTy::new(r).unwrap()),
        gio::DBusCallFlags::NONE,
        2000,
    )
    .await
    .map_err(|e| {
        tracing::warn!(method, error = %e, "compositor call failed");
        "the window manager didn't respond".to_string()
    })
}

async fn windows() -> Result<Vec<Win>, String> {
    let v = compositor_call("GetWindowsMru", None, Some("(a(tssiiiiu))")).await?;
    let (rows,) = v
        .get::<(Vec<(u64, String, String, i32, i32, i32, i32, u32)>,)>()
        .unwrap_or_default();
    Ok(rows
        .into_iter()
        .map(|(id, app_id, title, _, _, _, _, flags)| Win {
            id,
            app_id,
            title,
            flags,
        })
        .collect())
}

async fn workspaces() -> Option<(u32, u32)> {
    let v = compositor_call("GetWorkspaces", None, Some("(uu)")).await.ok()?;
    v.get::<(u32, u32)>().filter(|(_, n)| *n > 0)
}

fn describe_window(w: &Win) -> String {
    let mut state = Vec::new();
    for (bit, name) in [
        (FOCUSED, "focused"),
        (MINIMIZED, "minimized"),
        (MAXIMIZED, "maximized"),
        (FULLSCREEN, "fullscreen"),
        (TILED, "tiled"),
    ] {
        if w.flags & bit != 0 {
            state.push(name);
        }
    }
    if w.flags & CURRENT_WORKSPACE == 0 {
        state.push("on another workspace");
    }
    let app = w.app_id.trim_end_matches(".desktop");
    let mut s = format!("{}. {} — {}", short_id(w.id), app, w.title);
    if !state.is_empty() {
        s.push_str(&format!(" ({})", state.join(", ")));
    }
    s
}

async fn list_windows() -> String {
    let wins = match windows().await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    let mut out: Vec<String> = wins.iter().map(describe_window).collect();
    if out.is_empty() {
        out.push("No windows are open.".into());
    }
    if let Some((cur, n)) = workspaces().await {
        out.push(format!("You are on workspace {} of {n}.", cur + 1));
    }
    out.join("\n")
}

/// Which window a tool call means: a number from list_windows, a piece of
/// an app id or title, or (nothing) the focused one.
fn pick_window<'a>(wins: &'a [Win], a: &Value) -> Option<&'a Win> {
    let want = match a.get("window") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    let want = want.trim_start_matches('#').trim();
    if want.is_empty() || want.eq_ignore_ascii_case("focused") || want.eq_ignore_ascii_case("current") {
        return wins.iter().find(|w| w.flags & FOCUSED != 0).or(wins.first());
    }
    if let Ok(n) = want.parse::<u64>()
        && let Some(id) = SHORT_IDS.with(|m| m.borrow().get(&n).copied())
    {
        return wins.iter().find(|w| w.id == id);
    }
    let q = want.to_lowercase();
    // MRU order, so "the terminal" means the one used last.
    wins.iter()
        .find(|w| w.app_id.to_lowercase().contains(&q))
        .or_else(|| wins.iter().find(|w| w.title.to_lowercase().contains(&q)))
}

async fn with_window(a: &Value) -> Result<Win, String> {
    let wins = windows().await?;
    if wins.is_empty() {
        return Err("no windows are open".into());
    }
    pick_window(&wins, a)
        .cloned()
        .ok_or_else(|| format!("no window matches \"{}\"; call list_windows", window_name(a)))
}

fn label(w: &Win) -> String {
    if w.title.is_empty() {
        w.app_id.clone()
    } else {
        w.title.clone()
    }
}

async fn focus_window(a: Value) -> String {
    let w = match with_window(&a).await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    match compositor_call("ActivateWindow", Some((w.id,).to_variant()), None).await {
        Ok(_) => {
            super::hide_when_done();
            format!("Switched to {}", label(&w))
        }
        Err(e) => err(e),
    }
}

/// `arrange_window` positions → `tiling::Action` numbers in umoja-mir.
const POSITIONS: &[(&str, u32)] = &[
    ("left", 1),
    ("right", 2),
    ("maximize", 3),
    ("restore", 4),
    ("minimize", 5),
    ("top_left", 7),
    ("top_right", 8),
    ("bottom_left", 9),
    ("bottom_right", 10),
    ("fullscreen", 14),
    ("monitor_left", 21),
    ("monitor_right", 22),
    ("monitor_up", 23),
    ("monitor_down", 24),
    ("top", 28),
    ("bottom", 29),
];

fn position_code(p: &str) -> Option<u32> {
    let p = p.trim().to_lowercase().replace([' ', '-'], "_");
    let p = match p.as_str() {
        "left_half" | "tile_left" => "left",
        "right_half" | "tile_right" => "right",
        "maximized" | "max" | "full" => "maximize",
        "unmaximize" | "normal" | "untile" => "restore",
        "minimized" | "hide" => "minimize",
        "top_half" => "top",
        "bottom_half" => "bottom",
        other => other,
    };
    POSITIONS.iter().find(|(n, _)| *n == p).map(|(_, c)| *c)
}

async fn arrange_window(a: Value) -> String {
    let Some(code) = arg_str(&a, "position").and_then(position_code) else {
        return err("unknown position");
    };
    let w = match with_window(&a).await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    match compositor_call("TileWindow", Some((w.id, code).to_variant()), None).await {
        Ok(_) => {
            super::hide_when_done();
            format!("Moved {} ({})", label(&w), arg_str(&a, "position").unwrap_or(""))
        }
        Err(e) => err(e),
    }
}

async fn close_window(a: Value) -> String {
    let w = match with_window(&a).await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    match compositor_call("CloseWindow", Some((w.id,).to_variant()), None).await {
        Ok(_) => format!("Closed {}", label(&w)),
        Err(e) => err(e),
    }
}

/// 1-based workspace number from the model → 0-based index, clamped.
async fn workspace_index(a: &Value) -> Result<u32, String> {
    let n = arg_u64(a, "number").ok_or("number is required")?;
    let (_, count) = workspaces()
        .await
        .ok_or("the window manager didn't respond")?;
    if n == 0 || n > count as u64 {
        return Err(format!("there are {count} workspaces"));
    }
    Ok(n as u32 - 1)
}

async fn switch_workspace(a: Value) -> String {
    let i = match workspace_index(&a).await {
        Ok(i) => i,
        Err(e) => return err(e),
    };
    match compositor_call("SwitchWorkspace", Some((i,).to_variant()), None).await {
        Ok(_) => {
            super::hide_when_done();
            format!("Switched to workspace {}", i + 1)
        }
        Err(e) => err(e),
    }
}

async fn move_to_workspace(a: Value) -> String {
    let w = match with_window(&a).await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    // Moving to the last (empty) workspace is allowed: dynamic workspaces
    // always keep one free at the end.
    let i = match workspace_index(&a).await {
        Ok(i) => i,
        Err(e) => return err(e),
    };
    match compositor_call(
        "MoveWindowToWorkspace",
        Some((w.id, i).to_variant()),
        None,
    )
    .await
    {
        Ok(_) => {
            super::hide_when_done();
            format!("Moved {} to workspace {}", label(&w), i + 1)
        }
        Err(e) => err(e),
    }
}

// ---- commands -------------------------------------------------------------------------

const OUTPUT_LIMIT: usize = 3000;

async fn run_command(a: Value) -> String {
    let Some(cmd) = arg_str(&a, "command").map(String::from) else {
        return err("command is required");
    };
    if crate::view::needs_terminal(&cmd) {
        return match crate::view::spawn_in_terminal(&cmd) {
            Ok(()) => {
                super::hide_when_done();
                "Opened a terminal running the command; the user will see its output there.".into()
            }
            Err(e) => err(e),
        };
    }
    // Isolated in a transient scope that can't gain privileges, and time-limited.
    let argv = [
        "systemd-run",
        "--user",
        "--scope",
        "--collect",
        "--quiet",
        "-p",
        "NoNewPrivileges=yes",
        "--",
        "timeout",
        "20",
        "sh",
        "-c",
        cmd.as_str(),
    ];
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    );
    launcher.set_cwd(glib::home_dir());
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|s| s.as_ref()).collect();
    let proc = match launcher.spawn(&argv) {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    let out = match proc.communicate_utf8_future(None).await {
        Ok((out, _)) => out.map(|s| s.to_string()).unwrap_or_default(),
        Err(e) => return err(e),
    };
    let status = if proc.is_successful() {
        "exit status 0".to_string()
    } else {
        format!("exit status {}", proc.exit_status())
    };
    let mut out = out.trim().to_string();
    if out.len() > OUTPUT_LIMIT {
        let cut = (0..=OUTPUT_LIMIT).rev().find(|i| out.is_char_boundary(*i)).unwrap_or(0);
        out.truncate(cut);
        out.push_str("\n[output truncated]");
    }
    format!("{status}\n{out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_matching() {
        assert_eq!(app_score("firefox", "Firefox", "firefox_firefox.desktop", ""), Some(0));
        assert_eq!(app_score("files", "Files", "org.gnome.Nautilus.desktop", ""), Some(0));
        assert_eq!(app_score("text editor", "Text Editor", "org.gnome.TextEditor.desktop", ""), Some(0));
        assert_eq!(app_score("term", "Terminal", "x.desktop", ""), Some(1));
        assert_eq!(app_score("browser", "Firefox", "firefox.desktop", "web browser internet"), Some(3));
        assert_eq!(app_score("gimp", "Firefox", "firefox.desktop", "web"), None);
    }

    #[test]
    fn yaru_keeps_accent() {
        assert_eq!(yaru_theme("Yaru", true).as_deref(), Some("Yaru-dark"));
        assert_eq!(yaru_theme("Yaru-blue-dark", false).as_deref(), Some("Yaru-blue"));
        assert_eq!(yaru_theme("Yaru-blue", true).as_deref(), Some("Yaru-blue-dark"));
        assert_eq!(yaru_theme("Adwaita", true), None);
    }

    #[test]
    fn positions() {
        assert_eq!(position_code("left"), Some(1));
        assert_eq!(position_code("Left half"), Some(1));
        assert_eq!(position_code("top-right"), Some(8));
        assert_eq!(position_code("maximized"), Some(3));
        assert_eq!(position_code("sideways"), None);
    }

    #[test]
    fn window_picking() {
        let wins = vec![
            Win { id: 900, app_id: "org.gnome.Ptyxis".into(), title: "ken@umoja: ~".into(), flags: CURRENT_WORKSPACE },
            Win { id: 901, app_id: "firefox".into(), title: "Ubuntu — Mozilla Firefox".into(), flags: FOCUSED | CURRENT_WORKSPACE },
        ];
        assert_eq!(pick_window(&wins, &json!({})).unwrap().id, 901);
        assert_eq!(pick_window(&wins, &json!({"window": "ptyxis"})).unwrap().id, 900);
        assert_eq!(pick_window(&wins, &json!({"window": "Mozilla"})).unwrap().id, 901);
        let n = short_id(900);
        assert_eq!(pick_window(&wins, &json!({"window": n})).unwrap().id, 900);
        assert_eq!(pick_window(&wins, &json!({"window": format!("#{n}")})).unwrap().id, 900);
        assert!(pick_window(&wins, &json!({"window": "gimp"})).is_none());
        assert!(describe_window(&wins[1]).contains("(focused)"));
    }

    #[test]
    fn schemas_require_only_real_arguments() {
        let s = window_schema(json!({"number": {"type": "integer"}}));
        assert_eq!(s["required"], json!(["number"]));
        assert_eq!(window_schema(json!({}))["required"], json!([]));
    }
}
