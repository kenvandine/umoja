//! Media and hardware keys (volume, brightness, playback, launchers).
//!
//! In GNOME, gnome-settings-daemon's media-keys plugin asks gnome-shell to
//! grab these keys and handles them itself. Umoja has no settings daemon,
//! so the compositor grabs the same keys, read from the same schema
//! (`org.gnome.settings-daemon.plugins.media-keys`, both the user-editable
//! `<name>` key and the fixed `<name>-static` one), and hands each press to
//! umoja-panel, which changes the volume or brightness and shows the OSD.
//! Keys the installed schema doesn't have (screen brightness, in recent
//! GNOME) use gnome-settings-daemon's old defaults.
//!
//! Binding codes are `BASE + index into KEYS`, alongside the window
//! manager's actions and the numbered workspace codes.

use crate::input::Binding;
use umoja_common::gsettings as gs;

pub const BASE: u32 = 300;

/// Answers for the shim (`umoja_media_action`).
pub const NOT_MEDIA: u32 = 0;
pub const MEDIA: u32 = 1;
/// Also act on key repeat while held, as volume and brightness do in GNOME.
pub const MEDIA_REPEATS: u32 = 3;

pub struct Key {
    /// The media-keys schema key, also the name passed to the panel.
    pub name: &'static str,
    pub repeats: bool,
    /// gnome-settings-daemon's `-static` defaults, used when the schema
    /// isn't installed.
    pub defaults: &'static [&'static str],
}

const fn key(name: &'static str, repeats: bool, defaults: &'static [&'static str]) -> Key {
    Key {
        name,
        repeats,
        defaults,
    }
}

pub const KEYS: &[Key] = &[
    key("volume-up", true, &["XF86AudioRaiseVolume", "<Ctrl>XF86AudioRaiseVolume"]),
    key("volume-down", true, &["XF86AudioLowerVolume", "<Ctrl>XF86AudioLowerVolume"]),
    key("volume-mute", false, &["XF86AudioMute"]),
    key(
        "volume-up-quiet",
        true,
        &["<Alt>XF86AudioRaiseVolume", "<Alt><Ctrl>XF86AudioRaiseVolume"],
    ),
    key(
        "volume-down-quiet",
        true,
        &["<Alt>XF86AudioLowerVolume", "<Alt><Ctrl>XF86AudioLowerVolume"],
    ),
    key("volume-mute-quiet", false, &["<Alt>XF86AudioMute"]),
    key(
        "volume-up-precise",
        true,
        &["<Shift>XF86AudioRaiseVolume", "<Ctrl><Shift>XF86AudioRaiseVolume"],
    ),
    key(
        "volume-down-precise",
        true,
        &["<Shift>XF86AudioLowerVolume", "<Ctrl><Shift>XF86AudioLowerVolume"],
    ),
    key("mic-mute", false, &["XF86AudioMicMute"]),
    key(
        "screen-brightness-up",
        true,
        &["XF86MonBrightnessUp", "<Ctrl>XF86MonBrightnessUp"],
    ),
    key(
        "screen-brightness-down",
        true,
        &["XF86MonBrightnessDown", "<Ctrl>XF86MonBrightnessDown"],
    ),
    key("keyboard-brightness-up", true, &["XF86KbdBrightnessUp"]),
    key("keyboard-brightness-down", true, &["XF86KbdBrightnessDown"]),
    key("keyboard-brightness-toggle", false, &["XF86KbdLightOnOff"]),
    key("play", false, &["XF86AudioPlay", "<Ctrl>XF86AudioPlay"]),
    key("pause", false, &["XF86AudioPause"]),
    key("stop", false, &["XF86AudioStop"]),
    key("previous", false, &["XF86AudioPrev", "<Ctrl>XF86AudioPrev"]),
    key("next", false, &["XF86AudioNext", "<Ctrl>XF86AudioNext"]),
    key("calculator", false, &["XF86Calculator"]),
    key("www", false, &["XF86WWW"]),
    key("email", false, &["XF86Mail"]),
    key("home", false, &["XF86Explorer"]),
    key("search", false, &["XF86Search"]),
    key("control-center", false, &["XF86Tools"]),
    key("screensaver", false, &["XF86ScreenSaver"]),
];

/// The media-key bindings, from GSettings when the schema is installed,
/// each with its schema key name and accelerator (for the shortcuts guide).
pub fn bindings() -> Vec<(&'static str, String, Binding)> {
    let schema = gs::settings(gs::MEDIA_KEYS);
    let mut out = Vec::new();
    for (i, k) in KEYS.iter().enumerate() {
        let static_key = format!("{}-static", k.name);
        // Newer gnome-settings-daemons dropped some keys (the screen
        // brightness ones moved to the shell); use the defaults for those.
        let in_schema = schema
            .as_ref()
            .is_some_and(|s| gs::has_key(s, k.name) || gs::has_key(s, &static_key));
        let accels: Vec<String> = if in_schema {
            let mut v = gs::get_strv(gs::MEDIA_KEYS, k.name);
            v.extend(gs::get_strv(gs::MEDIA_KEYS, &static_key));
            v
        } else {
            k.defaults.iter().map(|s| s.to_string()).collect()
        };
        for a in &accels {
            if let Some((keysym, mods)) = crate::input::parse_accelerator(a) {
                let binding = Binding {
                    keysym,
                    mods,
                    action: BASE + i as u32,
                };
                out.push((k.name, a.clone(), binding));
            }
        }
    }
    out
}

pub fn get(code: u32) -> Option<&'static Key> {
    KEYS.get(code.checked_sub(BASE)? as usize)
}

