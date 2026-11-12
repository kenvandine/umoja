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
            "read_aloud",
            "Read text out loud in Umoja's voice. Use when the user asks you to say, speak or read something aloud (\"read that to me\", \"say it out loud\"); pass the words to speak, usually your answer. stop=true stops reading. This is not the screen reader.",
            obj(json!({
                "text": {"type": "string", "description": "What to say, in plain words"},
                "stop": {"type": "boolean", "description": "Stop reading instead"}
            }), &[]),
            |a| Box::pin(read_aloud(a)),
        )
        .with_summary(|a| if arg_bool(a, "stop") == Some(true) {
            "Stop reading aloud".into()
        } else {
            "Read aloud".into()
        }),
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
            "open_in_terminal",
            "Run a command-line program or command in a new window of the user's default terminal, where they can see and use it (btop, htop, a script). The user is asked first. If the program isn't installed this says so: install it with install_app, then call open_in_terminal again. Never start a terminal app such as gnome-terminal yourself; to open a program by name, launch_app is enough.",
            obj(json!({"command": {"type": "string", "description": "The command line, e.g. \"btop\""}}), &["command"]),
            |a| Box::pin(open_in_terminal(a)),
        )
        .confirm()
        .with_preflight(open_in_terminal_preflight)
        .with_summary(|a| format!("Run \"{}\" in a terminal", arg_str(a, "command").unwrap_or(""))),
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
        Tool::new(
            "hand_off_to_agent",
            "Hand a coding job to the user's AI coding agent (Claude Code, Codex, Gemini CLI or OpenCode, whichever is installed and chosen): writing a script or program, changing code in a project, fixing a build. It opens in a terminal with the task as its first message and asks the user before changing anything. Pass the whole task in the user's words, and a folder when the job is about one.",
            obj(json!({
                "task": {"type": "string", "description": "The job, fully described"},
                "folder": {"type": "string", "description": "Optional project folder to start in"}
            }), &["task"]),
            |a| Box::pin(async move { hand_off(&a).hide_if_ok() }),
        )
        .with_risk(crate::tool::medium)
        .with_summary(|a| {
            let who = umoja_common::agents::chosen().map(|g| g.name).unwrap_or("a coding agent");
            match arg_str(a, "task") {
                Some(t) => format!("Open {who} to: {}", truncate_words(t, 80)),
                None => format!("Open {who}"),
            }
        }),
        Tool::new(
            "free_ai_memory",
            "Unload the assistant's local AI model from memory, to free RAM and graphics memory for a game, a build or a VM. It loads again by itself with the next question (which then takes a few seconds longer).",
            obj(json!({}), &[]),
            |_| Box::pin(async {
                Outcome::from(blocking(|| match umoja_common::lemonade::unload() {
                    Ok(true) => Ok("Unloaded the local AI model; its memory is free until the next question.".to_string()),
                    Ok(false) => Ok("The local AI server isn't running, so it isn't using any memory.".to_string()),
                    Err(e) => Err(format!("Couldn't unload the model: {e}")),
                }).await.and_then(|r| r))
            }),
        )
        .with_summary(|_| "Free the AI model's memory".into()),
        Tool::new(
            "report_crash",
            "Open Ubuntu's crash reporter for the newest crash of a program, so the user can send it to Ubuntu's developers. Nothing is sent unless the user chooses to in that window.",
            obj(json!({"program": {"type": "string", "description": "Program name, e.g. nautilus; empty for the newest crash"}}), &[]),
            |a| Box::pin(async move { Outcome::from(blocking(move || report_crash(&a)).await.and_then(|r| r)) }),
        )
        .with_summary(|a| match arg_str(a, "program").filter(|p| !p.is_empty()) {
            Some(p) => format!("Open the crash report for {p}"),
            None => "Open the crash report".into(),
        }),
    ]
}

