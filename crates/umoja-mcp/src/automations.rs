//! Automations in plain words: "when I plug in the monitor, turn on Dark
//! Style and open Slack".
//!
//! The model turns the sentence into a rule once, with `create_automation`
//! (a trigger plus tool calls, confirmed with the user in plain words);
//! from then on `umoja-mcp --automations` (the `umoja-automations` user
//! service) runs it whenever the trigger happens, with no model involved.
//! Rules live in `~/.config/umoja/automations.json`; the service reloads
//! them when the file changes.
//!
//! Only everyday, undoable tools can be steps ([`ALLOWED`]): nothing that
//! removes, installs, runs commands or ends the session.

use crate::tool::{Outcome, Tool, arg_bool, arg_str};
use gio::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use umoja_common::dbus;

/// Tools a rule may run.
pub const ALLOWED: &[&str] = &[
    "set_dark_style",
    "set_wifi",
    "set_bluetooth",
    "set_do_not_disturb",
    "set_night_light",
    "set_power_mode",
    "set_volume",
    "set_brightness",
    "set_audio_device",
    "set_wallpaper",
    "change_setting",
    "launch_app",
    "open_setup",
    "open_url",
    "open_folder",
    "media_control",
    "switch_workspace",
    "arrange_window",
    "bluetooth_device",
    "show_notification",
];

