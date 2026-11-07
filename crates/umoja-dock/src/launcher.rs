//! Mapping between Wayland app-ids and `.desktop` files, and launching apps.

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;
use gtk4 as gtk;

/// Find the desktop entry for a running window's app-id.
///
/// Tries, in order: `<app_id>.desktop`, the lower-cased id, the last
/// reverse-DNS component, and finally any entry whose `StartupWMClass`
/// matches (this is how Ubuntu Dock matches Electron and X11 apps).
pub fn app_for_window(app_id: &str) -> Option<gio::DesktopAppInfo> {
    if app_id.is_empty() {
        return None;
    }
    let lower = app_id.to_lowercase();
    let last = lower.rsplit('.').next().unwrap_or(&lower).to_string();
    for candidate in [app_id.to_string(), lower.clone(), last] {
        if let Some(info) = gio::DesktopAppInfo::new(&format!("{candidate}.desktop")) {
            return Some(info);
        }
    }
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|a| a.downcast::<gio::DesktopAppInfo>().ok())
        .find(|d| {
            d.startup_wm_class()
                .is_some_and(|c| c.eq_ignore_ascii_case(app_id))
        })
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