fn truncate_words(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

fn hand_off(a: &Value) -> Outcome {
    use umoja_common::agents;
    let Some(task) = arg_str(a, "task").filter(|t| !t.trim().is_empty()) else {
        return err("task is required");
    };
    let Some(agent) = agents::chosen() else {
        return err(if agents::installed().is_empty() {
            "No coding agent is installed (Claude Code, Codex, Gemini CLI or OpenCode). Tell the user they can install one, for example Claude Code, and Umoja will connect it at the next login."
        } else {
            "The coding agent hand-off is turned off (org.umoja.shell ai-agent is 'none')."
        });
    };
    let folder = arg_str(a, "folder").map(expand);
    match agents::launch(agent, task, folder.as_deref()) {
        Ok(()) => Outcome::ok(format!(
            "Opened {} in a terminal with the task. It will ask the user before it changes anything.",
            agent.name
        )),
        Err(e) => err(format!("Couldn't open {}: {e}", agent.name)),
    }
}

/// Hand the Apport report for a crash to `ubuntu-bug`, which shows what
/// will be sent and asks before sending anything.
fn report_crash(a: &Value) -> Result<String, String> {
    use umoja_common::crashes::{self, Source};
    let want = arg_str(a, "program").unwrap_or("").trim().to_lowercase();
    let crashes = crashes::recent(30);
    let crash = crashes
        .iter()
        .find(|c| want.is_empty() || c.program().to_lowercase().contains(&want))
        .ok_or_else(|| {
            if want.is_empty() {
                "There are no crash reports from the last 30 days.".to_string()
            } else {
                format!("There is no recent crash report for {want}.")
            }
        })?;
    let Source::Apport(path) = &crash.source else {
        return Err(format!(
            "{} has no Apport report, only a core dump (coredumpctl), so it can't be sent with Ubuntu's reporter. Report it to the program's developers instead.",
            crash.program()
        ));
    };
    if crash.package.is_none() {
        return Err(format!(
            "{} isn't from an Ubuntu package, so Ubuntu's reporter won't take it. Report it to the program's developers instead.",
            crash.program()
        ));
    }
    if !have("ubuntu-bug") {
        return Err("Ubuntu's crash reporter isn't installed (the apport package).".into());
    }
    let mut child = std::process::Command::new("ubuntu-bug")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Couldn't start ubuntu-bug: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(format!(
        "Opened Ubuntu's crash reporter for {}. It shows what would be sent and only sends it if the user chooses to.",
        crash.program()
    ))
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

