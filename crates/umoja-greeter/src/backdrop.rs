//! Full-screen wallpaper, softly blurred and dimmed, drawn with GSK so it
//! stays sharp-edged and cheap at any scale.

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
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Backdrop {
        const NAME: &'static str = "UmojaBackdrop";
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
                let blur = self.blur.get() as f32;
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
        let texture = path
            .and_then(|p| gdk::Texture::from_filename(p).ok())
            .or_else(|| gdk::Texture::from_filename(FALLBACK_WALLPAPER).ok());
        *self.imp().texture.borrow_mut() = texture;
        self.queue_draw();
    }
}

/// The system default wallpaper, as Ubuntu configures it for new users
/// (`org.gnome.desktop.background picture-uri[-dark]`, set by ubuntu-settings).
pub fn system_wallpaper() -> Option<String> {
    // The greeter always uses the dark variant, which suits the glass card.
    umoja_common::gsettings::wallpaper_path(true).map(|p| p.to_string_lossy().into_owned())
}
