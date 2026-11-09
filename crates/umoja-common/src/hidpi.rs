//! HiDPI helpers shared by the compositor, greeter and DisplayConfig service.
//!
//! The scale rules follow mutter (Ubuntu's GNOME): quarter and third steps
//! from 100% to 400% that divide the mode into whole logical pixels, so
//! 2880×1800 offers 166⅔% but not 175%.
//! The logical screen never shrinks below 640×480, and a preferred scale is chosen
//! from the panel's physical DPI (target ~135 DPI for built-in laptop
//! panels, ~110 for external monitors).

use std::path::Path;

pub const MIN_SCALE: f64 = 1.0;
pub const MAX_SCALE: f64 = 4.0;
pub const SCALE_STEP: f64 = 0.25;
const MIN_LOGICAL_WIDTH: f64 = 640.0;
const MIN_LOGICAL_HEIGHT: f64 = 480.0;
const LAPTOP_TARGET_DPI: f64 = 135.0;
const DESKTOP_TARGET_DPI: f64 = 110.0;

/// A connected panel, as read from the kernel's DRM sysfs + EDID.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// Kernel connector name, e.g. `eDP-1`, `HDMI-A-1`, `DP-1` (Mir calls
    /// that one `DisplayPort-1`).
    pub connector: String,
    pub width_px: u32,
    pub height_px: u32,
    pub width_mm: u32,
    pub height_mm: u32,
}

impl Panel {
    pub fn is_builtin(&self) -> bool {
        is_builtin_connector(&self.connector)
    }

    pub fn preferred_scale(&self) -> f64 {
        preferred_scale(
            self.width_px,
            self.height_px,
            self.width_mm,
            self.height_mm,
            self.is_builtin(),
        )
    }
}

pub fn is_builtin_connector(name: &str) -> bool {
    ["eDP", "LVDS", "DSI"].iter().any(|p| name.starts_with(p))
}

/// Scales offered in Settings › Displays for a mode.
pub fn supported_scales(width: u32, height: u32) -> Vec<f64> {
    // Quarter and third steps that split the mode into whole logical
    // pixels, as mutter offers them (2880x1800: 125, 133, 150, 167, 200%...).
    let mut out: Vec<f64> = Vec::new();
    for q in [4u64, 3] {
        for p in q..=q * MAX_SCALE as u64 {
            let s = p as f64 / q as f64;
            let whole = (width as u64 * q) % p == 0 && (height as u64 * q) % p == 0;
            if whole
                && width as f64 / s >= MIN_LOGICAL_WIDTH
                && height as f64 / s >= MIN_LOGICAL_HEIGHT
                && !out.iter().any(|o| (s - o).abs() < 1e-9)
            {
                out.push(s);
            }
        }
    }
    out.sort_by(f64::total_cmp);
    if out.is_empty() {
        out.push(MIN_SCALE);
    }
    out
}

/// The scale a fresh session should use for this panel.
pub fn preferred_scale(
    width: u32,
    height: u32,
    width_mm: u32,
    height_mm: u32,
    builtin: bool,
) -> f64 {
    // Projectors and bogus EDIDs report 0 or tiny sizes; stay at 100%.
    if width_mm < 100 || height_mm < 60 {
        return MIN_SCALE;
    }
    let dpi_x = width as f64 / (width_mm as f64 / 25.4);
    let dpi_y = height as f64 / (height_mm as f64 / 25.4);
    let dpi = (dpi_x + dpi_y) / 2.0;
    let target = if builtin {
        LAPTOP_TARGET_DPI
    } else {
        DESKTOP_TARGET_DPI
    };
    let ideal = dpi / target;
    supported_scales(width, height)
        .into_iter()
        .min_by(|a, b| (a - ideal).abs().total_cmp(&(b - ideal).abs()))
        .unwrap_or(MIN_SCALE)
}

/// Preferred mode and physical size from a raw EDID block, using its first
/// detailed timing descriptor.
pub fn parse_edid(edid: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if !is_edid(edid) {
        return None;
    }
    let d = &edid[54..72];
    let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
    if pixel_clock == 0 {
        return None;
    }
    let width = d[2] as u32 | ((d[4] as u32 >> 4) << 8);
    let height = d[5] as u32 | ((d[7] as u32 >> 4) << 8);
    let width_mm = d[12] as u32 | ((d[14] as u32 >> 4) << 8);
    let height_mm = d[13] as u32 | ((d[14] as u32 & 0x0f) << 8);
    Some((width, height, width_mm, height_mm))
}

