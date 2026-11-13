//! Captive portals (hotel, airport and café Wi-Fi login pages), handled
//! like gnome-shell's CaptivePortalHandler (js/ui/status/network.js).
//!
//! When the primary connection is up but NetworkManager reports
//! `NM_CONNECTIVITY_PORTAL`, a "Sign in to Network" notification appears.
//! Clicking it opens the Hotspot Login window (`umoja-portal-helper`, over
//! `org.umoja.PortalHelper`) on NetworkManager's connectivity-check URL,
//! which the portal redirects to its login page; without the helper, the
//! default browser opens that URL instead. Each page the login moves to
//! makes NetworkManager check again, and once it reports full connectivity
//! the window closes by itself.
//!
//! `UMOJA_CONNECTIVITY_TEST=1` (or gnome-shell's
//! `GNOME_SHELL_CONNECTIVITY_TEST`) treats any connectivity short of full
//! as a portal, for testing behind a phone's hotspot with no data.

use crate::network::{self, CONNECTIVITY_FULL, CONNECTIVITY_PORTAL, Kind, NetState, Network};
use crate::notifications::CloseReason;
use gio::prelude::*;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use umoja_common::dbus::{
    PORTAL_COMPLETED, PORTAL_HELPER_BUS_NAME, PORTAL_HELPER_INTERFACE, PORTAL_HELPER_OBJECT_PATH,
    PORTAL_RECHECK,
};
use umoja_common::i18n;

/// Used when NetworkManager has no check URL of its own (as gnome-shell's
/// portal helper does).
const FALLBACK_URI: &str = "http://nmcheck.gnome.org";
const HELPER_DESKTOP_ID: &str = "org.umoja.PortalHelper.desktop";

/// Where a connection's login stands, until it gets full connectivity or
/// stops being the primary connection.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Login {
    /// "Sign in to Network" is showing (notification id).
    Notified(u32),
    /// The notification was dismissed: don't ask again for now.
    Dismissed,
    /// The Hotspot Login window has it.
    Helper,
    /// Opened in the default browser.
    Browser,
}

struct Portal {
    /// Active connection path -> its login.
    logins: RefCell<HashMap<String, Login>>,
    check_uri: RefCell<String>,
    session: Option<gio::DBusConnection>,
    /// StatusChanged from the helper is subscribed to.
    listening: std::cell::Cell<bool>,
}

thread_local! {
    static PORTAL: RefCell<Option<Rc<Portal>>> = const { RefCell::new(None) };
}

pub fn start() {
    let portal = Rc::new(Portal {
        logins: RefCell::default(),
        check_uri: RefCell::default(),
        session: gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .inspect_err(|e| tracing::warn!(error = %e, "no session bus"))
            .ok(),
        listening: std::cell::Cell::new(false),
    });
    let test = ["UMOJA_CONNECTIVITY_TEST", "GNOME_SHELL_CONNECTIVITY_TEST"]
        .iter()
        .any(|v| std::env::var_os(v).is_some());
    let weak = Rc::downgrade(&portal);
    Network::get().subscribe(move |s| {
        if let Some(p) = weak.upgrade() {
            p.sync(s, test);
        }
    });
    PORTAL.with(|p| p.replace(Some(portal)));
}

/// Whether the state calls for a login: the primary connection is fully up
/// and NetworkManager found a portal.
fn wants_login(s: &NetState, test: bool) -> Option<&network::ActiveConn> {
    let primary = s
        .primary
        .as_ref()
        .filter(|p| p.activated && !p.path.is_empty())?;
    let portal =
        s.connectivity == CONNECTIVITY_PORTAL || (test && s.connectivity < CONNECTIVITY_FULL);
    portal.then_some(primary)
}

impl Portal {
    fn sync(self: &Rc<Self>, s: &NetState, test: bool) {
        self.check_uri.replace(s.check_uri.clone());
        match wants_login(s, test) {
            Some(primary) => {
                // A different connection than the one we were logging into.
                let stale: Vec<String> = self
                    .logins
                    .borrow()
                    .keys()
                    .filter(|p| **p != primary.path)
                    .cloned()
                    .collect();
                for path in stale {
                    self.remove(&path);
                }
                self.add(primary);
            }
            None => self.clear(),
        }
    }

