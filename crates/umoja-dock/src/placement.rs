//! Which monitors get a dock, as Ubuntu Dock decides it.
//!
//! `org.gnome.shell.extensions.dash-to-dock`:
//!
//! * `multi-monitor`: a dock on every monitor (Umoja defaults it on).
//! * `preferred-monitor-by-connector`: the monitor for the one dock, by
//!   connector ("eDP-1", "HDMI-1"), or "primary". Settings › Ubuntu Desktop
//!   › Dock › "Show on" writes it. A connector that isn't plugged in falls
//!   back to the primary monitor.
//! * `preferred-monitor`: the older index form (-2 primary, else an index).
//!
//! * `isolate-monitors`: each dock lists only the windows on its monitor.
//!   Foreign-toplevel handles say nothing about outputs under Mir, so
//!   window positions come from `org.umoja.Compositor.GetWindows`, matched
//!   to the dock's windows by app id and title (see [`Homes`]).
//!
//! The docks follow hotplug, the primary monitor changing (Settings ›
//! Displays) and these keys, live.

use crate::{Dock, Shared, launcher_entry, new_dock, tracker};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tracker::Event;
use tracker::{WindowId, WindowInfo};
use umoja_common::gsettings as gs;
use umoja_common::{dbus, monitors, screen};

pub struct Docks {
    app: adw::Application,
    shared: Shared,
    docks: RefCell<Vec<(gdk::Monitor, Rc<Dock>)>>,
    _settings: Vec<gio::Settings>,
}

thread_local! {
    static DOCKS: RefCell<Option<Rc<Docks>>> = const { RefCell::new(None) };
}

impl Docks {
    pub fn start(app: &adw::Application) {
        // Docks come and go with monitors; the app outlives them all.
        std::mem::forget(app.hold());

        let tracker = tracker::start();
        let entries = launcher_entry::start(|key| {
            for d in all() {
                d.update_badge(key);
            }
        });
        let mut settings = Vec::new();
        for schema in [gs::DASH_TO_DOCK, gs::SHELL] {
            if let Some(s) = gs::settings(schema) {
                settings.push(s);
            }
        }
        let docks = Rc::new(Docks {
            app: app.clone(),
            shared: Shared {
                windows: Rc::default(),
                homes: Rc::default(),
                commands: tracker.as_ref().map(|t| t.commands.clone()),
                entries,
            },
            docks: RefCell::default(),
            _settings: settings.clone(),
        });
        DOCKS.with(|d| *d.borrow_mut() = Some(docks.clone()));

        for s in &settings {
            s.connect_changed(None, |_, key| {
                for d in all() {
                    *d.config.borrow_mut() = crate::Config::load();
                    d.apply_layout();
                    d.rebuild();
                    d.refresh_autohide();
                }
                if matches!(
                    key,
                    "multi-monitor" | "preferred-monitor" | "preferred-monitor-by-connector"
                ) {
                    sync();
                }
            });
        }

        if let Some(t) = tracker {
            let windows = docks.shared.windows.clone();
            glib::spawn_future_local(async move {
                while let Ok(ev) = t.events.recv().await {
                    // Only what the buttons show needs a rebuild: which apps
                    // have windows, and which is focused. A title change (a
                    // terminal running a command, a browser tab) doesn't.
                    let rebuild = match ev {
                        Event::Changed(info) => {
                            let old = windows.borrow_mut().insert(info.id, info.clone());
                            old.is_none_or(|o| {
                                o.app_id != info.app_id
                                    || o.activated != info.activated
                                    || o.minimized != info.minimized
                            })
                        }
                        Event::Closed(id) => windows.borrow_mut().remove(&id).is_some(),
                    };
                    if rebuild {
                        queue_rebuild();
                    }
                    // A new window lands on its monitor's dock promptly.
                    refresh_homes();
                }
            });
        }

        // Ctrl+Alt+Tab focuses the dock on the primary monitor.
        crate::keynav::serve(|| {
            let docks = DOCKS.with(|d| d.borrow().clone())?;
            let docks = docks.docks.borrow();
            docks
                .iter()
                .find(|(m, _)| monitors::is_primary(m))
                .or_else(|| docks.first())
                .map(|(_, d)| d.clone())
        });

        monitors::connect_changed(sync);
        docks.sync_now();
        // Windows dragged to another monitor move to its dock.
        on_windows_changed(refresh_homes);
    }

    /// Add and remove docks to match the monitors and settings.
    fn sync_now(&self) {
        let all = monitors::all();
        if all.is_empty() {
            return;
        }
        let wanted: Vec<gdk::Monitor> = wanted_indexes(&all)
            .into_iter()
            .map(|i| all[i].clone())
            .collect();
        let gone: Vec<Rc<Dock>> = {
            let mut docks = self.docks.borrow_mut();
            let (keep, gone): (Vec<_>, Vec<_>) =
                docks.drain(..).partition(|(m, _)| wanted.contains(m));
            *docks = keep;
            gone.into_iter().map(|(_, d)| d).collect()
        };
        for d in gone {
            tracing::info!("removing a dock");
            d.window.destroy();
        }
        for m in wanted {
            if self.docks.borrow().iter().any(|(have, _)| *have == m) {
                continue;
            }
            tracing::info!(
                monitor = m.connector().as_deref().unwrap_or("?"),
                "adding a dock"
            );
            let dock = new_dock(&self.app, &self.shared, &m);
            self.docks.borrow_mut().push((m, dock));
        }
    }
}

