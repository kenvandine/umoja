//! Which monitor a shell surface goes on, as GNOME decides it.
//!
//! GTK has no primary monitor on Wayland. GNOME's is the one mutter marks
//! primary in `org.gnome.Mutter.DisplayConfig`; umoja-mir serves the same
//! API (the choice in Settings › Displays, else `~/.config/monitors.xml`,
//! else the built-in panel), so the panel, notifications and dock ask it,
//! and follow `MonitorsChanged` and GTK's monitor list for hotplug.
//!
//! Everything here is main-thread state; call [`start`] once GTK is up.

use gtk4::gdk;
use gtk4::gdk::prelude::*;
use gtk4::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

const NAME: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
/// Hotplug sends GTK's monitor list and the D-Bus signal at slightly
/// different times; act once both have settled.
const SETTLE_MS: u64 = 200;

#[derive(Default)]
struct State {
    started: bool,
    /// Connector of the primary monitor, as DisplayConfig names it.
    primary: Option<String>,
    /// What listeners last saw: the primary and every connector, in order.
    seen: Option<(Option<String>, Vec<String>)>,
    listeners: Vec<Rc<dyn Fn()>>,
    pending: Option<glib::SourceId>,
    query_serial: u64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Mutter calls HDMI-A connectors "HDMI-n"; Mir and the kernel "HDMI-A-n".
fn normalize(connector: &str) -> String {
    match connector.strip_prefix("HDMI-A-") {
        Some(n) => format!("HDMI-{n}"),
        None => connector.to_string(),
    }
}

/// Do two connector names mean the same port?
pub fn same_connector(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

/// GTK's monitors, in the compositor's order.
pub fn all() -> Vec<gdk::Monitor> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    display
        .monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .collect()
}

/// The monitor on this connector, if it is connected.
pub fn by_connector(connector: &str) -> Option<gdk::Monitor> {
    all().into_iter().find(|m| {
        m.connector()
            .is_some_and(|c| same_connector(c.as_str(), connector))
    })
}

/// The primary monitor's connector, if DisplayConfig named one.
pub fn primary_connector() -> Option<String> {
    STATE.with(|s| s.borrow().primary.clone())
}

/// The primary monitor: DisplayConfig's choice, else GTK's first monitor.
pub fn primary() -> Option<gdk::Monitor> {
    primary_connector()
        .and_then(|c| by_connector(&c))
        .or_else(|| all().into_iter().next())
}

/// Is `monitor` the primary one?
pub fn is_primary(monitor: &gdk::Monitor) -> bool {
    primary().as_ref() == Some(monitor)
}

/// Call `f` whenever the primary monitor or the set of monitors changes
/// (never while GTK has no monitors at all, as while outputs are being
/// reconfigured). Starts watching if [`start`] was not called yet.
pub fn connect_changed(f: impl Fn() + 'static) {
    start();
    STATE.with(|s| s.borrow_mut().listeners.push(Rc::new(f)));
}

/// The primary connector in a `GetCurrentState` reply.
pub fn primary_in_state(reply: &glib::Variant) -> Option<String> {
    // (u a(...) a(iiduba(ssss)a{sv}) a{sv}): logical monitors are child 2.
    if reply.n_children() < 3 {
        return None;
    }
    let logical = reply.child_value(2);
    for i in 0..logical.n_children() {
        let lm = logical.child_value(i);
        if lm.n_children() < 6 || lm.child_value(4).get::<bool>() != Some(true) {
            continue;
        }
        let specs = lm.child_value(5);
        if specs.n_children() > 0 {
            let spec = specs.child_value(0);
            if let Some(c) = spec.child_value(0).str() {
                return Some(c.to_string());
            }
        }
    }
    None
}

fn query_sync(conn: &gio::DBusConnection) -> Option<String> {
    let reply = conn
        .call_sync(
            Some(NAME),
            PATH,
            NAME,
            "GetCurrentState",
            None,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            1000,
            gio::Cancellable::NONE,
        )
        .ok()?;
    primary_in_state(&reply)
}

fn query(conn: &gio::DBusConnection) {
    let serial = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.query_serial += 1;
        s.query_serial
    });
    let conn = conn.clone();
    glib::spawn_future_local(async move {
        let reply = conn
            .call_future(
                Some(NAME),
                PATH,
                NAME,
                "GetCurrentState",
                None,
                None,
                gio::DBusCallFlags::NO_AUTO_START,
                2000,
            )
            .await;
        let primary = match reply {
            Ok(r) => primary_in_state(&r),
            Err(e) => {
                tracing::debug!(error = %e, "DisplayConfig unavailable");
                None
            }
        };
        let current = STATE.with(|s| {
            let mut s = s.borrow_mut();
            if s.query_serial == serial {
                s.primary = primary;
            }
            s.query_serial == serial
        });
        if current {
            schedule();
        }
    });
}

