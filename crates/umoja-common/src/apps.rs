//! Mapping a Wayland app-id to its `.desktop` entry (name and icon).

use gio::prelude::*;

/// Find the desktop entry for a window's app-id.
///
/// Tries, in order: `<app_id>.desktop`, the lower-cased id, the last
/// reverse-DNS component, and finally any entry whose `StartupWMClass`
/// matches (this is how Ubuntu Dock matches Electron and X11 apps), the
/// app-id itself or, for an Electron `Name.Desktop` id, its `Name`.
///
/// A hidden (`NoDisplay=true`) match gives way to a visible entry for the
/// same app: Chrome's app-id is `com.google.Chrome`, whose desktop file is a
/// hidden alias of the `google-chrome.desktop` people pin, so without this
/// its windows got a second, unpinned dock icon and the pin no dot.
pub fn app_for_window(app_id: &str) -> Option<gio::DesktopAppInfo> {
    let found = lookup(app_id)?;
    if !found.is_nodisplay() {
        return Some(found);
    }
    let mut classes = vec![app_id.to_string()];
    if let Some(c) = found.startup_wm_class() {
        classes.push(c.to_string());
    }
    let exec = executable(&found);
    visible_entries()
        .into_iter()
        .find(|d| {
            d.startup_wm_class()
                .is_some_and(|c| classes.iter().any(|w| c.eq_ignore_ascii_case(w)))
        })
        .or_else(|| {
            // No class to go by: the same program (Exec) shown under another id.
            exec.as_ref().and_then(|exec| {
                visible_entries()
                    .into_iter()
                    .find(|d| executable(d).as_ref() == Some(exec))
            })
        })
        .or(Some(found))
}

/// An app's executable, or `None` when its entry has no `Exec` line.
///
/// Use this instead of `AppInfo::executable()`: gio returns NULL for an
/// entry without `Exec` (QEMU's hidden `qemu.desktop` is one), and the
/// binding's non-optional `PathBuf` turns that NULL into a `strlen(NULL)`
/// segfault, which took down the dock, the overlay and the automations
/// service whenever a QEMU window was open.
pub fn executable(info: &impl IsA<gio::AppInfo>) -> Option<std::path::PathBuf> {
    use glib::translate::{FromGlibPtrNone, ToGlibPtr};
    let p = unsafe { gio::ffi::g_app_info_get_executable(info.as_ref().to_glib_none().0) };
    if p.is_null() {
        return None;
    }
    let path: std::path::PathBuf = unsafe { FromGlibPtrNone::from_glib_none(p) };
    (!path.as_os_str().is_empty()).then_some(path)
}

fn lookup(app_id: &str) -> Option<gio::DesktopAppInfo> {
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
    let entries = all_entries();
    let by_class = |class: &str| {
        entries.iter().find(|d| {
            d.startup_wm_class()
                .is_some_and(|c| c.eq_ignore_ascii_case(class))
        })
    };
    by_class(app_id)
        .or_else(|| electron_class(app_id).and_then(by_class))
        .cloned()
}

/// The X11 class an Electron app keeps in `StartupWMClass` when its Wayland
/// app-id comes from `desktopName` instead: Mattermost's windows say
/// `Mattermost.Desktop` while its snap's desktop file has
/// `StartupWMClass=Mattermost` (upstream's own builds say
/// `com.Mattermost.Desktop`). Only for ids ending in `.Desktop`, so
/// reverse-DNS ids like `org.gnome.Foo` never match a class named `org`.
fn electron_class(app_id: &str) -> Option<&str> {
    let (prefix, last) = app_id.rsplit_once('.')?;
    let name = prefix.rsplit('.').next()?;
    (!name.is_empty() && last.eq_ignore_ascii_case("desktop")).then_some(name)
}

fn all_entries() -> Vec<gio::DesktopAppInfo> {
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|a| a.downcast::<gio::DesktopAppInfo>().ok())
        .collect()
}

