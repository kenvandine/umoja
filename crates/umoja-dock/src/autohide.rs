//! Ubuntu Dock's "Auto-hide the Dock" (dash-to-dock `dock-fixed=false`).
//!
//! Like dash-to-dock, an unfixed dock reserves no screen space and:
//!
//! * with `intellihide` (Ubuntu's default) stays visible until a window
//!   overlaps it, then slides away; which windows count follows
//!   `intellihide-mode` (all windows, the focused app's windows, maximized
//!   windows only);
//! * with `autohide` comes back when the pointer pushes against the screen
//!   edge under it (GNOME's pressure barrier: `pressure-threshold` pixels of
//!   motion into the edge within a second, sensed by umoja-mir and sent as
//!   `EdgePressure`), and leaves again `hide-delay` after the pointer does.
//!   Only reaching the edge is not enough, so a fullscreen app's controls at
//!   the bottom of the screen stay clickable. With `require-pressure-to-show`
//!   off, resting on the edge for `show-delay` brings it back instead;
//! * doesn't come back over a focused fullscreen window unless
//!   `autohide-in-fullscreen` is on;
//! * stays up while one of its menus is open or it has the keyboard
//!   (Ctrl+Alt+Tab).
//!
//! Window geometry comes from umoja-mir's `org.umoja.Compositor.GetWindows`,
//! polled while intellihide needs it (foreign-toplevel has no geometry, and
//! moving or resizing a window sends no event the dock could follow).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::AnimationExt;
use umoja_common::dbus;
use umoja_common::gsettings as gs;
use umoja_common::types::DockPosition;

use crate::Dock;

/// Width of the strip left on the screen edge while the dock is hidden: the
/// pointer touching it brings the dock back.
const HIDDEN_SIZE: i32 = 2;
/// The slide takes this much longer than dash-to-dock's animation-time
/// (0.2 s by default, so 0.35 s): a fade that quick reads fine, a slide
/// that quick looks like a jump.
const SLIDE_SLOWER: f64 = 1.75;
/// How often intellihide looks at window positions.
const POLL: Duration = Duration::from_millis(250);
/// How long a push keeps the dock up if the pointer never enters it.
const PUSH_GRACE: Duration = Duration::from_millis(1500);

// org.umoja.Compositor window flags (UMOJA_WINDOW_* in the shim header).
const FLAG_FOCUSED: u32 = 1;
const FLAG_MINIMIZED: u32 = 2;
const FLAG_CURRENT_WORKSPACE: u32 = 4;
const FLAG_MAXIMIZED: u32 = 8;
const FLAG_FULLSCREEN: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    All,
    FocusedApp,
    Maximized,
}

#[derive(Clone, Copy, Debug)]
pub struct HideConfig {
    pub autohide: bool,
    pub intellihide: bool,
    pub mode: Mode,
    pub show_delay: Duration,
    pub hide_delay: Duration,
    pub animation: Duration,
    pub require_pressure: bool,
    pub in_fullscreen: bool,
}

fn seconds(schema: &str, key: &str, default: f64) -> Duration {
    let v = gs::settings(schema)
        .filter(|s| gs::has_key(s, key))
        .map(|s| s.double(key))
        .unwrap_or(default);
    Duration::from_secs_f64(v.clamp(0.0, 5.0))
}

impl HideConfig {
    pub fn load() -> Self {
        let d = gs::DASH_TO_DOCK;
        // An enum key: read the nick, g_settings_get_string() refuses enums.
        let mode = gs::settings(d)
            .filter(|s| gs::has_key(s, "intellihide-mode"))
            .and_then(|s| s.value("intellihide-mode").str().map(str::to_owned))
            .unwrap_or_default();
        Self {
            autohide: gs::get_bool(d, "autohide", true),
            intellihide: gs::get_bool(d, "intellihide", true),
            mode: match mode.as_str() {
                "ALL_WINDOWS" | "ALWAYS_ON_TOP" => Mode::All,
                "MAXIMIZED_WINDOWS" => Mode::Maximized,
                _ => Mode::FocusedApp,
            },
            show_delay: seconds(d, "show-delay", 0.25),
            hide_delay: seconds(d, "hide-delay", 0.20),
            animation: seconds(d, "animation-time", 0.20),
            require_pressure: gs::get_bool(d, "require-pressure-to-show", true),
            in_fullscreen: gs::get_bool(d, "autohide-in-fullscreen", false),
        }
    }
}