const DAYS: &[&str] = &["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// Logging in (the service starting).
    Login,
    /// Every day (or on `days`, "mon".."sun") at `at` ("HH:MM", local).
    Time {
        at: String,
        #[serde(default)]
        days: Vec<String>,
    },
    MonitorConnected,
    MonitorDisconnected,
    PluggedIn,
    OnBattery,
    /// Connected to a network (`name`: its name/SSID; empty = any).
    NetworkConnected {
        #[serde(default)]
        name: String,
    },
    NetworkDisconnected,
    /// An app opened its first window (`app`: desktop id).
    AppOpened { app: String },
    /// A Bluetooth device connected (`device`: part of its name; empty = any).
    BluetoothConnected {
        #[serde(default)]
        device: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: u64,
    /// The user's own words.
    pub description: String,
    pub trigger: Trigger,
    pub actions: Vec<Step>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// Something that happened, as the watchers report it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Login,
    /// (hour, minute, weekday 0 = Monday)
    Minute(u32, u32, usize),
    MonitorConnected,
    MonitorDisconnected,
    PluggedIn,
    OnBattery,
    NetworkConnected(String),
    NetworkDisconnected,
    AppOpened(String),
    BluetoothConnected(String),
}

/// "7:05" / "19:30" → (7, 5).
pub fn parse_time(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

pub fn matches(t: &Trigger, e: &Event) -> bool {
    let contains = |hay: &str, needle: &str| needle.is_empty() || hay.to_lowercase().contains(&needle.to_lowercase());
    match (t, e) {
        (Trigger::Login, Event::Login)
        | (Trigger::MonitorConnected, Event::MonitorConnected)
        | (Trigger::MonitorDisconnected, Event::MonitorDisconnected)
        | (Trigger::PluggedIn, Event::PluggedIn)
        | (Trigger::OnBattery, Event::OnBattery)
        | (Trigger::NetworkDisconnected, Event::NetworkDisconnected) => true,
        (Trigger::Time { at, days }, Event::Minute(h, m, wd)) => {
            parse_time(at) == Some((*h, *m)) && (days.is_empty() || days.iter().any(|d| d.to_lowercase().starts_with(DAYS[*wd])))
        }
        (Trigger::NetworkConnected { name }, Event::NetworkConnected(n)) => name.is_empty() || n.eq_ignore_ascii_case(name),
        (Trigger::AppOpened { app }, Event::AppOpened(a)) => a == app || a.trim_end_matches(".desktop") == app.trim_end_matches(".desktop"),
        (Trigger::BluetoothConnected { device }, Event::BluetoothConnected(d)) => contains(d, device),
        _ => false,
    }
}

/// "when I plug in a monitor" etc., for confirmations and lists.
pub fn trigger_label(t: &Trigger) -> String {
    match t {
        Trigger::Login => "when you log in".into(),
        Trigger::Time { at, days } if days.is_empty() => format!("every day at {at}"),
        Trigger::Time { at, days } => format!("at {at} on {}", days.join(", ")),
        Trigger::MonitorConnected => "when a monitor is plugged in".into(),
        Trigger::MonitorDisconnected => "when a monitor is unplugged".into(),
        Trigger::PluggedIn => "when the computer is plugged in".into(),
        Trigger::OnBattery => "when the computer goes on battery".into(),
        Trigger::NetworkConnected { name } if name.is_empty() => "when connecting to a network".into(),
        Trigger::NetworkConnected { name } => format!("when connecting to “{name}”"),
        Trigger::NetworkDisconnected => "when the network disconnects".into(),
        Trigger::AppOpened { app } => format!("when {} opens", app.trim_end_matches(".desktop")),
        Trigger::BluetoothConnected { device } if device.is_empty() => "when a Bluetooth device connects".into(),
        Trigger::BluetoothConnected { device } => format!("when “{device}” connects"),
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("umoja").join("automations.json")
}

pub fn load() -> Vec<Rule> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store(rules: &[Rule]) -> Result<(), String> {
    let p = path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(rules).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Check a rule from the model: a known trigger, allowed tools that exist.
pub fn validate(trigger: &Trigger, actions: &[Step], known: &[&str]) -> Result<(), String> {
    if let Trigger::Time { at, days } = trigger {
        parse_time(at).ok_or("time triggers need at as HH:MM (24-hour)")?;
        if let Some(d) = days.iter().find(|d| !DAYS.iter().any(|k| d.to_lowercase().starts_with(k))) {
            return Err(format!("unknown day “{d}” (use mon, tue, wed, thu, fri, sat, sun)"));
        }
    }
    if actions.is_empty() {
        return Err("actions is empty".into());
    }
    for s in actions {
        if !known.contains(&s.tool.as_str()) {
            return Err(format!("there is no tool {}", s.tool));
        }
        if !ALLOWED.contains(&s.tool.as_str()) {
            return Err(format!("{} can't run in an automation; allowed: {}", s.tool, ALLOWED.join(", ")));
        }
    }
    Ok(())
}

fn summary(rule_trigger: &Trigger, actions: &[Step], tools: &[Tool]) -> String {
    let steps: Vec<String> = actions
        .iter()
        .map(|s| match tools.iter().find(|t| t.name == s.tool) {
            Some(t) => t.describe(&s.arguments),
            None => s.tool.clone(),
        })
        .collect();
    let mut t = trigger_label(rule_trigger);
    if let Some(c) = t.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    format!("{t}: {}", steps.join(", "))
}

fn parse_request(a: &Value) -> Result<(Trigger, Vec<Step>), String> {
    let mut trig = a.get("trigger").cloned().ok_or("trigger is required")?;
    // "app" given by name: store the launcher's id.
    if trig["kind"] == "app_opened"
        && let Some(app) = trig["app"].as_str()
        && let Some(info) = crate::apps::find_launcher(app)
        && let Some(id) = info.id()
    {
        trig["app"] = json!(id.to_string());
    }
    let trigger: Trigger = serde_json::from_value(trig).map_err(|e| format!("bad trigger: {e}"))?;
    let actions: Vec<Step> = serde_json::from_value(a.get("actions").cloned().unwrap_or(json!([])))
        .map_err(|e| format!("bad actions: {e}"))?;
    Ok((trigger, actions))
}

fn create_preflight(a: Value) -> crate::tool::LocalFuture<Result<Option<String>, String>> {
    Box::pin(async move {
        let (trigger, actions) = parse_request(&a)?;
        let tools = crate::registry();
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        validate(&trigger, &actions, &names)?;
        Ok(Some(summary(&trigger, &actions, &tools)))
    })
}

fn create(a: &Value) -> Outcome {
    let (trigger, actions) = match parse_request(a) {
        Ok(v) => v,
        Err(e) => return Outcome::err(e),
    };
    let tools = crate::registry();
    let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    if let Err(e) = validate(&trigger, &actions, &names) {
        return Outcome::err(e);
    }
    let mut rules = load();
    let id = rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
    let text = summary(&trigger, &actions, &tools);
    rules.push(Rule {
        id,
        description: arg_str(a, "description").unwrap_or(&text).to_string(),
        trigger,
        actions,
        enabled: true,
    });
    match store(&rules) {
        Ok(()) => Outcome::ok(format!("Automation {id} saved. {text}")),
        Err(e) => Outcome::err(e),
    }
}

fn list() -> Outcome {
    let rules = load();
    if rules.is_empty() {
        return Outcome::ok("No automations yet.");
    }
    let tools = crate::registry();
    Outcome::ok(
        rules
            .iter()
            .map(|r| {
                format!(
                    "{}. {}{} ({})",
                    r.id,
                    summary(&r.trigger, &r.actions, &tools),
                    if r.enabled { "" } else { " [off]" },
                    r.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn edit(a: &Value, remove: bool) -> Outcome {
    let Some(id) = crate::tool::arg_u64(a, "id") else { return Outcome::err("id is required (see list_automations)") };
    let mut rules = load();
    let Some(i) = rules.iter().position(|r| r.id == id) else { return Outcome::err(format!("there is no automation {id}")) };
    let msg = if remove {
        let r = rules.remove(i);
        format!("Deleted automation {id} ({})", r.description)
    } else {
        let on = arg_bool(a, "enabled").unwrap_or(true);
        rules[i].enabled = on;
        format!("Automation {id} is now {}", if on { "on" } else { "off" })
    };
    match store(&rules) {
        Ok(()) => Outcome::ok(msg),
        Err(e) => Outcome::err(e),
    }
}

async fn notify(title: &str, body: &str) -> Result<(), String> {
    let c = crate::desktop::session_bus().await?;
    let hints = glib::VariantDict::new(None).end();
    c.call_future(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&glib::Variant::tuple_from_iter([
            "Umoja".to_variant(),
            0u32.to_variant(),
            "preferences-system-time-symbolic".to_variant(),
            title.to_variant(),
            body.to_variant(),
            Vec::<String>::new().to_variant(),
            hints,
            (-1i32).to_variant(),
        ])),
        None,
        gio::DBusCallFlags::NONE,
        2000,
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

pub fn tools() -> Vec<Tool> {
    let id = json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]});
    vec![
        Tool::new(
            "create_automation",
            "Make an automation the desktop runs by itself from now on, for requests like \"when I plug in the monitor, turn on dark style and open Slack\" or \"every weekday at 9:00 open my Coding setup\". trigger.kind is one of: login, time (with at \"HH:MM\" 24-hour and optional days [\"mon\"..\"sun\"]), monitor_connected, monitor_disconnected, plugged_in, on_battery, network_connected (optional name), network_disconnected, app_opened (app: its name), bluetooth_connected (optional device name). actions are tool calls ({\"tool\": ..., \"arguments\": {...}}) using only: ".to_string()
                + &ALLOWED.join(", ")
                + ". show_notification works as a reminder. The user is asked to confirm.",
            json!({"type": "object", "properties": {
                "description": {"type": "string", "description": "The user's request in their words"},
                "trigger": {"type": "object", "properties": {"kind": {"type": "string"}}, "required": ["kind"]},
                "actions": {"type": "array", "items": {"type": "object", "properties": {
                    "tool": {"type": "string"}, "arguments": {"type": "object"}
                }, "required": ["tool"]}}
            }, "required": ["trigger", "actions"]}),
            |a| Box::pin(async move { create(&a) }),
        )
        .confirm()
        .with_preflight(create_preflight)
        .with_summary(|_| "Create an automation".into()),
        Tool::read_only("list_automations", "List the automations, with their numbers.", json!({"type": "object", "properties": {}}), |_| {
            Box::pin(async { list() })
        })
        .with_summary(|_| "Look at the automations".into()),
        Tool::new("delete_automation", "Delete an automation by number (from list_automations).", id.clone(), |a| {
            Box::pin(async move { edit(&a, true) })
        })
        .confirm()
        .with_summary(|a| format!("Delete automation {}", crate::tool::arg_u64(a, "id").unwrap_or(0))),
        Tool::new(
            "set_automation_enabled",
            "Turn an automation (by number) off or back on.",
            json!({"type": "object", "properties": {"id": {"type": "integer"}, "enabled": {"type": "boolean"}}, "required": ["id", "enabled"]}),
            |a| Box::pin(async move { edit(&a, false) }),
        )
        .with_summary(|a| format!("Turn automation {} {}", crate::tool::arg_u64(a, "id").unwrap_or(0), if arg_bool(a, "enabled") == Some(false) { "off" } else { "on" })),
        Tool::new(
            "show_notification",
            "Show a notification (a reminder) with a title and optional body.",
            json!({"type": "object", "properties": {"title": {"type": "string"}, "body": {"type": "string"}}, "required": ["title"]}),
            |a| {
                Box::pin(async move {
                    let title = arg_str(&a, "title").unwrap_or("Reminder").to_string();
                    let body = arg_str(&a, "body").unwrap_or("").to_string();
                    notify(&title, &body).await.map(|_| format!("Showed “{title}”")).into()
                })
            },
        )
        .with_summary(|a| format!("Show “{}”", arg_str(a, "title").unwrap_or("a notification"))),
    ]
}

// ---- the service -------------------------------------------------------------------

/// Run every enabled rule that `event` triggers.
async fn fire(event: Event, rules: &Rc<RefCell<Vec<Rule>>>, last: &Rc<RefCell<HashMap<u64, Instant>>>) {
    let due: Vec<Rule> = rules.borrow().iter().filter(|r| r.enabled && matches(&r.trigger, &event)).cloned().collect();
    if due.is_empty() {
        return;
    }
    let tools = crate::registry();
    for rule in due {
        // A flapping trigger (a dock reconnecting) runs a rule once.
        if !matches!(rule.trigger, Trigger::Time { .. })
            && last.borrow().get(&rule.id).is_some_and(|t| t.elapsed() < Duration::from_secs(30))
        {
            continue;
        }
        last.borrow_mut().insert(rule.id, Instant::now());
        tracing::info!(id = rule.id, ?event, "running automation");
        for step in &rule.actions {
            if !ALLOWED.contains(&step.tool.as_str()) {
                continue;
            }
            let Some(tool) = tools.iter().find(|t| t.name == step.tool) else { continue };
            let o = (tool.run)(step.arguments.clone()).await;
            if o.is_error {
                tracing::warn!(id = rule.id, tool = %step.tool, error = %o.text, "automation step failed");
                let _ = notify(&format!("Automation didn't finish: {}", rule.description), o.text.trim_start_matches("error: ")).await;
                break;
            }
        }
    }
}

fn subscribe(
    c: &gio::DBusConnection,
    sender: &str,
    iface: &str,
    member: &str,
    path: Option<&str>,
    arg0: Option<&str>,
    f: impl Fn(&str, &glib::Variant) + 'static,
) {
    let id = c.signal_subscribe(
        Some(sender),
        Some(iface),
        Some(member),
        path,
        arg0,
        gio::DBusSignalFlags::NONE,
        move |_, _, path, _, _, params| f(path, params),
    );
    std::mem::forget(id);
}

async fn monitor_count(c: &gio::DBusConnection) -> Option<usize> {
    let v = c
        .call_future(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
            "GetCurrentState",
            None,
            None,
            gio::DBusCallFlags::NONE,
            3000,
        )
        .await
        .ok()?;
    Some(v.child_value(1).n_children())
}

async fn on_battery(c: &gio::DBusConnection) -> Option<bool> {
    crate::desktop::get_prop(c, "org.freedesktop.UPower", "/org/freedesktop/UPower", "org.freedesktop.UPower", "OnBattery")
        .await
        .ok()?
        .get::<bool>()
}

async fn primary_network(c: &gio::DBusConnection) -> Option<String> {
    use crate::desktop::{NM, NM_PATH, get_prop};
    let p = get_prop(c, NM, NM_PATH, NM, "PrimaryConnection").await.ok()?;
    let path = p.str().map(String::from).or_else(|| p.get::<glib::variant::ObjectPath>().map(|o| o.as_str().to_string()))?;
    if path == "/" {
        return Some(String::new());
    }
    get_prop(c, NM, &path, "org.freedesktop.NetworkManager.Connection.Active", "Id").await.ok()?.str().map(String::from)
}

/// `umoja-mcp --automations`: watch for triggers and run the rules.
pub async fn serve() {
    let rules = Rc::new(RefCell::new(load()));
    let last: Rc<RefCell<HashMap<u64, Instant>>> = Rc::default();
    let (tx, rx) = async_channel::unbounded::<Event>();
    // Reload when the file changes (the assistant added or removed a rule).
    let file = gio::File::for_path(path());
    let monitor = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE).ok();
    if let Some(m) = &monitor {
        let rules = rules.clone();
        m.connect_changed(move |_, _, _, ev| {
            if matches!(ev, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created | gio::FileMonitorEvent::Deleted) {
                *rules.borrow_mut() = load();
                tracing::info!(count = rules.borrow().len(), "automations reloaded");
            }
        });
    }
    let Ok(session) = crate::desktop::session_bus().await else { return };
    let system = crate::desktop::system_bus().await.ok();

    // Monitors.
    let monitors = Rc::new(std::cell::Cell::new(monitor_count(&session).await.unwrap_or(0)));
    {
        let (c, tx, monitors) = (session.clone(), tx.clone(), monitors.clone());
        subscribe(&session, "org.gnome.Mutter.DisplayConfig", "org.gnome.Mutter.DisplayConfig", "MonitorsChanged", None, None, move |_, _| {
            let (c, tx, monitors) = (c.clone(), tx.clone(), monitors.clone());
            glib::spawn_future_local(async move {
                let Some(n) = monitor_count(&c).await else { return };
                let before = monitors.replace(n);
                if n > before {
                    let _ = tx.try_send(Event::MonitorConnected);
                } else if n < before {
                    let _ = tx.try_send(Event::MonitorDisconnected);
                }
            });
        });
    }
    if let Some(sys) = &system {
        // Power.
        let battery = Rc::new(std::cell::Cell::new(on_battery(sys).await));
        {
            let (c, tx, battery) = (sys.clone(), tx.clone(), battery.clone());
            subscribe(sys, "org.freedesktop.UPower", "org.freedesktop.DBus.Properties", "PropertiesChanged", Some("/org/freedesktop/UPower"), None, move |_, _| {
                let (c, tx, battery) = (c.clone(), tx.clone(), battery.clone());
                glib::spawn_future_local(async move {
                    let now = on_battery(&c).await;
                    if now.is_some() && battery.replace(now) != now {
                        let _ = tx.try_send(if now == Some(true) { Event::OnBattery } else { Event::PluggedIn });
                    }
                });
            });
        }
        // Network.
        let net = Rc::new(RefCell::new(primary_network(sys).await));
        {
            let (c, tx, net) = (sys.clone(), tx.clone(), net.clone());
            subscribe(sys, crate::desktop::NM, "org.freedesktop.DBus.Properties", "PropertiesChanged", Some(crate::desktop::NM_PATH), None, move |_, _| {
                let (c, tx, net) = (c.clone(), tx.clone(), net.clone());
                glib::spawn_future_local(async move {
                    let now = primary_network(&c).await;
                    if now.is_none() || *net.borrow() == now {
                        return;
                    }
                    *net.borrow_mut() = now.clone();
                    let _ = tx.try_send(match now.unwrap_or_default() {
                        n if n.is_empty() => Event::NetworkDisconnected,
                        n => Event::NetworkConnected(n),
                    });
                });
            });
        }
        // Bluetooth devices.
        {
            let (c, tx) = (sys.clone(), tx.clone());
            subscribe(sys, "org.bluez", "org.freedesktop.DBus.Properties", "PropertiesChanged", None, Some("org.bluez.Device1"), move |path, params| {
                let changed = glib::VariantDict::new(Some(&params.child_value(1)));
                if changed.lookup::<bool>("Connected").ok().flatten() != Some(true) {
                    return;
                }
                let (c, tx, path) = (c.clone(), tx.clone(), path.to_string());
                glib::spawn_future_local(async move {
                    let alias = crate::desktop::get_prop(&c, "org.bluez", &path, "org.bluez.Device1", "Alias")
                        .await
                        .ok()
                        .and_then(|v| v.str().map(String::from))
                        .unwrap_or_default();
                    let _ = tx.try_send(Event::BluetoothConnected(alias));
                });
            });
        }
    }
    // Apps opening their first window.
    {
        let seen: Rc<RefCell<HashSet<String>>> = Rc::default();
        let apps_now = || async {
            crate::desktop::windows()
                .await
                .unwrap_or_default()
                .iter()
                .map(|w| {
                    umoja_common::apps::app_for_window(&w.app_id)
                        .and_then(|a| a.id())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| w.app_id.clone())
                })
                .collect::<HashSet<String>>()
        };
        *seen.borrow_mut() = apps_now().await;
        let tx = tx.clone();
        let busy = Rc::new(std::cell::Cell::new(false));
        subscribe(&session, dbus::COMPOSITOR_BUS_NAME, dbus::COMPOSITOR_INTERFACE, "WindowsChanged", Some(dbus::COMPOSITOR_OBJECT_PATH), None, move |_, _| {
            if busy.replace(true) {
                return;
            }
            let (tx, seen, busy) = (tx.clone(), seen.clone(), busy.clone());
            glib::spawn_future_local(async move {
                let now = apps_now().await;
                for app in now.difference(&seen.borrow()) {
                    let _ = tx.try_send(Event::AppOpened(app.clone()));
                }
                *seen.borrow_mut() = now;
                busy.set(false);
            });
        });
    }
    // The clock.
    {
        let tx = tx.clone();
        let fired = Rc::new(RefCell::new(String::new()));
        glib::timeout_add_seconds_local(15, move || {
            if let Ok(now) = glib::DateTime::now_local() {
                let key = now.format("%F %H:%M").map(|s| s.to_string()).unwrap_or_default();
                if *fired.borrow() != key {
                    *fired.borrow_mut() = key;
                    let wd = (now.day_of_week() as usize).saturating_sub(1).min(6);
                    let _ = tx.try_send(Event::Minute(now.hour() as u32, now.minute() as u32, wd));
                }
            }
            glib::ControlFlow::Continue
        });
    }
    // Logging in: give the session a moment to settle.
    {
        let tx = tx.clone();
        glib::timeout_add_seconds_local_once(5, move || {
            let _ = tx.try_send(Event::Login);
        });
    }
    tracing::info!(count = rules.borrow().len(), "automations running");
    let _keep = monitor;
    while let Ok(ev) = rx.recv().await {
        fire(ev, &rules, &last).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_match_events() {
        let t = Trigger::Time { at: "9:00".into(), days: vec!["mon".into(), "Friday".into()] };
        assert!(matches(&t, &Event::Minute(9, 0, 0)));
        assert!(matches(&t, &Event::Minute(9, 0, 4)));
        assert!(!matches(&t, &Event::Minute(9, 0, 2)));
        assert!(!matches(&t, &Event::Minute(9, 1, 0)));
        let n = Trigger::NetworkConnected { name: "Home".into() };
        assert!(matches(&n, &Event::NetworkConnected("home".into())));
        assert!(!matches(&n, &Event::NetworkConnected("Office".into())));
        assert!(matches(&Trigger::NetworkConnected { name: String::new() }, &Event::NetworkConnected("x".into())));
        let b = Trigger::BluetoothConnected { device: "buds".into() };
        assert!(matches(&b, &Event::BluetoothConnected("Galaxy Buds2".into())));
        assert!(matches(&Trigger::AppOpened { app: "slack.desktop".into() }, &Event::AppOpened("slack".into())));
        assert!(!matches(&Trigger::Login, &Event::MonitorConnected));
    }

    #[test]
    fn validation() {
        let known = ["set_dark_style", "run_command", "launch_app"];
        let ok = vec![Step { tool: "set_dark_style".into(), arguments: json!({"enabled": true}) }];
        assert!(validate(&Trigger::MonitorConnected, &ok, &known).is_ok());
        let bad = vec![Step { tool: "run_command".into(), arguments: json!({}) }];
        assert!(validate(&Trigger::MonitorConnected, &bad, &known).unwrap_err().contains("can't run"));
        assert!(validate(&Trigger::Time { at: "25:00".into(), days: vec![] }, &ok, &known).is_err());
        assert!(validate(&Trigger::Time { at: "7:30".into(), days: vec!["someday".into()] }, &ok, &known).is_err());
        assert!(validate(&Trigger::Login, &[], &known).is_err());
    }

    #[test]
    fn requests_parse() {
        let a = json!({
            "trigger": {"kind": "time", "at": "18:00"},
            "actions": [{"tool": "set_night_light", "arguments": {"enabled": true}}]
        });
        let (t, steps) = parse_request(&a).unwrap();
        assert_eq!(t, Trigger::Time { at: "18:00".into(), days: vec![] });
        assert_eq!(steps[0].tool, "set_night_light");
        assert!(parse_request(&json!({"trigger": {"kind": "sometimes"}})).is_err());
    }

    #[test]
    fn every_allowed_tool_exists() {
        let tools = crate::registry();
        for name in ALLOWED {
            let t = tools.iter().find(|t| t.name == *name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!((t.risk)(&json!({})), crate::Risk::Low, "{name} must be low risk");
        }
    }
}
