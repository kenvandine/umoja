//! Everyday actions people ask the assistant for, each as one tool with a
//! plain-language confirmation instead of a shell command: volume, sound
//! devices, brightness, media playback, Do Not Disturb, Night Light,
//! airplane mode, joining a Wi-Fi network, Bluetooth devices, web pages and
//! searches, well-known folders, the wallpaper, the Trash, removable
//! drives, force-quitting a frozen app, installing apps and updates, and
//! the date, time and system details a model can't know by itself.
//!
//! [`redirect`] maps the shell commands models still reach for
//! ("systemctl poweroff", "wpctl set-volume", "apt install vlc") to these
//! tools, so `run_command` never asks the user to approve one of them.

use crate::apps::{have, last_lines, needs_auth, privileged, run, Privileged};
use crate::desktop::{login_session, session_bus, set_bluetooth, set_prop, set_wifi, system_bus, NM, NM_PATH};
use crate::tool::{Risk, arg_bool, arg_str, arg_u64, blocking, expand, high, tilde, LocalFuture, Outcome, Tool};
use gio::prelude::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use umoja_common::dbus;
use umoja_common::gsettings as gs;

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "set_volume",
            "Change the sound volume or mute/unmute the speakers or the microphone. Use for \"turn it up\", \"volume to 30%\", \"mute\", \"mute my mic\".",
            obj(json!({
                "level": {"type": "integer", "description": "New volume in percent, 0 to 100"},
                "change": {"type": "string", "enum": ["up", "down"], "description": "Relative change of 10 percent"},
                "mute": {"type": "boolean"},
                "device": {"type": "string", "enum": ["speakers", "microphone"], "description": "Default speakers"}
            }), &[]),
            |a| Box::pin(set_volume(a)),
        )
        .with_summary(volume_summary),
        Tool::new(
            "set_audio_device",
            "Switch the sound output (speakers, headphones, HDMI, a Bluetooth headset) or the microphone the computer uses. Call without device to list them.",
            obj(json!({
                "device": {"type": "string", "description": "Part of the device's name, e.g. \"headphones\", \"HDMI\", \"USB\""},
                "kind": {"type": "string", "enum": ["output", "input"], "description": "output (default) or input (microphone)"}
            }), &[]),
            |a| Box::pin(set_audio_device(a)),
        )
        .with_summary(|a| match arg_str(a, "device") {
            Some(d) => format!("Play sound through {d}"),
            None => "List sound devices".into(),
        }),
        Tool::new(
            "set_brightness",
            "Change the screen brightness: a percentage, or up/down.",
            obj(json!({
                "level": {"type": "integer", "description": "Percent, 1 to 100"},
                "change": {"type": "string", "enum": ["up", "down"]}
            }), &[]),
            |a| Box::pin(set_brightness(a)),
        )
        .with_summary(|a| match (arg_u64(a, "level"), arg_str(a, "change")) {
            (Some(l), _) => format!("Set the screen brightness to {l}%"),
            (None, Some("down")) => "Dim the screen".into(),
            _ => "Brighten the screen".into(),
        }),
        Tool::new(
            "media_control",
            "Control the music or video that is playing in any app: play/pause, next, previous, stop.",
            obj(json!({"action": {"type": "string", "enum": ["play_pause", "pause", "next", "previous", "stop"]}}), &["action"]),
            |a| Box::pin(media_control(a)),
        )
        .with_summary(|a| {
            match arg_str(a, "action").unwrap_or("play_pause") {
                "pause" => "Pause playback",
                "next" => "Skip to the next track",
                "previous" => "Go back to the previous track",
                "stop" => "Stop playback",
                _ => "Play or pause",
            }
            .into()
        }),
        Tool::read_only(
            "now_playing",
            "What music or video is playing, in which app, and whether it is paused.",
            obj(json!({}), &[]),
            |_| Box::pin(now_playing()),
        ),
        Tool::new(
            "set_do_not_disturb",
            "Turn Do Not Disturb on or off (hides notification pop-ups).",
            obj(json!({"enabled": {"type": "boolean"}}), &["enabled"]),
            |a| Box::pin(async move { set_do_not_disturb(&a) }),
        )
        .with_summary(|a| on_off("Do Not Disturb", a)),
        Tool::new(
            "set_night_light",
            "Turn Night Light (warmer screen colours) on or off.",
            obj(json!({"enabled": {"type": "boolean"}}), &["enabled"]),
            |a| Box::pin(async move { set_night_light(&a) }),
        )
        .with_summary(|a| on_off("Night Light", a)),
        Tool::new(
            "set_airplane_mode",
            "Turn airplane mode on (Wi-Fi, Bluetooth and mobile broadband off) or off.",
            obj(json!({"enabled": {"type": "boolean"}}), &["enabled"]),
            |a| Box::pin(set_airplane_mode(a)),
        )
        .with_summary(|a| on_off("airplane mode", a)),
        Tool::new(
            "connect_wifi",
            "Join a Wi-Fi network by name. Pass the password only if the user gave one; known and open networks need none.",
            obj(json!({
                "network": {"type": "string", "description": "The network name (SSID)"},
                "password": {"type": "string"}
            }), &["network"]),
            |a| Box::pin(connect_wifi(a)),
        )
        .with_summary(|a| format!("Connect to the Wi-Fi network “{}”", arg_str(a, "network").unwrap_or(""))),
        Tool::new(
            "bluetooth_device",
            "Connect or disconnect a paired Bluetooth device (headphones, mouse, speaker) by name. Call with action list to see paired devices.",
            obj(json!({
                "device": {"type": "string"},
                "action": {"type": "string", "enum": ["connect", "disconnect", "list"]}
            }), &["action"]),
            |a| Box::pin(bluetooth_device(a)),
        )
        .with_summary(|a| {
            let d = arg_str(a, "device").unwrap_or("the device");
            match arg_str(a, "action").unwrap_or("") {
                "disconnect" => format!("Disconnect {d}"),
                "connect" => format!("Connect {d}"),
                _ => "List Bluetooth devices".into(),
            }
        }),
        Tool::new(
            "open_url",
            "Open a web page in the default browser, or search the web. Pass url for an address, or query to search.",
            obj(json!({
                "url": {"type": "string"},
                "query": {"type": "string", "description": "Words to search the web for"}
            }), &[]),
            |a| Box::pin(async move { open_url(&a).hide_if_ok() }),
        )
        .with_summary(|a| match (arg_str(a, "url"), arg_str(a, "query")) {
            (Some(u), _) => format!("Open {u}"),
            (None, Some(q)) => format!("Search the web for “{q}”"),
            _ => "Open the browser".into(),
        }),
        Tool::new(
            "open_folder",
            "Open a well-known folder in Files: home, desktop, documents, downloads, music, pictures, videos, screenshots, screencasts, trash.",
            obj(json!({"folder": {"type": "string"}}), &["folder"]),
            |a| Box::pin(async move { open_folder(&a).hide_if_ok() }),
        )
        .with_summary(|a| format!("Open {}", folder_label(arg_str(a, "folder").unwrap_or("home")))),
        Tool::new(
            "set_wallpaper",
            "Use an image file as the desktop background. Use a path from search_files.",
            obj(json!({"path": {"type": "string"}}), &["path"]),
            |a| Box::pin(async move { set_wallpaper(&a) }),
        )
        .with_summary(|a| {
            let p = arg_str(a, "path").unwrap_or("");
            let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("Set {name} as the background")
        }),
        Tool::new(
            "empty_trash",
            "Permanently delete everything in the Trash. The user is asked first.",
            obj(json!({}), &[]),
            |_| Box::pin(empty_trash()),
        )
        // Nothing to ask about when it's already empty.
        .with_risk(|_| if trash_count().unwrap_or(1) == 0 { Risk::Low } else { Risk::Medium })
        .with_preflight(trash_preflight)
        .with_summary(|_| "Empty the Trash".into()),
        Tool::new(
            "clear_folder",
            "Move everything inside a folder (e.g. Downloads, Desktop, or a path) to the Trash, keeping the folder itself. Use for \"delete everything in Downloads\" or \"clean up my desktop\". Items can be restored from the Trash. The user is asked first.",
            obj(json!({"folder": {"type": "string", "description": "A well-known folder name (downloads, desktop...) or a path"}}), &["folder"]),
            |a| Box::pin(clear_folder(a)),
        )
        .confirm()
        .with_preflight(clear_folder_preflight)
        .with_summary(|a| format!("Move everything in {} to the Trash", folder_label(arg_str(a, "folder").unwrap_or("")))),
        Tool::new(
            "eject_drive",
            "Safely remove (eject or unmount) a USB stick, SD card or external disk. Call without drive to list them.",
            obj(json!({"drive": {"type": "string", "description": "Part of the drive's name; omit to list"}}), &[]),
            |a| Box::pin(eject_drive(a)),
        )
        .with_summary(|a| match arg_str(a, "drive") {
            Some(d) => format!("Safely remove {d}"),
            None => "List removable drives".into(),
        }),
        Tool::new(
            "force_quit_app",
            "Force an app that is frozen or not responding to quit. Unsaved work in it is lost, so the user is asked first. For a normal close use close_window.",
            obj(json!({"app": {"type": "string", "description": "The app's name, e.g. \"Firefox\""}}), &["app"]),
            |a| Box::pin(force_quit(a)),
        )
        .confirm()
        .with_preflight(force_quit_preflight)
        .with_summary(|a| format!("Force quit {}", arg_str(a, "app").unwrap_or("the app"))),
        Tool::new(
            "install_app",
            "Install an app by name (from the Snap Store, or the Ubuntu archive when there is no snap). Just pass the app's name; it finds the package and asks the user itself. Never tell the user to run apt or snap.",
            obj(json!({
                "name": {"type": "string"},
                "source": {"type": "string", "enum": ["snap", "deb"], "description": "Only if the user asked for one"}
            }), &["name"]),
            |a| Box::pin(install_app(a)),
        )
        .with_risk(high)
        .with_preflight(install_preflight)
        .with_summary(|a| format!("Install {}", arg_str(a, "name").unwrap_or("the app"))),
        Tool::read_only(
            "check_updates",
            "List the software updates (debs and snaps) waiting to be installed.",
            obj(json!({}), &[]),
            |_| Box::pin(async { Outcome::from(blocking(updates_summary).await.and_then(|r| r)) }),
        ),
        Tool::new(
            "install_updates",
            "Install all available software updates (apt and snaps). The user sees what will be updated and is asked first.",
            obj(json!({}), &[]),
            |_| Box::pin(install_updates()),
        )
        .with_risk(high)
        .with_preflight(|_| Box::pin(async { blocking(updates_summary).await.and_then(|r| r).map(Some) }))
        .with_summary(|_| "Install software updates".into()),
        Tool::read_only(
            "date_time",
            "The current date, time, time zone and week number.",
            obj(json!({}), &[]),
            |_| Box::pin(async { Outcome::text(date_time()) }),
        ),
        Tool::read_only(
            "system_info",
            "About this computer: Ubuntu version, kernel, computer model, CPU, memory, uptime, host name and IP addresses.",
            obj(json!({}), &[]),
            |_| Box::pin(async { Outcome::from(blocking(system_info).await) }),
        ),
    ]
}

