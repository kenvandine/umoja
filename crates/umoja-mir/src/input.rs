//! Keybindings, read live from the GNOME schemas.
//!
//! Umoja honours the same keys as Ubuntu's GNOME session
//! (`org.gnome.desktop.wm.keybindings`, `org.gnome.mutter.keybindings`,
//! `org.gnome.shell.keybindings`), so shortcuts changed in Settings apply
//! here immediately. Changes are picked up by a GLib main loop on a helper
//! thread that rebuilds the table behind an `RwLock`.
//!
//! GNOME's custom shortcuts (media-keys `custom-keybindings`, which
//! gsd-media-keys runs in a GNOME session) are honoured too: each one runs
//! its command. That is how Myna's dictation key works: Myna Settings writes
//! a custom shortcut (Super+J by default) whose command calls Myna's
//! `Toggle` over D-Bus.

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
    /// A lone tap of the left Alt key opens Ask Umoja. On by default, and
    /// off once a Copilot key has been seen: then that key is the only one
    /// (see [`copilot_pressed`]).
    pub ask_alt_tap: bool,
    pub edge_tiling: bool,
    /// Ubuntu Tiling Assistant's layout picker (drag to the top edge).
    pub layout_picker: bool,
    /// Tiling Assistant: focusing a tiled window raises its tile group.
    pub raise_tile_group: bool,
    pub center_new_windows: bool,
    /// `org.gnome.desktop.wm.preferences mouse-button-modifier` as
    /// `MOD_*` bits (0: off). Holding it, left drag moves a window, middle
    /// drag resizes it and right click opens the window menu.
    pub mouse_button_modifier: u32,
    /// `resize-with-right-button`: swap the middle and right buttons.
    pub resize_with_right_button: bool,
    pub workspaces: workspaces::Settings,
    /// Commands of GNOME custom shortcuts; binding action
    /// `CUSTOM_BASE + i` runs `custom_commands[i]`.
    pub custom_commands: Vec<String>,
    /// (id, accelerator) of every binding above, for the shortcuts guide:
    /// the GSettings key it came from (`custom:<name>` for a custom
    /// shortcut, `overlay-key` for the lone Super tap).
    pub shortcuts: Vec<(String, String)>,
}

/// Action codes from here up run a GNOME custom shortcut's command.
/// Mirrors `UMOJA_ACTION_CUSTOM_BASE` in the shim.
pub const CUSTOM_BASE: u32 = 1000;

/// Ubuntu's Tiling Assistant extension (enabled by default in Ubuntu).
pub const TILING_ASSISTANT: &str = "org.gnome.shell.extensions.tiling-assistant";
const TILING_ASSISTANT_UUID: &str = "tiling-assistant@ubuntu.com";