pub fn classify(code: u32) -> u32 {
    match get(code) {
        Some(k) if k.repeats => MEDIA_REPEATS,
        Some(_) => MEDIA,
        None => NOT_MEDIA,
    }
}

/// XF86 keysym names that appear in the media-keys schema.
pub fn keysym(name: &str) -> Option<u32> {
    Some(match name {
        "XF86MonBrightnessUp" => 0x1008ff02,
        "XF86MonBrightnessDown" => 0x1008ff03,
        "XF86KbdLightOnOff" => 0x1008ff04,
        "XF86KbdBrightnessUp" => 0x1008ff05,
        "XF86KbdBrightnessDown" => 0x1008ff06,
        "XF86MonBrightnessCycle" => 0x1008ff07,
        "XF86AudioLowerVolume" => 0x1008ff11,
        "XF86AudioMute" => 0x1008ff12,
        "XF86AudioRaiseVolume" => 0x1008ff13,
        "XF86AudioPlay" => 0x1008ff14,
        "XF86AudioStop" => 0x1008ff15,
        "XF86AudioPrev" => 0x1008ff16,
        "XF86AudioNext" => 0x1008ff17,
        "XF86HomePage" => 0x1008ff18,
        "XF86Mail" => 0x1008ff19,
        "XF86Search" => 0x1008ff1b,
        "XF86Calculator" => 0x1008ff1d,
        "XF86Eject" => 0x1008ff2c,
        "XF86ScreenSaver" => 0x1008ff2d,
        "XF86WWW" => 0x1008ff2e,
        "XF86AudioPause" => 0x1008ff31,
        "XF86AudioMedia" => 0x1008ff32,
        "XF86Explorer" => 0x1008ff5d,
        "XF86Tools" => 0x1008ff81,
        "XF86AudioMicMute" => 0x1008ffb2,
        _ => return None,
    })
}

/// Hand a press to umoja-panel.
pub fn press(code: u32) {
    if let Some(k) = get(code) {
        tracing::debug!(key = k.name, "media key");
        crate::session::media_key(k.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{MOD_ALT, MOD_CTRL, MOD_SHIFT, parse_accelerator};

    #[test]
    fn parses_media_accelerators() {
        assert_eq!(parse_accelerator("XF86AudioRaiseVolume"), Some((0x1008ff13, 0)));
        assert_eq!(
            parse_accelerator("<Alt><Ctrl>XF86AudioLowerVolume"),
            Some((0x1008ff11, MOD_ALT | MOD_CTRL))
        );
        assert_eq!(
            parse_accelerator("<Shift>XF86MonBrightnessUp"),
            Some((0x1008ff02, MOD_SHIFT))
        );
    }

    #[test]
    fn classifies_codes() {
        let up = KEYS.iter().position(|k| k.name == "volume-up").unwrap() as u32;
        let mute = KEYS.iter().position(|k| k.name == "volume-mute").unwrap() as u32;
        assert_eq!(classify(BASE + up), MEDIA_REPEATS);
        assert_eq!(classify(BASE + mute), MEDIA);
        assert_eq!(classify(BASE + KEYS.len() as u32), NOT_MEDIA);
        assert_eq!(classify(1), NOT_MEDIA);
        for k in KEYS {
            for a in k.defaults {
                assert!(parse_accelerator(a).is_some(), "{a}");
            }
        }
    }
}