fn on_off(what: &str, a: &Value) -> String {
    match arg_bool(a, "enabled") {
        Some(false) => format!("Turn off {what}"),
        _ => format!("Turn on {what}"),
    }
}

fn err(msg: impl std::fmt::Display) -> Outcome {
    Outcome::err(msg)
}

/// Run `argv` without blocking the main loop.
async fn cmd(argv: &[&str]) -> (bool, String) {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    blocking(move || {
        let a: Vec<&str> = argv.iter().map(String::as_str).collect();
        let (ok, _, out) = run(&a);
        (ok, out)
    })
    .await
    .unwrap_or_else(|e| (false, e))
}

/// The panel's OSD (volume/brightness bubble), as gnome-shell's ShowOSD.
async fn show_osd(icon: &str, level: Option<f64>) {
    let Ok(c) = session_bus().await else { return };
    let d = glib::VariantDict::new(None);
    d.insert("icon", icon);
    if let Some(l) = level {
        d.insert("level", l);
    }
    let _ = c
        .call_future(
            Some(dbus::MEDIA_KEYS_BUS_NAME),
            dbus::MEDIA_KEYS_OBJECT_PATH,
            dbus::MEDIA_KEYS_INTERFACE,
            "ShowOSD",
            Some(&(d.end(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await;
}

async fn press_media_key(key: &str) -> Result<(), String> {
    let c = session_bus().await?;
    c.call_future(
        Some(dbus::MEDIA_KEYS_BUS_NAME),
        dbus::MEDIA_KEYS_OBJECT_PATH,
        dbus::MEDIA_KEYS_INTERFACE,
        "Press",
        Some(&(key,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        2000,
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---- volume ---------------------------------------------------------------------

fn is_mic(a: &Value) -> bool {
    arg_str(a, "device").is_some_and(|d| {
        let d = d.to_lowercase();
        d.starts_with("mic") || d == "input" || d == "source"
    })
}

fn volume_summary(a: &Value) -> String {
    let what = if is_mic(a) { "the microphone" } else { "the sound" };
    match (arg_bool(a, "mute"), arg_u64(a, "level"), arg_str(a, "change")) {
        (Some(true), _, _) => format!("Mute {what}"),
        (_, Some(l), _) => format!("Set the volume to {}%", l.min(100)),
        (_, _, Some("down")) => "Turn the volume down".into(),
        (_, _, Some(_)) => "Turn the volume up".into(),
        (Some(false), _, _) => format!("Unmute {what}"),
        _ => "Read the volume".into(),
    }
}

/// `wpctl get-volume`: "Volume: 0.45 [MUTED]".
pub fn parse_volume(out: &str) -> Option<(u32, bool)> {
    let v: f64 = out.split_whitespace().nth(1)?.parse().ok()?;
    Some(((v * 100.0).round() as u32, out.contains("[MUTED]")))
}

async fn set_volume(a: Value) -> Outcome {
    if !have("wpctl") {
        return err("wpctl (WirePlumber) is not installed");
    }
    let mic = is_mic(&a);
    let target = if mic { "@DEFAULT_AUDIO_SOURCE@" } else { "@DEFAULT_AUDIO_SINK@" };
    let level = arg_u64(&a, "level").map(|l| l.min(100));
    let change = arg_str(&a, "change").map(str::to_lowercase);
    let mute = arg_bool(&a, "mute");
    let step = match (level, change.as_deref()) {
        (Some(l), _) => Some(format!("{l}%")),
        (None, Some("down" | "lower" | "decrease")) => Some("10%-".into()),
        (None, Some(_)) => Some("10%+".into()),
        _ => None,
    };
    if let Some(m) = mute {
        let (ok, out) = cmd(&["wpctl", "set-mute", target, if m { "1" } else { "0" }]).await;
        if !ok {
            return err(out);
        }
    }
    if let Some(step) = step {
        // Changing the level unmutes, as the volume keys do.
        if mute.is_none() {
            cmd(&["wpctl", "set-mute", target, "0"]).await;
        }
        let (ok, out) = cmd(&["wpctl", "set-volume", "-l", "1.0", target, &step]).await;
        if !ok {
            return err(out);
        }
    }
    let (_, out) = cmd(&["wpctl", "get-volume", target]).await;
    let Some((pct, muted)) = parse_volume(&out) else {
        return err(format!("couldn't read the volume: {out}"));
    };
    let icon = match (mic, muted, pct) {
        (true, true, _) => "microphone-sensitivity-muted-symbolic",
        (true, false, _) => "microphone-sensitivity-high-symbolic",
        (false, true, _) | (false, false, 0) => "audio-volume-muted-symbolic",
        (false, false, 1..=33) => "audio-volume-low-symbolic",
        (false, false, 34..=66) => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    };
    if mute.is_some() || level.is_some() || change.is_some() {
        show_osd(icon, Some(if muted { 0.0 } else { pct as f64 / 100.0 })).await;
    }
    let what = if mic { "Microphone" } else { "Volume" };
    Outcome::ok(if muted { format!("{what} is muted ({pct}%)") } else { format!("{what} is at {pct}%") })
}

// ---- sound devices ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct AudioNode {
    pub id: u32,
    pub name: String,
    pub default: bool,
    pub input: bool,
}

/// The Audio Sinks and Sources from `wpctl status`.
pub fn parse_wpctl_status(text: &str) -> Vec<AudioNode> {
    let mut out = Vec::new();
    let mut in_audio = false;
    let mut section: Option<bool> = None; // Some(input?)
    for line in text.lines() {
        let t = line.trim_start_matches(|c: char| " │├└─".contains(c)).trim_end();
        if !line.starts_with(' ') && !line.starts_with('│') && !t.is_empty() {
            in_audio = t == "Audio";
            section = None;
            continue;
        }
        if !in_audio {
            continue;
        }
        if t.ends_with(':') {
            section = match t {
                "Sinks:" => Some(false),
                "Sources:" => Some(true),
                _ => None,
            };
            continue;
        }
        let Some(input) = section else { continue };
        let (default, rest) = match t.strip_prefix('*') {
            Some(r) => (true, r.trim_start()),
            None => (false, t),
        };
        let Some((id, name)) = rest.split_once(". ") else { continue };
        let Ok(id) = id.trim().parse() else { continue };
        let name = name.split(" [").next().unwrap_or(name).trim().to_string();
        out.push(AudioNode { id, name, default, input });
    }
    out
}

async fn set_audio_device(a: Value) -> Outcome {
    if !have("wpctl") {
        return err("wpctl (WirePlumber) is not installed");
    }
    let input = matches!(arg_str(&a, "kind"), Some("input")) || is_mic(&json!({"device": arg_str(&a, "kind").unwrap_or("")}));
    let (ok, status) = cmd(&["wpctl", "status"]).await;
    if !ok {
        return err(status);
    }
    let nodes: Vec<AudioNode> = parse_wpctl_status(&status).into_iter().filter(|n| n.input == input).collect();
    let list = || {
        nodes
            .iter()
            .map(|n| format!("- {}{}", n.name, if n.default { " (in use)" } else { "" }))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let Some(want) = arg_str(&a, "device").map(str::to_lowercase) else {
        return Outcome::ok(format!("{} devices:\n{}", if input { "Input" } else { "Output" }, list()));
    };
    let words: Vec<&str> = want.split_whitespace().collect();
    let hit = nodes
        .iter()
        .find(|n| n.name.to_lowercase().contains(&want))
        .or_else(|| nodes.iter().find(|n| words.iter().all(|w| n.name.to_lowercase().contains(w))))
        .or_else(|| {
            // "headphones"/"speakers" on the built-in card are ports, not nodes.
            let alias = match want.as_str() {
                w if w.contains("headphone") || w.contains("headset") => &["headphone", "headset", "usb", "bluetooth"][..],
                w if w.contains("speaker") || w.contains("built") => &["built-in", "speaker", "analog"][..],
                w if w.contains("tv") || w.contains("monitor") => &["hdmi", "displayport"][..],
                _ => &[][..],
            };
            nodes.iter().find(|n| alias.iter().any(|w| n.name.to_lowercase().contains(w)))
        });
    let Some(node) = hit else {
        return err(format!("no sound device matches “{want}”. Devices:\n{}", list()));
    };
    let (ok, out) = cmd(&["wpctl", "set-default", &node.id.to_string()]).await;
    if !ok {
        return err(out);
    }
    Outcome::ok(format!("Now using {} for {}", node.name, if input { "the microphone" } else { "sound" }))
}

// ---- brightness -------------------------------------------------------------------

struct Backlight {
    name: String,
    dir: PathBuf,
    min: i64,
    max: i64,
}

fn read_int(p: &Path) -> Option<i64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// firmware → platform → raw, as gsd-backlight picks (same as the panel).
fn backlight() -> Option<Backlight> {
    let mut found: Vec<(String, PathBuf, String)> = std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .filter_map(Result::ok)
        .map(|e| {
            let dir = e.path();
            let kind = std::fs::read_to_string(dir.join("type")).unwrap_or_default().trim().to_string();
            (e.file_name().to_string_lossy().into_owned(), dir, kind)
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let (name, dir, kind) = ["firmware", "platform", "raw"]
        .iter()
        .find_map(|t| found.iter().find(|(_, _, k)| k == t).cloned())?;
    let max = read_int(&dir.join("max_brightness"))?;
    let mut min = ((max as f64 * 0.01) as i64).max(1);
    if max < 99 && kind == "raw" {
        min = 0;
    }
    (min < max).then_some(Backlight { name, dir, min, max })
}

/// The new raw value for `level`% or a 10% step from `now`.
pub fn brightness_target(min: i64, max: i64, now: i64, level: Option<u64>, up: bool) -> i64 {
    let range = (max - min) as f64;
    let pct = match level {
        Some(l) => l.clamp(1, 100) as f64,
        None => {
            let cur = (now - min) as f64 * 100.0 / range;
            (if up { cur + 10.0 } else { cur - 10.0 }).clamp(1.0, 100.0)
        }
    };
    (min as f64 + range * pct / 100.0).round() as i64
}

async fn set_brightness(a: Value) -> Outcome {
    let Some(b) = backlight() else {
        return err("this screen's brightness can't be changed from the computer (no backlight control)");
    };
    let now = read_int(&b.dir.join("brightness")).unwrap_or(b.max);
    let up = !matches!(arg_str(&a, "change"), Some("down" | "lower" | "dim"));
    let level = arg_u64(&a, "level");
    if level.is_none() && arg_str(&a, "change").is_none() {
        let pct = (now - b.min) * 100 / (b.max - b.min);
        return Outcome::ok(format!("Screen brightness is {pct}%"));
    }
    let new = brightness_target(b.min, b.max, now, level, up);
    let c = match system_bus().await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let session = match login_session(&c).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    if let Err(e) = c
        .call_future(
            Some("org.freedesktop.login1"),
            &session,
            "org.freedesktop.login1.Session",
            "SetBrightness",
            Some(&("backlight", b.name.as_str(), new as u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await
    {
        return err(e);
    }
    let pct = ((new - b.min) * 100) as f64 / (b.max - b.min) as f64;
    show_osd("display-brightness-symbolic", Some(pct / 100.0)).await;
    Outcome::ok(format!("Screen brightness is now {}%", pct.round()))
}

// ---- media ---------------------------------------------------------------------------

async fn media_control(a: Value) -> Outcome {
    let key = match arg_str(&a, "action").unwrap_or("play_pause") {
        "pause" => "pause",
        "next" => "next",
        "previous" | "prev" | "back" => "previous",
        "stop" => "stop",
        _ => "play",
    };
    match press_media_key(key).await {
        Ok(()) => Outcome::ok(format!("Sent {} to the media player", key.replace("play", "play/pause"))),
        Err(e) => err(e),
    }
}

async fn now_playing() -> Outcome {
    let c = match session_bus().await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let names = match c
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ListNames",
            None,
            Some(glib::VariantTy::new("(as)").unwrap()),
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await
    {
        Ok(v) => v.get::<(Vec<String>,)>().map(|(n,)| n).unwrap_or_default(),
        Err(e) => return err(e),
    };
    let mut lines = Vec::new();
    for name in names.iter().filter(|n| n.starts_with("org.mpris.MediaPlayer2.")) {
        let get = |iface: &'static str, prop: &'static str| {
            let c = c.clone();
            let name = name.clone();
            async move { crate::desktop::get_prop(&c, &name, "/org/mpris/MediaPlayer2", iface, prop).await.ok() }
        };
        let app = get("org.mpris.MediaPlayer2", "Identity").await.and_then(|v| v.str().map(String::from)).unwrap_or_else(|| name[23..].to_string());
        let status = get("org.mpris.MediaPlayer2.Player", "PlaybackStatus").await.and_then(|v| v.str().map(String::from)).unwrap_or_default();
        let meta = get("org.mpris.MediaPlayer2.Player", "Metadata").await;
        let (mut title, mut artist) = (String::new(), String::new());
        if let Some(m) = meta {
            let d = glib::VariantDict::new(Some(&m));
            title = d.lookup::<String>("xesam:title").ok().flatten().unwrap_or_default();
            artist = d.lookup::<Vec<String>>("xesam:artist").ok().flatten().unwrap_or_default().join(", ");
        }
        let what = match (title.is_empty(), artist.is_empty()) {
            (true, _) => "nothing loaded".to_string(),
            (false, true) => format!("“{title}”"),
            (false, false) => format!("“{title}” by {artist}"),
        };
        lines.push(format!("{app}: {what} ({})", if status.is_empty() { "unknown" } else { &status }));
    }
    if lines.is_empty() {
        Outcome::ok("No media player is running.")
    } else {
        Outcome::ok(lines.join("\n"))
    }
}

// ---- notifications, night light, airplane mode ----------------------------------------

fn set_do_not_disturb(a: &Value) -> Outcome {
    let Some(on) = arg_bool(a, "enabled") else { return err("enabled must be true or false") };
    let Some(s) = gs::settings("org.gnome.desktop.notifications") else {
        return err("org.gnome.desktop.notifications is not installed");
    };
    if let Err(e) = s.set_boolean("show-banners", !on) {
        return err(e);
    }
    gio::Settings::sync();
    Outcome::ok(format!("Do Not Disturb is now {}", if on { "on" } else { "off" }))
}

fn set_night_light(a: &Value) -> Outcome {
    let Some(on) = arg_bool(a, "enabled") else { return err("enabled must be true or false") };
    let Some(s) = gs::settings("org.gnome.settings-daemon.plugins.color") else {
        return err("Night Light settings are not installed");
    };
    if let Err(e) = s.set_boolean("night-light-enabled", on) {
        return err(e);
    }
    gio::Settings::sync();
    Outcome::ok(format!("Night Light is now {}", if on { "on" } else { "off" }))
}

async fn set_airplane_mode(a: Value) -> Outcome {
    let Some(on) = arg_bool(&a, "enabled") else { return err("enabled must be true or false") };
    let radios = json!({"enabled": !on});
    let wifi = set_wifi(radios.clone()).await;
    let bt = set_bluetooth(radios).await;
    if let Ok(c) = system_bus().await {
        let _ = set_prop(&c, NM, NM_PATH, NM, "WwanEnabled", (!on).to_variant()).await;
    }
    let state = if on { "on" } else { "off" };
    let notes: Vec<String> = [wifi, bt].into_iter().filter(|r| r.starts_with("error")).collect();
    if notes.len() == 2 {
        return err(notes.join("; "));
    }
    Outcome::ok(if notes.is_empty() {
        format!("Airplane mode is now {state}")
    } else {
        format!("Airplane mode is now {state} ({})", notes.join("; "))
    })
}

// ---- Wi-Fi networks and Bluetooth devices ------------------------------------------------

async fn connect_wifi(a: Value) -> Outcome {
    let Some(ssid) = arg_str(&a, "network").map(String::from) else { return err("network is required") };
    if !have("nmcli") {
        return err("NetworkManager's nmcli is not installed");
    }
    // Wi-Fi may be off.
    let _ = set_wifi(json!({"enabled": true})).await;
    let mut argv = vec!["nmcli", "--wait", "30", "device", "wifi", "connect", ssid.as_str()];
    let pw = arg_str(&a, "password").map(String::from);
    if let Some(p) = &pw {
        argv.extend(["password", p.as_str()]);
    }
    let (ok, out) = cmd(&argv).await;
    if ok {
        return Outcome::ok(format!("Connected to {ssid}"));
    }
    let low = out.to_lowercase();
    if low.contains("secrets were required") || low.contains("no secrets") || low.contains("password") {
        return err(format!("{ssid} needs a password. Ask the user for it, or open the Wi-Fi settings."));
    }
    if low.contains("no network with ssid") {
        return err(format!("no Wi-Fi network called {ssid} is in range"));
    }
    err(last_lines(&out))
}

/// `bluetoothctl devices Paired`: "Device AA:BB:CC:DD:EE:FF Name".
pub fn parse_bt_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("Device ")?;
            let (mac, name) = rest.split_once(' ')?;
            Some((mac.to_string(), name.trim().to_string()))
        })
        .collect()
}

async fn bluetooth_device(a: Value) -> Outcome {
    if !have("bluetoothctl") {
        return err("bluetoothctl is not installed");
    }
    let (_, out) = cmd(&["bluetoothctl", "devices", "Paired"]).await;
    let mut devices = parse_bt_devices(&out);
    if devices.is_empty() {
        let (_, out) = cmd(&["bluetoothctl", "devices"]).await;
        devices = parse_bt_devices(&out);
    }
    let list = devices.iter().map(|(_, n)| format!("- {n}")).collect::<Vec<_>>().join("\n");
    let action = arg_str(&a, "action").unwrap_or("list");
    let Some(want) = arg_str(&a, "device").map(str::to_lowercase).filter(|_| action != "list") else {
        return Outcome::ok(if list.is_empty() { "No Bluetooth devices are paired.".into() } else { format!("Paired devices:\n{list}") });
    };
    let Some((mac, name)) = devices.iter().find(|(_, n)| n.to_lowercase().contains(&want)) else {
        return err(format!(
            "no paired Bluetooth device matches “{want}”. Paired: {}. New devices are paired in Settings › Bluetooth.",
            if list.is_empty() { "none".into() } else { list.replace('\n', " ") }
        ));
    };
    if action == "connect" {
        let _ = set_bluetooth(json!({"enabled": true})).await;
    }
    let verb = if action == "disconnect" { "disconnect" } else { "connect" };
    let (ok, out) = cmd(&["timeout", "25", "bluetoothctl", verb, mac]).await;
    if ok && !out.contains("Failed") {
        Outcome::ok(format!("{} {name}", if verb == "connect" { "Connected" } else { "Disconnected" }))
    } else {
        err(format!("couldn't {verb} {name}: {}", last_lines(&out)))
    }
}

// ---- web, folders, wallpaper ------------------------------------------------------------

pub fn web_uri(a: &Value) -> Option<String> {
    if let Some(u) = arg_str(a, "url") {
        if u.contains("://") || u.starts_with("mailto:") {
            return Some(u.to_string());
        }
        if !u.contains(' ') && u.contains('.') {
            return Some(format!("https://{u}"));
        }
        return Some(search_uri(u));
    }
    arg_str(a, "query").map(search_uri)
}

fn search_uri(q: &str) -> String {
    format!("https://duckduckgo.com/?q={}", percent_encode(q))
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn open_url(a: &Value) -> Outcome {
    let Some(uri) = web_uri(a) else { return err("url or query is required") };
    match gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        Ok(()) => Outcome::ok(format!("Opened {uri}")),
        Err(e) => err(e),
    }
}

fn folder_label(f: &str) -> String {
    match f.to_lowercase().trim_start_matches("my ").trim() {
        "trash" | "bin" | "recycle bin" => "the Trash".into(),
        "home" | "home folder" | "~" => "your home folder".into(),
        other => format!("{} folder", capitalize(other)),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Folder name → URI (`trash:///` or a file URI).
pub fn folder_uri(f: &str) -> Option<String> {
    use glib::UserDirectory as D;
    let f = f.to_lowercase();
    let f = f.trim_start_matches("my ").trim_end_matches(" folder").trim();
    let special = |d: D, fallback: &str| glib::user_special_dir(d).unwrap_or_else(|| glib::home_dir().join(fallback));
    let path = match f {
        "trash" | "bin" | "recycle bin" | "wastebasket" => return Some("trash:///".into()),
        "home" | "~" | "" => glib::home_dir(),
        "desktop" => special(D::Desktop, "Desktop"),
        "documents" | "docs" => special(D::Documents, "Documents"),
        "downloads" | "download" => special(D::Downloads, "Downloads"),
        "music" => special(D::Music, "Music"),
        "pictures" | "photos" | "images" => special(D::Pictures, "Pictures"),
        "videos" | "movies" => special(D::Videos, "Videos"),
        "screenshots" => special(D::Pictures, "Pictures").join(umoja_common::i18n::shell("Screenshots")),
        "screencasts" | "recordings" | "screen recordings" => special(D::Videos, "Videos").join("Screencasts"),
        other => expand(other),
    };
    path.is_dir().then(|| gio::File::for_path(&path).uri().to_string())
}

fn open_folder(a: &Value) -> Outcome {
    let name = arg_str(a, "folder").unwrap_or("home");
    let Some(uri) = folder_uri(name) else { return err(format!("there is no {} yet", folder_label(name))) };
    match gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        Ok(()) => Outcome::ok(format!("Opened {}", folder_label(name))),
        Err(e) => err(e),
    }
}

fn set_wallpaper(a: &Value) -> Outcome {
    let Some(p) = arg_str(a, "path") else { return err("path is required") };
    let path = expand(p);
    if !path.is_file() {
        return err(format!("{p} doesn't exist"));
    }
    let (mime, _) = gio::content_type_guess(Some(&path), &[]);
    if !mime.starts_with("image/") && !gio::content_type_is_a(&mime, "image/*") {
        return err(format!("{p} isn't an image"));
    }
    let Some(s) = gs::settings("org.gnome.desktop.background") else {
        return err("org.gnome.desktop.background is not installed");
    };
    let uri = gio::File::for_path(&path).uri();
    let _ = s.set_string("picture-uri", &uri);
    if gs::has_key(&s, "picture-uri-dark") {
        let _ = s.set_string("picture-uri-dark", &uri);
    }
    if s.string("picture-options") == "none" {
        let _ = s.set_string("picture-options", "zoom");
    }
    gio::Settings::sync();
    Outcome::ok(format!("The background is now {}", tilde(&path.to_string_lossy())))
}

// ---- Trash ----------------------------------------------------------------------------

fn trash_count() -> Result<usize, String> {
    let (ok, out) = {
        let (ok, _, out) = run(&["gio", "list", "trash:///"]);
        (ok, out)
    };
    if !ok {
        return Err(out);
    }
    Ok(out.lines().filter(|l| !l.trim().is_empty()).count())
}

fn trash_preflight(_: Value) -> LocalFuture<Result<Option<String>, String>> {
    Box::pin(async {
        match blocking(trash_count).await? {
            Ok(0) => Ok(None),
            Ok(n) => Ok(Some(format!(
                "{n} item{} will be deleted permanently.",
                if n == 1 { "" } else { "s" }
            ))),
            Err(e) => Err(e),
        }
    })
}

async fn empty_trash() -> Outcome {
    let n = blocking(trash_count).await.ok().and_then(Result::ok).unwrap_or(0);
    if n == 0 {
        return Outcome::ok("The Trash is already empty");
    }
    let (ok, out) = cmd(&["gio", "trash", "--empty"]).await;
    if ok {
        Outcome::ok(format!("Emptied the Trash ({n} item{})", if n == 1 { "" } else { "s" }))
    } else {
        err(out)
    }
}

/// A folder to clear: a well-known name or a path under home (never home
/// itself or anything outside it).
fn clearable_folder(name: &str) -> Result<PathBuf, String> {
    let home = glib::home_dir();
    let path = match folder_uri(name) {
        Some(uri) if uri.starts_with("file://") => gio::File::for_uri(&uri).path(),
        Some(_) => return Err("use empty_trash for the Trash".into()),
        None => Some(expand(name)),
    }
    .ok_or("that folder can't be cleared")?;
    let path = path.canonicalize().map_err(|_| format!("there is no folder {name}"))?;
    if !path.is_dir() {
        return Err(format!("{name} isn't a folder"));
    }
    if path == home || !path.starts_with(&home) {
        return Err("only folders inside the home folder (not the home folder itself) can be cleared".into());
    }
    Ok(path)
}

fn folder_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(std::fs::read_dir(dir).map_err(|e| e.to_string())?.filter_map(Result::ok).map(|e| e.path()).collect())
}

fn clear_folder_preflight(a: Value) -> LocalFuture<Result<Option<String>, String>> {
    Box::pin(async move {
        let dir = clearable_folder(arg_str(&a, "folder").unwrap_or(""))?;
        let items = folder_entries(&dir)?;
        if items.is_empty() {
            return Err(format!("{} is already empty", tilde(&dir.to_string_lossy())));
        }
        let mut names: Vec<String> = items.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        names.sort();
        let more = names.len().saturating_sub(6);
        names.truncate(6);
        Ok(Some(format!(
            "{} item{} in {} go to the Trash, where they can be restored: {}{}",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            tilde(&dir.to_string_lossy()),
            names.join(", "),
            if more > 0 { format!(" and {more} more") } else { String::new() }
        )))
    })
}

async fn clear_folder(a: Value) -> Outcome {
    let dir = match clearable_folder(arg_str(&a, "folder").unwrap_or("")) {
        Ok(d) => d,
        Err(e) => return err(e),
    };
    let items = match folder_entries(&dir) {
        Ok(i) => i,
        Err(e) => return err(e),
    };
    let (mut moved, mut failed) = (0, Vec::new());
    for p in &items {
        match gio::File::for_path(p).trash(gio::Cancellable::NONE) {
            Ok(()) => moved += 1,
            Err(e) => failed.push(format!("{}: {e}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
        }
    }
    let where_ = tilde(&dir.to_string_lossy());
    match (moved, failed.is_empty()) {
        (_, true) => Outcome::ok(format!("Moved {moved} item{} from {where_} to the Trash", if moved == 1 { "" } else { "s" })),
        (0, false) => err(failed.join("; ")),
        _ => Outcome::ok(format!("Moved {moved} items from {where_} to the Trash; couldn't move {}", failed.join("; "))),
    }
}

// ---- removable drives -------------------------------------------------------------------

fn removable_mounts() -> Vec<(String, gio::Mount)> {
    gio::VolumeMonitor::get()
        .mounts()
        .into_iter()
        .filter(|m| {
            let removable = m.drive().is_some_and(|d| d.is_removable() || d.is_media_removable() || d.can_eject());
            !m.is_shadowed() && (m.can_eject() || (m.can_unmount() && removable))
        })
        .map(|m| (m.name().to_string(), m))
        .collect()
}

async fn eject_drive(a: Value) -> Outcome {
    let mounts = removable_mounts();
    let names = mounts.iter().map(|(n, _)| format!("- {n}")).collect::<Vec<_>>().join("\n");
    let Some(want) = arg_str(&a, "drive").map(str::to_lowercase) else {
        return Outcome::ok(if names.is_empty() { "No removable drives are connected.".into() } else { format!("Removable drives:\n{names}") });
    };
    let generic = ["usb", "usb stick", "usb drive", "drive", "disk", "sd card", "the drive", "external drive", "external disk"];
    let hit = mounts
        .iter()
        .find(|(n, _)| n.to_lowercase().contains(&want))
        .or_else(|| (mounts.len() == 1 && generic.contains(&want.as_str())).then(|| &mounts[0]));
    let Some((name, mount)) = hit else {
        return err(if names.is_empty() {
            "no removable drives are connected".to_string()
        } else {
            format!("no drive matches “{want}”. Connected:\n{names}")
        });
    };
    let path = mount.root().path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let flag = if mount.can_eject() { "-e" } else { "-u" };
    let (ok, out) = cmd(&["gio", "mount", flag, &path]).await;
    if ok {
        Outcome::ok(format!("{name} can be unplugged now"))
    } else if out.to_lowercase().contains("busy") {
        err(format!("{name} is still in use; close any files or windows using it first"))
    } else {
        err(last_lines(&out))
    }
}

// ---- force quit -----------------------------------------------------------------------------

/// The process name to look for: the launcher's program (`/snap/bin/firefox`
/// → `firefox`), cut to the kernel's 15-character comm.
pub fn process_name(commandline: &str) -> Option<String> {
    let words = glib::shell_parse_argv(commandline).ok()?;
    let mut it = words.iter().map(|w| w.to_string_lossy().into_owned());
    let mut prog = it.next()?;
    // env FOO=1 prog / flatpak run id
    if prog.ends_with("env") {
        prog = it.find(|w| !w.contains('=') && !w.starts_with('-'))?;
    }
    let base = Path::new(&prog).file_name()?.to_string_lossy().into_owned();
    Some(base.chars().take(15).collect())
}

fn app_process(app: &str) -> Result<(String, String), String> {
    // A launcher if there is one, else the name itself.
    if let Some(info) = crate::apps::find_launcher(app)
        && let Some(cl) = info.commandline()
        && let Some(p) = process_name(&cl.to_string_lossy())
    {
        return Ok((info.display_name().to_string(), p));
    }
    let p: String = app.split_whitespace().next().unwrap_or(app).chars().take(15).collect();
    Ok((app.to_string(), p))
}

fn uid() -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid().to_string()).unwrap_or_else(|_| "0".into())
}

fn pids(proc_name: &str) -> Vec<String> {
    let (_, _, out) = run(&["pgrep", "-u", &uid(), "-i", "-x", proc_name]);
    out.lines().filter(|l| l.trim().parse::<u32>().is_ok()).map(|l| l.trim().to_string()).collect()
}

fn force_quit_preflight(a: Value) -> LocalFuture<Result<Option<String>, String>> {
    Box::pin(async move {
        let app = arg_str(&a, "app").ok_or("app is required")?.to_string();
        let (name, p) = app_process(&app)?;
        let found = blocking(move || pids(&p)).await?;
        if found.is_empty() {
            return Err(format!("{name} isn't running"));
        }
        Ok(Some(format!("Any unsaved work in {name} will be lost.")))
    })
}

async fn force_quit(a: Value) -> Outcome {
    let Some(app) = arg_str(&a, "app").map(String::from) else { return err("app is required") };
    let (name, p) = match app_process(&app) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let r = blocking(move || {
        let uid = uid();
        if pids(&p).is_empty() {
            return Err("not running".to_string());
        }
        run(&["pkill", "-TERM", "-u", &uid, "-i", "-x", &p]);
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            if pids(&p).is_empty() {
                return Ok(());
            }
        }
        run(&["pkill", "-KILL", "-u", &uid, "-i", "-x", &p]);
        std::thread::sleep(std::time::Duration::from_millis(300));
        if pids(&p).is_empty() { Ok(()) } else { Err("it is still running".into()) }
    })
    .await
    .and_then(|r| r);
    match r {
        Ok(()) => Outcome::ok(format!("{name} was force quit")),
        Err(e) => err(format!("couldn't force quit {name}: {e}")),
    }
}

// ---- installing apps and updates ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum InstallPlan {
    Snap { name: String, version: String, publisher: String, summary: String, classic: bool },
    Deb { package: String, version: String },
}

/// `snap info NAME`: (publisher, summary, stable version, classic?, installed?).
pub fn parse_snap_info(text: &str) -> Option<(String, String, String, bool, bool)> {
    let mut publisher = String::new();
    let mut summary = String::new();
    let mut version = String::new();
    let mut classic = false;
    let mut installed = false;
    let mut seen_name = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            seen_name = !v.trim().is_empty();
        } else if let Some(v) = line.strip_prefix("summary:") {
            summary = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("publisher:") {
            publisher = v.trim().trim_end_matches('*').trim_end_matches('✓').trim().to_string();
        } else if line.starts_with("installed:") {
            installed = true;
        } else if let Some(v) = t.strip_prefix("latest/stable:").or_else(|| t.strip_prefix("stable:"))
            && version.is_empty()
        {
            let v = v.trim();
            if v != "--" && v != "^" {
                version = v.split_whitespace().next().unwrap_or("").to_string();
                classic |= v.split_whitespace().any(|w| w == "classic");
            }
        }
    }
    (seen_name && !version.is_empty()).then_some((publisher, summary, version, classic, installed))
}

/// `apt-cache policy PKG`: (installed, candidate).
pub fn parse_apt_policy(text: &str) -> Option<(Option<String>, String)> {
    let field = |k: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().to_string()))
    };
    let candidate = field("Candidate:").filter(|c| c != "(none)")?;
    let installed = field("Installed:").filter(|c| c != "(none)");
    Some((installed, candidate))
}

fn package_name(app: &str) -> String {
    app.trim().to_lowercase().trim_start_matches("the ").trim_end_matches(" app").replace(' ', "-")
}

fn plan_install(a: &Value) -> Result<InstallPlan, String> {
    let app = arg_str(a, "name").ok_or("name is required")?;
    let name = package_name(app);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-.+".contains(c)) {
        return Err(format!("“{app}” isn't a package name"));
    }
    let want = arg_str(a, "source");
    if want != Some("deb") && have("snap") {
        let (ok, _, out) = run(&["snap", "info", &name]);
        if ok && let Some((publisher, summary, version, classic, installed)) = parse_snap_info(&out) {
            if installed {
                return Err(format!("{app} is already installed (snap)"));
            }
            return Ok(InstallPlan::Snap { name, version, publisher, summary, classic });
        }
    }
    if want != Some("snap") {
        let (_, _, out) = run(&["apt-cache", "policy", &name]);
        if let Some((installed, candidate)) = parse_apt_policy(&out) {
            if installed.is_some() {
                return Err(format!("{app} is already installed (deb)"));
            }
            return Ok(InstallPlan::Deb { package: name, version: candidate });
        }
    }
    // Suggest close snap names so the model can try again.
    let mut hint = String::new();
    if have("snap") {
        let (ok, _, out) = run(&["snap", "find", app]);
        if ok {
            let names: Vec<&str> = out.lines().skip(1).filter_map(|l| l.split_whitespace().next()).take(5).collect();
            if !names.is_empty() {
                hint = format!(" Similar snaps: {}. Call install_app again with one of these names if it is what the user meant.", names.join(", "));
            }
        }
    }
    Err(format!("no snap or Ubuntu package called “{name}” was found.{hint}"))
}

fn plan_detail(p: &InstallPlan) -> String {
    match p {
        InstallPlan::Snap { name, version, publisher, summary, classic } => format!(
            "{name} {version} from the Snap Store{}{}{}",
            if publisher.is_empty() { String::new() } else { format!(", published by {publisher}") },
            if summary.is_empty() { String::new() } else { format!(": {summary}") },
            if *classic { "\nThis snap runs without the usual sandbox (classic confinement)." } else { "" }
        ),
        InstallPlan::Deb { package, version } => format!("{package} {version} from the Ubuntu archive (deb)"),
    }
}

fn install_preflight(a: Value) -> LocalFuture<Result<Option<String>, String>> {
    Box::pin(async move { blocking(move || plan_install(&a)).await?.map(|p| Some(plan_detail(&p))) })
}

async fn install_app(a: Value) -> Outcome {
    let r = blocking(move || {
        let plan = plan_install(&a)?;
        let (p, terminal) = match &plan {
            InstallPlan::Snap { name, classic, .. } => {
                let mut argv = vec!["snap", "install", name.as_str()];
                if *classic {
                    argv.push("--classic");
                }
                let (ok, _, out) = run(&argv);
                if ok {
                    return Ok(format!("Installed {name} (snap)"));
                }
                if !needs_auth(&out) {
                    return Err(last_lines(&out));
                }
                (privileged(&argv), format!("sudo {}", argv.join(" ")))
            }
            InstallPlan::Deb { package, .. } => (
                privileged(&["env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "install", "-y", package]),
                format!("sudo apt-get install {package}"),
            ),
        };
        let name = match &plan {
            InstallPlan::Snap { name, .. } => name.clone(),
            InstallPlan::Deb { package, .. } => package.clone(),
        };
        match p {
            Privileged::Ran { ok: true, .. } => Ok(format!("Installed {name}")),
            Privileged::Ran { ok: false, output } => Err(last_lines(&output)),
            Privileged::Dismissed => Err("the password prompt was dismissed; nothing was installed".into()),
            Privileged::NeedsTerminal => umoja_common::terminal::spawn_in_terminal(&terminal)
                .map(|_| format!("Opened a terminal to install {name}; it asks for the password there"))
                .map_err(|e| e.to_string()),
        }
    })
    .await
    .and_then(|r| r);
    match r {
        Ok(t) => Outcome::ok(t),
        Err(e) => err(e),
    }
}

fn updates_summary() -> Result<String, String> {
    let mut debs: Vec<String> = Vec::new();
    if have("apt") {
        let (_, _, out) = run(&["apt", "list", "--upgradable"]);
        debs = out
            .lines()
            .filter(|l| l.contains("[upgradable"))
            .filter_map(|l| l.split('/').next().map(String::from))
            .collect();
    }
    let mut snaps: Vec<String> = Vec::new();
    if have("snap") {
        let (ok, _, out) = run(&["snap", "refresh", "--list"]);
        if ok {
            snaps = out.lines().skip(1).filter_map(|l| l.split_whitespace().next().map(String::from)).collect();
        }
    }
    if debs.is_empty() && snaps.is_empty() {
        return Ok("Everything is up to date (as of the last package list refresh).".into());
    }
    let show = |v: &[String]| {
        let mut s = v.iter().take(12).cloned().collect::<Vec<_>>().join(", ");
        if v.len() > 12 {
            s.push_str(&format!(" and {} more", v.len() - 12));
        }
        s
    };
    let mut parts = Vec::new();
    if !debs.is_empty() {
        parts.push(format!("{} deb package update{}: {}", debs.len(), if debs.len() == 1 { "" } else { "s" }, show(&debs)));
    }
    if !snaps.is_empty() {
        parts.push(format!("{} snap update{}: {}", snaps.len(), if snaps.len() == 1 { "" } else { "s" }, show(&snaps)));
    }
    Ok(parts.join("\n"))
}

async fn install_updates() -> Outcome {
    let r = blocking(|| {
        let mut done = Vec::new();
        if have("apt-get") {
            match privileged(&["sh", "-c", "apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get -y -q full-upgrade"]) {
                Privileged::Ran { ok: true, .. } => done.push("system packages updated".to_string()),
                Privileged::Ran { ok: false, output } => return Err(last_lines(&output)),
                Privileged::Dismissed => return Err("the password prompt was dismissed; nothing was updated".into()),
                Privileged::NeedsTerminal => {
                    return umoja_common::terminal::spawn_in_terminal("sudo apt update && sudo apt full-upgrade && sudo snap refresh")
                        .map(|_| "Opened a terminal to install the updates; it asks for the password there".to_string())
                        .map_err(|e| e.to_string());
                }
            }
        }
        if have("snap") {
            let (ok, _, out) = run(&["snap", "refresh"]);
            if ok {
                done.push("snaps refreshed".into());
            } else if needs_auth(&out) {
                if let Privileged::Ran { ok: true, .. } = privileged(&["snap", "refresh"]) {
                    done.push("snaps refreshed".into());
                }
            } else {
                done.push(format!("snap refresh failed: {}", last_lines(&out)));
            }
        }
        Ok(format!("Updates installed: {}", done.join("; ")))
    })
    .await
    .and_then(|r| r);
    match r {
        Ok(t) => Outcome::ok(t),
        Err(e) => err(e),
    }
}

// ---- facts the model can't know ------------------------------------------------------------

fn date_time() -> String {
    let Ok(now) = glib::DateTime::now_local() else { return "error: no clock".into() };
    let f = |s: &str| now.format(s).map(|g| g.to_string()).unwrap_or_default();
    let tz = std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| p.to_string_lossy().split("zoneinfo/").nth(1).map(String::from))
        .unwrap_or_else(|| f("%Z"));
    format!(
        "{}\nTime zone: {tz} (UTC{})\nWeek {} of {}",
        f("%A, %-d %B %Y, %H:%M"),
        f("%:z"),
        f("%V"),
        f("%G")
    )
}

fn system_info() -> String {
    let read = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default();
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        .unwrap_or_else(|| "Linux".into());
    let cpuinfo = read("/proc/cpuinfo");
    let cpu = cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("model name").and_then(|v| v.split_once(':')).map(|(_, v)| v.trim().to_string()))
        .unwrap_or_default();
    let threads = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
    let mem_kb: u64 = read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.split_whitespace().next()?.parse().ok()))
        .unwrap_or(0);
    let up: f64 = read("/proc/uptime").split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let (d, h, m) = ((up / 86400.0) as u64, (up as u64 % 86400) / 3600, (up as u64 % 3600) / 60);
    let uptime = match d {
        0 => format!("{h} h {m} min"),
        _ => format!("{d} days {h} h"),
    };
    let model = [read("/sys/class/dmi/id/sys_vendor"), read("/sys/class/dmi/id/product_version"), read("/sys/class/dmi/id/product_name")]
        .into_iter()
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none") && !s.contains("To be filled"))
        .collect::<Vec<_>>()
        .join(" ");
    let (_, _, ips) = run(&["ip", "-brief", "address"]);
    let ips: Vec<String> = ips
        .lines()
        .filter(|l| !l.starts_with("lo ") && l.contains(" UP "))
        .map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            format!("{}: {}", w[0], w[2..].iter().map(|a| a.split('/').next().unwrap_or(a)).collect::<Vec<_>>().join(", "))
        })
        .collect();
    let mut out = vec![
        format!("OS: {os}, Umoja {}", env!("CARGO_PKG_VERSION")),
        format!("Kernel: {}", read("/proc/sys/kernel/osrelease")),
        format!("Host name: {}", read("/proc/sys/kernel/hostname")),
    ];
    if !model.is_empty() {
        out.push(format!("Computer: {model}"));
    }
    out.push(format!("CPU: {cpu} ({threads} threads)"));
    out.push(format!("Memory: {:.1} GB", mem_kb as f64 / 1024.0 / 1024.0));
    out.push(format!("Uptime: {uptime}"));
    if !ips.is_empty() {
        out.push(format!("IP addresses: {}", ips.join("; ")));
    }
    out.join("\n")
}

