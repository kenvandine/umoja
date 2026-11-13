//! logind helpers. The panel runs as a systemd user service, outside the
//! login session's scope and without XDG_SESSION_ID, so "my session" has to
//! be looked up: it is the user's graphical (Display) session.

use crate::bus;
use gio::prelude::*;
use gtk4::glib;

pub const LOGIN1: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const USER: &str = "org.freedesktop.login1.User";
pub const SESSION: &str = "org.freedesktop.login1.Session";

/// Object path of the session this panel belongs to.
pub fn session_path(conn: &gio::DBusConnection) -> Option<String> {
    // Prefer the session we were started from, if the environment has it.
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && let Ok(reply) = conn.call_sync(
            Some(LOGIN1),
            LOGIN1_PATH,
            MANAGER,
            "GetSession",
            Some(&(id.as_str(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )
        && let Some(p) = reply.child_value(0).str()
    {
        return Some(p.to_string());
    }
    let user = format!("{LOGIN1_PATH}/user/_{}", unsafe { libc::getuid() });
    let display = bus::get_all(conn, LOGIN1, &user, USER)?;
    // Display is (so): session id and object path.
    let path = display
        .get("Display")?
        .try_child_value(1)?
        .str()?
        .to_string();
    (path != "/").then_some(path)
}

/// logind's id for that session ("2", "c1"...), as polkit names it.
pub fn session_id(conn: &gio::DBusConnection) -> Option<String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        return Some(id);
    }
    let path = session_path(conn)?;
    bus::get_all(conn, LOGIN1, &path, SESSION)?.str("Id")
}

pub fn locked_hint(conn: &gio::DBusConnection, session: &str) -> bool {
    bus::get_all(conn, LOGIN1, session, SESSION)
        .and_then(|p| p.bool("LockedHint"))
        .unwrap_or(false)
}

pub fn set_locked_hint(locked: bool) {
    let Some(conn) = bus::system_bus() else {
        return;
    };
    if let Some(s) = session_path(&conn) {
        bus::call(
            &conn,
            LOGIN1,
            &s,
            SESSION,
            "SetLockedHint",
            Some((locked,).to_variant()),
        );
    }
}

/// Log Out: end the graphical session, as gnome-session does.
pub fn terminate_session() {
    let Some(conn) = bus::system_bus() else {
        return;
    };
    match session_path(&conn) {
        Some(s) => {
            tracing::info!(session = s, "logging out");
            bus::call(&conn, LOGIN1, &s, SESSION, "Terminate", None);
        }
        None => {
            tracing::warn!("no login session found; stopping the Umoja session instead");
            // systemd's user manager, over D-Bus (what `systemctl --user
            // stop` calls).
            if let Ok(user) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
                bus::call(
                    &user,
                    "org.freedesktop.systemd1",
                    "/org/freedesktop/systemd1",
                    "org.freedesktop.systemd1.Manager",
                    "StopUnit",
                    Some(("umoja-session.target", "replace").to_variant()),
                );
            }
        }
    }
}

/// Calls `f(signal_name, params)` for the Lock/Unlock signals on our session
/// and PrepareForSleep on the manager.
pub fn watch(f: impl Fn(&str, &glib::Variant) + 'static) {
    let Some(conn) = bus::system_bus() else {
        return;
    };
    let f = std::rc::Rc::new(f);
    if let Some(session) = session_path(&conn) {
        let f = f.clone();
        #[allow(deprecated)]
        conn.signal_subscribe(
            Some(LOGIN1),
            Some(SESSION),
            None,
            Some(&session),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, signal, params| f(signal, params),
        );
    }
    #[allow(deprecated)]
    conn.signal_subscribe(
        Some(LOGIN1),
        Some(MANAGER),
        Some("PrepareForSleep"),
        Some(LOGIN1_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, signal, params| f(signal, params),
    );
}

/// org.freedesktop.login1.Manager.PowerOff / Reboot, interactive so polkit
/// can ask (the panel's own agent answers) when other users are logged in
/// or something holds a shutdown inhibitor.
pub fn manager_call(method: &str) {
    let Some(conn) = bus::system_bus() else {
        tracing::warn!(method, "no system bus; cannot reach logind");
        return;
    };
    bus::call(
        &conn,
        LOGIN1,
        LOGIN1_PATH,
        MANAGER,
        method,
        Some((true,).to_variant()),
    );
}
