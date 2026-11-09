//! Which monitors get a dock, as Ubuntu Dock decides it.
//!
//! `org.gnome.shell.extensions.dash-to-dock`:
//!
//! * `multi-monitor`: a dock on every monitor.
//! * `preferred-monitor-by-connector`: the monitor for the one dock, by
//!   connector ("eDP-1", "HDMI-1"), or "primary". Settings › Ubuntu Desktop
//!   › Dock › "Show on" writes it. A connector that isn't plugged in falls
//!   back to the primary monitor.
//! * `preferred-monitor`: the older index form (-2 primary, else an index).
//!
//! The docks follow hotplug, the primary monitor changing (Settings ›
//! Displays) and these keys, live.

use crate::{Dock, Shared, launcher_entry, new_dock, tracker};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use tracker::Event;
use umoja_common::gsettings as gs;
use umoja_common::{monitors, screen};

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
                    match ev {
                        Event::Changed(info) => {
                            windows.borrow_mut().insert(info.id, info);
                        }
                        Event::Closed(id) => {
                            windows.borrow_mut().remove(&id);
                        }
                    }
                    for d in all() {
                        d.rebuild();
                    }
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
        gs::get_bool(d, "multi-monitor", false),
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
}