pub static CONFIG: RwLock<Config> = RwLock::new(Config {
    bindings: Vec::new(),
    overlay_keysym: Some(XK_SUPER_L),
    ask_alt_tap: true,
    edge_tiling: true,
    layout_picker: true,
    raise_tile_group: true,
    center_new_windows: false,
    mouse_button_modifier: MOD_SUPER,
    resize_with_right_button: false,
    workspaces: workspaces::Settings {
        dynamic: true,
        fixed_count: 4,
        only_on_primary: true,
    },
    custom_commands: Vec::new(),
    shortcuts: Vec::new(),
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
    (
        gs::SHELL_KEYBINDINGS,
        "toggle-overview",
        Action::ToggleOverview,
    ),
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
    (gs::WM_KEYBINDINGS, "cycle-group", Action::CycleGroup),
    (
        gs::WM_KEYBINDINGS,
        "cycle-group-backward",
        Action::CycleGroupBackward,
    ),
    (gs::WM_KEYBINDINGS, "cycle-windows", Action::CycleWindows),
    (
        gs::WM_KEYBINDINGS,
        "cycle-windows-backward",
        Action::CycleWindowsBackward,
    ),
    (gs::WM_KEYBINDINGS, "switch-panels", Action::SwitchPanels),
    (
        gs::WM_KEYBINDINGS,
        "switch-panels-backward",
        Action::SwitchPanelsBackward,
    ),
    (gs::WM_KEYBINDINGS, "cycle-panels", Action::CyclePanels),
    (
        gs::WM_KEYBINDINGS,
        "cycle-panels-backward",
        Action::CyclePanelsBackward,
    ),
    (gs::WM_KEYBINDINGS, "begin-move", Action::BeginMove),
    (gs::WM_KEYBINDINGS, "begin-resize", Action::BeginResize),
    (
        gs::WM_KEYBINDINGS,
        "activate-window-menu",
        Action::WindowMenu,
    ),
    (gs::WM_KEYBINDINGS, "always-on-top", Action::ToggleAbove),
    (
        gs::WM_KEYBINDINGS,
        "toggle-on-all-workspaces",
        Action::ToggleSticky,
    ),
    (
        gs::SHELL_KEYBINDINGS,
        "toggle-message-tray",
        Action::ToggleMessageTray,
    ),
    // Ubuntu's Tiling Assistant takes over Super+arrows (it empties mutter's
    // toggle-tiled-*/maximize keys while enabled) and adds keypad quarters.
    (TILING_ASSISTANT, "tile-left-half", Action::TileLeft),
    (TILING_ASSISTANT, "tile-right-half", Action::TileRight),
    (TILING_ASSISTANT, "tile-top-half", Action::TileTopHalf),
    (TILING_ASSISTANT, "tile-bottom-half", Action::TileBottomHalf),
    (
        TILING_ASSISTANT,
        "tile-topleft-quarter",
        Action::TileTopLeft,
    ),
    (
        TILING_ASSISTANT,
        "tile-topright-quarter",
        Action::TileTopRight,
    ),
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
    (
        gs::SHELL_KEYBINDINGS,
        "show-screen-recording-ui",
        Action::ScreenRecordingUi,
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
    (
        gs::UMOJA_SHELL,
        "ask-about-screen-hotkey",
        Action::AskAboutScreen,
    ),
    (
        gs::UMOJA_SHELL,
        "clipboard-history-hotkey",
        Action::ClipboardHistory,
    ),
    (gs::UMOJA_SHELL, "copy-text-hotkey", Action::CopyTextUi),
    (
        gs::UMOJA_SHELL,
        "shortcuts-guide-hotkey",
        Action::ShowShortcuts,
    ),
    (gs::UMOJA_SHELL, "ai-agent-hotkey", Action::LaunchAgent),
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
    (
        "<Shift><Super>Tab",
        Action::SwitchApplicationsBackward as u32,
    ),
    ("<Alt>Above_Tab", Action::SwitchGroup as u32),
    ("<Super>Above_Tab", Action::SwitchGroup as u32),
    ("<Shift><Alt>Above_Tab", Action::SwitchGroupBackward as u32),
    (
        "<Shift><Super>Above_Tab",
        Action::SwitchGroupBackward as u32,
    ),
    ("<Alt>F5", Action::Unmaximize as u32),
    ("<Alt>F10", Action::ToggleMaximized as u32),
    ("<Alt>F6", Action::CycleGroup as u32),
    ("<Shift><Alt>F6", Action::CycleGroupBackward as u32),
    ("<Alt>Escape", Action::CycleWindows as u32),
    ("<Shift><Alt>Escape", Action::CycleWindowsBackward as u32),
    ("<Control><Alt>Tab", Action::SwitchPanels as u32),
    (
        "<Shift><Control><Alt>Tab",
        Action::SwitchPanelsBackward as u32,
    ),
    ("<Control><Alt>Escape", Action::CyclePanels as u32),
    (
        "<Shift><Control><Alt>Escape",
        Action::CyclePanelsBackward as u32,
    ),
    ("<Alt>F7", Action::BeginMove as u32),
    ("<Alt>F8", Action::BeginResize as u32),
    ("<Alt>space", Action::WindowMenu as u32),
    ("<Super>v", Action::ToggleMessageTray as u32),
    ("<Super>m", Action::ToggleMessageTray as u32),
    ("<Super>KP_7", Action::TileTopLeft as u32),
    ("<Super>KP_9", Action::TileTopRight as u32),
    ("<Super>KP_1", Action::TileBottomLeft as u32),
    ("<Super>KP_3", Action::TileBottomRight as u32),
    ("<Super>KP_8", Action::TileTopHalf as u32),
    ("<Super>KP_2", Action::TileBottomHalf as u32),
    ("<Super><Shift>a", Action::AskAboutScreen as u32),
    ("<Super><Shift>v", Action::ClipboardHistory as u32),
    ("<Super><Control>Print", Action::CopyTextUi as u32),
    ("<Super>slash", Action::ShowShortcuts as u32),
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
    ("<Control><Shift><Alt>R", Action::ScreenRecordingUi as u32),
];

/// Rebuild [`CONFIG`] from GSettings.
pub fn reload() {
    let mut bindings = Vec::new();
    let mut shortcuts = Vec::new();
    let mut seen_schema = false;
    let mut push = |id: &str, accels: Vec<String>, action: u32| {
        for a in accels {
            if let Some((keysym, mods)) = parse_accelerator(&a) {
                bindings.push(Binding {
                    keysym,
                    mods,
                    action,
                });
                shortcuts.push((id.to_string(), a));
            }
        }
    };
    let ta = tiling_assistant_active();
    // Accelerators Tiling Assistant binds itself (Super+arrows, keypad).
    let ta_accels: Vec<(u32, u32)> = if ta {
        SOURCES
            .iter()
            .filter(|(schema, _, _)| *schema == TILING_ASSISTANT)
            .flat_map(|(schema, key, _)| gs::get_strv(schema, key))
            .filter_map(|a| parse_accelerator(&a))
            .collect()
    } else {
        Vec::new()
    };
    for (schema, key, action) in SOURCES {
        seen_schema |= gs::settings(schema).is_some();
        let mut accels = gs::get_strv(schema, key);
        if ta && accels.is_empty() && TA_EMPTIED.contains(&(*schema, *key)) {
            accels = ta_restored(schema, key, &ta_accels);
        }
        push(key, accels, *action as u32);
    }
    // switch-to-workspace-N / move-to-workspace-N
    for n in 1..=workspaces::MAX_NUMBERED {
        let key = format!("switch-to-workspace-{n}");
        push(
            &key,
            gs::get_strv(gs::WM_KEYBINDINGS, &key),
            workspaces::switch_code(n),
        );
        let key = format!("move-to-workspace-{n}");
        push(
            &key,
            gs::get_strv(gs::WM_KEYBINDINGS, &key),
            workspaces::move_code(n),
        );
    }
    if !seen_schema {
        bindings.clear();
        shortcuts.clear();
        for (a, action) in DEFAULTS {
            if let Some((keysym, mods)) = parse_accelerator(a) {
                bindings.push(Binding {
                    keysym,
                    mods,
                    action: *action,
                });
                shortcuts.push((default_id(*action), a.to_string()));
            }
        }
    }
    // Volume, brightness and playback keys, from gnome-settings-daemon's
    // schema (or its defaults when that isn't installed).
    for (name, accel, binding) in crate::media_keys::bindings() {
        bindings.push(binding);
        shortcuts.push((name.to_string(), accel));
    }
    // GNOME custom shortcuts come after the built-in ones, so a built-in
    // binding on the same key wins (gsd-media-keys can't grab a key the
    // shell already holds either).
    let mut custom_commands = Vec::new();
    for (name, binding, command) in custom_keybindings() {
        let Some((keysym, mods)) = parse_accelerator(&binding) else {
            continue;
        };
        bindings.push(Binding {
            keysym,
            mods,
            action: CUSTOM_BASE + custom_commands.len() as u32,
        });
        let name = if name.is_empty() {
            command.clone()
        } else {
            name
        };
        shortcuts.push((format!("custom:{name}"), binding));
        custom_commands.push(command);
    }
    let overlay = gs::get_string(gs::MUTTER, "overlay-key", "Super_L");
    if keysym_from_name(&overlay).is_some() {
        shortcuts.insert(0, ("overlay-key".to_string(), overlay.clone()));
    }
    let ask_alt_tap = alt_tap_wanted(
        gs::get_bool(gs::UMOJA_SHELL, ASK_ALT_TAP_KEY, true),
        gs::get_bool(gs::UMOJA_SHELL, COPILOT_SEEN_KEY, false),
    );
    shortcuts.insert(
        0,
        (
            "ask-umoja".to_string(),
            if ask_alt_tap { "Alt_L" } else { "Copilot" }.to_string(),
        ),
    );
    let cfg = Config {
        bindings,
        overlay_keysym: keysym_from_name(&overlay),
        ask_alt_tap,
        // Tiling Assistant switches mutter's edge-tiling off and does the
        // edge tiling itself, so with it, edge tiling is on.
        edge_tiling: ta || gs::get_bool(gs::MUTTER, "edge-tiling", true),
        layout_picker: !ta || gs::get_bool(TILING_ASSISTANT, "enable-layout-picker", true),
        raise_tile_group: !ta || gs::get_bool(TILING_ASSISTANT, "enable-raise-tile-group", true),
        center_new_windows: gs::get_bool(gs::MUTTER, "center-new-windows", false),
        mouse_button_modifier: parse_modifiers(&gs::get_string(
            gs::WM_PREFERENCES,
            "mouse-button-modifier",
            "<Super>",
        )),
        resize_with_right_button: gs::get_bool(
            gs::WM_PREFERENCES,
            "resize-with-right-button",
            false,
        ),
        workspaces: workspaces::Settings {
            dynamic: gs::get_bool(gs::MUTTER, "dynamic-workspaces", true),
            // mutter caps num-workspaces at 36.
            fixed_count: gs::get_int(gs::WM_PREFERENCES, "num-workspaces", 4).clamp(1, 36) as usize,
            only_on_primary: gs::get_bool(gs::MUTTER, "workspaces-only-on-primary", true),
        },
        custom_commands,
        shortcuts,
    };
    tracing::info!(
        bindings = cfg.bindings.len(),
        edge_tiling = cfg.edge_tiling,
        custom = cfg.custom_commands.len(),
        "keybindings loaded"
    );
    let new_workspaces = cfg.workspaces;
    let old_workspaces =
        std::mem::replace(&mut *CONFIG.write().unwrap_or_else(|e| e.into_inner()), cfg).workspaces;
    // Switching dynamic workspaces off, changing the count or the
    // primary-only rule applies straight away, as in GNOME.
    if new_workspaces != old_workspaces {
        tracing::info!(?new_workspaces, "workspace settings changed");
        crate::shell::workspace_settings_changed();
    }
}

/// (name, binding, command) of every GNOME custom shortcut with a binding
/// and a command, in `custom-keybindings` order.
fn custom_keybindings() -> Vec<(String, String, String)> {
    custom_keybinding_paths()
        .iter()
        .filter_map(|path| {
            let s = gs::settings_at(gs::CUSTOM_KEYBINDING, path)?;
            let name = s.string("name").trim().to_string();
            let binding = s.string("binding").to_string();
            let command = s.string("command").trim().to_string();
            (!binding.is_empty() && !command.is_empty()).then_some((name, binding, command))
        })
        .collect()
}

/// The GSettings key a fallback binding stands for (the first key in
/// [`SOURCES`] with that action), so the guide can name it.
fn default_id(action: u32) -> String {
    if let Some((_, key, _)) = SOURCES.iter().find(|(_, _, a)| *a as u32 == action) {
        return key.to_string();
    }
    if action == workspaces::switch_code(1) {
        return "switch-to-workspace-1".into();
    }
    if action == workspaces::move_code(1) {
        return "move-to-workspace-1".into();
    }
    String::new()
}

/// (id, accelerator) of every shortcut the compositor handles, for the
/// shortcuts guide (`org.umoja.Compositor.GetShortcuts`).
pub fn shortcuts() -> Vec<(String, String)> {
    read().shortcuts.clone()
}

fn custom_keybinding_paths() -> Vec<String> {
    gs::get_strv(gs::MEDIA_KEYS, "custom-keybindings")
        .into_iter()
        .filter(|p| p.starts_with('/') && p.ends_with('/'))
        .collect()
}

/// Run the command of custom shortcut `code`, as gsd-media-keys does:
/// shell-style word splitting, no shell, not waited for.
pub fn run_custom(code: u32) {
    let Some(index) = code.checked_sub(CUSTOM_BASE) else {
        return;
    };
    let command = {
        let cfg = CONFIG.read().unwrap_or_else(|e| e.into_inner());
        match cfg.custom_commands.get(index as usize) {
            Some(c) => c.clone(),
            None => return,
        }
    };
    let argv = match glib::shell_parse_argv(&command) {
        Ok(argv) if !argv.is_empty() => argv,
        _ => {
            tracing::warn!(command, "custom shortcut: cannot parse command");
            return;
        }
    };
    tracing::info!(command, "custom shortcut");
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    match cmd.spawn() {
        // Reap it off Mir's input thread.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(command, error = %e, "custom shortcut: could not run"),
    }
}

/// Keys Tiling Assistant empties in dconf while it runs in GNOME (and leaves
/// empty after a GNOME session that didn't disable it cleanly).
const TA_EMPTIED: &[(&str, &str)] = &[
    (gs::WM_KEYBINDINGS, "maximize"),
    (gs::WM_KEYBINDINGS, "unmaximize"),
    (gs::MUTTER_KEYBINDINGS, "toggle-tiled-left"),
    (gs::MUTTER_KEYBINDINGS, "toggle-tiled-right"),
];

/// The schema default of a key Tiling Assistant emptied, minus what it binds
/// itself, so e.g. unmaximize keeps Alt+F5 while Super+Down stays Tiling
/// Assistant's restore-window (as Ubuntu documents).
fn ta_restored(schema: &str, key: &str, ta_accels: &[(u32, u32)]) -> Vec<String> {
    let Some(s) = gs::settings(schema) else {
        return Vec::new();
    };
    s.default_value(key)
        .and_then(|v| v.get::<Vec<String>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|a| parse_accelerator(a).is_some_and(|p| !ta_accels.contains(&p)))
        .collect()
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
                schemas.push(gs::MEDIA_KEYS);
                schemas.sort_unstable();
                schemas.dedup();
                for schema in schemas {
                    if let Some(s) = gs::settings(schema) {
                        s.connect_changed(None, |_, _| reload());
                        keep.push(s);
                    }
                }
                // Custom shortcuts: the list, and each entry it names (Myna
                // Settings rebinds by editing its entry in place).
                let entries = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                watch_custom_entries(&entries);
                if let Some(s) = gs::settings(gs::MEDIA_KEYS) {
                    let entries = entries.clone();
                    s.connect_changed(Some("custom-keybindings"), move |_, _| {
                        watch_custom_entries(&entries);
                        reload();
                    });
                    keep.push(s);
                }
                glib::MainLoop::new(Some(&ctx), false).run();
                drop(keep);
                drop(entries);
            });
        })
        .expect("spawn settings thread");
}

