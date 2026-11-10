//! Mapping between Wayland app-ids and `.desktop` files, and launching apps.

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::collections::HashMap;

/// Find the desktop entry for a running window's app-id.
pub fn app_for_window(app_id: &str) -> Option<gio::DesktopAppInfo> {
    cached(&format!("window:{app_id}"), || umoja_common::apps::app_for_window(app_id))
}

thread_local! {
    /// Desktop-file lookups by window app id and by favorite id. A window
    /// with no direct match (Chrome's alias, Electron apps, games) made
    /// `app_for_window` read every .desktop file on the system, and the
    /// dock looked each window up on every rebuild.
    static CACHE: RefCell<HashMap<String, Option<gio::DesktopAppInfo>>> = {
        // Installing or removing apps starts over.
        let monitor = gio::AppInfoMonitor::get();
        monitor.connect_changed(|_| CACHE.with(|c| c.borrow_mut().clear()));
        std::mem::forget(monitor);
        RefCell::default()
    };
}

fn cached(
    key: &str,
    lookup: impl FnOnce() -> Option<gio::DesktopAppInfo>,
) -> Option<gio::DesktopAppInfo> {
    if let Some(hit) = CACHE.with(|c| c.borrow().get(key).cloned()) {
        return hit;
    }
    let found = lookup();
    CACHE.with(|c| c.borrow_mut().insert(key.to_string(), found.clone()));
    found
}

/// Desktop id without the `.desktop` suffix, used as the dock's key.
pub fn key_for(info: &gio::DesktopAppInfo) -> String {
    info.id()
        .map(|s| s.trim_end_matches(".desktop").to_string())
        .unwrap_or_default()
}

/// A launch context that puts the app in its own `app-*.scope`, so it isn't
/// charged to (or killed with) umoja-dock.service.
fn context() -> gio::AppLaunchContext {
    match gdk::Display::default() {
        Some(d) => umoja_common::app_scope::scoped(d.app_launch_context()).upcast(),
        None => umoja_common::app_scope::launch_context(),
    }
}

pub fn launch(info: &gio::DesktopAppInfo) {
    let ctx = context();
    if let Err(e) = info.launch(&[], Some(&ctx)) {
        tracing::warn!(app = ?info.id(), error = %e, "launch failed");
    }
}

/// Run one of the app's desktop actions ("New Private Window"...).
pub fn launch_action(info: &gio::DesktopAppInfo, action: &str) {
    info.launch_action(action, Some(&context()));
}

/// `org.gnome.shell favorite-apps`, the same pinned list Ubuntu Dock uses.
pub fn favorites() -> Vec<gio::DesktopAppInfo> {
    umoja_common::apps::favorites()
        .iter()
        .filter_map(|id| cached(&format!("id:{id}"), || gio::DesktopAppInfo::new(id)))
        .collect()
}
