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
    largest_monitor().map(|(w, h, _)| (w, h))
}

/// The largest monitor: device pixels and its scale.
pub fn largest_monitor() -> Option<(i32, i32, f64)> {
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
                s,
            )
        })
        .max_by_key(|(w, h, _)| i64::from(*w) * i64::from(*h))
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

/// [`decode_cover`], blurred once on the CPU (a Gaussian of `sigma` image
/// pixels, as three box blurs) so the lock screen can draw it plainly
/// instead of having GSK blur a full-screen texture on every frame it
/// redraws (its clock ticks every second).
pub fn texture_cover_blurred(path: &Path, need: (i32, i32), sigma: f64) -> Option<gdk::Texture> {
    let p = decode_cover(path, need)?;
    let (w, h, stride) = (p.width() as usize, p.height() as usize, p.rowstride() as usize);
    let channels = p.n_channels() as usize;
    let mut data = p.read_pixel_bytes().to_vec();
    blur(&mut data, w, h, stride, channels, sigma);
    let format = if p.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    let bytes = gtk4::glib::Bytes::from_owned(data);
    Some(gdk::MemoryTexture::new(w as i32, h as i32, format, &bytes, stride).upcast())
}

/// Approximate a Gaussian blur of `sigma` pixels with three box blurs,
/// horizontally then vertically, clamping at the edges.
pub fn blur(data: &mut [u8], w: usize, h: usize, stride: usize, channels: usize, sigma: f64) {
    if sigma < 0.5 || w == 0 || h == 0 {
        return;
    }
    // Box width for three passes: sqrt(12 sigma^2 / 3 + 1).
    let radius = (((4.0 * sigma * sigma + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    let mut line = vec![0u8; w.max(h) * channels];
    for _ in 0..3 {
        for y in 0..h {
            let row = y * stride;
            box_pass(data, row, channels, w, channels, radius, &mut line);
        }
        for x in 0..w {
            box_pass(data, x * channels, stride, h, channels, radius, &mut line);
        }
    }
}

/// One box blur along a line of `len` pixels starting at `start`, `step`
/// bytes apart.
fn box_pass(data: &mut [u8], start: usize, step: usize, len: usize, channels: usize, radius: usize, line: &mut [u8]) {
    for i in 0..len {
        line[i * channels..(i + 1) * channels]
            .copy_from_slice(&data[start + i * step..start + i * step + channels]);
    }
    let window = (2 * radius + 1) as u32;
    for c in 0..channels {
        let at = |i: isize| -> u32 { u32::from(line[(i.clamp(0, len as isize - 1) as usize) * channels + c]) };
        let mut sum: u32 = (-(radius as isize)..=radius as isize).map(at).sum();
        for i in 0..len {
            data[start + i * step + c] = ((sum + window / 2) / window) as u8;
            sum += at(i as isize + radius as isize + 1);
            sum -= at(i as isize - radius as isize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_keeps_flat_colour_and_spreads_a_dot() {
        let (w, h) = (16, 8);
        let mut flat = vec![200u8; w * h * 3];
        blur(&mut flat, w, h, w * 3, 3, 3.0);
        assert!(flat.iter().all(|&v| v == 200));

        let mut dot = vec![0u8; w * h * 3];
        dot[(4 * w + 8) * 3] = 255;
        blur(&mut dot, w, h, w * 3, 3, 2.0);
        let centre = dot[(4 * w + 8) * 3];
        assert!(centre > 0 && centre < 255);
        assert!(dot[(4 * w + 10) * 3] > 0);
        assert_eq!(dot[(4 * w + 8) * 3 + 1], 0, "channels stay apart");
    }

    #[test]
    fn cover_keeps_aspect_and_never_grows() {
        assert_eq!(cover_size((3840, 2160), (2880, 1800)), (3200, 1800));
        assert_eq!(cover_size((3840, 2160), (1920, 1080)), (1920, 1080));
        assert_eq!(cover_size((1920, 1080), (3840, 2160)), (1920, 1080));
        assert_eq!(cover_size((3840, 2160), (400, 100)), (400, 225));
    }
}
