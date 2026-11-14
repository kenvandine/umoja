//! Icons gnome-shell draws from its own copies, kept by Umoja too, so the
//! shell looks right on systems without GNOME Shell installed.
//!
//! The SVGs (all of gnome-shell's, see data/icons/README.md) live in
//! data/icons/hicolor and are installed to
//! /usr/share/umoja/icons/hicolor. Adding that folder to the icon theme's
//! search path makes them part of the hicolor theme, which every theme
//! inherits, so a theme that has its own version of an icon still wins.


/// Where the package installs the icons.
const INSTALLED: &str = "/usr/share/umoja/icons";

/// Folders to search: $UMOJA_ICON_DIR (for running from a checkout), then
/// the installed copy.
fn dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("UMOJA_ICON_DIR") {
        dirs.push(d.into());
    }
    dirs.push(INSTALLED.into());
    dirs
}

/// Add Umoja's icons to the default display's icon theme, and take the
/// desktop's theme settings from GSettings ([`follow_desktop_theme`]).
/// Call once at startup, before building any widgets.
pub fn add_shell_icons() {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    follow_desktop_theme();
    let theme = gtk4::IconTheme::for_display(&display);
    for d in dirs() {
        if d.is_dir() {
            theme.add_search_path(&d);
        }
    }
}

/// Apply the desktop's icon theme, font and cursor from GSettings
/// (org.gnome.desktop.interface) to GTK, and follow later changes.
///
/// GTK 4 on Wayland only learns these from the Settings portal. When the
/// portal is stuck (a stale D-Bus activation of its GTK backend left over
/// from the last logout kept it waiting for two minutes), GTK gives up
/// after 25 s and keeps its built-in defaults: the shell showed Adwaita
/// icons instead of Yaru until it was restarted. The shell isn't sandboxed,
/// so it can read the same keys itself. (GDK_DEBUG=no-portals is no help:
/// GTK then doesn't read GSettings either.)
pub fn follow_desktop_theme() {
    use gio::prelude::*;
    let Some(gs) = crate::gsettings::settings(crate::gsettings::DESKTOP_INTERFACE) else {
        return;
    };
    let apply = |gs: &gio::Settings| {
        let Some(gtk) = gtk4::Settings::default() else { return };
        let keys = gs.settings_schema().map(|s| s.list_keys()).unwrap_or_default();
        let has = |k: &str| keys.iter().any(|x| x == k);
        let text = |k: &str| (has(k)).then(|| gs.string(k)).filter(|v| !v.is_empty());
        if let Some(v) = text("icon-theme") {
            gtk.set_gtk_icon_theme_name(Some(v.as_str()));
        }
        // Not gtk-theme: libadwaita styles the shell, and warns when it is set.
        if let Some(v) = text("font-name") {
            gtk.set_gtk_font_name(Some(v.as_str()));
        }
        if let Some(v) = text("cursor-theme") {
            gtk.set_gtk_cursor_theme_name(Some(v.as_str()));
        }
        if has("cursor-size") {
            gtk.set_gtk_cursor_theme_size(gs.int("cursor-size"));
        }
    };
    apply(&gs);
    gs.connect_changed(None, move |gs, _| apply(gs));
    // The handler lives as long as the settings object.
    std::mem::forget(gs);
}
