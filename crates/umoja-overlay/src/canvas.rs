//! A widget drawn with GtkSnapshot nodes from a closure: a GPU-drawn
//! stand-in for a full-screen `gtk::DrawingArea`. A DrawingArea rasterizes
//! its whole size with cairo on the CPU and uploads it on every redraw,
//! which for a monitor-sized surface (the snap preview, the screenshot
//! UI's dimming) meant megabytes of pixels per pointer motion.

use gtk::graphene::{Rect, Size};
use gtk::gsk::RoundedRect;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use std::cell::RefCell;

type DrawFn = Box<dyn Fn(&gtk::Snapshot, f32, f32)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Canvas {
        pub draw: RefCell<Option<DrawFn>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "UmojaCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Canvas {}

    impl WidgetImpl for Canvas {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let w = self.obj();
            if let Some(draw) = self.draw.borrow().as_ref() {
                draw(snapshot, w.width() as f32, w.height() as f32);
            }
        }
    }
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>) @extends gtk::Widget;
}

impl Canvas {
    pub fn new() -> Self {
        let c: Self = glib::Object::new();
        c.set_hexpand(true);
        c.set_vexpand(true);
        c
    }

    /// Draw with `f(snapshot, width, height)` on every redraw.
    pub fn set_draw(&self, f: impl Fn(&gtk::Snapshot, f32, f32) + 'static) {
        self.imp().draw.replace(Some(Box::new(f)));
        self.queue_draw();
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect::new(x as f32, y as f32, w.max(0.0) as f32, h.max(0.0) as f32)
}

pub fn rounded(r: Rect, radius: f64) -> RoundedRect {
    let radius = (radius as f32)
        .min(r.width() / 2.0)
        .min(r.height() / 2.0)
        .max(0.0);
    let c = Size::new(radius, radius);
    RoundedRect::new(r, c, c, c, c)
}

/// Fill a rounded rectangle.
pub fn fill_rounded(s: &gtk::Snapshot, r: Rect, radius: f64, color: &gdk::RGBA) {
    s.push_rounded_clip(&rounded(r, radius));
    s.append_color(color, &r);
    s.pop();
}

/// Outline a rounded rectangle with a `width` line centred on its edge, as
/// cairo's stroke() draws it.
pub fn stroke_rounded(s: &gtk::Snapshot, r: Rect, radius: f64, width: f64, color: &gdk::RGBA) {
    let half = (width / 2.0) as f32;
    let outer = Rect::new(
        r.x() - half,
        r.y() - half,
        r.width() + 2.0 * half,
        r.height() + 2.0 * half,
    );
    let w = width as f32;
    s.append_border(&rounded(outer, radius + width / 2.0), &[w; 4], &[*color; 4]);
}

/// A filled circle.
pub fn dot(s: &gtk::Snapshot, x: f64, y: f64, radius: f64, color: &gdk::RGBA) {
    let r = Rect::new(
        (x - radius) as f32,
        (y - radius) as f32,
        (2.0 * radius) as f32,
        (2.0 * radius) as f32,
    );
    fill_rounded(s, r, radius, color);
}
