//! Desktop actions Ask Umoja runs straight away, without the model: each
//! is one umoja-mcp tool call with fixed arguments (so the server's confirm
//! rules still apply), or one of the overlay's own UIs.

/// What a row does when picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// An on/off switch: `tool` with `{"enabled": bool}`. `state` names the
    /// `desktop_status` line that says whether it is on now.
    Toggle {
        tool: &'static str,
        state: &'static str,
    },
    /// `tool` with these JSON arguments.
    Call {
        tool: &'static str,
        args: &'static str,
    },
    /// One of the overlay's own screens.
    Local(Local),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    ScreenshotUi,
    ScreenRecordingUi,
    ClipboardHistory,
    AskAboutWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Switches and one-step desktop actions.
    Control,
    /// A page of Settings.
    Setting,
}

#[derive(Debug, Clone, Copy)]
pub struct Instant {
    /// Stable id, for usage history.
    pub id: &'static str,
    pub title: &'static str,
    /// Extra words it answers to (the title's words count too).
    pub keywords: &'static [&'static str],
    pub icon: &'static str,
    pub group: Group,
    pub run: Run,
}

/// Icons GNOME Shell ships itself, or that older icon themes lack, with
/// stock icons to fall back on (Yaru and Adwaita don't all have them).
const ICON_FALLBACKS: &[(&str, &[&str])] = &[
    ("screenshot-recorded-symbolic", &["camera-photo-symbolic"]),
    ("focus-windows-symbolic", &["window-new-symbolic"]),
    ("night-light-symbolic", &["weather-clear-night-symbolic"]),
    (
        "power-profile-power-saver-symbolic",
        &["battery-level-50-symbolic", "battery-symbolic"],
    ),
    ("power-profile-balanced-symbolic", &["battery-symbolic"]),
    (
        "power-profile-performance-symbolic",
        &["power-profile-balanced-symbolic", "battery-symbolic"],
    ),
    (
        "preferences-desktop-apps-symbolic",
        &[
            "view-app-grid-symbolic",
            "application-x-executable-symbolic",
        ],
    ),
    (
        "notifications-disabled-symbolic",
        &["preferences-system-notifications-symbolic"],
    ),
];

/// `icon` then its fallbacks, best first, ending with an icon every theme
/// has, so a row never shows the "missing image" icon.
pub fn icon_names(icon: &'static str) -> Vec<&'static str> {
    let mut names = vec![icon];
    if let Some((_, more)) = ICON_FALLBACKS.iter().find(|(n, _)| *n == icon) {
        names.extend_from_slice(more);
    }
    names.push("emblem-system-symbolic");
    names
}

const fn control(
    id: &'static str,
    title: &'static str,
    keywords: &'static [&'static str],
    icon: &'static str,
    run: Run,
) -> Instant {
    Instant {
        id,
        title,
        keywords,
        icon,
        group: Group::Control,
        run,
    }
}

const fn settings(
    id: &'static str,
    title: &'static str,
    keywords: &'static [&'static str],
    icon: &'static str,
    args: &'static str,
) -> Instant {
    Instant {
        id,
        title,
        keywords,
        icon,
        group: Group::Setting,
        run: Run::Call {
            tool: "open_settings",
            args,
        },
    }
}

