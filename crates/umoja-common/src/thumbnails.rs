//! Live window thumbnails from umoja-mir (org.umoja.Compositor.CaptureWindow),
//! for the Alt+Tab switcher, the overview and the dock's window menu.
//!
//! umoja-mir renders the one window on its own, so covered and minimized
//! windows and windows on other workspaces get a real picture too. It needs
//! Umoja's patched Mir (packaging/mir 3018); without it every capture comes
//! back empty and callers keep their fallback (an icon, a crop of a screen
//! capture).

use crate::dbus;
use gtk4::{gdk, gio, glib, prelude::*};

/// Small thumbnails (Alt+Tab, dock menu): the longer side is 512 pixels.
pub const SMALL: u32 = 512;
/// Large thumbnails (the overview): 1024 pixels.
pub const LARGE: u32 = 1024;

/// Capture window `id` with its longer side about `size` pixels.
pub async fn capture(id: u64, size: u32) -> Option<gdk::Texture> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    let reply = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            "CaptureWindow",
            Some(&(id, size).to_variant()),
            Some(glib::VariantTy::new("(uuay)").ok()?),
            gio::DBusCallFlags::NONE,
            3000,
        )
        .await
        .ok()?;
    let w = reply.child_value(0).get::<u32>()?;
    let h = reply.child_value(1).get::<u32>()?;
    let bytes = reply.child_value(2).data_as_bytes();
    if w == 0 || h == 0 || bytes.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    Some(
        gdk::MemoryTexture::new(
            w as i32,
            h as i32,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            w as usize * 4,
        )
        .upcast(),
    )
}

/// A thumbnail of `texture` scaled to fit `width` x `height` (logical
/// pixels) keeping its aspect ratio. Unlike `gtk::Picture`, whose natural
/// size is the texture's pixel size (512 or 1024 here, which blew the
/// Alt+Tab popup past the screen edges), it asks for exactly the fitted
/// size. Rounded corners come from the caller's CSS (overflow is hidden).
pub fn picture(texture: &gdk::Texture, width: i32, height: i32) -> Pane {
    let (tw, th) = (
        texture.width().max(1) as f64,
        texture.height().max(1) as f64,
    );
    let scale = (width as f64 / tw).min(height as f64 / th);
    let pane: Pane = glib::Object::new();
    pane.imp().texture.replace(Some(texture.clone()));
    pane.set_size_request(
        ((tw * scale).round() as i32).max(1),
        ((th * scale).round() as i32).max(1),
    );
    pane.set_overflow(gtk4::Overflow::Hidden);
    pane.set_halign(gtk4::Align::Center);
    pane.set_valign(gtk4::Align::Center);
    pane
}

mod imp {
    use gtk4::subclass::prelude::*;
    use gtk4::{gdk, glib, graphene, prelude::*};
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Pane {
        pub texture: RefCell<Option<gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Pane {
        const NAME: &'static str = "UmojaThumbnailPane";
        type Type = super::Pane;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for Pane {}

    impl WidgetImpl for Pane {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let widget = self.obj();
            if let Some(tex) = self.texture.borrow().as_ref() {
                let bounds =
                    graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32);
                snapshot.append_scaled_texture(tex, gtk4::gsk::ScalingFilter::Trilinear, &bounds);
            }
        }
    }
}

glib::wrapper! {
    /// A texture drawn at the size the widget is given, with no natural size
    /// of its own beyond its size request.
    pub struct Pane(ObjectSubclass<imp::Pane>) @extends gtk4::Widget;
}

use gtk4::subclass::prelude::ObjectSubclassIsExt;
