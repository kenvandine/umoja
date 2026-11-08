//! Keybindings, read live from the GNOME schemas.
//!
//! Umoja honours the same keys as Ubuntu's GNOME session
//! (`org.gnome.desktop.wm.keybindings`, `org.gnome.mutter.keybindings`,
//! `org.gnome.shell.keybindings`), so shortcuts changed in Settings apply
//! here immediately. Changes are picked up by a GLib main loop on a helper
//! thread that rebuilds the table behind an `RwLock`.

use crate::tiling::Action;
use crate::workspaces;
use gio::prelude::*;
use std::sync::RwLock;
use umoja_common::gsettings as gs;

pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 1;
pub const MOD_ALT: u32 = 1 << 2;
pub const MOD_SUPER: u32 = 1 << 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub keysym: u32,
    pub mods: u32,
    /// An `Action` value, or a numbered workspace code (see workspaces.rs).
    pub action: u32,
}

pub struct Config {
    pub bindings: Vec<Binding>,
    pub overlay_keysym: Option<u32>,
    pub edge_tiling: bool,
    /// Ubuntu Tiling Assistant's layout picker (drag to the top edge).
    pub layout_picker: bool,
    /// Tiling Assistant: focusing a tiled window raises its tile group.
    pub raise_tile_group: bool,
    pub center_new_windows: bool,
    pub workspaces: workspaces::Settings,
}

/// Ubuntu's Tiling Assistant extension (enabled by default in Ubuntu).
pub const TILING_ASSISTANT: &str = "org.gnome.shell.extensions.tiling-assistant";
const TILING_ASSISTANT_UUID: &str = "tiling-assistant@ubuntu.com";

pub static CONFIG: RwLock<Config> = RwLock::new(Config {
    bindings: Vec::new(),
    overlay_keysym: Some(XK_SUPER_L),
    edge_tiling: true,
    layout_picker: true,
    raise_tile_group: true,
    center_new_windows: false,
    workspaces: workspaces::Settings {
        dynamic: true,
        fixed_count: 4,
        only_on_primary: true,
    },
});

const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;
/// Not a real keysym: stands for "either Super key" (mutter's `Super`).
const XK_SUPER: u32 = 0x1_00ff_eb;

