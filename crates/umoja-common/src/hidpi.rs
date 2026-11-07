//! HiDPI helpers shared by the compositor, greeter and DisplayConfig service.
//!
//! The scale rules follow mutter (Ubuntu's GNOME): fractional scales in
//! quarter steps from 100% to 400%, never shrinking the logical screen
//! below 800×480, and a preferred scale chosen from the panel's physical DPI
//! (target ~135 DPI for built-in laptop panels, ~110 for external monitors).

use std::path::Path;

pub const MIN_SCALE: f64 = 1.0;
pub const MAX_SCALE: f64 = 4.0;
pub const SCALE_STEP: f64 = 0.25;
const MIN_LOGICAL_WIDTH: f64 = 800.0;
const MIN_LOGICAL_HEIGHT: f64 = 480.0;
const LAPTOP_TARGET_DPI: f64 = 135.0;
const DESKTOP_TARGET_DPI: f64 = 110.0;

/// A connected panel, as read from the kernel's DRM sysfs + EDID.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// Kernel connector name, e.g. `eDP-1`, `HDMI-A-1` (the names Mir uses).
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
    let mut out = Vec::new();
    let mut s = MIN_SCALE;
    while s <= MAX_SCALE + f64::EPSILON {
        if width as f64 / s >= MIN_LOGICAL_WIDTH && height as f64 / s >= MIN_LOGICAL_HEIGHT {
            out.push(s);
        }
        s += SCALE_STEP;
    }
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

/// Preferred mode and physical size from a raw EDID block.
pub fn parse_edid(edid: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if edid.len() < 128 || edid[0..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return None;
    }
    // First detailed timing descriptor = preferred mode.
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
            let (width_px, height_px, width_mm, height_mm) = parse_edid(&edid)?;
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
        assert_eq!(supported_scales(1024, 768), vec![1.0, 1.25]);
        assert_eq!(supported_scales(640, 480), vec![1.0]);
    }

    #[test]
    fn preferred_scales() {
        // 14" 2880x1800 laptop panel (~243 DPI) -> 175%.
        assert_eq!(preferred_scale(2880, 1800, 302, 189, true), 1.75);
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
}
