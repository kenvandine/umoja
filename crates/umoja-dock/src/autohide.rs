//! Ubuntu Dock's "Auto-hide the Dock" (dash-to-dock `dock-fixed=false`).
//!
//! Like dash-to-dock, an unfixed dock reserves no screen space and:
//!
//! * with `intellihide` (Ubuntu's default) stays visible until a window
//!   overlaps it, then slides away; which windows count follows
//!   `intellihide-mode` (all windows, the focused app's windows, maximized
//!   windows only);
//! * with `autohide` comes back while the pointer rests on the screen edge
//!   (after `show-delay`) and leaves again `hide-delay` after the pointer
//!   does;
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
        }
    }
}

/// Auto-hide state, kept on the [`Dock`].
#[derive(Default)]
pub struct Hide {
    /// The dock is (being) shown.
    shown: Cell<bool>,
    hovered: Cell<bool>,
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
        if h.autohide && self.hide.hovered.get() {
            return true;
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
        let (_, along) = self.hide.extent.get();
        let along = along.max(HIDDEN_SIZE);
        if self.config.borrow().position.is_vertical() {
            self.window.set_default_size(HIDDEN_SIZE, along);
        } else {
            self.window.set_default_size(along, HIDDEN_SIZE);
        }
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

impl Rect {
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