/// (schema, key, action) for every shortcut the compositor handles itself.
/// These are the same keys mutter/gnome-shell read in Ubuntu's GNOME session.
const SOURCES: &[(&str, &str, Action)] = &[
    (
        gs::MUTTER_KEYBINDINGS,
        "toggle-tiled-left",
        Action::TileLeft,
    ),
    (
        gs::MUTTER_KEYBINDINGS,
        "toggle-tiled-right",
        Action::TileRight,
    ),
    (gs::WM_KEYBINDINGS, "maximize", Action::Maximize),
    (gs::WM_KEYBINDINGS, "unmaximize", Action::Unmaximize),
    (
        gs::WM_KEYBINDINGS,
        "toggle-maximized",
        Action::ToggleMaximized,
    ),
    (
        gs::WM_KEYBINDINGS,
        "toggle-fullscreen",
        Action::ToggleFullscreen,
    ),
    (gs::WM_KEYBINDINGS, "minimize", Action::Minimize),
    (gs::WM_KEYBINDINGS, "close", Action::Close),
    (
        gs::WM_KEYBINDINGS,
        "switch-to-workspace-left",
        Action::WorkspacePrev,
    ),
    (
        gs::WM_KEYBINDINGS,
        "switch-to-workspace-right",
        Action::WorkspaceNext,
    ),
    (
        gs::WM_KEYBINDINGS,
        "switch-to-workspace-up",
        Action::WorkspacePrev,
    ),
    (
        gs::WM_KEYBINDINGS,
        "switch-to-workspace-down",
        Action::WorkspaceNext,
    ),
    (
        gs::WM_KEYBINDINGS,
        "switch-to-workspace-last",
        Action::WorkspaceLast,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-workspace-left",
        Action::MoveToWorkspacePrev,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-workspace-right",
        Action::MoveToWorkspaceNext,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-workspace-up",
        Action::MoveToWorkspacePrev,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-workspace-down",
        Action::MoveToWorkspaceNext,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-workspace-last",
        Action::MoveToWorkspaceLast,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-monitor-left",
        Action::MoveToMonitorLeft,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-monitor-right",
        Action::MoveToMonitorRight,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-monitor-up",
        Action::MoveToMonitorUp,
    ),
    (
        gs::WM_KEYBINDINGS,
        "move-to-monitor-down",
        Action::MoveToMonitorDown,
    ),
    (
        gs::SHELL_KEYBINDINGS,
        "toggle-application-view",
        Action::ToggleOverlay,
    ),
    (gs::SHELL_KEYBINDINGS, "toggle-overview", Action::ToggleOverview),
    (
        gs::SHELL_KEYBINDINGS,
        "shift-overview-up",
        Action::ShiftOverviewUp,
    ),
    (
        gs::SHELL_KEYBINDINGS,
        "shift-overview-down",
        Action::ShiftOverviewDown,
    ),
    (
        gs::WM_KEYBINDINGS,
        "switch-applications",
        Action::SwitchApplications,
    ),
    (
        gs::WM_KEYBINDINGS,
        "switch-applications-backward",
        Action::SwitchApplicationsBackward,
    ),
    (gs::WM_KEYBINDINGS, "switch-group", Action::SwitchGroup),
    (
        gs::WM_KEYBINDINGS,
        "switch-group-backward",
        Action::SwitchGroupBackward,
    ),
    (gs::WM_KEYBINDINGS, "switch-windows", Action::SwitchWindows),
    (
        gs::WM_KEYBINDINGS,
        "switch-windows-backward",
        Action::SwitchWindowsBackward,
    ),
    // Ubuntu's Tiling Assistant takes over Super+arrows (it empties mutter's
    // toggle-tiled-*/maximize keys while enabled) and adds keypad quarters.
    (TILING_ASSISTANT, "tile-left-half", Action::TileLeft),
    (TILING_ASSISTANT, "tile-right-half", Action::TileRight),
    (TILING_ASSISTANT, "tile-top-half", Action::TileTopHalf),
    (TILING_ASSISTANT, "tile-bottom-half", Action::TileBottomHalf),
    (TILING_ASSISTANT, "tile-topleft-quarter", Action::TileTopLeft),
    (TILING_ASSISTANT, "tile-topright-quarter", Action::TileTopRight),
    (
        TILING_ASSISTANT,
        "tile-bottomleft-quarter",
        Action::TileBottomLeft,
    ),
    (
        TILING_ASSISTANT,
        "tile-bottomright-quarter",
        Action::TileBottomRight,
    ),
    (TILING_ASSISTANT, "tile-maximize", Action::ToggleMaximized),
    (TILING_ASSISTANT, "restore-window", Action::Unmaximize),
    (
        gs::SHELL_KEYBINDINGS,
        "show-screenshot-ui",
        Action::ScreenshotUi,
    ),
    (gs::SHELL_KEYBINDINGS, "screenshot", Action::Screenshot),
    (
        gs::SHELL_KEYBINDINGS,
        "screenshot-window",
        Action::ScreenshotWindow,
    ),
    (gs::UMOJA_SHELL, "tile-top-left", Action::TileTopLeft),
    (gs::UMOJA_SHELL, "tile-top-right", Action::TileTopRight),
    (gs::UMOJA_SHELL, "tile-bottom-left", Action::TileBottomLeft),
    (
        gs::UMOJA_SHELL,
        "tile-bottom-right",
        Action::TileBottomRight,
    ),
    (gs::UMOJA_SHELL, "dictation-hotkey", Action::ToggleDictation),
    (gs::UMOJA_SHELL, "ask-about-screen-hotkey", Action::AskAboutScreen),
    (gs::UMOJA_SHELL, "clipboard-history-hotkey", Action::ClipboardHistory),
];