/// Auto-hide state, kept on the [`Dock`].
#[derive(Default)]
pub struct Hide {
    /// The dock is (being) shown.
    shown: Cell<bool>,
    hovered: Cell<bool>,
    /// The pointer pushed against the edge: show until it enters and leaves
    /// the dock (or never arrives).
    pushed: Cell<bool>,
    push_timer: RefCell<Option<glib::SourceId>>,
    /// Intellihide: a window covers the dock.
    overlapped: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
    anim: RefCell<Option<adw::TimedAnimation>>,
    /// How far the dock has slid off the edge: 0 shown, 1 gone.
    progress: Cell<f64>,
    polling: Cell<bool>,
    in_flight: Cell<bool>,
    /// Size of the shown dock (across the edge, along it), for the hidden
    /// strip and the intellihide hit box.
    extent: Cell<(i32, i32)>,
}

impl Hide {
    pub fn new() -> Self {
        let h = Self::default();
        h.shown.set(true);
        h
    }

    pub fn shown(&self) -> bool {
        self.shown.get()
    }
}

impl Dock {
    fn wants_shown(&self) -> bool {
        let cfg = *self.config.borrow();
        if cfg.fixed || self.menu_open.get() || crate::keynav::focused() {
            return true;
        }
        let h = &cfg.hide;
        if h.autohide {
            if self.hide.pushed.get() {
                return true;
            }
            // Under the pressure barrier, hovering only keeps a shown dock
            // up; it never brings a hidden one back.
            if self.hide.hovered.get() && (self.hide.shown.get() || !h.require_pressure) {
                return true;
            }
        }
        if h.intellihide {
            return !self.hide.overlapped.get();
        }
        // dash-to-dock with neither: autohide alone keeps it hidden until
        // hovered; with both off the unfixed dock just stays up.
        !h.autohide
    }

    /// Re-apply settings: start or stop intellihide and show or hide.
    pub fn refresh_autohide(self: &Rc<Self>) {
        let cfg = *self.config.borrow();
        if !cfg.fixed && cfg.hide.intellihide {
            self.start_polling();
        } else {
            self.hide.overlapped.set(false);
        }
        self.update_visibility();
    }