    fn add(self: &Rc<Self>, conn: &network::ActiveConn) {
        if self.logins.borrow().contains_key(&conn.path) {
            return;
        }
        let Some(center) = crate::notification_center() else {
            return;
        };
        let icon = match conn.kind {
            Kind::Wired => "network-wired-symbolic",
            Kind::Mobile => "network-cellular-symbolic",
            _ => "network-wireless-symbolic",
        };
        // The handler is dropped when the notification closes; if it wasn't
        // clicked, the user dismissed it.
        struct Guard {
            portal: Weak<Portal>,
            path: String,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let Some(p) = self.portal.upgrade() else {
                    return;
                };
                if let Ok(mut logins) = p.logins.try_borrow_mut()
                    && let Some(l @ Login::Notified(_)) = logins.get_mut(&self.path)
                {
                    *l = Login::Dismissed;
                }
            }
        }
        let guard = Guard {
            portal: Rc::downgrade(self),
            path: conn.path.clone(),
        };
        // Reserve the entry first: notify may call back synchronously.
        self.logins
            .borrow_mut()
            .insert(conn.path.clone(), Login::Notified(0));
        let nid = center.notify_with_actions(
            &i18n::shell("Sign in to Network"),
            &conn.id,
            icon,
            &[("default", "")],
            move |_| {
                if let Some(p) = guard.portal.upgrade() {
                    let path = guard.path.clone();
                    glib::idle_add_local_once(move || p.open(&path));
                }
            },
        );
        if let Some(l @ Login::Notified(_)) = self.logins.borrow_mut().get_mut(&conn.path) {
            *l = Login::Notified(nid);
        }
    }

    /// The notification was clicked: open the login page.
    fn open(self: &Rc<Self>, path: &str) {
        let uri = Some(self.check_uri.borrow().clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| FALLBACK_URI.to_string());
        let helper = gio::DesktopAppInfo::new(HELPER_DESKTOP_ID);
        let login = match (helper, &self.session) {
            (Some(info), Some(bus)) => {
                self.listen(bus);
                let token = gtk4::gdk::Display::default()
                    .and_then(|d| {
                        use gtk4::prelude::*;
                        d.app_launch_context().startup_notify_id(Some(&info), &[])
                    })
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                let path_v = crate::bus::object_path(path);
                bus.call(
                    Some(PORTAL_HELPER_BUS_NAME),
                    PORTAL_HELPER_OBJECT_PATH,
                    PORTAL_HELPER_INTERFACE,
                    "Authenticate",
                    Some(&(path_v, uri.as_str(), token.as_str()).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    -1,
                    gio::Cancellable::NONE,
                    move |r| {
                        if let Err(e) = r {
                            tracing::warn!(error = %e, "could not open the Hotspot Login window");
                        }
                    },
                );
                Login::Helper
            }
            _ => {
                match gio::AppInfo::default_for_uri_scheme("http") {
                    Some(browser) => crate::launch::launch_info(browser, Some(uri)),
                    None => tracing::warn!("no web browser for the captive portal"),
                }
                Login::Browser
            }
        };
        if let Some(l) = self.logins.borrow_mut().get_mut(path) {
            *l = login;
        }
    }

    fn listen(self: &Rc<Self>, bus: &gio::DBusConnection) {
        if self.listening.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        #[allow(deprecated)]
        bus.signal_subscribe(
            Some(PORTAL_HELPER_BUS_NAME),
            Some(PORTAL_HELPER_INTERFACE),
            Some("StatusChanged"),
            Some(PORTAL_HELPER_OBJECT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                let (Some(p), Some((path, result))) = (
                    weak.upgrade(),
                    params.get::<(glib::variant::ObjectPath, u32)>(),
                ) else {
                    return;
                };
                p.status_changed(path.as_str(), result);
            },
        );
    }

    fn status_changed(self: &Rc<Self>, path: &str, result: u32) {
        match result {
            PORTAL_COMPLETED => self.remove(path),
            PORTAL_RECHECK => {
                let weak = Rc::downgrade(self);
                let path = path.to_string();
                glib::spawn_future_local(async move {
                    let state = Network::get().check_connectivity().await;
                    if state.is_some_and(|s| s >= CONNECTIVITY_FULL)
                        && let Some(p) = weak.upgrade()
                    {
                        p.remove(&path);
                    }
                });
            }
            // Cancelled (the window was closed): keep the entry, so we
            // don't ask again until the connection changes or gets
            // through some other way.
            _ => {}
        }
    }

    /// Done with this connection: close its window or notification.
    fn remove(&self, path: &str) {
        let login = self.logins.borrow_mut().remove(path);
        if let Some(login) = login {
            self.finish(path, login);
        }
    }

    fn clear(&self) {
        if self.logins.borrow().is_empty() {
            return;
        }
        let logins = self.logins.take();
        for (path, login) in logins {
            self.finish(&path, login);
        }
    }

    fn finish(&self, path: &str, login: Login) {
        match login {
            Login::Notified(nid) if nid != 0 => {
                if let Some(c) = crate::notification_center() {
                    c.close(nid, CloseReason::Closed);
                }
            }
            Login::Helper => {
                if let Some(bus) = &self.session {
                    let args = (crate::bus::object_path(path),).to_variant();
                    // Don't start the helper just to close nothing.
                    bus.call(
                        Some(PORTAL_HELPER_BUS_NAME),
                        PORTAL_HELPER_OBJECT_PATH,
                        PORTAL_HELPER_INTERFACE,
                        "Close",
                        Some(&args),
                        None,
                        gio::DBusCallFlags::NO_AUTO_START,
                        -1,
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::ActiveConn;

    fn state(connectivity: u32, activated: bool) -> NetState {
        NetState {
            running: true,
            connectivity,
            primary: Some(ActiveConn {
                kind: Kind::Wifi,
                id: "Hotel Guest".into(),
                activating: !activated,
                path: "/org/freedesktop/NetworkManager/ActiveConnection/3".into(),
                activated,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn portal_on_an_active_connection() {
        let s = state(CONNECTIVITY_PORTAL, true);
        assert_eq!(
            wants_login(&s, false).map(|c| c.id.as_str()),
            Some("Hotel Guest")
        );
    }

    #[test]
    fn nothing_while_connecting_or_online() {
        assert!(wants_login(&state(CONNECTIVITY_PORTAL, false), false).is_none());
        assert!(wants_login(&state(CONNECTIVITY_FULL, true), false).is_none());
        // Limited (no route past the gateway) is not a portal...
        assert!(wants_login(&state(3, true), false).is_none());
        // ...unless testing.
        assert!(wants_login(&state(3, true), true).is_some());
        assert!(wants_login(&state(CONNECTIVITY_FULL, true), true).is_none());
        let mut none = state(CONNECTIVITY_PORTAL, true);
        none.primary = None;
        assert!(wants_login(&none, false).is_none());
    }
}