/// Rebuild every dock once the current burst of events is handled (a focus
/// change alone sends two: one window loses it, another gains it).
fn queue_rebuild() {
    thread_local!(static QUEUED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if QUEUED.with(|q| q.replace(true)) {
        return;
    }
    glib::idle_add_local_once(|| {
        QUEUED.with(|q| q.set(false));
        for d in all() {
            d.rebuild();
        }
    });
}

fn all() -> Vec<Rc<Dock>> {
    DOCKS.with(|d| {
        d.borrow()
            .as_ref()
            .map(|docks| {
                docks
                    .docks
                    .borrow()
                    .iter()
                    .map(|(_, d)| d.clone())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Re-place the docks once the screen is on: mapping a layer surface while
/// umoja-mir has the outputs powered off wedged the compositor.
fn sync() {
    let Some(docks) = DOCKS.with(|d| d.borrow().clone()) else {
        return;
    };
    let app = docks.app.clone();
    screen::when_on(&app, move || docks.sync_now());
}

fn wanted_indexes(all: &[gdk::Monitor]) -> Vec<usize> {
    let d = gs::DASH_TO_DOCK;
    let connectors: Vec<String> = all
        .iter()
        .map(|m| m.connector().map(|c| c.to_string()).unwrap_or_default())
        .collect();
    pick(
        gs::get_bool(d, "multi-monitor", true),
        &gs::get_string(d, "preferred-monitor-by-connector", "primary"),
        gs::get_int(d, "preferred-monitor", -2),
        &connectors,
        all.iter().position(monitors::is_primary),
    )
}

/// The monitors (indexes into `connectors`) that get a dock.
fn pick(
    multi: bool,
    by_connector: &str,
    preferred: i32,
    connectors: &[String],
    primary: Option<usize>,
) -> Vec<usize> {
    if connectors.is_empty() {
        return Vec::new();
    }
    if multi {
        return (0..connectors.len()).collect();
    }
    let primary = primary.unwrap_or(0);
    let by_name = (!by_connector.is_empty() && by_connector != "primary")
        .then(|| {
            connectors
                .iter()
                .position(|c| monitors::same_connector(c, by_connector))
        })
        .flatten();
    let by_index = (by_connector.is_empty() || by_connector == "primary")
        .then(|| usize::try_from(preferred).ok())
        .flatten()
        .filter(|&i| i < connectors.len());
    vec![by_name.or(by_index).unwrap_or(primary)]
}

/// Call `f` on every org.umoja.Compositor WindowsChanged and
/// WorkspacesChanged (umoja-mir coalesces them to one per 250 ms): windows
/// opened, closed, moved, resized, raised, focused, or a workspace switch.
pub fn on_windows_changed(f: impl Fn() + 'static) {
    let f = Rc::new(f);
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        for member in ["WindowsChanged", "WorkspacesChanged"] {
            let f = f.clone();
            #[allow(deprecated)]
            let id = conn.signal_subscribe(
                Some(dbus::COMPOSITOR_BUS_NAME),
                Some(dbus::COMPOSITOR_INTERFACE),
                Some(member),
                Some(dbus::COMPOSITOR_OBJECT_PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, _| f(),
            );
            // Lives as long as the dock (the process).
            std::mem::forget(id);
        }
    });
}

/// Which monitors (connectors) each window is on, for isolate-monitors.
/// A window with no entry yet shows on every dock.
#[derive(Default)]
pub struct Homes {
    map: RefCell<HashMap<WindowId, Vec<String>>>,
    in_flight: Cell<bool>,
}

impl Homes {
    pub fn shows_on(&self, id: WindowId, monitor: &gdk::Monitor) -> bool {
        let Some(here) = monitor.connector() else {
            return true;
        };
        match self.map.borrow().get(&id) {
            Some(on) => on.iter().any(|c| monitors::same_connector(c, &here)),
            None => true,
        }
    }
}

/// A window as GetWindows reports it: app id, title, centre point.
struct Placed {
    app_id: String,
    title: String,
    centre: (i32, i32),
}

/// Re-read where windows are and rebuild the docks if any moved monitor.
fn refresh_homes() {
    let Some(docks) = DOCKS.with(|d| d.borrow().clone()) else {
        return;
    };
    if !docks
        .docks
        .borrow()
        .iter()
        .any(|(_, d)| d.config.borrow().isolate_monitors)
    {
        return;
    }
    let homes = docks.shared.homes.clone();
    if homes.in_flight.replace(true) {
        return;
    }
    glib::spawn_future_local(async move {
        let placed = fetch_placed().await;
        homes.in_flight.set(false);
        let Some(placed) = placed else { return };
        let geometry: Vec<(String, gdk::Rectangle)> = monitors::all()
            .iter()
            .filter_map(|m| Some((m.connector()?.to_string(), m.geometry())))
            .collect();
        let new = {
            let windows = docks.shared.windows.borrow();
            match_homes(windows.values(), &placed, &geometry)
        };
        if *homes.map.borrow() != new {
            *homes.map.borrow_mut() = new;
            for d in all() {
                d.rebuild();
            }
        }
    });
}

/// Each window's monitors: those holding a GetWindows entry with the same
/// app id and title (or, with none, the same app id). Two identical windows
/// on different monitors can't be told apart, so both show on both docks.
fn match_homes<'a>(
    windows: impl Iterator<Item = &'a WindowInfo>,
    placed: &[Placed],
    geometry: &[(String, gdk::Rectangle)],
) -> HashMap<WindowId, Vec<String>> {
    let monitor_of = |(x, y): (i32, i32)| {
        geometry
            .iter()
            .find(|(_, g)| {
                x >= g.x() && y >= g.y() && x < g.x() + g.width() && y < g.y() + g.height()
            })
            .map(|(c, _)| c.clone())
    };
    let mut out = HashMap::new();
    for w in windows {
        let same_app = || placed.iter().filter(|p| p.app_id == w.app_id);
        let mut candidates: Vec<&Placed> = same_app().filter(|p| p.title == w.title).collect();
        if candidates.is_empty() {
            candidates = same_app().collect();
        }
        let mut on: Vec<String> = candidates
            .iter()
            .filter_map(|p| monitor_of(p.centre))
            .collect();
        on.sort_unstable();
        on.dedup();
        if !on.is_empty() {
            out.insert(w.id, on);
        }
    }
    out
}

async fn fetch_placed() -> Option<Vec<Placed>> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    let v = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            "GetWindows",
            None,
            Some(glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await
        .map_err(|e| tracing::debug!(error = %e, "GetWindows failed"))
        .ok()?;
    let (rows,) = v.get::<(Vec<(u64, String, String, i32, i32, i32, i32, u32)>,)>()?;
    Some(
        rows.into_iter()
            .map(|(_, app_id, title, x, y, w, h, _)| Placed {
                app_id,
                title,
                centre: (x + w / 2, y + h / 2),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::pick;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn follows_ubuntu_dock_settings() {
        let two = names(&["eDP-1", "HDMI-A-1"]);
        // Default: the primary monitor only.
        assert_eq!(pick(false, "primary", -2, &two, Some(1)), vec![1]);
        assert_eq!(pick(false, "primary", -2, &two, None), vec![0]);
        // Every monitor.
        assert_eq!(pick(true, "primary", -2, &two, Some(1)), vec![0, 1]);
        // "Show on" a connector, in mutter's spelling.
        assert_eq!(pick(false, "eDP-1", -2, &two, Some(1)), vec![0]);
        assert_eq!(pick(false, "HDMI-1", -2, &two, Some(0)), vec![1]);
        // Unplugged: back to the primary.
        assert_eq!(pick(false, "DP-3", -2, &two, Some(1)), vec![1]);
        // The old index key, when the connector key says primary.
        assert_eq!(pick(false, "primary", 0, &two, Some(1)), vec![0]);
        assert_eq!(pick(false, "primary", 5, &two, Some(1)), vec![1]);
        assert!(pick(false, "primary", -2, &[], None).is_empty());
    }

    #[test]
    fn windows_belong_to_the_monitor_under_their_centre() {
        use super::{Placed, match_homes};
        use crate::tracker::WindowInfo;
        let geometry = vec![
            (
                "eDP-1".to_string(),
                gtk4::gdk::Rectangle::new(0, 0, 1728, 1080),
            ),
            (
                "HDMI-A-1".to_string(),
                gtk4::gdk::Rectangle::new(1728, 0, 2560, 1440),
            ),
        ];
        let win = |id, app: &str, title: &str| WindowInfo {
            id,
            app_id: app.into(),
            title: title.into(),
            ..Default::default()
        };
        let at = |app: &str, title: &str, x| Placed {
            app_id: app.into(),
            title: title.into(),
            centre: (x, 500),
        };
        let windows = [
            win(1, "firefox", "News"),
            win(2, "firefox", "Mail"),
            win(3, "term", "ken@umoja: ~"),
            win(4, "term", "ken@umoja: ~"),
            win(5, "new", "Opening"),
        ];
        let placed = [
            at("firefox", "News", 100),
            at("firefox", "Mail", 2000),
            at("term", "ken@umoja: ~", 100),
            at("term", "ken@umoja: ~", 3000),
        ];
        let homes = match_homes(windows.iter(), &placed, &geometry);
        assert_eq!(homes[&1], vec!["eDP-1".to_string()]);
        assert_eq!(homes[&2], vec!["HDMI-A-1".to_string()]);
        // Identical windows on two monitors show on both docks.
        assert_eq!(homes[&3], vec!["HDMI-A-1".to_string(), "eDP-1".to_string()]);
        assert_eq!(homes[&3], homes[&4]);
        // Not reported yet: no entry, so every dock shows it.
        assert!(!homes.contains_key(&5));
    }
}
