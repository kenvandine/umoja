//! A one-child container that can draw its child shifted, so the dock can
//! slide off the screen edge while auto-hiding.
//!
//! It asks for exactly its child's size, so the layer surface keeps its
//! size during the slide (Mir doesn't re-place a layer surface as it
//! shrinks) and the part of the dock pushed past the surface is simply cut
//! off by the screen edge.

use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Slide {
        pub child: RefCell<Option<gtk::Widget>>,
        pub offset: Cell<(f32, f32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Slide {
        const NAME: &'static str = "UmojaDockSlide";
        type Type = super::Slide;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Slide {
        fn dispose(&self) {
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Slide {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            match self.child.borrow().as_ref() {
                Some(c) if c.is_visible() => c.measure(orientation, for_size),
                _ => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.child.borrow().clone() else {
                return;
            };
            let (x, y) = self.offset.get();
            let transform = (x != 0.0 || y != 0.0)
                .then(|| gsk::Transform::new().translate(&graphene::Point::new(x, y)));
            child.allocate(width, height, baseline, transform);
        }
    }
}

glib::wrapper! {
    pub struct Slide(ObjectSubclass<imp::Slide>) @extends gtk::Widget;
}

impl Slide {
    pub fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let s: Self = glib::Object::new();
        child.set_parent(&s);
        s.imp().child.replace(Some(child.clone().upcast()));
        s
    }

    /// Draw the child moved by (x, y) logical pixels.
    pub fn set_offset(&self, x: f32, y: f32) {
        if self.imp().offset.replace((x, y)) != (x, y) {
            self.queue_allocate();
        }
    }
}