/// Fallbacks used when a schema is missing (e.g. running nested for development).
const DEFAULTS: &[(&str, u32)] = &[
    ("<Super>Left", Action::TileLeft as u32),
    ("<Super>Right", Action::TileRight as u32),
    ("<Super>Up", Action::Maximize as u32),
    ("<Super>Down", Action::Unmaximize as u32),
    ("<Super>h", Action::Minimize as u32),
    ("<Alt>F4", Action::Close as u32),
    ("<Super>a", Action::ToggleOverlay as u32),
    ("<Super>s", Action::ToggleOverview as u32),
    ("<Alt>Tab", Action::SwitchApplications as u32),
    ("<Super>Tab", Action::SwitchApplications as u32),
    ("<Shift><Alt>Tab", Action::SwitchApplicationsBackward as u32),
    ("<Shift><Super>Tab", Action::SwitchApplicationsBackward as u32),
    ("<Alt>Above_Tab", Action::SwitchGroup as u32),
    ("<Super>Above_Tab", Action::SwitchGroup as u32),
    ("<Shift><Alt>Above_Tab", Action::SwitchGroupBackward as u32),
    ("<Shift><Super>Above_Tab", Action::SwitchGroupBackward as u32),
    ("<Super>KP_7", Action::TileTopLeft as u32),
    ("<Super>KP_9", Action::TileTopRight as u32),
    ("<Super>KP_1", Action::TileBottomLeft as u32),
    ("<Super>KP_3", Action::TileBottomRight as u32),
    ("<Super>KP_8", Action::TileTopHalf as u32),
    ("<Super>KP_2", Action::TileBottomHalf as u32),
    ("<Super>d", Action::ToggleDictation as u32),
    ("<Super><Shift>a", Action::AskAboutScreen as u32),
    ("<Super><Shift>v", Action::ClipboardHistory as u32),
    // gsettings-desktop-schemas' and gnome-shell's workspace defaults.
    ("<Super>Page_Up", Action::WorkspacePrev as u32),
    ("<Super>Page_Down", Action::WorkspaceNext as u32),
    ("<Super><Alt>Left", Action::WorkspacePrev as u32),
    ("<Super><Alt>Right", Action::WorkspaceNext as u32),
    ("<Control><Alt>Left", Action::WorkspacePrev as u32),
    ("<Control><Alt>Right", Action::WorkspaceNext as u32),
    ("<Super>Home", workspaces::SWITCH_BASE + 1),
    ("<Super>End", Action::WorkspaceLast as u32),
    ("<Super><Shift>Page_Up", Action::MoveToWorkspacePrev as u32),
    (
        "<Super><Shift>Page_Down",
        Action::MoveToWorkspaceNext as u32,
    ),
    (
        "<Super><Shift><Alt>Left",
        Action::MoveToWorkspacePrev as u32,
    ),
    (
        "<Super><Shift><Alt>Right",
        Action::MoveToWorkspaceNext as u32,
    ),
    (
        "<Control><Shift><Alt>Left",
        Action::MoveToWorkspacePrev as u32,
    ),
    (
        "<Control><Shift><Alt>Right",
        Action::MoveToWorkspaceNext as u32,
    ),
    ("<Super><Shift>Home", workspaces::MOVE_BASE + 1),
    ("<Super><Shift>End", Action::MoveToWorkspaceLast as u32),
    ("<Super><Alt>Up", Action::ShiftOverviewUp as u32),
    ("<Super><Alt>Down", Action::ShiftOverviewDown as u32),
    ("<Super><Shift>Left", Action::MoveToMonitorLeft as u32),
    ("<Super><Shift>Right", Action::MoveToMonitorRight as u32),
    ("Print", Action::ScreenshotUi as u32),
    ("<Shift>Print", Action::Screenshot as u32),
    ("<Alt>Print", Action::ScreenshotWindow as u32),
];

