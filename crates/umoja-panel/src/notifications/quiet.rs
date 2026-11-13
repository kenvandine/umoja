//! Automatic Do Not Disturb: no banners while the screen is being recorded
//! or a fullscreen window (a game, a video, a presentation) has the focus.
//!
//! Notifications still reach the message list; only their banners are
//! held back, and critical ones still show. When the quiet time ends, one
//! banner says how many arrived, and clicking it opens the message list,
//! where the digest sums them up. Turned off with `org.umoja.shell
//! auto-do-not-disturb`.

use super::Center;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use std::rc::{Rc, Weak};
use umoja_common::dbus;
use umoja_common::gsettings as gs;

/// Why banners are held back (bits of `Center::quiet`).
pub const RECORDING: u8 = 1;
pub const FULLSCREEN: u8 = 2;

/// `GetWindowsMru` flags (umoja-mir): focused, fullscreen.
const WIN_FOCUSED: u32 = 1;
const WIN_FULLSCREEN: u32 = 16;

pub fn enabled() -> bool {
    gs::settings(gs::UMOJA_SHELL)
        .filter(|s| gs::has_key(s, "auto-do-not-disturb"))
        .is_none_or(|s| s.boolean("auto-do-not-disturb"))
}

/// The banner shown once the quiet time is over: (title, body).
pub fn summary(count: usize, reasons: u8) -> (String, String) {
    let title = if count == 1 {
        "1 new notification".to_string()
    } else {
        format!("{count} new notifications")
    };
    let why = if reasons & RECORDING != 0 {
        "while you were recording the screen"
    } else {
        "while a window was in full screen"
    };
    (title, format!("They arrived {why}."))
}

/// Whether the focused window is fullscreen, from `GetWindowsMru` rows.
pub fn focused_fullscreen(flags: &[u32]) -> bool {
    flags
        .iter()
        .find(|f| *f & WIN_FOCUSED != 0)
        .is_some_and(|f| f & WIN_FULLSCREEN != 0)
}

/// Watch recording and fullscreen and tell `center`.
pub fn start(center: &Rc<Center>) {
    let weak = Rc::downgrade(center);
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        watch_recording(&conn, weak.clone()).await;
        watch_fullscreen(&conn, weak);
        std::mem::forget(conn);
    });
}

fn set(center: &Weak<Center>, reason: u8, on: bool) {
    if let Some(c) = center.upgrade() {
        c.set_quiet(reason, on && enabled());
    }
}

async fn watch_recording(conn: &gio::DBusConnection, center: Weak<Center>) {
    let c = center.clone();
    let id = conn.signal_subscribe(
        Some(dbus::OVERLAY_BUS_NAME),
        Some(dbus::OVERLAY_INTERFACE),
        Some("ScreencastChanged"),
        Some(dbus::OVERLAY_OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            if let Some((active,)) = params.get::<(bool,)>() {
                set(&c, RECORDING, active);
            }
        },
    );
    std::mem::forget(id);
    let reply = conn
        .call_future(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            "IsScreencasting",
            None,
            Some(glib::VariantTy::new("(b)").unwrap()),
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await;
    if let Some((true,)) = reply.ok().and_then(|v| v.get::<(bool,)>()) {
        set(&center, RECORDING, true);
    }
}

/// Ask the compositor right now whether the focused window is fullscreen.
/// No answer (no compositor, an error) counts as not fullscreen.
pub fn fullscreen_now(done: impl FnOnce(bool) + 'static) {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return done(false);
        };
        let reply = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                "GetWindowsMru",
                None,
                Some(glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
                gio::DBusCallFlags::NONE,
                500,
            )
            .await;
        let flags: Vec<u32> = reply
            .ok()
            .and_then(|v| v.get::<(Vec<super::WindowRow>,)>())
            .map(|(rows,)| rows.iter().map(|r| r.7).collect())
            .unwrap_or_default();
        done(focused_fullscreen(&flags));
    });
}

fn watch_fullscreen(conn: &gio::DBusConnection, center: Weak<Center>) {
    let check = {
        let conn = conn.clone();
        move || {
            let center = center.clone();
            conn.call(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                "GetWindowsMru",
                None,
                Some(glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
                move |res| {
                    let Some((rows,)) = res.ok().and_then(|v| v.get::<(Vec<super::WindowRow>,)>())
                    else {
                        return;
                    };
                    let flags: Vec<u32> = rows.iter().map(|r| r.7).collect();
                    set(&center, FULLSCREEN, focused_fullscreen(&flags));
                },
            );
        }
    };
    let on_signal = check.clone();
    // WindowsChanged is already coalesced (at most every 250 ms).
    let id = conn.signal_subscribe(
        Some(dbus::COMPOSITOR_BUS_NAME),
        Some(dbus::COMPOSITOR_INTERFACE),
        Some("WindowsChanged"),
        Some(dbus::COMPOSITOR_OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, _| on_signal(),
    );
    std::mem::forget(id);
    check();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_only_counts_when_focused() {
        assert!(focused_fullscreen(&[WIN_FOCUSED | WIN_FULLSCREEN, 0]));
        assert!(!focused_fullscreen(&[WIN_FOCUSED, WIN_FULLSCREEN]));
        assert!(!focused_fullscreen(&[]));
    }

    #[test]
    fn summary_text() {
        assert_eq!(summary(1, RECORDING).0, "1 new notification");
        assert!(summary(3, FULLSCREEN | RECORDING).1.contains("recording"));
        assert!(summary(2, FULLSCREEN).1.contains("full screen"));
    }
}