    /// Show or hide the dock after the GNOME delays.
    pub fn update_visibility(self: &Rc<Self>) {
        let want = self.wants_shown();
        if let Some(id) = self.hide.timer.borrow_mut().take() {
            id.remove();
        }
        if want == self.hide.shown.get() {
            return;
        }
        let cfg = self.config.borrow().hide;
        let delay = if !want {
            cfg.hide_delay
        } else if self.hide.pushed.get() {
            Duration::ZERO
        } else if self.hide.hovered.get() {
            cfg.show_delay
        } else {
            Duration::ZERO
        };
        // Menus and the keyboard need the dock now, as does a settings change.
        if delay.is_zero() || self.menu_open.get() || crate::keynav::focused() {
            self.set_shown(want);
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(delay, move || {
            if let Some(d) = weak.upgrade() {
                d.hide.timer.borrow_mut().take();
                let want = d.wants_shown();
                if want != d.hide.shown.get() {
                    d.set_shown(want);
                }
            }
        });
        *self.hide.timer.borrow_mut() = Some(id);
    }

    fn set_shown(self: &Rc<Self>, show: bool) {
        self.hide.shown.set(show);
        if let Some(a) = self.hide.anim.borrow_mut().take() {
            a.pause();
        }
        let base = self.config.borrow().hide.animation.as_millis() as f64;
        let ms = (base * SLIDE_SLOWER).round().max(1.0) as u32;
        let from = self.hide.progress.get();
        if show {
            self.set_input(true);
            self.list.set_visible(true);
            self.update_reserved_space();
        }
        // Start from where it is now, so a reversal mid-slide is smooth and
        // a dock coming back from the hidden strip starts off screen.
        self.place_slide();
        let weak = Rc::downgrade(self);
        let target = adw::CallbackAnimationTarget::new(move |p| {
            if let Some(d) = weak.upgrade() {
                d.hide.progress.set(p);
                d.place_slide();
            }
        });
        let anim =
            adw::TimedAnimation::new(&self.list, from, if show { 0.0 } else { 1.0 }, ms, target);
        anim.set_easing(if show {
            adw::Easing::EaseOutCubic
        } else {
            adw::Easing::EaseInOutCubic
        });
        if !show {
            let weak = Rc::downgrade(self);
            anim.connect_done(move |_| {
                if let Some(d) = weak.upgrade()
                    && !d.hide.shown.get()
                {
                    d.collapse();
                }
            });
        }
        anim.play();
        *self.hide.anim.borrow_mut() = Some(anim);
    }

    /// Shift the dock towards its screen edge by the slide's progress.
    pub fn place_slide(&self) {
        let p = self.hide.progress.get() as f32;
        let distance = self.thickness() as f32 * p;
        let (x, y) = match self.config.borrow().position {
            DockPosition::Left => (-distance, 0.0),
            DockPosition::Right => (distance, 0.0),
            DockPosition::Top => (0.0, -distance),
            DockPosition::Bottom => (0.0, distance),
        };
        self.slide.set_offset(x, y);
    }

    /// Shrink to the thin strip along the edge that brings the dock back.
    pub fn collapse(&self) {
        self.list.set_visible(false);
        // Behind the pressure barrier the strip only needs to exist: clicks
        // and hovers on the screen edge go to the window underneath.
        let cfg = *self.config.borrow();
        self.set_input(!(cfg.hide.autohide && cfg.hide.require_pressure));
        let (_, along) = self.hide.extent.get();
        let along = along.max(HIDDEN_SIZE);
        if self.config.borrow().position.is_vertical() {
            self.window.set_default_size(HIDDEN_SIZE, along);
        } else {
            self.window.set_default_size(along, HIDDEN_SIZE);
        }
    }

    /// Let the dock take pointer input, or let it all through.
    fn set_input(&self, on: bool) {
        let Some(surface) = self.window.surface() else {
            return;
        };
        let rect = if on {
            gtk::cairo::RectangleInt::new(0, 0, i32::MAX / 2, i32::MAX / 2)
        } else {
            gtk::cairo::RectangleInt::new(0, 0, 0, 0)
        };
        surface.set_input_region(&gtk::cairo::Region::create_rectangle(&rect));
    }

    /// umoja-mir's EdgePressure: the pointer pushed against `edge` at (x, y).
    fn on_edge_pressure(self: &Rc<Self>, edge: u32, x: i32, y: i32) {
        let cfg = *self.config.borrow();
        let ours = match cfg.position {
            DockPosition::Left => 1,
            DockPosition::Right => 2,
            DockPosition::Top => 4,
            DockPosition::Bottom => 8,
        };
        if cfg.fixed || !cfg.hide.autohide || edge != ours || self.hide.shown.get() {
            return;
        }
        // Like the barrier, only along the dock itself, on its monitor.
        let Some(dock) = self.dock_rect() else { return };
        let along = if cfg.position.is_vertical() {
            (dock.y..dock.y + dock.h).contains(&y)
        } else {
            (dock.x..dock.x + dock.w).contains(&x)
        };
        let Some(monitor) = self.monitor_rect() else {
            return;
        };
        if !along || !monitor.contains(x, y) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let windows = if cfg.hide.in_fullscreen {
                None
            } else {
                fetch_windows().await
            };
            let Some(d) = weak.upgrade() else { return };
            if windows.is_some_and(|w| fullscreen_on(&w, &monitor)) {
                tracing::debug!("edge pressure over a fullscreen window: dock stays hidden");
                return;
            }
            d.push();
        });
    }

    fn push(self: &Rc<Self>) {
        self.hide.pushed.set(true);
        if let Some(id) = self.hide.push_timer.borrow_mut().take() {
            id.remove();
        }
        // Normally the pointer enters the dock as it slides in, and leaving
        // it ends the push; give up if it never does.
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(PUSH_GRACE, move || {
            if let Some(d) = weak.upgrade() {
                d.hide.push_timer.borrow_mut().take();
                if d.hide.pushed.replace(false) {
                    d.update_visibility();
                }
            }
        });
        *self.hide.push_timer.borrow_mut() = Some(id);
        self.update_visibility();
    }

    fn end_push(&self) {
        self.hide.pushed.set(false);
        if let Some(id) = self.hide.push_timer.borrow_mut().take() {
            id.remove();
        }
    }

    fn monitor_rect(&self) -> Option<Rect> {
        let surface = self.window.surface()?;
        let g = surface.display().monitor_at_surface(&surface)?.geometry();
        Some(Rect {
            x: g.x(),
            y: g.y(),
            w: g.width(),
            h: g.height(),
        })
    }

    /// Remember the shown dock's size (call when its contents change).
    pub fn remember_extent(&self, across: i32) {
        if !self.list.is_visible() || self.list.first_child().is_none() {
            return;
        }
        let along = if self.config.borrow().position.is_vertical() {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        };
        let (_, natural, _, _) = self.list.measure(along, -1);
        self.hide.extent.set((across, natural));
    }

    pub fn install_autohide(self: &Rc<Self>) {
        let motion = gtk::EventControllerMotion::new();
        let enter = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(d) = enter.upgrade() {
                d.hide.hovered.set(true);
                // In the dock now: hovering keeps it up from here.
                d.end_push();
                d.update_visibility();
            }
        });
        let leave = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(d) = leave.upgrade() {
                d.hide.hovered.set(false);
                d.update_visibility();
            }
        });
        self.window.add_controller(motion);
        // A dock that starts out hidden collapsed before it had a surface.
        let mapped = Rc::downgrade(self);
        self.window.connect_map(move |_| {
            if let Some(d) = mapped.upgrade()
                && !d.hide.shown.get()
            {
                d.collapse();
            }
        });
        self.subscribe_edge_pressure();
    }

    fn subscribe_edge_pressure(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                return;
            };
            #[allow(deprecated)]
            let id = conn.signal_subscribe(
                Some(dbus::COMPOSITOR_BUS_NAME),
                Some(dbus::COMPOSITOR_INTERFACE),
                Some("EdgePressure"),
                Some(dbus::COMPOSITOR_OBJECT_PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, params| {
                    let Some(d) = weak.upgrade() else { return };
                    if let Some((edge, x, y)) = params.get::<(u32, i32, i32)>() {
                        d.on_edge_pressure(edge, x, y);
                    }
                },
            );
            // Lives as long as the dock (the process).
            std::mem::forget(id);
        });
    }

    fn start_polling(self: &Rc<Self>) {
        if self.hide.polling.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(POLL, move || {
            let Some(d) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let cfg = *d.config.borrow();
            if cfg.fixed || !cfg.hide.intellihide {
                d.hide.polling.set(false);
                return glib::ControlFlow::Break;
            }
            if !d.hide.in_flight.replace(true) {
                let weak = Rc::downgrade(&d);
                glib::spawn_future_local(async move {
                    let windows = fetch_windows().await;
                    let Some(d) = weak.upgrade() else { return };
                    d.hide.in_flight.set(false);
                    let Some(windows) = windows else { return };
                    let overlapped = d.covered_by(&windows);
                    if overlapped != d.hide.overlapped.replace(overlapped) {
                        tracing::debug!(overlapped, "intellihide");
                        d.update_visibility();
                    }
                });
            }
            glib::ControlFlow::Continue
        });
    }

    /// Where the shown dock sits, in the compositor's logical coordinates.
    fn dock_rect(&self) -> Option<Rect> {
        let surface = self.window.surface()?;
        let g = surface.display().monitor_at_surface(&surface)?.geometry();
        let cfg = *self.config.borrow();
        let (across, along) = self.hide.extent.get();
        if across <= 0 {
            return None;
        }
        let len = |full: i32| {
            if cfg.extend || along <= 0 {
                full
            } else {
                along.min(full)
            }
        };
        Some(match cfg.position {
            DockPosition::Left | DockPosition::Right => {
                let h = len(g.height());
                let x = if cfg.position == DockPosition::Left {
                    g.x()
                } else {
                    g.x() + g.width() - across
                };
                Rect {
                    x,
                    y: g.y() + (g.height() - h) / 2,
                    w: across,
                    h,
                }
            }
            DockPosition::Top | DockPosition::Bottom => {
                let w = len(g.width());
                let y = if cfg.position == DockPosition::Top {
                    g.y()
                } else {
                    g.y() + g.height() - across
                };
                Rect {
                    x: g.x() + (g.width() - w) / 2,
                    y,
                    w,
                    h: across,
                }
            }
        })
    }

    fn covered_by(&self, windows: &[Row]) -> bool {
        let Some(dock) = self.dock_rect() else {
            return false;
        };
        let mode = self.config.borrow().hide.mode;
        let focused_app = windows
            .iter()
            .find(|w| w.flags & FLAG_FOCUSED != 0)
            .map(|w| w.app_id.as_str());
        windows.iter().any(|w| {
            let counts = match mode {
                Mode::All => true,
                Mode::Maximized => w.flags & (FLAG_MAXIMIZED | FLAG_FULLSCREEN) != 0,
                Mode::FocusedApp => focused_app == Some(w.app_id.as_str()),
            };
            counts
                && w.flags & FLAG_CURRENT_WORKSPACE != 0
                && w.flags & FLAG_MINIMIZED == 0
                && dock.intersects(&w.rect)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// A focused fullscreen window on `monitor` (GNOME keeps the dock away
/// from it unless autohide-in-fullscreen is set).
fn fullscreen_on(windows: &[Row], monitor: &Rect) -> bool {
    windows.iter().any(|w| {
        w.flags & FLAG_FULLSCREEN != 0
            && w.flags & FLAG_FOCUSED != 0
            && w.flags & FLAG_CURRENT_WORKSPACE != 0
            && w.flags & FLAG_MINIMIZED == 0
            && monitor.intersects(&w.rect)
    })
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }

    fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

struct Row {
    app_id: String,
    rect: Rect,
    flags: u32,
}

async fn fetch_windows() -> Option<Vec<Row>> {
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
            .map(|(_, app_id, _, x, y, w, h, flags)| Row {
                app_id,
                rect: Rect { x, y, w, h },
                flags,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::Rect;

    #[test]
    fn touching_edges_do_not_overlap() {
        let dock = Rect {
            x: 0,
            y: 100,
            w: 78,
            h: 600,
        };
        let after = Rect {
            x: 78,
            y: 0,
            w: 800,
            h: 900,
        };
        let under = Rect {
            x: 70,
            y: 0,
            w: 800,
            h: 900,
        };
        assert!(!dock.intersects(&after));
        assert!(dock.intersects(&under));
    }
}