/// Rebuild [`CONFIG`] from GSettings.
pub fn reload() {
    let mut bindings = Vec::new();
    let mut seen_schema = false;
    let mut push = |accels: Vec<String>, action: u32| {
        bindings.extend(accels.iter().filter_map(|a| parse_accelerator(a)).map(
            |(keysym, mods)| Binding {
                keysym,
                mods,
                action,
            },
        ));
    };
    for (schema, key, action) in SOURCES {
        seen_schema |= gs::settings(schema).is_some();
        push(gs::get_strv(schema, key), *action as u32);
    }
    // switch-to-workspace-N / move-to-workspace-N
    for n in 1..=workspaces::MAX_NUMBERED {
        push(
            gs::get_strv(gs::WM_KEYBINDINGS, &format!("switch-to-workspace-{n}")),
            workspaces::switch_code(n),
        );
        push(
            gs::get_strv(gs::WM_KEYBINDINGS, &format!("move-to-workspace-{n}")),
            workspaces::move_code(n),
        );
    }
    if !seen_schema {
        bindings = DEFAULTS
            .iter()
            .filter_map(|(a, action)| {
                parse_accelerator(a).map(|(k, m)| Binding {
                    keysym: k,
                    mods: m,
                    action: *action,
                })
            })
            .collect();
    }
    let overlay = gs::get_string(gs::MUTTER, "overlay-key", "Super_L");
    let ta = tiling_assistant_active();
    let cfg = Config {
        bindings,
        overlay_keysym: keysym_from_name(&overlay),
        // Tiling Assistant switches mutter's edge-tiling off and does the
        // edge tiling itself, so with it, edge tiling is on.
        edge_tiling: ta || gs::get_bool(gs::MUTTER, "edge-tiling", true),
        layout_picker: !ta || gs::get_bool(TILING_ASSISTANT, "enable-layout-picker", true),
        raise_tile_group: !ta || gs::get_bool(TILING_ASSISTANT, "enable-raise-tile-group", true),
        center_new_windows: gs::get_bool(gs::MUTTER, "center-new-windows", false),
        workspaces: workspaces::Settings {
            dynamic: gs::get_bool(gs::MUTTER, "dynamic-workspaces", true),
            // mutter caps num-workspaces at 36.
            fixed_count: gs::get_int(gs::WM_PREFERENCES, "num-workspaces", 4).clamp(1, 36) as usize,
            only_on_primary: gs::get_bool(gs::MUTTER, "workspaces-only-on-primary", true),
        },
    };
    tracing::info!(
        bindings = cfg.bindings.len(),
        edge_tiling = cfg.edge_tiling,
        "keybindings loaded"
    );
    let new_workspaces = cfg.workspaces;
    let old_workspaces = std::mem::replace(
        &mut *CONFIG.write().unwrap_or_else(|e| e.into_inner()),
        cfg,
    )
    .workspaces;
    // Switching dynamic workspaces off, changing the count or the
    // primary-only rule applies straight away, as in GNOME.
    if new_workspaces != old_workspaces {
        tracing::info!(?new_workspaces, "workspace settings changed");
        crate::shell::workspace_settings_changed();
    }
}

/// Ubuntu ships Tiling Assistant enabled through its session mode; it is
/// active unless the user disabled it.
fn tiling_assistant_active() -> bool {
    if gs::settings(TILING_ASSISTANT).is_none() {
        return false;
    }
    !gs::get_strv(gs::SHELL, "disabled-extensions")
        .iter()
        .any(|e| e == TILING_ASSISTANT_UUID)
}

/// Load once, then keep watching for changes on a helper thread.
pub fn start_watching() {
    reload();
    std::thread::Builder::new()
        .name("umoja-settings".into())
        .spawn(|| {
            let ctx = glib::MainContext::new();
            let _guard = ctx.acquire();
            let _ = ctx.with_thread_default(|| {
                let mut keep = Vec::new();
                let mut schemas: Vec<&str> = SOURCES.iter().map(|s| s.0).collect();
                schemas.push(gs::MUTTER);
                schemas.push(gs::WM_PREFERENCES);
                schemas.push(gs::SHELL);
                schemas.sort_unstable();
                schemas.dedup();
                for schema in schemas {
                    if let Some(s) = gs::settings(schema) {
                        s.connect_changed(None, |_, _| reload());
                        keep.push(s);
                    }
                }
                glib::MainLoop::new(Some(&ctx), false).run();
                drop(keep);
            });
        })
        .expect("spawn settings thread");
}

/// Look up the action code bound to a key press (0 = none).
pub fn lookup(keysym: u32, mods: u32) -> u32 {
    let keysym = normalize(keysym);
    let cfg = CONFIG.read().unwrap_or_else(|e| e.into_inner());
    let action = cfg
        .bindings
        .iter()
        .find(|b| b.keysym == keysym && b.mods == mods)
        .map(|b| b.action)
        .unwrap_or(0);
    tracing::trace!(keysym, mods, action, "key");
    action
}

pub fn read() -> std::sync::RwLockReadGuard<'static, Config> {
    CONFIG.read().unwrap_or_else(|e| e.into_inner())
}

pub fn is_overlay_key(keysym: u32) -> bool {
    let configured = CONFIG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .overlay_keysym;
    // mutter's default is "Super" (GNOME 46+), meaning either Super key.
    configured == Some(keysym) || (configured == Some(XK_SUPER) && matches!(keysym, XK_SUPER_L | XK_SUPER_R))
}

pub fn edge_tiling_enabled() -> bool {
    CONFIG.read().unwrap_or_else(|e| e.into_inner()).edge_tiling
}