/// Physical size in mm: from the first detailed timing descriptor, or else
/// the base block's size in cm. Newer panels (e.g. many OLED laptops) put
/// their timings in a DisplayID extension and leave the descriptor empty.
pub fn edid_size_mm(edid: &[u8]) -> Option<(u32, u32)> {
    if let Some((_, _, w, h)) = parse_edid(edid) {
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    if !is_edid(edid) || edid[21] == 0 || edid[22] == 0 {
        return None;
    }
    Some((edid[21] as u32 * 10, edid[22] as u32 * 10))
}

fn is_edid(edid: &[u8]) -> bool {
    edid.len() >= 128 && edid[0..8] == [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]
}

/// "2880x1800" (the first line of a connector's `modes` file).
fn parse_mode(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.lines().next()?.trim().split_once('x')?;
    // Interlaced modes are listed as e.g. "1920x1080i".
    let h = h.trim_end_matches(|c: char| !c.is_ascii_digit());
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Every connected DRM connector with a readable EDID.
pub fn connected_panels() -> Vec<Panel> {
    scan(Path::new("/sys/class/drm"))
}

fn scan(root: &Path) -> Vec<Panel> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut panels: Vec<Panel> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // "card1-eDP-1" -> "eDP-1"
            let connector = name.strip_prefix("card")?.split_once('-')?.1.to_string();
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            if status.trim() != "connected" {
                return None;
            }
            let edid = std::fs::read(e.path().join("edid")).ok()?;
            // The kernel lists the preferred mode first; that also covers
            // modes that only a DisplayID extension describes.
            let (width_px, height_px) = std::fs::read_to_string(e.path().join("modes"))
                .ok()
                .and_then(|m| parse_mode(&m))
                .or_else(|| parse_edid(&edid).map(|(w, h, _, _)| (w, h)))?;
            let (width_mm, height_mm) = edid_size_mm(&edid).unwrap_or((0, 0));
            Some(Panel {
                connector,
                width_px,
                height_px,
                width_mm,
                height_mm,
            })
        })
        .collect();
    panels.sort_by(|a, b| {
        b.is_builtin()
            .cmp(&a.is_builtin())
            .then(a.connector.cmp(&b.connector))
    });
    panels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_follow_mutter_rules() {
        let s = supported_scales(1920, 1080);
        assert_eq!(s.first(), Some(&1.0));
        assert!(s.contains(&2.0));
        assert!(!s.contains(&2.5)); // 1920/2.5 = 768 < 800
        assert_eq!(supported_scales(640, 480), vec![1.0]);
        // Exactly what mutter offers for a 2880x1800 panel.
        let s: Vec<String> = supported_scales(2880, 1800)
            .iter()
            .map(|s| format!("{s:.3}"))
            .collect();
        assert_eq!(
            s,
            ["1.000", "1.250", "1.333", "1.500", "1.667", "2.000", "2.250", "2.500", "2.667", "3.000", "3.333", "3.750"]
        );
        // Every offered scale gives whole logical pixels.
        for sc in supported_scales(1024, 768) {
            assert_eq!((1024.0 / sc).fract(), 0.0);
        }
    }

    #[test]
    fn preferred_scales() {
        // 14" 2880x1800 laptop panel (~243 DPI) -> 166⅔%, as mutter picks.
        assert!((preferred_scale(2880, 1800, 300, 190, true) - 5.0 / 3.0).abs() < 1e-9);
        // 13.3" 1920x1080 laptop (~166 DPI) -> 125%.
        assert_eq!(preferred_scale(1920, 1080, 294, 165, true), 1.25);
        // 27" 4K desktop monitor (~163 DPI) -> 150%.
        assert_eq!(preferred_scale(3840, 2160, 597, 336, false), 1.5);
        // 24" 1080p -> 100%.
        assert_eq!(preferred_scale(1920, 1080, 531, 299, false), 1.0);
        // Projector with no size.
        assert_eq!(preferred_scale(3840, 2160, 0, 0, false), 1.0);
    }

    #[test]
    fn edid_preferred_mode() {
        let mut edid = vec![0u8; 128];
        edid[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        let d = &mut edid[54..72];
        d[0] = 0x01; // non-zero pixel clock
        d[2] = (2880 & 0xff) as u8;
        d[4] = ((2880 >> 8) << 4) as u8;
        d[5] = (1800 & 0xff) as u8;
        d[7] = ((1800 >> 8) << 4) as u8;
        d[12] = (302 & 0xff) as u8;
        d[13] = (189 & 0xff) as u8;
        d[14] = 0x10; // upper nibbles: width 302 = 0x12e, height 189 = 0x0bd
        assert_eq!(parse_edid(&edid), Some((2880, 1800, 302, 189)));
        assert_eq!(parse_edid(&edid[..64]), None);
    }

    #[test]
    fn edid_without_timing_descriptor() {
        // ThinkPad P14s Gen 6 OLED: no DTD, size only in the base block.
        let mut edid = vec![0u8; 128];
        edid[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        edid[21] = 0x1e;
        edid[22] = 0x13;
        assert_eq!(parse_edid(&edid), None);
        assert_eq!(edid_size_mm(&edid), Some((300, 190)));
        assert_eq!(parse_mode("2880x1800\n2880x1800\n1920x1200\n"), Some((2880, 1800)));
        assert_eq!(parse_mode("1920x1080i\n"), Some((1920, 1080)));
    }
}
