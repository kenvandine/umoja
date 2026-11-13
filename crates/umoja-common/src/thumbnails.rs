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

/// A picture of `texture` scaled to fit `width` x `height` (logical pixels)
/// keeping its aspect ratio, with rounded corners from the `thumbnail` CSS
/// class of the caller's stylesheet.
pub fn picture(texture: &gdk::Texture, width: i32, height: i32) -> gtk4::Picture {
    let picture = gtk4::Picture::for_paintable(texture);
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    let (tw, th) = (texture.width().max(1) as f64, texture.height().max(1) as f64);
    let scale = (width as f64 / tw).min(height as f64 / th);
    picture.set_size_request((tw * scale).round() as i32, (th * scale).round() as i32);
    picture.set_overflow(gtk4::Overflow::Hidden);
    picture
}
