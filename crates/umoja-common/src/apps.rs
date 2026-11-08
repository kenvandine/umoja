//! Mapping a Wayland app-id to its `.desktop` entry (name and icon).

use gio::prelude::*;

/// Find the desktop entry for a window's app-id.
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
