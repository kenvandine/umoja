//! The dock list's layout: a row (or column) like GtkBox's, except that
//! icons glide to a new place instead of jumping when the order changes,
//! and one "floating" child can be drawn anywhere along the dock, on top
//! of the others, without taking a place of its own (the icon being
//! dragged, see drag.rs).
//!
//! The list stays a gtk::Box for its child list API; this replaces its
//! BoxLayout, so the Box's own orientation and spacing setters must not be
//! used (they assume a BoxLayout): set them here instead.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;

/// How long an icon takes to glide to its new place, in microseconds.
const GLIDE_US: i64 = 180_000;

#[derive(Clone, Copy)]
pub struct Place {
    /// Where the layout puts the child along the dock.
    pos: i32,
    /// Distance from `pos` it was drawn at when the glide began.
    from: f64,
    /// Frame time the glide began.
    start: i64,
}

impl Place {
    fn offset(&self, now: i64) -> f64 {
        let t = ((now - self.start) as f64 / GLIDE_US as f64).clamp(0.0, 1.0);
        // Ease out cubic.
        self.from * (1.0 - t).powi(3)
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct DockLayout {
        pub vertical: Cell<bool>,
        pub spacing: Cell<i32>,
        pub places: RefCell<HashMap<gtk::Widget, Place>>,
        /// The floating child and where its start edge goes along the dock.
        pub float: RefCell<Option<(gtk::Widget, f64)>>,
        pub tick: RefCell<Option<gtk::TickCallbackId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DockLayout {
        const NAME: &'static str = "UmojaDockLayout";
        type Type = super::DockLayout;
        type ParentType = gtk::LayoutManager;
    }

    impl ObjectImpl for DockLayout {}

    impl LayoutManagerImpl for DockLayout {
        fn measure(
            &self,
            widget: &gtk::Widget,
            orientation: gtk::Orientation,
            _for_size: i32,
        ) -> (i32, i32, i32, i32) {
            let along = orientation == self.axis();
            let (mut min, mut nat, mut n) = (0, 0, 0);
            for c in self.flow(widget) {
                let (cmin, cnat, _, _) = c.measure(orientation, -1);
                if along {
                    min += cmin;
                    nat += cnat;
                } else {
                    min = min.max(cmin);
                    nat = nat.max(cnat);
                }
                n += 1;
            }
            if along && n > 1 {
                let gaps = self.spacing.get() * (n - 1);
                min += gaps;
                nat += gaps;
            }
            (min, nat, -1, -1)
        }

        fn allocate(&self, widget: &gtk::Widget, width: i32, height: i32, _baseline: i32) {
            let vertical = self.vertical.get();
            let axis = self.axis();
            let (length, across) = if vertical {
                (height, width)
            } else {
                (width, height)
            };
            let children = self.flow(widget);
            let mut sizes: Vec<(i32, i32)> = children
                .iter()
                .map(|c| {
                    let (min, nat, _, _) = c.measure(axis, across);
                    (min, nat)
                })
                .collect();
            let spacing = self.spacing.get() * (children.len() as i32 - 1).max(0);
            let natural: i32 = sizes.iter().map(|s| s.1).sum::<i32>() + spacing;
            let mut extra = length - natural;
            let mut lens: Vec<i32> = sizes.iter().map(|s| s.1).collect();
            if extra > 0 {
                let grow: Vec<usize> = (0..children.len())
                    .filter(|&i| children[i].compute_expand(axis))
                    .collect();
                for (k, &i) in grow.iter().enumerate() {
                    let share = extra / (grow.len() - k) as i32;
                    lens[i] += share;
                    extra -= share;
                }
            } else if extra < 0 {
                // Too long for the screen: shrink towards minimum sizes.
                for (i, (min, _)) in sizes.iter_mut().enumerate() {
                    let cut = (lens[i] - *min).min(-extra);
                    lens[i] -= cut;
                    extra += cut;
                }
            }

            let now = widget.frame_clock().map(|c| c.frame_time()).unwrap_or(0);
            let animate = widget.is_mapped();
            let mut old = self.places.take();
            let mut places = HashMap::new();
            let mut gliding = false;
            let mut pos = 0;
            for (c, len) in children.iter().zip(lens) {
                let place = match old.remove(c) {
                    Some(p) if p.pos != pos && animate => Place {
                        pos,
                        from: (p.pos - pos) as f64 + p.offset(now),
                        start: now,
                    },
                    Some(p) => Place { pos, ..p },
                    None => Place {
                        pos,
                        from: 0.0,
                        start: now,
                    },
                };
                let offset = place.offset(now);
                gliding |= offset.abs() >= 0.5;
                let shift = if offset.abs() >= 0.5 { offset } else { 0.0 };
                let at = pos as f64 + shift;
                Self::put(c, vertical, at, len, across);
                places.insert(c.clone(), place);
                pos += len + self.spacing.get();
            }
            *self.places.borrow_mut() = places;

            if let Some((f, at)) = self.float.borrow().as_ref() {
                let (_, len, _, _) = f.measure(axis, across);
                Self::put(f, vertical, at.clamp(0.0, (length - len).max(0) as f64), len, across);
            }

            // Keep laying out each frame while anything is still gliding.
            let mut tick = self.tick.borrow_mut();
            if gliding && tick.is_none() {
                *tick = Some(widget.add_tick_callback(|w, _| {
                    w.queue_allocate();
                    glib::ControlFlow::Continue
                }));
            } else if !gliding && let Some(id) = tick.take() {
                id.remove();
            }
        }
    }

    impl DockLayout {
        fn axis(&self) -> gtk::Orientation {
            if self.vertical.get() {
                gtk::Orientation::Vertical
            } else {
                gtk::Orientation::Horizontal
            }
        }

        /// The children that take a place: shown ones, minus the float.
        fn flow(&self, widget: &gtk::Widget) -> Vec<gtk::Widget> {
            let float = self.float.borrow().as_ref().map(|(f, _)| f.clone());
            let mut out = Vec::new();
            let mut c = widget.first_child();
            while let Some(w) = c {
                if w.should_layout() && Some(&w) != float.as_ref() {
                    out.push(w.clone());
                }
                c = w.next_sibling();
            }
            out
        }

        fn put(c: &gtk::Widget, vertical: bool, at: f64, len: i32, across: i32) {
            let (x, y, w, h) = if vertical {
                (0.0, at, across, len)
            } else {
                (at, 0.0, len, across)
            };
            let transform =
                gsk::Transform::new().translate(&graphene::Point::new(x as f32, y as f32));
            c.allocate(w, h, -1, Some(transform));
        }
    }
}

glib::wrapper! {
    pub struct DockLayout(ObjectSubclass<imp::DockLayout>) @extends gtk::LayoutManager;
}

impl DockLayout {
    pub fn new(spacing: i32) -> Self {
        let l: Self = glib::Object::new();
        l.imp().spacing.set(spacing);
        l
    }

    pub fn set_vertical(&self, vertical: bool) {
        if self.imp().vertical.replace(vertical) != vertical {
            // Don't glide everything across when the dock changes edge.
            self.imp().places.borrow_mut().clear();
            self.layout_changed();
        }
    }

    /// Draw `child` (already in the list) floating with its start edge at
    /// `at` along the dock, or stop floating with `None`.
    pub fn set_float(&self, float: Option<(&gtk::Widget, f64)>) {
        *self.imp().float.borrow_mut() = float.map(|(w, at)| (w.clone(), at));
        self.layout_changed();
    }
}
