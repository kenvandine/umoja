//! Icons gnome-shell draws from its own copies, kept by Umoja too, so the
//! shell looks right on systems without GNOME Shell installed.
//!
//! The SVGs live in data/icons/hicolor and are installed to
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

/// Add Umoja's icons to the default display's icon theme. Call once at
/// startup, before building any widgets.
pub fn add_shell_icons() {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    let theme = gtk4::IconTheme::for_display(&display);
    for d in dirs() {
        if d.is_dir() {
            theme.add_search_path(&d);
        }
    }
}
