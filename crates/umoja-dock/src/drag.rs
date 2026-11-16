//! Drag dock icons to reorder them, as in GNOME's dash and Ubuntu Dock.
//!
//! - Drag a pinned app along the dock to move it among the pins.
//! - Drag a running app that isn't pinned in among the pins to pin it there.
//! - Drop a pinned app on Show Apps to unpin it.
//! - Let go off the dock and nothing changes.
//!
//! The icon moves slot to slot inside the dock while the button is held
//! (a pointer grab, not Wayland drag and drop), and the new order is
//! written to `org.umoja.shell favorite-apps` on release, which rebuilds
//! every dock. Ask Umoja and Show Apps are fixed and can't be dragged.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::{graphene, prelude::*};
use gtk4 as gtk;

use crate::Dock;

/// How far the pointer moves before a press becomes a drag (GTK's default
/// gtk-dnd-drag-threshold).
const THRESHOLD: f64 = 8.0;

/// An app icon in the dock, in the order shown.
pub struct Item {
    pub widget: gtk::Widget,
    /// Desktop id with its `.desktop` suffix, as favorite-apps stores it.
    pub id: String,
    pub pinned: bool,
}

/// The dock's drag targets other than the app icons.
#[derive(Default)]
pub struct Targets {
    pub items: Vec<Item>,
    pub show_apps: Option<(gtk::Widget, gtk::Image)>,
}

struct Drag {
    /// Pointer at the press, in the list's coordinates.
    start: (f64, f64),
    /// Past the threshold: the icon is being dragged.
    started: bool,
    /// Where the icon started, among `items`.
    home: usize,
    /// An unpinned app has been dragged in among the pins.
    as_pin: bool,
    /// The pointer is on Show Apps with a pinned app (drop unpins).
    on_show_apps: bool,
    /// The pointer is off the dock (drop puts everything back).
    outside: bool,
}

/// Let `widget`, an app icon already listed in `dock.drag`, be dragged.
pub fn attach(dock: &Rc<Dock>, widget: &gtk::Widget) {
    let gesture = gtk::GestureDrag::new();
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    // Ahead of the button's own click gesture, which loses the press once
    // the drag claims it (so a drag never also launches the app).
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state: Rc<RefCell<Option<Drag>>> = Rc::default();

    let (weak, s) = (Rc::downgrade(dock), state.clone());
    gesture.connect_drag_begin(move |g, x, y| {
        let (Some(d), Some(w)) = (weak.upgrade(), g.widget()) else {
            return;
        };
        let Some(p) = w.compute_point(&d.list, &graphene::Point::new(x as f32, y as f32)) else {
            return;
        };
        let Some(home) = d.drag.borrow().items.iter().position(|i| i.widget == w) else {
            return;
        };
        *s.borrow_mut() = Some(Drag {
            start: (p.x() as f64, p.y() as f64),
            started: false,
            home,
            as_pin: false,
            on_show_apps: false,
            outside: false,
        });
    });

    let (weak, s) = (Rc::downgrade(dock), state.clone());
    gesture.connect_drag_update(move |g, dx, dy| {
        let (Some(d), Some(w)) = (weak.upgrade(), g.widget()) else {
            return;
        };
        let mut guard = s.borrow_mut();
        let Some(drag) = guard.as_mut() else { return };
        if !drag.started {
            if dx.hypot(dy) < THRESHOLD || d.menu_open.get() {
                return;
            }
            drag.started = true;
            g.set_state(gtk::EventSequenceState::Claimed);
            d.dragging.set(true);
            w.add_css_class("dock-drag");
            d.window.set_cursor_from_name(Some("grabbing"));
        }
        let (x, y) = (drag.start.0 + dx, drag.start.1 + dy);
        d.drag_to(&w, drag, x, y);
    });

    let (weak, s) = (Rc::downgrade(dock), state);
    gesture.connect_drag_end(move |g, _, _| {
        let (Some(d), Some(w)) = (weak.upgrade(), g.widget()) else {
            return;
        };
        let Some(drag) = s.borrow_mut().take() else {
            return;
        };
        if drag.started {
            d.drop_drag(&w, &drag);
        }
    });
    widget.add_controller(gesture);
}