fn visible_entries() -> Vec<gio::DesktopAppInfo> {
    all_entries()
        .into_iter()
        .filter(|d| !d.is_nodisplay())
        .collect()
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

/// App Center's desktop id, which Ubuntu Dock uses for snaps' App Details.
const APP_CENTER: &str = "snap-store_snap-store.desktop";
const GNOME_SOFTWARE: &str = "org.gnome.Software.desktop";

/// Where "App Details" opens, decided as Ubuntu Dock does: a snap (its
/// desktop file has `X-SnapInstanceName`) goes to App Center, anything else
/// to GNOME Software. `None` when the matching store isn't installed.
pub enum Details {
    AppCenter {
        store: gio::DesktopAppInfo,
        snap: String,
    },
    Software {
        desktop_id: String,
    },
}

pub fn details_for(info: &gio::DesktopAppInfo) -> Option<Details> {
    match info.string("X-SnapInstanceName").filter(|s| !s.is_empty()) {
        Some(snap) => gio::DesktopAppInfo::new(APP_CENTER).map(|store| Details::AppCenter {
            store,
            snap: snap.to_string(),
        }),
        None => {
            gio::DesktopAppInfo::new(GNOME_SOFTWARE)?;
            Some(Details::Software {
                desktop_id: info.id()?.to_string(),
            })
        }
    }
}

/// Open the app's store page, as Ubuntu Dock's "App Details" does.
pub fn show_details(details: &Details) {
    match details {
        Details::AppCenter { store, snap } => {
            // Ubuntu Dock runs App Center's command line with the snap name
            // appended; drop the desktop-entry field codes (%U...) first.
            let mut argv: Vec<String> = store
                .commandline()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default()
                .split_whitespace()
                .filter(|a| !a.starts_with('%'))
                .map(str::to_string)
                .collect();
            if argv.is_empty() {
                return;
            }
            argv.push(snap.clone());
            let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
            // GSubprocess reaps the child, so the long-running dock leaves no zombie.
            if let Err(e) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
                tracing::warn!(error = %e, snap, "could not open App Center");
            }
        }
        Details::Software { desktop_id } => {
            // org.gtk.Actions.Activate("details", [<(id, "")>], {}), as GNOME does.
            let target = (desktop_id.as_str(), "").to_variant();
            let params = glib::Variant::tuple_from_iter([
                "details".to_variant(),
                glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::VARIANT,
                    [target.to_variant()],
                ),
                glib::VariantDict::new(None).end(),
            ]);
            gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |conn| {
                let Ok(conn) = conn else { return };
                conn.call(
                    Some("org.gnome.Software"),
                    "/org/gnome/Software",
                    "org.gtk.Actions",
                    "Activate",
                    Some(&params),
                    None,
                    gio::DBusCallFlags::NONE,
                    -1,
                    gio::Cancellable::NONE,
                    |r| {
                        if let Err(e) = r {
                            tracing::warn!(error = %e, "could not open GNOME Software");
                        }
                    },
                );
            });
        }
    }
}

/// GNOME's `shell_app_can_open_new_window()` for a desktop entry: not when
/// the app declares a single main window.
pub fn can_open_new_window(info: &gio::DesktopAppInfo) -> bool {
    !(info.boolean("SingleMainWindow") || info.boolean("X-GNOME-SingleWindow"))
}

#[cfg(test)]
mod tests {
    use super::{electron_class, with_favorite};

    fn entry(body: &str) -> gio::DesktopAppInfo {
        let kf = glib::KeyFile::new();
        kf.load_from_data(body, glib::KeyFileFlags::NONE).unwrap();
        gio::DesktopAppInfo::from_keyfile(&kf).unwrap()
    }

    #[test]
    fn executable_of_an_entry_without_exec_is_none() {
        // QEMU's qemu.desktop: hidden and no Exec line.
        let qemu = entry(
            "[Desktop Entry]\nType=Application\nName=QEMU\nIcon=qemu\nNoDisplay=true\n",
        );
        assert_eq!(super::executable(&qemu), None);
    }

    #[test]
    fn executable_of_an_entry_with_exec() {
        let app = entry("[Desktop Entry]\nType=Application\nName=Sh\nExec=sh -c true\n");
        assert_eq!(super::executable(&app), Some("sh".into()));
    }

    #[test]
    fn electron_class_from_desktop_name_ids() {
        assert_eq!(electron_class("Mattermost.Desktop"), Some("Mattermost"));
        assert_eq!(electron_class("com.Mattermost.Desktop"), Some("Mattermost"));
        assert_eq!(electron_class("org.gnome.Nautilus"), None);
        assert_eq!(electron_class("firefox_firefox"), None);
        assert_eq!(electron_class(".desktop"), None);
    }

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