/// Shifted keysyms match their unshifted binding: uppercase Latin letters,
/// ISO_Left_Tab (Shift+Tab) and asciitilde (Shift+`, for Above_Tab).
fn normalize(keysym: u32) -> u32 {
    match keysym {
        0x41..=0x5a => keysym + 0x20,
        0xfe20 => 0xff09,
        0x7e => 0x60,
        _ => keysym,
    }
}

/// Parse a GTK accelerator like `<Super><Shift>Left` into (keysym, mods).
pub fn parse_accelerator(accel: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut rest = accel.trim();
    while let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?;
        mods |= match stripped[..end].to_ascii_lowercase().as_str() {
            "super" | "mod4" | "meta" => MOD_SUPER,
            "shift" => MOD_SHIFT,
            "ctrl" | "control" | "primary" => MOD_CTRL,
            "alt" | "mod1" => MOD_ALT,
            _ => return None,
        };
        rest = &stripped[end + 1..];
    }
    if rest.is_empty() || rest.eq_ignore_ascii_case("disabled") {
        return None;
    }
    keysym_from_name(rest).map(|k| (normalize(k), mods))
}

/// Subset of X keysym names that show up in GNOME's default bindings.
pub fn keysym_from_name(name: &str) -> Option<u32> {
    let named = match name {
        "Left" => 0xff51,
        "Up" => 0xff52,
        "Right" => 0xff53,
        "Down" => 0xff54,
        "Page_Up" => 0xff55,
        "Page_Down" => 0xff56,
        "Home" => 0xff50,
        "End" => 0xff57,
        "Return" => 0xff0d,
        "Escape" => 0xff1b,
        "Tab" => 0xff09,
        "space" => 0x20,
        "BackSpace" => 0xff08,
        "Delete" => 0xffff,
        "Print" => 0xff61,
        "Super" => XK_SUPER,
        "Super_L" => 0xffeb,
        "Super_R" => 0xffec,
        "Above_Tab" | "grave" => 0x60,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    if let Some(n) = name
        .strip_prefix("KP_")
        .and_then(|d| d.parse::<u32>().ok())
        .filter(|n| *n <= 9)
    {
        return Some(0xffb0 + n);
    }
    if let Some(n) = name
        .strip_prefix('F')
        .and_then(|d| d.parse::<u32>().ok())
        .filter(|n| (1..=12).contains(n))
    {
        return Some(0xffbe + n - 1);
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() => Some(c.to_ascii_lowercase() as u32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnome_accelerators() {
        assert_eq!(parse_accelerator("<Super>Left"), Some((0xff51, MOD_SUPER)));
        assert_eq!(parse_accelerator("<Super>h"), Some(('h' as u32, MOD_SUPER)));
        assert_eq!(
            parse_accelerator("<Primary><Alt>Delete"),
            Some((0xffff, MOD_CTRL | MOD_ALT))
        );
        assert_eq!(parse_accelerator("<Alt>F4"), Some((0xffc1, MOD_ALT)));
        assert_eq!(parse_accelerator("<Super>KP_7"), Some((0xffb7, MOD_SUPER)));
        assert_eq!(parse_accelerator("<Super>A"), Some(('a' as u32, MOD_SUPER)));
        assert_eq!(parse_accelerator(""), None);
        assert_eq!(parse_accelerator("<Hyper>x"), None);
    }

    #[test]
    fn lookup_normalizes_case() {
        *CONFIG.write().unwrap() = Config {
            bindings: vec![Binding {
                keysym: 'h' as u32,
                mods: MOD_SUPER,
                action: Action::Minimize as u32,
            }],
            overlay_keysym: Some(XK_SUPER_L),
            edge_tiling: true,
            layout_picker: true,
            raise_tile_group: true,
            center_new_windows: false,
            workspaces: workspaces::Settings::default(),
        };
        assert_eq!(lookup('H' as u32, MOD_SUPER), Action::Minimize as u32);
        assert_eq!(lookup('h' as u32, 0), 0);
        assert_eq!(normalize(0xfe20), 0xff09);
        assert!(is_overlay_key(0xffeb));
        CONFIG.write().unwrap().overlay_keysym = keysym_from_name("Super");
        assert!(is_overlay_key(0xffeb) && is_overlay_key(0xffec));
        assert!(!is_overlay_key('a' as u32));
    }
}