// ---- run_command redirects --------------------------------------------------------------

/// What to do instead of a shell command that has a dedicated tool.
#[derive(Debug, Clone, PartialEq)]
pub enum Redirect {
    /// Run this tool instead (its own plain confirmation applies).
    Call(&'static str, Value),
    /// Tell the model which tool to use; nothing runs and nobody is asked.
    Hint(&'static str),
}

/// A shell command a model reached for that one of our tools does better.
pub fn redirect(command: &str) -> Option<Redirect> {
    use Redirect::*;
    let c = command.trim().to_lowercase();
    let c = c.strip_prefix("sudo ").unwrap_or(&c).trim();
    let words: Vec<&str> = c.split_whitespace().collect();
    let first = Path::new(words.first()?).file_name()?.to_str()?;
    let has = |w: &str| words.contains(&w);
    let session = |action: &str| Some(Call("session", json!({"action": action})));
    match first {
        "poweroff" | "halt" => return session("power_off"),
        "reboot" => return session("restart"),
        "shutdown" => return if has("-r") || has("--reboot") { session("restart") } else { session("power_off") },
        "systemctl" if has("poweroff") || has("halt") => return session("power_off"),
        "systemctl" if has("reboot") => return session("restart"),
        "systemctl" if has("suspend") || has("hibernate") || has("hybrid-sleep") => return session("suspend"),
        "loginctl" if has("lock-session") || has("lock-sessions") => return session("lock"),
        "loginctl" if has("terminate-session") || has("terminate-user") || has("kill-session") => return session("log_out"),
        "gnome-session-quit" => {
            return if has("--power-off") { session("power_off") } else if has("--reboot") { session("restart") } else { session("log_out") };
        }
        "xdg-screensaver" | "gnome-screensaver-command" | "dm-tool" if c.contains("lock") => return session("lock"),
        "date" | "timedatectl" if words.len() == 1 || has("status") => return Some(Call("date_time", json!({}))),
        "cal" | "ncal" => return Some(Call("date_time", json!({}))),
        "uname" | "lsb_release" | "hostnamectl" | "neofetch" | "fastfetch" | "hostname" => return Some(Call("system_info", json!({}))),
        "df" => return Some(Call("disk_usage", json!({}))),
        "free" | "top" | "htop" | "uptime" => return Some(Call("system_resources", json!({}))),
        _ => {}
    }
    if c.contains("cat /etc/os-release") || c.contains("/proc/cpuinfo") || c.contains("/proc/meminfo") {
        return Some(Call("system_info", json!({})));
    }
    if c.contains("gio trash --empty") || c.contains(".local/share/trash") {
        return Some(Call("empty_trash", json!({})));
    }
    if first == "apt" && has("list") && has("--upgradable") || first == "snap" && has("refresh") && has("--list") {
        return Some(Call("check_updates", json!({})));
    }
    let hint = match first {
        "grim" | "gnome-screenshot" | "scrot" | "spectacle" | "flameshot" | "import" | "maim" => {
            "Don't run a command for screenshots: call take_screenshot (target window, screen or area)."
        }
        "wf-recorder" | "ffmpeg" if c.contains("x11grab") || first == "wf-recorder" => {
            "Use screen_recording (action start or stop) instead of a command."
        }
        "wpctl" | "pactl" | "amixer" | "pamixer" | "pulsemixer" => {
            if has("set-default") || has("set-default-sink") || has("set-default-source") {
                "Use set_audio_device to switch speakers, headphones or the microphone."
            } else {
                "Use set_volume (level, change or mute) instead of a command."
            }
        }
        "brightnessctl" | "light" | "xbacklight" => "Use set_brightness instead of a command.",
        "playerctl" => "Use media_control (or now_playing) instead of a command.",
        "nmcli" if has("connect") => "Use connect_wifi with the network name instead of a command.",
        "nmcli" if has("radio") => "Use set_wifi or set_airplane_mode instead of a command.",
        "rfkill" => "Use set_airplane_mode, set_wifi or set_bluetooth instead of a command.",
        "bluetoothctl" if has("power") => "Use set_bluetooth instead of a command.",
        "bluetoothctl" => "Use bluetooth_device (connect, disconnect or list) instead of a command.",
        "xdg-open" | "firefox" | "google-chrome" | "chromium" | "sensible-browser" | "x-www-browser"
            if c.contains("http") || c.contains("www.") =>
        {
            "Use open_url instead of a command."
        }
        "xdg-open" | "nautilus" => "Use open_folder (for Downloads, Documents...) or open_file instead of a command.",
        "apt" | "apt-get" | "aptitude" | "snap" | "flatpak" if has("install") => {
            "Use install_app with the app's name; it finds the package and asks the user itself."
        }
        "apt" | "apt-get" | "snap" | "flatpak" if has("remove") || has("purge") || has("uninstall") => {
            "Use remove_app with the app's name; it works out how it was installed and asks the user itself."
        }
        "apt" | "apt-get" | "snap" | "flatpak" if has("upgrade") || has("full-upgrade") || has("dist-upgrade") || has("update") || has("refresh") => {
            "Use check_updates to see updates and install_updates to install them."
        }
        "rm" | "find" | "gio" if c.contains("-delete") || c.contains("trash") || first == "rm" => {
            "Use clear_folder to move a folder's contents to the Trash (or empty_trash), instead of a command."
        }
        "kill" | "pkill" | "killall" | "xkill" => "Use force_quit_app with the app's name (or close_window) instead of a command.",
        "udisksctl" | "umount" | "eject" => "Use eject_drive instead of a command.",
        "gsettings" if has("set") || has("reset") => {
            "Use list_settings and change_setting (or set_gsetting for other org.gnome keys) instead of a command."
        }
        "gnome-control-center" => "Use open_settings with the panel name instead of a command.",
        "gnome-extensions" => "GNOME Shell extensions don't run on Umoja.",
        _ => return None,
    };
    Some(Hint(hint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_parsing_and_summaries() {
        assert_eq!(parse_volume("Volume: 0.45\n"), Some((45, false)));
        assert_eq!(parse_volume("Volume: 0.30 [MUTED]"), Some((30, true)));
        assert_eq!(parse_volume("nope"), None);
        assert_eq!(volume_summary(&json!({"level": 30})), "Set the volume to 30%");
        assert_eq!(volume_summary(&json!({"mute": true, "device": "microphone"})), "Mute the microphone");
        assert_eq!(volume_summary(&json!({"change": "down"})), "Turn the volume down");
    }

    #[test]
    fn wpctl_status() {
        let s = "PipeWire 'pipewire-0' [1.4.7, ken@umoja, cookie:1]\n └─ Clients:\n        33. WirePlumber\n\nAudio\n ├─ Devices:\n │      44. Built-in Audio                      [alsa]\n │  \n ├─ Sinks:\n │  *   47. Built-in Audio Analog Stereo        [vol: 0.40]\n │      61. Jabra Evolve2 65                    [vol: 1.00]\n │  \n ├─ Sources:\n │  *   48. Built-in Audio Digital Microphone   [vol: 0.55]\n │  \n ├─ Filters:\n │  \n └─ Streams:\n        70. Firefox\n\nVideo\n ├─ Devices:\n ├─ Sinks:\n │      90. Not audio\n";
        let n = parse_wpctl_status(s);
        assert_eq!(n.len(), 3, "{n:?}");
        assert_eq!(n[0], AudioNode { id: 47, name: "Built-in Audio Analog Stereo".into(), default: true, input: false });
        assert_eq!(n[1].name, "Jabra Evolve2 65");
        assert!(!n[1].default);
        assert!(n[2].input && n[2].default);
    }

    #[test]
    fn brightness_math() {
        assert_eq!(brightness_target(0, 100, 50, Some(30), true), 30);
        assert_eq!(brightness_target(0, 100, 50, None, true), 60);
        assert_eq!(brightness_target(0, 100, 5, None, false), 1);
        assert_eq!(brightness_target(10, 1010, 1010, None, true), 1010);
    }

    #[test]
    fn snap_and_apt_parsing() {
        let info = "name:      vlc\nsummary:   The ultimate media player\npublisher: VideoLAN✓\nlicense:   GPL-2.0+\ndescription: |\n  VLC\nchannels:\n  latest/stable:    3.0.21    2024-06-13 (3777) 341MB -\n  latest/candidate: 3.0.21    2024-06-13 (3777) 341MB -\n";
        assert_eq!(
            parse_snap_info(info),
            Some(("VideoLAN".into(), "The ultimate media player".into(), "3.0.21".into(), false, false))
        );
        let classic = "name: code\nsummary: Code editing.\npublisher: Visual Studio Code (vscode✓)\nchannels:\n  latest/stable:    e170252f 2025-01-01 (180) 330MB classic\ninstalled: e170252f (180) 330MB classic\n";
        let p = parse_snap_info(classic).unwrap();
        assert!(p.3 && p.4);
        assert_eq!(parse_snap_info("error: no snap found for \"zzz\""), None);
        let pol = "vlc:\n  Installed: (none)\n  Candidate: 3.0.21-1\n  Version table:\n";
        assert_eq!(parse_apt_policy(pol), Some((None, "3.0.21-1".into())));
        assert_eq!(parse_apt_policy("foo:\n  Installed: (none)\n  Candidate: (none)\n"), None);
        assert_eq!(parse_apt_policy(""), None);
        assert_eq!(package_name("The GIMP"), "gimp");
        assert_eq!(package_name("Visual Studio Code"), "visual-studio-code");
    }

    #[test]
    fn bluetooth_devices() {
        let d = parse_bt_devices("Device 11:22:33:44:55:66 WH-1000XM4\nDevice AA:BB:CC:DD:EE:FF MX Master 3\n");
        assert_eq!(d[1], ("AA:BB:CC:DD:EE:FF".into(), "MX Master 3".into()));
    }

    #[test]
    fn urls_and_folders() {
        assert_eq!(web_uri(&json!({"url": "ubuntu.com"})).unwrap(), "https://ubuntu.com");
        assert_eq!(web_uri(&json!({"url": "https://x.org/a"})).unwrap(), "https://x.org/a");
        assert_eq!(web_uri(&json!({"query": "rust gtk4"})).unwrap(), "https://duckduckgo.com/?q=rust%20gtk4");
        assert_eq!(folder_uri("Trash").unwrap(), "trash:///");
        assert_eq!(folder_label("downloads"), "Downloads folder");
        assert_eq!(folder_label("my trash"), "the Trash");
        assert!(clearable_folder("~").is_err());
        assert!(clearable_folder("/etc").is_err());
    }

    #[test]
    fn process_names() {
        assert_eq!(process_name("/snap/bin/firefox %u").as_deref(), Some("firefox"));
        assert_eq!(process_name("env BAMF=1 /usr/bin/gnome-text-editor").as_deref(), Some("gnome-text-edit"));
        assert_eq!(process_name("Discord").as_deref(), Some("Discord"));
    }

    #[test]
    fn shell_commands_go_to_tools() {
        let call = |c: &str| match redirect(c) {
            Some(Redirect::Call(t, a)) => format!("{t} {a}"),
            Some(Redirect::Hint(h)) => format!("hint: {h}"),
            None => "none".into(),
        };
        assert_eq!(call("sudo shutdown -h now"), r#"session {"action":"power_off"}"#);
        assert_eq!(call("shutdown -r now"), r#"session {"action":"restart"}"#);
        assert_eq!(call("systemctl reboot"), r#"session {"action":"restart"}"#);
        assert_eq!(call("systemctl suspend"), r#"session {"action":"suspend"}"#);
        assert_eq!(call("gnome-session-quit --logout --no-prompt"), r#"session {"action":"log_out"}"#);
        assert_eq!(call("loginctl lock-session"), r#"session {"action":"lock"}"#);
        assert_eq!(call("/usr/bin/poweroff"), r#"session {"action":"power_off"}"#);
        assert_eq!(call("date"), "date_time {}");
        assert_eq!(call("lsb_release -a"), "system_info {}");
        assert_eq!(call("gio trash --empty"), "empty_trash {}");
        assert!(call("grim ~/shot.png").contains("take_screenshot"));
        assert!(call("wpctl set-volume @DEFAULT_AUDIO_SINK@ 50%").contains("set_volume"));
        assert!(call("sudo apt install vlc").contains("install_app"));
        assert!(call("snap remove discord").contains("remove_app"));
        assert!(call("sudo apt update && sudo apt upgrade").contains("install_updates"));
        assert!(call("xdg-open https://ubuntu.com").contains("open_url"));
        assert!(call("pkill firefox").contains("force_quit_app"));
        assert!(call("find ~/Downloads -mindepth 1 -exec gio trash {} +").contains("clear_folder"));
        assert!(call("rm -rf ~/Downloads/*").contains("clear_folder"));
        assert_eq!(call("ls ~/Downloads"), "none");
        assert_eq!(call("date +%s"), "none");
        assert_eq!(call("systemctl status bluetooth"), "none");
    }
}