impl Dock {
    /// Follow the pointer at (`x`, `y`) in list coordinates.
    fn drag_to(&self, widget: &gtk::Widget, drag: &mut Drag, x: f64, y: f64) {
        let vertical = self.config.borrow().position.is_vertical();
        let along = if vertical { y } else { x };
        let inside = |w: &gtk::Widget| {
            w.compute_bounds(&self.list).is_some_and(|b| {
                let (bx, by) = (b.x() as f64, b.y() as f64);
                let (bw, bh) = (b.width() as f64, b.height() as f64);
                x >= bx && x < bx + bw && y >= by && y < by + bh
            })
        };
        drag.outside = !inside(self.list.upcast_ref());

        let mut targets = self.drag.borrow_mut();
        let Some(cur) = targets.items.iter().position(|i| &i.widget == widget) else {
            return;
        };
        let pinned = targets.items[cur].pinned;

        let on_show_apps =
            pinned && !drag.outside && targets.show_apps.as_ref().is_some_and(|(b, _)| inside(b));
        if on_show_apps != drag.on_show_apps {
            drag.on_show_apps = on_show_apps;
            if let Some((b, image)) = &targets.show_apps {
                if on_show_apps {
                    b.add_css_class("drop-unpin");
                    image.set_icon_name(Some("user-trash-symbolic"));
                } else {
                    b.remove_css_class("drop-unpin");
                    image.set_icon_name(Some("view-app-grid-symbolic"));
                }
            }
        }
        if drag.outside || on_show_apps {
            return;
        }

        // The icon under the pointer along the dock, if any.
        let over = targets.items.iter().position(|i| {
            i.widget != *widget
                && i.widget.compute_bounds(&self.list).is_some_and(|b| {
                    let (start, len) = if vertical {
                        (b.y() as f64, b.height() as f64)
                    } else {
                        (b.x() as f64, b.width() as f64)
                    };
                    along >= start && along < start + len
                })
        });
        let Some(over) = over else { return };
        let others_pinned = targets
            .items
            .iter()
            .enumerate()
            .filter(|(n, i)| *n != cur && i.pinned)
            .count();
        let over_pinned = targets.items[over].pinned;
        let (to, as_pin) = slot(over, over_pinned, drag.home, pinned, others_pinned);
        drag.as_pin = as_pin;
        if to == cur {
            return;
        }
        let item = targets.items.remove(cur);
        targets.items.insert(to, item);
        // Put the widget before whichever icon now follows it, or after
        // the one before it when it is last.
        let prev = match targets.items.get(to + 1) {
            Some(next) => next.widget.prev_sibling(),
            None => Some(targets.items[to - 1].widget.clone()),
        };
        if prev.as_ref() != Some(widget) {
            self.list.reorder_child_after(widget, prev.as_ref());
        }
    }

