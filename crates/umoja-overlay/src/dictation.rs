//! Myna dictation as the settings popover's Dictation page sets it up: the
//! myna snap (the daemon that listens and types) plus one model snap
//! connected to its `backend` plug.
//!
//! Fresh Umoja installs get Myna with Parakeet on first boot from the copy
//! in the image (data/myna/umoja-myna-setup). Everywhere else this does
//! what Myna Settings' onboarding does, through snapd as the user, so
//! polkit asks for the password: turn on `experimental.user-daemons`
//! (snapd won't install Myna's user daemon without it), install `myna` and
//! the model from latest/edge (the only channel Myna is published to; the
//! model snap's install hook fetches its weights), and connect
//! `myna:backend` to the model's `provider` slot. Changing the model
//! swaps that connection and restarts the daemon, as Myna Settings does.

use std::path::Path;
use std::process::Stdio;

use gtk4::gio::prelude::*;
use umoja_common::{dbus, gsettings as gs};

/// Myna and its models are only published here.
const CHANNEL: &str = "latest/edge";
pub const MYNA_SNAP: &str = "myna";
const MYNA_USER_UNIT: &str = "snap.myna.myna.service";

/// A speech-to-text model Myna can use: one backend snap each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub snap: &'static str,
    pub name: &'static str,
    /// English text, translated when shown.
    pub blurb: &'static str,
}

/// The models Myna knows (myna-core's ModelFamily), the default first:
/// Parakeet is Myna's pick for English and most European languages.
pub const MODELS: [Model; 3] = [
    Model {
        snap: "myna-parakeet",
        name: "Parakeet",
        blurb: "Fastest and most accurate, in English and 24 other languages",
    },
    Model {
        snap: "myna-whisper",
        name: "Whisper",
        blurb: "The most languages, with uneven accuracy",
    },
    Model {
        snap: "myna-funasr",
        name: "FunASR",
        blurb: "Best for Chinese, Japanese and Korean",
    },
];

pub const DEFAULT_MODEL: &str = "myna-parakeet";

/// Whether a snap is installed (without asking snapd).
pub fn installed(snap: &str) -> bool {
    Path::new("/snap").join(snap).join("current").exists()
}

/// snapd is here to install Myna.
pub fn can_install() -> bool {
    crate::speech::can_install_snap()
}

/// The model snap `myna:backend` is connected to, from `snap connections
/// myna` (its line reads "content[inference-provider] myna:backend
/// myna-parakeet:provider -").
pub fn parse_backend(connections: &str) -> Option<String> {
    connections.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        words.find(|w| *w == "myna:backend")?;
        let slot = words.next()?;
        let (snap, slot) = slot.split_once(':')?;
        (slot == "provider" && !snap.is_empty()).then(|| snap.to_owned())
    })
}

pub async fn active_backend() -> Option<String> {
    if !installed(MYNA_SNAP) {
        return None;
    }
    let out = tokio::process::Command::new("/usr/bin/snap")
        .args(["connections", MYNA_SNAP])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    parse_backend(&String::from_utf8_lossy(&out.stdout))
}

/// What setting up or switching to `backend` takes, in order, each as
/// `snap` arguments, except a final `systemctl --user restart` when the
/// daemon was already running with another model.
pub fn plan(backend: &str, myna_installed: bool, backend_installed: bool, current: Option<&str>) -> Vec<Vec<String>> {
    let w = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut steps = Vec::new();
    if !myna_installed {
        steps.push(w(&["set", "system", "experimental.user-daemons=true"]));
        steps.push(w(&["install", "--channel", CHANNEL, MYNA_SNAP]));
    }
    if !backend_installed {
        steps.push(w(&["install", "--channel", CHANNEL, backend]));
    }
    if current == Some(backend) {
        return steps;
    }
    if current.is_some() {
        steps.push(w(&["disconnect", "myna:backend"]));
    }
    steps.push(w(&["connect", "myna:backend", &format!("{backend}:provider")]));
    if current.is_some() {
        steps.push(w(&["restart"]));
    }
    steps
}

/// A step's progress line, for the page.
pub fn step_label(step: &[String]) -> &'static str {
    match step.first().map(String::as_str) {
        Some("set") => "Getting ready…",
        Some("install") if step.last().map(String::as_str) == Some(MYNA_SNAP) => {
            "Installing Myna…"
        }
        Some("install") => "Downloading the speech model… This can take a few minutes.",
        Some("restart") => "Restarting dictation…",
        _ => "Connecting the model…",
    }
}

/// Set Myna up with `backend`, or switch it to `backend`. `progress` hears
/// each step before it runs.
pub async fn use_model(backend: &str, progress: impl Fn(&'static str)) -> Result<(), String> {
    let current = active_backend().await;
    for step in plan(backend, installed(MYNA_SNAP), installed(backend), current.as_deref()) {
        progress(step_label(&step));
        let mut cmd = if step[0] == "restart" {
            let mut c = tokio::process::Command::new("systemctl");
            c.args(["--user", "restart", MYNA_USER_UNIT]);
            c
        } else {
            let mut c = tokio::process::Command::new("/usr/bin/snap");
            c.args(&step);
            c
        };
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("could not run {}: {e}", step[0]))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim().trim_start_matches("error: ");
            // Already connected or set is fine.
            if step[0] == "set" || err.contains("already connected") {
                continue;
            }
            return Err(err.to_owned());
        }
    }
    Ok(())
}

/// The dictation shortcut, as keycaps joined with "+" ("Super+J"): Myna's
/// GNOME custom shortcut, else Umoja's own `dictation-hotkey`.
pub fn shortcut() -> Option<String> {
    let custom = gs::settings(gs::MEDIA_KEYS)
        .filter(|s| gs::has_key(s, "custom-keybindings"))
        .and_then(|media| {
            media.strv("custom-keybindings").iter().find_map(|path| {
                let s = gs::settings_at(gs::CUSTOM_KEYBINDING, path.as_str())?;
                dbus::is_myna_toggle(&s.string("command"))
                    .then(|| s.string("binding").to_string())
                    .filter(|b| !b.trim().is_empty())
            })
        });
    let accel = custom.or_else(|| gs::get_strv(gs::UMOJA_SHELL, "dictation-hotkey").into_iter().next())?;
    crate::shortcuts::keycaps(&accel).map(|caps| caps.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_is_read_from_snap_connections() {
        let out = "Interface                      Plug             Slot                    Notes\n\
                   content[inference-provider]    myna:backend     myna-whisper:provider   manual\n\
                   pipewire                       myna:pipewire    :pipewire               manual\n";
        assert_eq!(parse_backend(out).as_deref(), Some("myna-whisper"));
        let none = "content[inference-provider]    myna:backend     -   -\n";
        assert_eq!(parse_backend(none), None);
    }

    #[test]
    fn fresh_setup_does_what_myna_settings_does() {
        let p = plan("myna-parakeet", false, false, None);
        let first: Vec<&str> = p.iter().map(|s| s[0].as_str()).collect();
        assert_eq!(first, ["set", "install", "install", "connect"]);
        assert_eq!(p[3], ["connect", "myna:backend", "myna-parakeet:provider"]);
    }

    #[test]
    fn switching_swaps_the_connection_and_restarts() {
        let p = plan("myna-parakeet", true, false, Some("myna-whisper"));
        let first: Vec<&str> = p.iter().map(|s| s[0].as_str()).collect();
        assert_eq!(first, ["install", "disconnect", "connect", "restart"]);
        assert!(plan("myna-whisper", true, true, Some("myna-whisper")).is_empty());
    }
}