/// Tell listeners, once things settle, if anything they care about changed.
fn schedule() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(id) = s.pending.take() {
            id.remove();
        }
        s.pending = Some(glib::timeout_add_local_once(
            std::time::Duration::from_millis(SETTLE_MS),
            || {
                STATE.with(|s| s.borrow_mut().pending = None);
                notify();
            },
        ));
    });
}

fn notify() {
    let monitors = all();
    if monitors.is_empty() {
        return;
    }
    let connectors: Vec<String> = monitors
        .iter()
        .map(|m| m.connector().map(|c| c.to_string()).unwrap_or_default())
        .collect();
    let primary = primary().and_then(|m| m.connector()).map(|c| c.to_string());
    let listeners = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let now = Some((primary.clone(), connectors.clone()));
        if s.seen == now {
            return Vec::new();
        }
        s.seen = now;
        s.listeners.clone()
    });
    if listeners.is_empty() {
        return;
    }
    tracing::info!(primary = primary.as_deref().unwrap_or("?"), monitors = ?connectors, "monitors changed");
    for f in listeners {
        f();
    }
}

/// Learn the primary monitor now (a short blocking D-Bus call, so the first
/// windows open in the right place) and follow changes from then on.
pub fn start() {
    if STATE.with(|s| std::mem::replace(&mut s.borrow_mut().started, true)) {
        return;
    }
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    let primary = query_sync(&conn);
    tracing::info!(
        primary = primary.as_deref().unwrap_or("?"),
        "primary monitor"
    );
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.primary = primary;
    });
    // What listeners start from, so the first change they hear is real.
    let connectors: Vec<String> = all()
        .iter()
        .map(|m| m.connector().map(|c| c.to_string()).unwrap_or_default())
        .collect();
    if !connectors.is_empty() {
        let primary = primary_by_now();
        STATE.with(|s| s.borrow_mut().seen = Some((primary, connectors)));
    }

    {
        let c = conn.clone();
        // Never unsubscribed: the shell watches for as long as it runs.
        #[allow(deprecated)]
        let _ = conn.signal_subscribe(
            Some(NAME),
            Some(NAME),
            Some("MonitorsChanged"),
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| query(&c),
        );
    }
    // umoja-mir restarting in place (or starting after us) owns the name anew.
    {
        let c = conn.clone();
        let _ = gio::bus_watch_name_on_connection(
            &conn,
            NAME,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| query(&c),
            |_, _| {},
        );
    }
    if let Some(display) = gdk::Display::default() {
        display
            .monitors()
            .connect_items_changed(|_, _, _, _| schedule());
    }
    std::mem::forget(conn);
}

fn primary_by_now() -> Option<String> {
    primary().and_then(|m| m.connector()).map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdmi_names_match_across_mutter_and_mir() {
        assert!(same_connector("HDMI-A-1", "HDMI-1"));
        assert!(same_connector("HDMI-1", "HDMI-A-1"));
        assert!(same_connector("eDP-1", "eDP-1"));
        assert!(!same_connector("DP-1", "DP-2"));
    }

    #[test]
    fn finds_the_primary_in_get_current_state() {
        let props = glib::VariantDict::new(None).end();
        let spec = |c: &str| (c.to_string(), "unknown", "x", "unknown");
        let lm = |x: i32, primary: bool, c: &str| {
            glib::Variant::tuple_from_iter([
                x.to_variant(),
                0i32.to_variant(),
                1.0f64.to_variant(),
                0u32.to_variant(),
                primary.to_variant(),
                vec![spec(c)].to_variant(),
                props.clone(),
            ])
        };
        let logical = glib::Variant::array_from_iter_with_type(
            lm(0, false, "x").type_(),
            [lm(0, false, "eDP-1"), lm(1920, true, "HDMI-1")],
        );
        let monitors = glib::Variant::array_from_iter_with_type(
            glib::VariantTy::new("(i)").unwrap(),
            Vec::<glib::Variant>::new(),
        );
        let reply =
            glib::Variant::tuple_from_iter([1u32.to_variant(), monitors, logical, props.clone()]);
        assert_eq!(primary_in_state(&reply).as_deref(), Some("HDMI-1"));
    }
}
