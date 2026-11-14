//! Keep the shell's own GTK processes off xdg-desktop-portal.
//!
//! GTK 4 on Wayland reads its theme settings (icon theme, fonts, colour
//! scheme) through the Settings portal when one is on the bus, and asks the
//! portal for its version with a blocking 25 s call. Right after a log out
//! and back in, the user manager's portal frontend can still be waiting on
//! a backend D-Bus activation from the dying session (xdg-desktop-portal-gtk
//! exits with "cannot open display" and dbus-daemon keeps the activation
//! pending for 120 s). On an 8 GB XPS 13 the panel, dock and Ask Umoja each
//! sat through several of those timeouts, so the desktop took about two
//! minutes to appear, and the dock and overlay fell back to GTK's default
//! icon theme instead of Yaru until they were restarted.
//!
//! The shell isn't sandboxed, so it reads the same settings from GSettings
//! directly: `GDK_DEBUG=no-portals` for GTK and `ADW_DISABLE_PORTAL=1` for
//! libadwaita. Both are read when the application starts up, so
//! [`restore`] takes them out of the environment again afterwards and the
//! apps the dock and Ask Umoja launch still use portals normally.

use std::cell::RefCell;
use std::ffi::OsString;

thread_local! {
    static SAVED: RefCell<Option<(Option<OsString>, Option<OsString>)>> = const { RefCell::new(None) };
}

/// Call first thing in `main`, before GTK opens the display.
pub fn bypass() {
    let gdk = std::env::var_os("GDK_DEBUG");
    let adw = std::env::var_os("ADW_DISABLE_PORTAL");
    let flags = match gdk.as_ref().and_then(|v| v.to_str()) {
        Some(v) if !v.is_empty() => format!("{v},no-portals"),
        _ => "no-portals".to_string(),
    };
    // SAFETY: called at the start of main, before any other thread runs.
    unsafe {
        std::env::set_var("GDK_DEBUG", flags);
        std::env::set_var("ADW_DISABLE_PORTAL", "1");
    }
    SAVED.with(|s| s.replace(Some((gdk, adw))));
}

/// Call from the application's `startup` handler (after GTK and libadwaita
/// have initialised), so launched apps don't inherit the bypass.
pub fn restore() {
    let Some((gdk, adw)) = SAVED.with(|s| s.take()) else { return };
    // SAFETY: on the main thread; GTK's helper threads don't read these.
    unsafe {
        match gdk {
            Some(v) => std::env::set_var("GDK_DEBUG", v),
            None => std::env::remove_var("GDK_DEBUG"),
        }
        match adw {
            Some(v) => std::env::set_var("ADW_DISABLE_PORTAL", v),
            None => std::env::remove_var("ADW_DISABLE_PORTAL"),
        }
    }
}
