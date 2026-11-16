//! Drag dock icons to reorder them, as in GNOME's dash and Ubuntu Dock.
//!
//! - Drag a pinned app along the dock to move it among the pins.
//! - Drag a running app that isn't pinned in among the pins to pin it there.
//! - Drop a pinned app on Show Apps to unpin it.
//! - Let go off the dock and it goes back where it was.
//!
//! While the button is held (a pointer grab, not Wayland drag and drop) a
//! copy of the icon follows the pointer along the dock, drawn on top by
//! dock_layout's float, and the icon's own place stays empty as a gap that
//! moves to wherever the copy would land; the other icons glide aside. On
//! release the new order is written to `org.umoja.shell favorite-apps`,
//! which rebuilds every dock. Ask Umoja and Show Apps stay fixed.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::{gio, graphene, prelude::*};
use gtk4 as gtk;

use crate::Dock;

/// How far the pointer moves before a press becomes a drag (GTK's default
/// gtk-dnd-drag-threshold).
const THRESHOLD: f64 = 8.0;
/// How far off the dock the pointer may stray before a drop cancels.
const SLOP: f64 = 24.0;

/// An app icon in the dock, in the order shown.
pub struct Item {
    pub widget: gtk::Widget,
    /// Desktop id with its `.desktop` suffix, as favorite-apps stores it.
    pub id: String,
    pub pinned: bool,
    pub icon: Option<gio::Icon>,
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
    /// Pointer at the press, from the icon's start edge along the dock.
    grab: f64,
    /// Past the threshold: the icon is being dragged.
    started: bool,
    /// Where the icon started, among `items`.
    home: usize,
    /// Start edge of each icon's place along the dock at the press, and
    /// an icon's length. Places stay put while icons trade them, so the
    /// landing spot comes from these rather than from icons mid-glide.
    slots: Vec<f64>,
    len: f64,
    /// The copy that follows the pointer.
    float: Option<gtk::Widget>,
    /// An unpinned app has been dragged in among the pins.
    as_pin: bool,
    /// The copy is on Show Apps with a pinned app (drop unpins).
    on_show_apps: bool,
    /// The pointer is off the dock (drop puts everything back).
    outside: bool,
}

fn along(vertical: bool, x: f64, y: f64) -> f64 {
    if vertical { y } else { x }
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
        let vertical = d.config.borrow().position.is_vertical();
        let Some(p) = w.compute_point(&d.list, &graphene::Point::new(x as f32, y as f32)) else {
            return;
        };
        let targets = d.drag.borrow();
        let Some(home) = targets.items.iter().position(|i| i.widget == w) else {
            return;
        };
        let bounds: Vec<_> = targets
            .items
            .iter()
            .filter_map(|i| i.widget.compute_bounds(&d.list))
            .collect();
        if bounds.len() != targets.items.len() {
            return;
        }
        let slots: Vec<f64> = bounds
            .iter()
            .map(|b| along(vertical, b.x() as f64, b.y() as f64))
            .collect();
        let len = along(vertical, bounds[home].width() as f64, bounds[home].height() as f64);
        let (px, py) = (p.x() as f64, p.y() as f64);
        *s.borrow_mut() = Some(Drag {
            start: (px, py),
            grab: along(vertical, px, py) - slots[home],
            started: false,
            home,
            slots,
            len,
            float: None,
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
            d.start_drag(&w, drag);
        }
        let (x, y) = (drag.start.0 + dx, drag.start.1 + dy);
        d.drag_to(&w, drag, x, y);
    });

    let (weak, s) = (Rc::downgrade(dock), state);
    let end = move |g: &gtk::GestureDrag| {
        let (Some(d), Some(w)) = (weak.upgrade(), g.widget()) else {
            return;
        };
        let Some(drag) = s.borrow_mut().take() else { return };
        if drag.started {
            d.drop_drag(&w, &drag);
        }
    };
    let end2 = end.clone();
    gesture.connect_drag_end(move |g, _, _| end(g));
    // A grab lost to the compositor ends the drag too.
    gesture.connect_cancel(move |g, _| {
        if let Some(g) = g.downcast_ref::<gtk::GestureDrag>() {
            end2(g);
        }
    });
    widget.add_controller(gesture);
}

