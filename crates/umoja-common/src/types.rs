//! Types shared across components.

use serde::{Deserialize, Serialize};

/// `org.gnome.shell.extensions.dash-to-dock dock-position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockPosition {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl DockPosition {
    pub fn from_gsettings(value: &str) -> Self {
        match value {
            "RIGHT" => Self::Right,
            "TOP" => Self::Top,
            "BOTTOM" => Self::Bottom,
            _ => Self::Left,
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// How dangerous an AI-suggested command is. Anything above `Low` needs an
/// explicit confirmation in the overlay before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    #[default]
    Medium,
    High,
}

/// A structured action the Lemonade assistant can return alongside its text.
/// Rendered as a clickable "chip" in the overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AssistantAction {
    /// Open a gnome-control-center panel, e.g. `network` / `vpn`.
    OpenSettings {
        panel: String,
        #[serde(default)]
        subpage: Option<String>,
    },
    /// Run a shell command after the user clicks it.
    RunCommand {
        cmd: String,
        #[serde(default)]
        risk: Risk,
    },
    /// Launch an installed application by desktop id.
    LaunchApp { app_id: String },
    /// Change a GSettings key.
    SetSetting {
        schema: String,
        key: String,
        value: serde_json::Value,
    },
}

impl AssistantAction {
    /// gnome-control-center panels Umoja allows the assistant to open.
    const KNOWN_PANELS: &'static [&'static str] = &[
        "applications",
        "background",
        "bluetooth",
        "color",
        "datetime",
        "display",
        "keyboard",
        "mouse",
        "multitasking",
        "network",
        "notifications",
        "online-accounts",
        "power",
        "printers",
        "privacy",
        "region",
        "search",
        "sharing",
        "sound",
        "system",
        "ubuntu",
        "universal-access",
        "users",
        "wacom",
        "wifi",
        "wwan",
    ];

    /// Reject actions that reference things we don't know how to do safely.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::OpenSettings { panel, .. } => Self::KNOWN_PANELS.contains(&panel.as_str()),
            Self::RunCommand { cmd, .. } => !cmd.trim().is_empty(),
            Self::LaunchApp { app_id } => !app_id.trim().is_empty(),
            Self::SetSetting { schema, key, .. } => {
                schema.starts_with("org.gnome.") && !key.is_empty()
            }
        }
    }

    /// Whether the overlay must ask before running it.
    pub fn needs_confirmation(&self) -> bool {
        match self {
            Self::RunCommand { risk, .. } => *risk != Risk::Low,
            Self::SetSetting { .. } => false,
            _ => false,
        }
    }
}

/// Extract every valid JSON action object from free-form assistant output.
///
/// Models put actions in fenced code blocks, inline, or as a JSON array, so
/// this scans for balanced `{...}`/`[...]` spans and keeps whatever parses.
pub fn extract_actions(text: &str) -> Vec<AssistantAction> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let open = bytes[i];
        if (open == b'{' || open == b'[')
            && let Some(end) = balanced_end(bytes, i) {
                let slice = &text[i..=end];
                if let Ok(a) = serde_json::from_str::<AssistantAction>(slice) {
                    if a.is_valid() {
                        out.push(a);
                    }
                    i = end + 1;
                    continue;
                }
                if let Ok(list) = serde_json::from_str::<Vec<AssistantAction>>(slice) {
                    out.extend(list.into_iter().filter(AssistantAction::is_valid));
                    i = end + 1;
                    continue;
                }
            }
        i += 1;
    }
    out
}

/// Strip action JSON from assistant output so only the prose remains.
pub fn strip_actions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut last = 0;
    while i < bytes.len() {
        if (bytes[i] == b'{' || bytes[i] == b'[')
            && let Some(end) = balanced_end(bytes, i) {
                let slice = &text[i..=end];
                if serde_json::from_str::<AssistantAction>(slice).is_ok()
                    || serde_json::from_str::<Vec<AssistantAction>>(slice).is_ok()
                {
                    out.push_str(&text[last..i]);
                    last = end + 1;
                    i = end + 1;
                    continue;
                }
            }
        i += 1;
    }
    out.push_str(&text[last..]);
    out.replace("```json", "")
        .replace("```", "")
        .trim()
        .to_string()
}

fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (off, &b) in bytes[start..].iter().enumerate() {
        if in_str {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + off);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blueprint_examples() {
        let text = r#"Open VPN settings:
```json
{"action": "open_settings", "panel": "network", "subpage": "vpn"}
```
Or convert: {"action": "run_command", "cmd": "mogrify -format webp *.png", "risk": "low"}"#;
        let actions = extract_actions(text);
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            AssistantAction::OpenSettings {
                panel: "network".into(),
                subpage: Some("vpn".into())
            }
        );
        assert!(!actions[1].needs_confirmation());
        let prose = strip_actions(text);
        assert!(prose.starts_with("Open VPN settings:"));
        assert!(!prose.contains("open_settings"));
    }

    #[test]
    fn rejects_unknown_panel_and_defaults_risk() {
        assert!(extract_actions(r#"{"action":"open_settings","panel":"rm -rf"}"#).is_empty());
        let a = extract_actions(r#"[{"action":"run_command","cmd":"sudo reboot"}]"#);
        assert_eq!(a.len(), 1);
        assert!(a[0].needs_confirmation());
    }

    #[test]
    fn dock_position() {
        assert_eq!(DockPosition::from_gsettings("BOTTOM"), DockPosition::Bottom);
        assert!(DockPosition::from_gsettings("bogus").is_vertical());
    }
}
