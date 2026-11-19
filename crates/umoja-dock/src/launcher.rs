//! Mapping between Wayland app-ids and `.desktop` files, and launching apps.

use crate::tracker::WindowInfo;
use gio::prelude::*;
use gtk::gdk;
use gtk::glib;
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

/// The dock's key for a window with no desktop file: its app id, or the
/// window itself when it has none.
pub fn window_key(w: &WindowInfo) -> String {
    if w.app_id.is_empty() {
        format!("window#{}", w.id)
    } else {
        format!("window:{}", w.app_id)
    }
}

/// A stand-in app for a window no desktop file claims (a script's window,
/// or an AppImage whose own entry can't be found), like gnome-shell's
/// window-backed apps: named after the window, with the icon its app id
/// names in the icon theme or the generic program icon. It has no id, so
/// it can't be pinned, and no Exec, so it only ever shows its windows.
pub fn window_backed(w: &WindowInfo) -> Option<gio::DesktopAppInfo> {
    if let Some(info) = umoja_common::appimage::entry_for_window(&w.app_id) {
        return Some(info);
    }
    let name = if w.title.is_empty() {
        &w.app_id
    } else {
        &w.title
    };
    let name = if name.is_empty() {
        "Unknown"
    } else {
        name.as_str()
    };
    let icon = gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d))
        .and_then(|theme| {
            [w.app_id.clone(), w.app_id.to_lowercase()]
                .into_iter()
                .find(|n| !n.is_empty() && theme.has_icon(n))
        })
        .unwrap_or_else(|| "application-x-executable".to_string());
    let kf = glib::KeyFile::new();
    let group = "Desktop Entry";
    kf.set_string(group, "Type", "Application");
    kf.set_string(group, "Name", name);
    kf.set_string(group, "Icon", &icon);
    // Nothing to launch, so no "New Window" item.
    kf.set_boolean(group, "SingleMainWindow", true);
    gio::DesktopAppInfo::from_keyfile(&kf)
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

/// A new window: the app's own "new-window" action when it has one (as
/// gnome-shell's app.open_new_window), else another launch.
pub fn new_window(info: &gio::DesktopAppInfo) {
    if info.list_actions().iter().any(|a| a == "new-window") {
        launch_action(info, "new-window");
    } else {
        launch(info);
    }
}

/// Run one of the app's desktop actions ("New Private Window"...).
pub fn launch_action(info: &gio::DesktopAppInfo, action: &str) {
    info.launch_action(action, Some(&context()));
}

/// `org.umoja.shell favorite-apps`: the apps pinned to the dock, in order.
pub fn favorites() -> Vec<gio::DesktopAppInfo> {
    umoja_common::apps::favorites()
        .iter()
        .filter_map(|id| cached(&format!("id:{id}"), || gio::DesktopAppInfo::new(id)))
        .collect()
}