impl Dock {
    /// The press became a drag: float a copy of the icon, leave a gap.
    fn start_drag(&self, widget: &gtk::Widget, drag: &mut Drag) {
        self.dragging.set(true);
        self.window.set_cursor_from_name(Some("grabbing"));
        let size = self.config.borrow().icon_size;
        let icon = self
            .drag
            .borrow()
            .items
            .iter()
            .find(|i| &i.widget == widget)
            .and_then(|i| i.icon.clone());
        let image = match icon {
            Some(icon) => gtk::Image::from_gicon(&icon),
            None => gtk::Image::from_icon_name("application-x-executable"),
        };
        image.set_pixel_size(size);
        let float = gtk::Box::builder()
            .css_classes(["dock-item", "dock-float"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .build();
        image.set_hexpand(true);
        image.set_vexpand(true);
        float.append(&image);
        let (w, h) = (widget.width(), widget.height());
        float.set_size_request(w, h);
        // Last child, so it draws over the others.
        self.list.append(&float);
        widget.set_opacity(0.0);
        drag.float = Some(float.upcast());
    }

    /// Follow the pointer at (`x`, `y`) in list coordinates.
    fn drag_to(&self, widget: &gtk::Widget, drag: &mut Drag, x: f64, y: f64) {
        let vertical = self.config.borrow().position.is_vertical();
        let along_ptr = along(vertical, x, y);
        let (lw, lh) = (self.list.width() as f64, self.list.height() as f64);
        drag.outside = x < -SLOP || y < -SLOP || x > lw + SLOP || y > lh + SLOP;

        // The copy follows the pointer along the dock.
        let at = along_ptr - drag.grab;
        if let Some(f) = &drag.float {
            f.set_opacity(if drag.outside { 0.5 } else { 1.0 });
            self.layout.set_float(Some((f, at)));
        }
        let center = at + drag.len / 2.0;

        let mut targets = self.drag.borrow_mut();
        let Some(cur) = targets.items.iter().position(|i| &i.widget == widget) else {
            return;
        };
        let pinned = targets.items[cur].pinned;

        let on_show_apps = pinned
            && !drag.outside
            && targets.show_apps.as_ref().is_some_and(|(b, _)| {
                b.compute_bounds(&self.list).is_some_and(|r| {
                    let (s, l) = if vertical {
                        (r.y() as f64, r.height() as f64)
                    } else {
                        (r.x() as f64, r.width() as f64)
                    };
                    center >= s && center < s + l
                })
            });
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

        // Off the dock the gap goes home; on Show Apps it stays put.
        let to = if drag.outside {
            drag.as_pin = false;
            drag.home
        } else if on_show_apps {
            cur
        } else {
            let Some(over) = nearest_slot(&drag.slots, drag.len, center) else {
                return;
            };
            if over == cur {
                return;
            }
            let others_pinned = targets
                .items
                .iter()
                .enumerate()
                .filter(|(n, i)| *n != cur && i.pinned)
                .count();
            let over_pinned = targets.items[over].pinned;
            let (to, as_pin) = slot(over, over_pinned, drag.home, pinned, others_pinned);
            drag.as_pin = as_pin;
            to
        };
        if to != cur {
            move_item(&self.list, &mut targets.items, cur, to);
        }
    }

    /// The button came up: save the new order, unpin, or put things back.
    fn drop_drag(self: &Rc<Self>, widget: &gtk::Widget, drag: &Drag) {
        self.dragging.set(false);
        self.window.set_cursor(None::<&gtk::gdk::Cursor>);
        self.layout.set_float(None);
        if let Some(f) = &drag.float {
            self.list.remove(f);
        }
        widget.set_opacity(1.0);
        let show_apps = self.drag.borrow().show_apps.clone();
        if let Some((b, image)) = show_apps {
            b.remove_css_class("drop-unpin");
            image.set_icon_name(Some("view-app-grid-symbolic"));
        }

        let old = umoja_common::apps::favorites();
        let wanted = {
            let mut targets = self.drag.borrow_mut();
            let Some(cur) = targets.items.iter().position(|i| &i.widget == widget) else {
                return;
            };
            if drag.outside {
                // Glide back home.
                move_item(&self.list, &mut targets.items, cur, drag.home);
                None
            } else if drag.on_show_apps {
                let id = &targets.items[cur].id;
                Some(old.iter().filter(|f| *f != id).cloned().collect())
            } else {
                let order: Vec<(String, bool)> = targets
                    .items
                    .iter()
                    .map(|i| (i.id.clone(), i.pinned))
                    .collect();
                Some(keep_missing(&old, pins_after_drop(&order, cur, drag.as_pin)))
            }
        };
        match wanted {
            // favorite-apps changing rebuilds every dock.
            Some(w) if w != old => umoja_common::apps::set_favorites(&w),
            _ => {
                // An unpinned app taken back out of the pins goes home.
                let mut targets = self.drag.borrow_mut();
                if let Some(cur) = targets.items.iter().position(|i| &i.widget == widget)
                    && cur != drag.home
                {
                    move_item(&self.list, &mut targets.items, cur, drag.home);
                }
                drop(targets);
                if self.rebuild_pending.get() {
                    self.rebuild();
                }
            }
        }
    }
}

/// Move the item at `from` to `to`, in `items` and in the list.
fn move_item(list: &gtk::Box, items: &mut Vec<Item>, from: usize, to: usize) {
    let item = items.remove(from);
    let widget = item.widget.clone();
    items.insert(to, item);
    // Put the widget before whichever icon now follows it, or after the
    // one before it when it is last.
    let prev = match items.get(to + 1) {
        Some(next) => next.widget.prev_sibling(),
        None => Some(items[to - 1].widget.clone()),
    };
    if prev.as_ref() != Some(&widget) {
        list.reorder_child_after(&widget, prev.as_ref());
    }
}

/// The place whose middle is nearest `center` along the dock, when
/// `center` is over the run of places (or within half a place of it).
fn nearest_slot(slots: &[f64], len: f64, center: f64) -> Option<usize> {
    let first = *slots.first()?;
    let last = *slots.last()? + len;
    if center < first - len / 2.0 || center > last + len / 2.0 {
        return None;
    }
    slots
        .iter()
        .enumerate()
        .min_by(|a, b| {
            let da = (a.1 + len / 2.0 - center).abs();
            let db = (b.1 + len / 2.0 - center).abs();
            da.total_cmp(&db)
        })
        .map(|(i, _)| i)
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
    fn nearest_place() {
        let slots = [0.0, 60.0, 120.0];
        assert_eq!(nearest_slot(&slots, 56.0, 10.0), Some(0));
        assert_eq!(nearest_slot(&slots, 56.0, 95.0), Some(1));
        assert_eq!(nearest_slot(&slots, 56.0, 150.0), Some(2));
        // Past the last place by more than half an icon: Ask Umoja, the
        // spacer or Show Apps, so no move.
        assert_eq!(nearest_slot(&slots, 56.0, 210.0), None);
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