    /// The button came up: save the new order, unpin, or put things back.
    fn drop_drag(self: &Rc<Self>, widget: &gtk::Widget, drag: &Drag) {
        self.dragging.set(false);
        widget.remove_css_class("dock-drag");
        self.window.set_cursor(None::<&gtk::gdk::Cursor>);
        let (show_apps, new) = {
            let targets = self.drag.borrow();
            let new = targets
                .items
                .iter()
                .position(|i| &i.widget == widget)
                .map(|cur| {
                    let order: Vec<(String, bool)> = targets
                        .items
                        .iter()
                        .map(|i| (i.id.clone(), i.pinned))
                        .collect();
                    pins_after_drop(&order, cur, drag.as_pin)
                });
            (targets.show_apps.clone(), new)
        };
        if let Some((b, image)) = show_apps {
            b.remove_css_class("drop-unpin");
            image.set_icon_name(Some("view-app-grid-symbolic"));
        }
        let id = self
            .drag
            .borrow()
            .items
            .iter()
            .find(|i| &i.widget == widget)
            .map(|i| i.id.clone());

        let old = umoja_common::apps::favorites();
        let wanted = if drag.outside {
            None
        } else if drag.on_show_apps {
            id.map(|id| old.iter().filter(|f| **f != id).cloned().collect())
        } else {
            new.map(|n| keep_missing(&old, n))
        };
        match wanted {
            // favorite-apps changing rebuilds every dock.
            Some(w) if w != old => umoja_common::apps::set_favorites(&w),
            // Nothing changed: put the icons back as they were.
            _ => self.rebuild(),
        }
    }
}

/// Where the dragged icon goes with the pointer over the icon at `over`
/// (`over_pinned`: a pin), and whether it would be pinned there. Pins come
/// first, so a pinned app stays among them and a running app that isn't
/// pinned goes in among them (pinning it) or back to where it started.
fn slot(
    over: usize,
    over_pinned: bool,
    home: usize,
    pinned: bool,
    others_pinned: usize,
) -> (usize, bool) {
    if over_pinned {
        (over, true)
    } else if pinned {
        (others_pinned, true)
    } else {
        (home, false)
    }
}

/// The pinned ids, in dock order, after dropping the icon now at `dragged`
/// (`as_pin`: an unpinned app was dragged in among the pins).
fn pins_after_drop(order: &[(String, bool)], dragged: usize, as_pin: bool) -> Vec<String> {
    order
        .iter()
        .enumerate()
        .filter(|(n, (_, pinned))| *pinned || (*n == dragged && as_pin))
        .map(|(_, (id, _))| id.clone())
        .collect()
}

/// `new`, plus the pins the dock doesn't show (their app is uninstalled for
/// now) kept at the end, so a drag never forgets them.
fn keep_missing(old: &[String], mut new: Vec<String>) -> Vec<String> {
    for id in old {
        if !new.contains(id) {
            new.push(id.clone());
        }
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(items: &[(&str, bool)]) -> Vec<(String, bool)> {
        items.iter().map(|(i, p)| (i.to_string(), *p)).collect()
    }

    #[test]
    fn pinned_stays_among_pins() {
        // a b c pinned, x running; drag b (1) over x (3): last pin slot.
        assert_eq!(slot(3, false, 1, true, 2), (2, true));
        assert_eq!(slot(0, true, 1, true, 2), (0, true));
    }

    #[test]
    fn running_pins_or_goes_home() {
        // a b pinned, x y running; drag y (home 3) over x: stays home.
        assert_eq!(slot(2, false, 3, false, 2), (3, false));
        // Over a: pinned at 0.
        assert_eq!(slot(0, true, 3, false, 2), (0, true));
        // ...then over x (index 3 once y moved up): back home, unpinned.
        assert_eq!(slot(3, false, 3, false, 2), (3, false));
        // Over b once y sits before it: pinned at the end.
        assert_eq!(slot(2, true, 3, false, 2), (2, true));
    }

    #[test]
    fn drop_orders_pins() {
        let o = order(&[("b", true), ("a", true), ("x", false)]);
        assert_eq!(pins_after_drop(&o, 0, false), ["b", "a"]);
        let o = order(&[("a", true), ("x", false), ("b", true)]);
        assert_eq!(pins_after_drop(&o, 1, true), ["a", "x", "b"]);
        assert_eq!(pins_after_drop(&o, 1, false), ["a", "b"]);
    }

    #[test]
    fn missing_pins_kept() {
        let old = vec!["a".to_string(), "gone".to_string(), "b".to_string()];
        assert_eq!(
            keep_missing(&old, vec!["b".into(), "a".into()]),
            ["b", "a", "gone"]
        );
    }
}
