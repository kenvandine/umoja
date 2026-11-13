//! Full-screen wallpaper, blurred and dimmed, for the lock screen. Same
//! drawing as umoja-greeter's backdrop so locking looks like the login screen.

use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};

/// Ubuntu's default wallpaper, used when nothing better is configured.
pub use umoja_common::gsettings::FALLBACK_WALLPAPER;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Backdrop {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub blur: Cell<f64>,
        /// `texture` is already blurred.
        pub preblurred: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Backdrop {
        const NAME: &'static str = "UmojaLockBackdrop";
        type Type = super::Backdrop;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Backdrop {}

    impl WidgetImpl for Backdrop {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            let bounds = graphene::Rect::new(0.0, 0.0, w, h);
            snapshot.push_clip(&bounds);
            if let Some(tex) = self.texture.borrow().as_ref() {
                // "Cover" fit, overscanned so blurred edges never show.
                let (tw, th) = (tex.width() as f32, tex.height() as f32);
                let blur = if self.preblurred.get() {
                    0.0
                } else {
                    self.blur.get() as f32
                };
                let scale = (w / tw).max(h / th);
                let (dw, dh) = (tw * scale + blur * 4.0, th * scale + blur * 4.0);
                let dest = graphene::Rect::new((w - dw) / 2.0, (h - dh) / 2.0, dw, dh);
                if blur > 0.0 {
                    snapshot.push_blur(blur as f64);
                }
                snapshot.append_texture(tex, &dest);
                if blur > 0.0 {
                    snapshot.pop();
                }
            } else {
                snapshot.append_color(&gdk::RGBA::new(0.17, 0.0, 0.12, 1.0), &bounds);
            }
            // Gentle vignette so white text reads on any wallpaper.
            snapshot.append_linear_gradient(
                &bounds,
                &graphene::Point::new(0.0, 0.0),
                &graphene::Point::new(0.0, h),
                &[
                    gtk::gsk::ColorStop::new(0.0, gdk::RGBA::new(0.0, 0.0, 0.0, 0.25)),
                    gtk::gsk::ColorStop::new(0.55, gdk::RGBA::new(0.0, 0.0, 0.0, 0.35)),
                    gtk::gsk::ColorStop::new(1.0, gdk::RGBA::new(0.0, 0.0, 0.0, 0.6)),
                ],
            );
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct Backdrop(ObjectSubclass<imp::Backdrop>) @extends gtk::Widget;
}

impl Backdrop {
    pub fn new(blur: f64) -> Self {
        let b: Self = glib::Object::new();
        b.imp().blur.set(blur);
        b.set_hexpand(true);
        b.set_vexpand(true);
        b
    }

    pub fn set_wallpaper(&self, path: Option<&str>) {
        // It is shown heavily blurred, so a quarter of the screen's size
        // looks the same as the full image at a sixteenth of the memory. It
        // is blurred once here, not by GSK on every frame (the clock ticks
        // every second): GSK's blur radius is two sigmas, in logical pixels,
        // and the image has scale/4 pixels per logical pixel.
        let (w, h, scale) = umoja_common::wallpaper::largest_monitor().unwrap_or((3840, 2160, 2.0));
        let need = ((w / 4).max(1), (h / 4).max(1));
        let sigma = self.imp().blur.get() / 2.0 * scale / 4.0;
        let load = |p: &str| {
            umoja_common::wallpaper::texture_cover_blurred(std::path::Path::new(p), need, sigma)
        };
        let texture = path.and_then(load).or_else(|| load(FALLBACK_WALLPAPER));
        self.imp().preblurred.set(texture.is_some());
        *self.imp().texture.borrow_mut() = texture;
        self.queue_draw();
    }
}