/// Ask Umoja's overlay to speak (it owns the voice, see umoja-overlay's
/// speech module).
async fn read_aloud(a: Value) -> Outcome {
    let stop = arg_bool(&a, "stop") == Some(true);
    let text = arg_str(&a, "text").unwrap_or("").trim().to_owned();
    if !stop && text.is_empty() {
        return Outcome::err("nothing to read: pass text");
    }
    let c = match session_bus().await {
        Ok(c) => c,
        Err(e) => return Outcome::err(e),
    };
    let (method, args) = if stop {
        ("StopSpeaking", None)
    } else {
        ("Speak", Some((text.as_str(),).to_variant()))
    };
    match c
        .call_future(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            method,
            args.as_ref(),
            None,
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await
    {
        Ok(_) if stop => Outcome::ok("Stopped reading."),
        Ok(_) => Outcome::ok(
            "Reading it aloud now. Answer with one short sentence saying so (\"Reading it out now.\"), \
             nothing else: don't repeat the text and don't bring up other topics.",
        ),
        Err(e) => Outcome::err(format!("could not read aloud: {e}")),
    }
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

/// The new level for `level` (absolute) or `change` (10% steps, as the
/// old `wpctl set-volume 10%+` did), kept within 0–100%.
pub fn volume_target(current: u32, level: Option<u64>, change: Option<&str>) -> Option<u32> {
    match (level, change) {
        (Some(l), _) => Some(l.min(100) as u32),
        (None, Some("down" | "lower" | "decrease")) => Some(current.saturating_sub(10)),
        (None, Some(_)) => Some((current + 10).min(100.max(current))),
        _ => None,
    }
}

async fn set_volume(a: Value) -> Outcome {
    let mic = is_mic(&a);
    let level = arg_u64(&a, "level").map(|l| l.min(100));
    let change = arg_str(&a, "change").map(str::to_lowercase);
    let mute = arg_bool(&a, "mute");
    let change2 = change.clone();
    let res = blocking(move || -> Result<(u32, bool), String> {
        let mut c = crate::pulse::Client::connect()?;
        let dev = c.default_device(mic)?;
        if let Some(m) = mute {
            c.set_mute(mic, m)?;
        }
        if let Some(pct) = volume_target(dev.percent(), level, change2.as_deref()) {
            // Changing the level unmutes, as the volume keys do.
            if mute.is_none() && dev.muted {
                c.set_mute(mic, false)?;
            }
            c.set_volume(mic, &crate::pulse::scaled(dev.volume, pct))?;
        }
        let now = c.default_device(mic)?;
        Ok((now.percent(), now.muted))
    })
    .await
    .unwrap_or_else(Err);
    let (pct, muted) = match res {
        Ok(v) => v,
        Err(e) => return err(e),
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

/// The device `want` names: its description, all its words, or a
/// headphones / speakers / TV alias.
pub fn find_device<'a>(devices: &'a [crate::pulse::Device], want: &str) -> Option<&'a crate::pulse::Device> {
    let want = want.to_lowercase();
    let words: Vec<&str> = want.split_whitespace().collect();
    let name = |d: &crate::pulse::Device| d.description.to_lowercase();
    devices
        .iter()
        .find(|d| name(d).contains(&want))
        .or_else(|| devices.iter().find(|d| words.iter().all(|w| name(d).contains(w))))
        .or_else(|| {
            // "headphones"/"speakers" on the built-in card are ports, not devices.
            let alias = match want.as_str() {
                w if w.contains("headphone") || w.contains("headset") => &["headphone", "headset", "usb", "bluetooth"][..],
                w if w.contains("speaker") || w.contains("built") => &["built-in", "speaker", "analog"][..],
                w if w.contains("tv") || w.contains("monitor") => &["hdmi", "displayport"][..],
                _ => &[][..],
            };
            devices.iter().find(|d| alias.iter().any(|w| name(d).contains(w)))
        })
}

async fn set_audio_device(a: Value) -> Outcome {
    let input = matches!(arg_str(&a, "kind"), Some("input")) || is_mic(&json!({"device": arg_str(&a, "kind").unwrap_or("")}));
    let want = arg_str(&a, "device").map(str::to_string);
    let res = blocking(move || -> Result<String, String> {
        let mut c = crate::pulse::Client::connect()?;
        let devices = c.devices(input)?;
        let list = || {
            devices
                .iter()
                .map(|d| format!("- {}{}", d.description, if d.default { " (in use)" } else { "" }))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let Some(want) = want else {
            return Ok(format!("{} devices:\n{}", if input { "Input" } else { "Output" }, list()));
        };
        let Some(dev) = find_device(&devices, &want) else {
            return Err(format!("no sound device matches “{want}”. Devices:\n{}", list()));
        };
        c.set_default(input, &dev.name)?;
        Ok(format!("Now using {} for {}", dev.description, if input { "the microphone" } else { "sound" }))
    })
    .await
    .unwrap_or_else(Err);
    match res {
        Ok(m) => Outcome::ok(m),
        Err(e) => err(e),
    }
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

/// Words that start a shell command without naming a program to look for.
const SHELL_WORDS: &[&str] = &[
    "cd", "echo", "export", "source", ".", "exec", "time", "nohup", "env", "for", "while", "if",
    "test", "[", "true", "false", "read", "printf", "set", "ulimit", "umask", "alias", "type",
];

/// The program `command` runs, when it can't be found in `dirs` (or as a
/// path): "btop -t" with no btop anywhere → Some("btop"). Shell words,
/// sudo and leading VAR=value are skipped; anything that can't be told
/// for sure counts as installed.
fn missing_program(command: &str, dirs: &[PathBuf]) -> Option<String> {
    let mut words = command.split_whitespace().skip_while(|w| {
        *w == "sudo" || (w.contains('=') && !w.starts_with('=') && !w.starts_with('-'))
    });
    let first = words.next()?.trim_matches(|c| c == '\'' || c == '"');
    if first.is_empty()
        || SHELL_WORDS.contains(&first)
        || first.contains(['$', '`', '(', ')', '{', '}', '|', ';', '&', '<', '>', '*', '~'])
    {
        return None;
    }
    if first.contains('/') {
        return (!Path::new(first).exists()).then(|| first.to_string());
    }
    let found = dirs.iter().any(|d| {
        let p = d.join(first);
        p.is_file()
            && std::fs::metadata(&p)
                .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0)
                .unwrap_or(false)
    });
    (!found).then(|| first.to_string())
}

/// Where the user's shell would look for a program: $PATH, plus the
/// places a login shell adds that a session service may lack.
fn program_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let extra = [
        Some(PathBuf::from("/snap/bin")),
        Some(PathBuf::from("/usr/games")),
        Some(PathBuf::from("/usr/local/bin")),
        Some(PathBuf::from("/usr/bin")),
        Some(PathBuf::from("/bin")),
        home.as_ref().map(|h| h.join(".local/bin")),
        home.as_ref().map(|h| h.join(".cargo/bin")),
    ];
    for d in extra.into_iter().flatten() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// Don't offer to run a program that isn't installed: the model is told
/// to install it first (install_app prefers the snap), then run it.
fn open_in_terminal_preflight(a: Value) -> LocalFuture<Result<Option<String>, String>> {
    Box::pin(async move {
        let cmd = arg_str(&a, "command").map(str::trim).unwrap_or("").to_string();
        match missing_program(&cmd, &program_dirs()) {
            Some(p) => Err(format!(
                "{p} isn't installed, so nothing was run and the user wasn't asked. Install it first with install_app (name \"{p}\"; it picks the snap when there is one, else the Ubuntu package and asks the user), then call open_in_terminal again with the same command."
            )),
            None => Ok(None),
        }
    })
}

async fn open_in_terminal(a: Value) -> Outcome {
    let Some(cmd) = arg_str(&a, "command").map(str::trim).filter(|c| !c.is_empty()) else {
        return Outcome::err("command is required");
    };
    match umoja_common::terminal::spawn_in_terminal(cmd) {
        Ok(()) => Outcome::ok(format!("Opened a terminal running \"{cmd}\"")).hide(),
        Err(e) => Outcome::err(format!("couldn't open a terminal: {e}")),
    }
}

/// Terminal emulators a model may name in a command ("gnome-terminal --
/// btop"). The user's own terminal is chosen by xdg-terminal-exec instead.
const TERMINALS: &[&str] = &[
    "gnome-terminal", "ptyxis", "kgx", "gnome-console", "konsole", "xterm", "uxterm",
    "x-terminal-emulator", "xdg-terminal-exec", "xfce4-terminal", "mate-terminal",
    "tilix", "terminator", "alacritty", "kitty", "foot", "wezterm", "ghostty",
];

/// "gnome-terminal -- btop" → Some("btop"); a bare "gnome-terminal" →
/// Some(""); anything else → None.
fn terminal_command(command: &str) -> Option<String> {
    let command = command.trim();
    let command = command.strip_prefix("sudo ").unwrap_or(command).trim_start();
    let mut words = command.split_whitespace();
    let first = Path::new(words.next()?).file_name()?.to_str()?.to_lowercase();
    if !TERMINALS.contains(&first.as_str()) {
        return None;
    }
    // Skip the terminal's own options up to the command: "--", "-e",
    // "-x", "--command=" or the first non-option word.
    let rest: Vec<&str> = words.collect();
    let mut i = 0;
    let mut cmd = String::new();
    while i < rest.len() {
        let w = rest[i];
        if matches!(w, "--" | "-e" | "-x" | "--command" | "--execute" | "-c") {
            cmd = rest[i + 1..].join(" ");
            break;
        }
        if let Some(c) = w.strip_prefix("--command=") {
            cmd = std::iter::once(c).chain(rest[i + 1..].iter().copied()).collect::<Vec<_>>().join(" ");
            break;
        }
        if !w.starts_with('-') {
            cmd = rest[i..].join(" ");
            break;
        }
        i += 1;
    }
    let cmd = cmd.trim();
    let unquoted = cmd
        .strip_prefix('\'')
        .and_then(|c| c.strip_suffix('\''))
        .or_else(|| cmd.strip_prefix('"').and_then(|c| c.strip_suffix('"')))
        .unwrap_or(cmd);
    Some(unquoted.trim().to_string())
}

/// A shell command a model reached for that one of our tools does better.
pub fn redirect(command: &str) -> Option<Redirect> {
    use Redirect::*;
    // "gnome-terminal -- btop": the user's terminal may be another one.
    if let Some(cmd) = terminal_command(command) {
        return Some(if cmd.is_empty() {
            Call("launch_app", json!({"name": "Terminal"}))
        } else {
            Call("open_in_terminal", json!({"command": cmd}))
        });
    }
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
        assert_eq!(volume_target(45, Some(130), None), Some(100));
        assert_eq!(volume_target(45, None, Some("down")), Some(35));
        assert_eq!(volume_target(5, None, Some("down")), Some(0));
        assert_eq!(volume_target(95, None, Some("up")), Some(100));
        assert_eq!(volume_target(45, None, None), None);
        assert_eq!(volume_summary(&json!({"level": 30})), "Set the volume to 30%");
        assert_eq!(volume_summary(&json!({"mute": true, "device": "microphone"})), "Mute the microphone");
        assert_eq!(volume_summary(&json!({"change": "down"})), "Turn the volume down");
    }

    #[test]
    fn finds_sound_devices() {
        let dev = |d: &str| crate::pulse::Device {
            name: d.into(),
            description: d.into(),
            default: false,
            input: false,
            volume: Default::default(),
            muted: false,
        };
        let devs = [dev("Built-in Audio Analog Stereo"), dev("Jabra Evolve2 65"), dev("HDA NVidia HDMI 1")];
        assert_eq!(find_device(&devs, "jabra").unwrap().name, "Jabra Evolve2 65");
        assert_eq!(find_device(&devs, "headphones").map(|d| d.name.as_str()), None);
        assert_eq!(find_device(&devs, "speakers").unwrap().name, "Built-in Audio Analog Stereo");
        assert_eq!(find_device(&devs, "the tv").unwrap().name, "HDA NVidia HDMI 1");
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

    #[test]
    fn missing_programs_are_found() {
        let dir = std::env::temp_dir().join(format!("umoja-mcp-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let htop = dir.join("htop");
        std::fs::write(&htop, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&htop, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let dirs = vec![dir.clone()];
        assert_eq!(missing_program("btop", &dirs), Some("btop".into()));
        assert_eq!(missing_program("sudo btop -t", &dirs), Some("btop".into()));
        assert_eq!(missing_program("TERM=xterm btop", &dirs), Some("btop".into()));
        assert_eq!(missing_program("htop", &dirs), None);
        assert_eq!(missing_program("cd /tmp && ls", &dirs), None);
        assert_eq!(missing_program("$EDITOR notes", &dirs), None);
        assert_eq!(missing_program("/no/such/tool", &dirs), Some("/no/such/tool".into()));
        assert_eq!(missing_program("", &dirs), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_terminals_use_the_default_one() {
        assert_eq!(terminal_command("gnome-terminal -- btop").as_deref(), Some("btop"));
        assert_eq!(terminal_command("xterm -e 'htop -d 5'").as_deref(), Some("htop -d 5"));
        assert_eq!(terminal_command("konsole --noclose -e btop").as_deref(), Some("btop"));
        assert_eq!(terminal_command("/usr/bin/kitty btop").as_deref(), Some("btop"));
        assert_eq!(terminal_command("gnome-terminal --command=btop").as_deref(), Some("btop"));
        assert_eq!(terminal_command("gnome-terminal").as_deref(), Some(""));
        assert_eq!(terminal_command("btop"), None);
        assert!(matches!(redirect("gnome-terminal -- btop"), Some(Redirect::Call("open_in_terminal", _))));
        assert!(matches!(redirect("ptyxis"), Some(Redirect::Call("launch_app", _))));
    }
}