pub const CATALOG: &[Instant] = &[
    control(
        "undo",
        "Undo Last Change",
        &["undo", "put back", "revert", "change back", "go back"],
        "edit-undo-symbolic",
        Run::Call {
            tool: "undo",
            args: "{}",
        },
    ),
    control(
        "dark-style",
        "Dark Style",
        &["dark", "theme", "night mode", "light mode", "appearance"],
        "weather-clear-night-symbolic",
        Run::Toggle {
            tool: "set_dark_style",
            state: "Dark Style",
        },
    ),
    control(
        "wifi",
        "Wi-Fi",
        &["wifi", "wi-fi", "wireless", "wlan", "internet"],
        "network-wireless-symbolic",
        Run::Toggle {
            tool: "set_wifi",
            state: "Wi-Fi",
        },
    ),
    control(
        "bluetooth",
        "Bluetooth",
        &["bluetooth", "bt"],
        "bluetooth-active-symbolic",
        Run::Toggle {
            tool: "set_bluetooth",
            state: "Bluetooth",
        },
    ),
    control(
        "dnd",
        "Do Not Disturb",
        &[
            "dnd",
            "do not disturb",
            "notifications",
            "quiet",
            "focus",
            "silence",
        ],
        "notifications-disabled-symbolic",
        Run::Toggle {
            tool: "set_do_not_disturb",
            state: "Do Not Disturb",
        },
    ),
    control(
        "night-light",
        "Night Light",
        &["night light", "warm", "blue light", "redshift"],
        "night-light-symbolic",
        Run::Toggle {
            tool: "set_night_light",
            state: "Night Light",
        },
    ),
    control(
        "power-saver",
        "Power Saver",
        &["power", "battery", "saver", "save battery", "power mode"],
        "power-profile-power-saver-symbolic",
        Run::Call {
            tool: "set_power_mode",
            args: r#"{"mode":"power-saver"}"#,
        },
    ),
    control(
        "balanced",
        "Balanced Power",
        &["power", "balanced", "power mode"],
        "power-profile-balanced-symbolic",
        Run::Call {
            tool: "set_power_mode",
            args: r#"{"mode":"balanced"}"#,
        },
    ),
    control(
        "performance",
        "Performance Mode",
        &["power", "performance", "fast", "power mode"],
        "power-profile-performance-symbolic",
        Run::Call {
            tool: "set_power_mode",
            args: r#"{"mode":"performance"}"#,
        },
    ),
    control(
        "mute",
        "Mute Sound",
        &["mute", "silence", "sound off", "volume"],
        "audio-volume-muted-symbolic",
        Run::Call {
            tool: "set_volume",
            args: r#"{"mute":true}"#,
        },
    ),
    control(
        "unmute",
        "Unmute Sound",
        &["unmute", "sound on", "volume"],
        "audio-volume-high-symbolic",
        Run::Call {
            tool: "set_volume",
            args: r#"{"mute":false}"#,
        },
    ),
    control(
        "play-pause",
        "Play or Pause",
        &["play", "pause", "music", "media", "resume"],
        "media-playback-start-symbolic",
        Run::Call {
            tool: "media_control",
            args: r#"{"action":"play_pause"}"#,
        },
    ),
    control(
        "next-track",
        "Next Track",
        &["next", "skip", "song", "track", "music"],
        "media-skip-forward-symbolic",
        Run::Call {
            tool: "media_control",
            args: r#"{"action":"next"}"#,
        },
    ),
    control(
        "screenshot",
        "Take a Screenshot",
        &[
            "screenshot",
            "screen shot",
            "capture",
            "print screen",
            "snip",
        ],
        "screenshot-recorded-symbolic",
        Run::Local(Local::ScreenshotUi),
    ),
    control(
        "screencast",
        "Record the Screen",
        &["record", "screencast", "screen recording", "video"],
        "media-record-symbolic",
        Run::Local(Local::ScreenRecordingUi),
    ),
    control(
        "clipboard",
        "Clipboard History",
        &["clipboard", "copied", "paste", "history"],
        "edit-paste-symbolic",
        Run::Local(Local::ClipboardHistory),
    ),
    control(
        "ask-window",
        "Ask About This Window",
        &["this window", "explain", "read", "what's on screen"],
        "focus-windows-symbolic",
        Run::Local(Local::AskAboutWindow),
    ),
    control(
        "empty-trash",
        "Empty Trash",
        &["trash", "empty", "bin", "delete"],
        "user-trash-symbolic",
        Run::Call {
            tool: "empty_trash",
            args: "{}",
        },
    ),
    control(
        "lock",
        "Lock Screen",
        &["lock", "away"],
        "system-lock-screen-symbolic",
        Run::Call {
            tool: "session",
            args: r#"{"action":"lock"}"#,
        },
    ),
    control(
        "suspend",
        "Suspend",
        &["suspend", "sleep"],
        "media-playback-pause-symbolic",
        Run::Call {
            tool: "session",
            args: r#"{"action":"suspend"}"#,
        },
    ),
    control(
        "log-out",
        "Log Out",
        &["log out", "logout", "sign out"],
        "system-log-out-symbolic",
        Run::Call {
            tool: "session",
            args: r#"{"action":"log_out"}"#,
        },
    ),
    control(
        "restart",
        "Restart",
        &["restart", "reboot"],
        "system-reboot-symbolic",
        Run::Call {
            tool: "session",
            args: r#"{"action":"restart"}"#,
        },
    ),
    control(
        "power-off",
        "Power Off",
        &["power off", "shut down", "shutdown", "turn off"],
        "system-shutdown-symbolic",
        Run::Call {
            tool: "session",
            args: r#"{"action":"power_off"}"#,
        },
    ),
    settings(
        "settings-wifi",
        "Wi-Fi Settings",
        &["network", "wifi", "wi-fi", "hotspot"],
        "network-wireless-symbolic",
        r#"{"panel":"wifi"}"#,
    ),
    settings(
        "settings-network",
        "Network Settings",
        &["network", "vpn", "proxy", "ethernet", "wired"],
        "network-wired-symbolic",
        r#"{"panel":"network"}"#,
    ),
    settings(
        "settings-bluetooth",
        "Bluetooth Settings",
        &["bluetooth", "pair", "headphones"],
        "bluetooth-active-symbolic",
        r#"{"panel":"bluetooth"}"#,
    ),
    settings(
        "settings-display",
        "Displays",
        &[
            "display",
            "monitor",
            "screen",
            "resolution",
            "scale",
            "refresh",
        ],
        "video-display-symbolic",
        r#"{"panel":"display"}"#,
    ),
    settings(
        "settings-sound",
        "Sound Settings",
        &[
            "sound",
            "audio",
            "speakers",
            "microphone",
            "volume",
            "output",
        ],
        "audio-speakers-symbolic",
        r#"{"panel":"sound"}"#,
    ),
    settings(
        "settings-power",
        "Power Settings",
        &["power", "battery", "suspend", "blank"],
        "battery-symbolic",
        r#"{"panel":"power"}"#,
    ),
    settings(
        "settings-appearance",
        "Appearance",
        &[
            "appearance",
            "wallpaper",
            "background",
            "accent",
            "theme",
            "dark",
        ],
        "preferences-desktop-appearance-symbolic",
        r#"{"panel":"ubuntu"}"#,
    ),
    settings(
        "settings-keyboard",
        "Keyboard Settings",
        &["keyboard", "shortcuts", "input", "layout", "language"],
        "input-keyboard-symbolic",
        r#"{"panel":"keyboard"}"#,
    ),
    settings(
        "settings-mouse",
        "Mouse & Touchpad",
        &["mouse", "touchpad", "trackpad", "scroll", "pointer"],
        "input-mouse-symbolic",
        r#"{"panel":"mouse"}"#,
    ),
    settings(
        "settings-notifications",
        "Notification Settings",
        &["notifications", "alerts", "banners"],
        "preferences-system-notifications-symbolic",
        r#"{"panel":"notifications"}"#,
    ),
    settings(
        "settings-privacy",
        "Privacy & Security",
        &["privacy", "security", "location", "camera", "screen lock"],
        "preferences-system-privacy-symbolic",
        r#"{"panel":"privacy"}"#,
    ),
    settings(
        "settings-accessibility",
        "Accessibility",
        &[
            "accessibility",
            "a11y",
            "screen reader",
            "zoom",
            "large text",
            "high contrast",
        ],
        "preferences-desktop-accessibility-symbolic",
        r#"{"panel":"universal-access"}"#,
    ),
    settings(
        "settings-apps",
        "Apps Settings",
        &["apps", "applications", "default apps", "permissions"],
        "preferences-desktop-apps-symbolic",
        r#"{"panel":"applications"}"#,
    ),
    settings(
        "settings-users",
        "Users",
        &["users", "account", "password", "login"],
        "system-users-symbolic",
        r#"{"panel":"users"}"#,
    ),
    settings(
        "settings-printers",
        "Printers",
        &["printer", "print", "scanner"],
        "printer-symbolic",
        r#"{"panel":"printers"}"#,
    ),
    settings(
        "settings-datetime",
        "Date & Time",
        &["date", "time", "clock", "timezone"],
        "preferences-system-time-symbolic",
        r#"{"panel":"datetime"}"#,
    ),
    settings(
        "settings-about",
        "About This Computer",
        &["about", "system", "version", "hardware", "info"],
        "computer-symbolic",
        r#"{"panel":"system"}"#,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_fall_back() {
        assert_eq!(
            icon_names("screenshot-recorded-symbolic"),
            vec![
                "screenshot-recorded-symbolic",
                "camera-photo-symbolic",
                "emblem-system-symbolic"
            ]
        );
        assert_eq!(
            icon_names("printer-symbolic"),
            vec!["printer-symbolic", "emblem-system-symbolic"]
        );
    }

    #[test]
    fn ids_are_unique_and_args_parse() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n);
        for i in CATALOG {
            if let Run::Call { args, .. } = i.run {
                assert!(
                    serde_json::from_str::<serde_json::Value>(args).is_ok(),
                    "{}",
                    i.id
                );
            }
        }
    }
}