fn watch_custom_entries(entries: &std::cell::RefCell<Vec<gio::Settings>>) {
    let mut entries = entries.borrow_mut();
    entries.clear();
    for path in custom_keybinding_paths() {
        if let Some(s) = gs::settings_at(gs::CUSTOM_KEYBINDING, &path) {
            s.connect_changed(None, |_, _| reload());
            // GSettings only emits `changed` for keys that were read.
            let _ = (s.string("binding"), s.string("command"));
            entries.push(s);
        }
    }
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

/// org.umoja.shell keys for the Ask Umoja key.
const ASK_ALT_TAP_KEY: &str = "ask-umoja-alt-tap";
const COPILOT_SEEN_KEY: &str = "ask-umoja-copilot-seen";

/// Alt taps open Ask Umoja when allowed and no Copilot key was seen yet:
/// keyboards with a Copilot key leave Alt to the apps (Firefox's menu bar).
fn alt_tap_wanted(allowed: bool, copilot_seen: bool) -> bool {
    allowed && !copilot_seen
}

pub fn ask_alt_tap_enabled() -> bool {
    CONFIG.read().unwrap_or_else(|e| e.into_inner()).ask_alt_tap
}

/// The Copilot key was pressed: open Ask Umoja, and from now on leave the
/// lone Alt tap alone. Keyboards report keys they don't have, so a real
/// press is the only reliable sign that this one exists. It is remembered
/// in GSettings (written off the input thread), which also reloads the
/// config through the watcher.
pub fn copilot_pressed() {
    let first = {
        let mut cfg = CONFIG.write().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut cfg.ask_alt_tap, false)
    };
    if first {
        tracing::info!("Copilot key seen: the Alt tap no longer opens Ask Umoja");
        std::thread::spawn(|| {
            if let Some(s) = gs::settings(gs::UMOJA_SHELL)
                && gs::has_key(&s, COPILOT_SEEN_KEY)
                && !s.boolean(COPILOT_SEEN_KEY)
            {
                let _ = s.set_boolean(COPILOT_SEEN_KEY, true);
                gio::Settings::sync();
            }
        });
    }
    crate::session::dispatch(Action::AskUmoja);
}

