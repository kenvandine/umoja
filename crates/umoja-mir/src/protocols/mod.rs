//! Wayland protocols umoja-mir enables beyond Mir's defaults.
//!
//! Mir already provides `xdg-shell`, `wl_seat`, etc. The extensions below
//! are what the Umoja shell components need:
//!
//! * `zwlr_layer_shell_v1` — panel, dock, overlay and wallpaper surfaces.
//! * `zxdg_output_manager_v1` — per-monitor logical geometry and scale.
//! * `zwlr_foreign_toplevel_manager_v1` — umoja-dock's running-app tracking.
//! * `zwp_virtual_keyboard_manager_v1` / `zwp_input_method_manager_v2` —
//!   Myna dictation typing into the focused application.
//! * `zwlr_screencopy_manager_v1` — screenshots/screencast for the portal.
//! * `ext_session_lock_manager_v1` — the lock screen.
//! * `zwp_primary_selection_device_manager_v1` — select-to-copy and
//!   middle-click paste, as on GNOME.
//! * `ext_data_control_manager_v1` — umoja-overlay's clipboard history.
//!   Only offered to Umoja's own `umoja-*` binaries (see the shim), so apps
//!   can't watch the clipboard.
//! * `wp_fractional_scale_manager_v1` / `wp_viewporter` — crisp fractional
//!   scaling (125%, 150%, 175%...) for GTK4, Qt6, Firefox and Chromium, as
//!   with mutter's scale-monitor-framebuffer. Skipped on Mir builds that
//!   predate them.
//! * `zwp_idle_inhibit_manager_v1` — video players and browsers keep the
//!   screen from blanking while they play (see [`crate::idle`]).
//! * `ext_background_effect_manager_v1` — blur behind the translucent panel,
//!   dock, menus, notification bubbles and OSD. Only in Umoja's patched Mir
//!   (`packaging/mir/patches/3004_background-blur.patch`); skipped elsewhere.
//!
//! `zxdg_decoration_manager_v1` is deliberately disabled (see the shim):
//! mutter doesn't offer it either, so apps decorate themselves exactly as
//! they do on Ubuntu's GNOME session.

use std::ffi::CString;

/// Names as MirAL spells them (`miral::WaylandExtensions::*`).
pub const ENABLED: &[&str] = &[
    "zwlr_layer_shell_v1",
    "zxdg_output_manager_v1",
    "zwlr_foreign_toplevel_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zwp_input_method_manager_v2",
    "zwlr_screencopy_manager_v1",
    "ext_session_lock_manager_v1",
    "zwp_primary_selection_device_manager_v1",
    "ext_data_control_manager_v1",
    "wp_fractional_scale_manager_v1",
    "wp_viewporter",
    "zwp_idle_inhibit_manager_v1",
    "ext_background_effect_manager_v1",
];

pub fn to_cstrings(names: &[&str]) -> Vec<CString> {
    names
        .iter()
        .map(|s| CString::new(*s).expect("no NUL"))
        .collect()
}
