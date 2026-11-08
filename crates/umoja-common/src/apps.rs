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

/// `org.gnome.shell favorite-apps`: the pinned apps Ubuntu Dock shows.
pub fn favorites() -> Vec<String> {
    crate::gsettings::get_strv(crate::gsettings::SHELL, "favorite-apps")
}

pub fn is_favorite(desktop_id: &str) -> bool {
    favorites().iter().any(|f| f == desktop_id)
}

/// Pin (append, as GNOME's "Pin to Dash" does) or unpin a desktop id.
pub fn set_favorite(desktop_id: &str, pinned: bool) {
    let Some(s) = crate::gsettings::settings(crate::gsettings::SHELL) else {
        return;
    };
    let favs = with_favorite(favorites(), desktop_id, pinned);
    let refs: Vec<&str> = favs.iter().map(String::as_str).collect();
    if let Err(e) = s.set_strv("favorite-apps", refs) {
        tracing::warn!(error = %e, desktop_id, "could not update favorite-apps");
    }
}

fn with_favorite(mut favs: Vec<String>, desktop_id: &str, pinned: bool) -> Vec<String> {
    if pinned {
        if !favs.iter().any(|f| f == desktop_id) {
            favs.push(desktop_id.to_string());
        }
    } else {
        favs.retain(|f| f != desktop_id);
    }
    favs
}

/// Whether "App Details" can be offered (GNOME shows it only when GNOME
/// Software is installed).
pub fn can_show_details() -> bool {
    glib::find_program_in_path("gnome-software").is_some()
}

/// Open the app's page in GNOME Software, as GNOME's "App Details" does.
pub fn show_details(desktop_id: &str) {
    // GSubprocess reaps the child, so the long-running dock leaves no zombie.
    let arg = format!("--details={desktop_id}");
    let argv = [
        std::ffi::OsStr::new("gnome-software"),
        std::ffi::OsStr::new(&arg),
    ];
    if let Err(e) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
        tracing::warn!(error = %e, desktop_id, "could not open GNOME Software");
    }
}

#[cfg(test)]
mod tests {
    use super::with_favorite;

    #[test]
    fn pin_appends_once_and_unpin_removes() {
        let favs = vec!["a.desktop".to_string()];
        let pinned = with_favorite(favs, "b.desktop", true);
        assert_eq!(pinned, ["a.desktop", "b.desktop"]);
        let again = with_favorite(pinned, "b.desktop", true);
        assert_eq!(again, ["a.desktop", "b.desktop"]);
        assert_eq!(with_favorite(again, "a.desktop", false), ["b.desktop"]);
    }
}