pub fn is_overlay_key(keysym: u32) -> bool {
    let configured = CONFIG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .overlay_keysym;
    // mutter's default is "Super" (GNOME 46+), meaning either Super key.
    configured == Some(keysym)
        || (configured == Some(XK_SUPER) && matches!(keysym, XK_SUPER_L | XK_SUPER_R))
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
    let (mods, rest) = split_modifiers(accel)?;
    if rest.is_empty() || rest.eq_ignore_ascii_case("disabled") {
        return None;
    }
    keysym_from_name(rest).map(|k| (normalize(k), mods))
}

/// Leading `<Mod>` tokens of an accelerator as `MOD_*` bits, and the rest.
fn split_modifiers(accel: &str) -> Option<(u32, &str)> {
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
    Some((mods, rest))
}

/// A modifiers-only accelerator such as `mouse-button-modifier`'s
/// `<Super>`. mutter ignores anything that is not just modifiers; an empty
/// or unparsable value turns modified clicks off.
pub fn parse_modifiers(accel: &str) -> u32 {
    match split_modifiers(accel) {
        Some((mods, rest)) if rest.trim().is_empty() => mods,
        _ => 0,
    }
}

/// Subset of X keysym names that show up in GNOME's default bindings.
pub fn keysym_from_name(name: &str) -> Option<u32> {
    if let Some(k) = crate::media_keys::keysym(name) {
        return Some(k);
    }
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
        "slash" => 0x2f,
        "backslash" => 0x5c,
        "question" => 0x3f,
        "comma" => 0x2c,
        "period" => 0x2e,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "plus" => 0x2b,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
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
    #[test]
    fn alt_tap_only_without_a_copilot_key() {
        assert!(alt_tap_wanted(true, false));
        assert!(!alt_tap_wanted(true, true));
        assert!(!alt_tap_wanted(false, false));
    }

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
    fn parses_mouse_button_modifier() {
        assert_eq!(parse_modifiers("<Super>"), MOD_SUPER);
        assert_eq!(parse_modifiers("<Alt>"), MOD_ALT);
        assert_eq!(parse_modifiers("<Control><Alt>"), MOD_CTRL | MOD_ALT);
        assert_eq!(parse_modifiers(""), 0);
        assert_eq!(parse_modifiers("disabled"), 0);
        assert_eq!(parse_modifiers("<Super>a"), 0);
        assert_eq!(parse_modifiers("<Hyper>"), 0);
    }

    #[test]
    fn every_fallback_binding_parses() {
        for (accel, action) in DEFAULTS {
            assert!(parse_accelerator(accel).is_some(), "{accel}");
            // 100+n / 200+n are numbered workspace codes.
            assert!(
                *action >= 100 || Action::from_u32(*action) != Action::None,
                "{accel}"
            );
        }
        assert_eq!(parse_accelerator("<Alt>space"), Some((0x20, MOD_ALT)));
        assert_eq!(parse_accelerator("<Alt>Escape"), Some((0xff1b, MOD_ALT)));
        assert_eq!(
            parse_accelerator("<Control><Alt>Tab"),
            Some((0xff09, MOD_CTRL | MOD_ALT))
        );
    }

    /// Ubuntu's "Navigate desktop" / "Navigate windows" shortcuts (the
    /// keyboard-navigation-shortcuts page) all resolve to the right action.
    #[test]
    fn ubuntu_navigation_shortcuts() {
        let find = |accel: &str| -> Vec<Action> {
            let (k, m) = parse_accelerator(accel).unwrap();
            DEFAULTS
                .iter()
                .filter(|(a, _)| parse_accelerator(a) == Some((k, m)))
                .map(|(_, act)| Action::from_u32(*act))
                .collect()
        };
        for (accel, want) in [
            ("<Super>Tab", Action::SwitchApplications),
            ("<Shift><Super>Tab", Action::SwitchApplicationsBackward),
            ("<Super>grave", Action::SwitchGroup),
            ("<Control><Alt>Tab", Action::SwitchPanels),
            ("<Alt>F6", Action::CycleGroup),
            ("<Alt>Escape", Action::CycleWindows),
            ("<Super>v", Action::ToggleMessageTray),
            ("<Alt>F4", Action::Close),
            ("<Alt>F5", Action::Unmaximize),
            ("<Super>Down", Action::Unmaximize),
            ("<Alt>F7", Action::BeginMove),
            ("<Alt>F8", Action::BeginResize),
            ("<Shift><Super>Left", Action::MoveToMonitorLeft),
            ("<Shift><Super>Right", Action::MoveToMonitorRight),
            ("<Alt>F10", Action::ToggleMaximized),
            ("<Super>Up", Action::Maximize),
            ("<Super>h", Action::Minimize),
            ("<Super>Left", Action::TileLeft),
            ("<Super>Right", Action::TileRight),
            ("<Alt>space", Action::WindowMenu),
        ] {
            assert_eq!(find(accel), vec![want], "{accel}");
        }
    }

    #[test]
    fn punctuation_keys_parse() {
        assert_eq!(parse_accelerator("<Super>slash"), Some((0x2f, MOD_SUPER)));
        assert_eq!(parse_accelerator("<Super>/"), Some((0x2f, MOD_SUPER)));
        assert_eq!(
            parse_accelerator("<Control>bracketleft"),
            Some((0x5b, MOD_CTRL))
        );
    }

    /// Every fallback binding has a GSettings key name for the guide.
    #[test]
    fn every_fallback_binding_has_an_id() {
        for (accel, action) in DEFAULTS {
            assert!(!default_id(*action).is_empty(), "{accel}");
        }
        assert_eq!(
            default_id(Action::ShowShortcuts as u32),
            "shortcuts-guide-hotkey"
        );
        assert_eq!(default_id(Action::TileLeft as u32), "toggle-tiled-left");
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
            ask_alt_tap: true,
            edge_tiling: true,
            layout_picker: true,
            raise_tile_group: true,
            center_new_windows: false,
            mouse_button_modifier: MOD_SUPER,
            resize_with_right_button: false,
            workspaces: workspaces::Settings::default(),
            custom_commands: Vec::new(),
            shortcuts: Vec::new(),
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
