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
    pub center_new_windows: bool,
    pub workspaces: workspaces::Settings,
}

pub static CONFIG: RwLock<Config> = RwLock::new(Config {
    bindings: Vec::new(),
    overlay_keysym: Some(XK_SUPER_L),
    edge_tiling: true,
    center_new_windows: false,
    workspaces: workspaces::Settings {
        dynamic: true,
        fixed_count: 4,
        only_on_primary: true,
    },
});

const XK_SUPER_L: u32 = 0xffeb;

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
    ("<Super>d", Action::ToggleDictation as u32),
    ("<Super>Page_Up", Action::WorkspacePrev as u32),
    ("<Super>Page_Down", Action::WorkspaceNext as u32),
    ("<Control><Alt>Left", Action::WorkspacePrev as u32),
    ("<Control><Alt>Right", Action::WorkspaceNext as u32),
    ("<Super><Shift>Page_Up", Action::MoveToWorkspacePrev as u32),
    (
        "<Super><Shift>Page_Down",
        Action::MoveToWorkspaceNext as u32,
    ),
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
    let cfg = Config {
        bindings,
        overlay_keysym: keysym_from_name(&overlay),
        edge_tiling: gs::get_bool(gs::MUTTER, "edge-tiling", true),
        center_new_windows: gs::get_bool(gs::MUTTER, "center-new-windows", false),
        workspaces: workspaces::Settings {
            dynamic: gs::get_bool(gs::MUTTER, "dynamic-workspaces", true),
            fixed_count: gs::get_int(gs::WM_PREFERENCES, "num-workspaces", 4).max(1) as usize,
            only_on_primary: gs::get_bool(gs::MUTTER, "workspaces-only-on-primary", true),
        },
    };
    tracing::info!(
        bindings = cfg.bindings.len(),
        edge_tiling = cfg.edge_tiling,
        "keybindings loaded"
    );
    *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = cfg;
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
    cfg.bindings
        .iter()
        .find(|b| b.keysym == keysym && b.mods == mods)
        .map(|b| b.action)
        .unwrap_or(0)
}

pub fn read() -> std::sync::RwLockReadGuard<'static, Config> {
    CONFIG.read().unwrap_or_else(|e| e.into_inner())
}

pub fn is_overlay_key(keysym: u32) -> bool {
    CONFIG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .overlay_keysym
        == Some(keysym)
}

pub fn edge_tiling_enabled() -> bool {
    CONFIG.read().unwrap_or_else(|e| e.into_inner()).edge_tiling
}

/// Uppercase Latin letters (Shift held) match their lowercase binding.
fn normalize(keysym: u32) -> u32 {
    if (0x41..=0x5a).contains(&keysym) {
        keysym + 0x20
    } else {
        keysym
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
            center_new_windows: false,
            workspaces: workspaces::Settings::default(),
        };
        assert_eq!(lookup('H' as u32, MOD_SUPER), Action::Minimize as u32);
        assert_eq!(lookup('h' as u32, 0), 0);
        assert!(is_overlay_key(0xffeb));
    }
}
