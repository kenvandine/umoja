//! Wallpaper images, decoded no bigger than the screens need.
//!
//! The Ubuntu wallpapers are 3840x2160 or larger; decoded at full size each
//! copy is 25 to 33 MB of RAM, plus the same again on the GPU, which on an
//! APU is system RAM too. Shell surfaces only ever show them at monitor size
//! (the desktop, the lock screen) or much smaller (the overview's cards), so
//! they are scaled down while decoding.

use gtk4::gdk;
use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::prelude::*;
use std::path::Path;

/// The largest monitor, in device pixels: what a full-screen wallpaper
/// needs. `None` before any monitor is known.
pub fn largest_monitor_pixels() -> Option<(i32, i32)> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
        .map(|m| {
            let g = m.geometry();
            let s = m.scale();
            (
                (f64::from(g.width()) * s).ceil() as i32,
                (f64::from(g.height()) * s).ceil() as i32,
            )
        })
        .max_by_key(|(w, h)| i64::from(*w) * i64::from(*h))
}

/// The size an image of `image` pixels is scaled to so that it still covers
/// a `need` box (never scaled up).
pub fn cover_size(image: (i32, i32), need: (i32, i32)) -> (i32, i32) {
    let (iw, ih) = image;
    let (nw, nh) = need;
    if iw <= 0 || ih <= 0 || nw <= 0 || nh <= 0 {
        return image;
    }
    let scale = (f64::from(nw) / f64::from(iw))
        .max(f64::from(nh) / f64::from(ih))
        .min(1.0);
    (
        ((f64::from(iw) * scale).ceil() as i32).max(1),
        ((f64::from(ih) * scale).ceil() as i32).max(1),
    )
}

/// Decode `path` just big enough to cover a `need` box. Safe off the main
/// thread (only gdk-pixbuf).
pub fn decode_cover(path: &Path, need: (i32, i32)) -> Option<Pixbuf> {
    let (w, h) = match Pixbuf::file_info(path) {
        Some((_, w, h)) => cover_size((w, h), need),
        None => need,
    };
    Pixbuf::from_file_at_scale(path, w, h, true)
        .map_err(|e| tracing::warn!(path = %path.display(), error = %e, "could not load the wallpaper"))
        .ok()
}

/// Decoded pixels that can cross threads (a Pixbuf can't).
pub struct Pixels {
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
    bytes: gtk4::glib::Bytes,
}

impl Pixels {
    pub fn texture(&self) -> gdk::Texture {
        let format = if self.alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        gdk::MemoryTexture::new(self.width, self.height, format, &self.bytes, self.stride).upcast()
    }
}

/// [`decode_cover`] for a worker thread (`gio::spawn_blocking`).
pub fn decode_cover_pixels(path: &Path, need: (i32, i32)) -> Option<Pixels> {
    let p = decode_cover(path, need)?;
    Some(Pixels {
        width: p.width(),
        height: p.height(),
        stride: p.rowstride() as usize,
        alpha: p.has_alpha(),
        bytes: p.read_pixel_bytes(),
    })
}

/// [`decode_cover`] as a texture. Main thread.
pub fn texture_cover(path: &Path, need: (i32, i32)) -> Option<gdk::Texture> {
    #[allow(deprecated)]
    decode_cover(path, need).map(|p| gdk::Texture::for_pixbuf(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_keeps_aspect_and_never_grows() {
        assert_eq!(cover_size((3840, 2160), (2880, 1800)), (3200, 1800));
        assert_eq!(cover_size((3840, 2160), (1920, 1080)), (1920, 1080));
        assert_eq!(cover_size((1920, 1080), (3840, 2160)), (1920, 1080));
        assert_eq!(cover_size((3840, 2160), (400, 100)), (400, 225));
    }
}
