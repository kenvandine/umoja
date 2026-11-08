//! Mapping between Wayland app-ids and `.desktop` files, and launching apps.

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;
use gtk4 as gtk;

/// Find the desktop entry for a running window's app-id.
pub fn app_for_window(app_id: &str) -> Option<gio::DesktopAppInfo> {
    umoja_common::apps::app_for_window(app_id)
}

/// Desktop id without the `.desktop` suffix, used as the dock's key.
pub fn key_for(info: &gio::DesktopAppInfo) -> String {
    info.id()
        .map(|s| s.trim_end_matches(".desktop").to_string())
        .unwrap_or_default()
}

pub fn launch(info: &gio::DesktopAppInfo) {
    let ctx = gdk::Display::default().map(|d| d.app_launch_context());
    if let Err(e) = info.launch(&[], ctx.as_ref()) {
        tracing::warn!(app = ?info.id(), error = %e, "launch failed");
    }
}

/// `org.gnome.shell favorite-apps`, the same pinned list Ubuntu Dock uses.
pub fn favorites() -> Vec<gio::DesktopAppInfo> {
    umoja_common::gsettings::get_strv(umoja_common::gsettings::SHELL, "favorite-apps")
        .iter()
        .filter_map(|id| gio::DesktopAppInfo::new(id))
        .collect()
}
