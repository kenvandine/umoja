//! A texture drawn at whatever size the widget is given.
//!
//! `gtk::Picture` asks for its texture's pixel size as natural size, which
//! pushes overview cards and thumbnails past the room they were laid out
//! for. This widget has no size of its own (set one with
//! `set_size_request`) and draws its texture filling it, or covering it
//! with the overflow cropped (for wallpapers).

use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TexPane {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub cover: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TexPane {
        const NAME: &'static str = "UmojaTexPane";
        type Type = super::TexPane;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for TexPane {}

    impl WidgetImpl for TexPane {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            let Some(tex) = self.texture.borrow().clone() else {
                return;
            };
            let bounds = graphene::Rect::new(0.0, 0.0, w, h);
            if !self.cover.get() {
                snapshot.append_texture(&tex, &bounds);
                return;
            }
            let (tw, th) = (tex.width().max(1) as f32, tex.height().max(1) as f32);
            let scale = (w / tw).max(h / th);
            let (dw, dh) = (tw * scale, th * scale);
            snapshot.push_clip(&bounds);
            snapshot.append_texture(&tex, &graphene::Rect::new((w - dw) / 2.0, (h - dh) / 2.0, dw, dh));
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct TexPane(ObjectSubclass<imp::TexPane>) @extends gtk::Widget;
}

impl TexPane {
    pub fn new(texture: Option<&gdk::Texture>, cover: bool, width: i32, height: i32) -> Self {
        let p: Self = glib::Object::new();
        p.imp().texture.replace(texture.cloned());
        p.imp().cover.set(cover);
        p.set_size_request(width, height);
        p
    }
}
