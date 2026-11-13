//! Multi-monitor support.
//!
//! * `neighbor` picks the monitor in a direction for `move-to-monitor-*`.
//! * `import_gnome_monitors` converts GNOME's `~/.config/monitors.xml`
//!   (written by gnome-control-center's Displays panel) into Mir's
//!   `~/.config/umoja-mir.display`, so a user switching from Ubuntu's GNOME
//!   session keeps their arrangement, scale, rotation and refresh rate.
//!   Mir watches that file and reapplies it live.
//!
//! TODO(phase 4): implement `org.gnome.Mutter.DisplayConfig` so the Displays
//! panel can change layouts live without the monitors.xml round trip.

use crate::tiling::Rect;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Index of the output adjacent to `from` in `dir`, preferring the one with
/// the most overlap on the perpendicular axis, then the nearest.
pub fn neighbor(dir: Direction, from: Rect, outputs: &[Rect]) -> Option<usize> {
    let cx = from.x + from.w / 2;
    let cy = from.y + from.h / 2;
    outputs
        .iter()
        .enumerate()
        .filter(|(_, o)| **o != from)
        .filter(|(_, o)| match dir {
            Direction::Left => o.x + o.w / 2 < cx,
            Direction::Right => o.x + o.w / 2 > cx,
            Direction::Up => o.y + o.h / 2 < cy,
            Direction::Down => o.y + o.h / 2 > cy,
        })
        .min_by_key(|(_, o)| {
            let overlap = match dir {
                Direction::Left | Direction::Right => {
                    (from.bottom().min(o.bottom()) - from.y.max(o.y)).max(0)
                }
                Direction::Up | Direction::Down => {
                    (from.right().min(o.right()) - from.x.max(o.x)).max(0)
                }
            };
            let distance = ((o.x + o.w / 2 - cx).abs() + (o.y + o.h / 2 - cy).abs()) as i64;
            (-(overlap as i64), distance)
        })
        .map(|(i, _)| i)
}

/// Keep a window at the same relative spot when it moves to another output.
pub fn translate(window: Rect, from: Rect, to: Rect) -> Rect {
    let rel_x = (window.x - from.x) as f64 / from.w.max(1) as f64;
    let rel_y = (window.y - from.y) as f64 / from.h.max(1) as f64;
    let w = window.w.min(to.w);
    let h = window.h.min(to.h);
    let x = (to.x + (rel_x * to.w as f64) as i32).clamp(to.x, to.right() - w);
    let y = (to.y + (rel_y * to.h as f64) as i32).clamp(to.y, to.bottom() - h);
    Rect { x, y, w, h }
}

#[derive(Debug, Default, PartialEq, Clone)]
pub(crate) struct MonitorConfig {
    pub connector: String,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
    pub rate: f64,
    pub rotation: &'static str,
    pub primary: bool,
    pub enabled: bool,
}

/// Mutter names HDMI-A connectors "HDMI-n"; Mir uses the kernel's "HDMI-A-n".
pub(crate) fn mir_connector(name: &str) -> String {
    match name
        .strip_prefix("HDMI-")
        .filter(|rest| rest.chars().all(|c| c.is_ascii_digit()))
    {
        Some(n) => format!("HDMI-A-{n}"),
        None => name.to_string(),
    }
}

fn mir_orientation(rotation: &str) -> &'static str {
    match rotation {
        "left" => "left",
        "right" => "right",
        "upside_down" => "inverted",
        _ => "normal",
    }
}

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, path: &[&str]) -> Option<&'a str> {
    let mut n = node;
    for name in path {
        n = n.children().find(|c| c.has_tag_name(*name))?;
    }
    n.text().map(str::trim)
}

/// Parse monitors.xml; returns one layout per `<configuration>`.
fn parse_monitors_xml(xml: &str) -> Option<Vec<Vec<MonitorConfig>>> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let mut layouts = Vec::new();
    for conf in doc
        .root_element()
        .children()
        .filter(|c| c.has_tag_name("configuration"))
    {
        let mut outputs = Vec::new();
        for lm in conf.children().filter(|c| c.has_tag_name("logicalmonitor")) {
            let x = child_text(lm, &["x"])
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let y = child_text(lm, &["y"])
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let scale = child_text(lm, &["scale"])
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0);
            let primary = child_text(lm, &["primary"]) == Some("yes");
            let rotation =
                mir_orientation(child_text(lm, &["transform", "rotation"]).unwrap_or("normal"));
            for mon in lm.children().filter(|c| c.has_tag_name("monitor")) {
                let Some(connector) = child_text(mon, &["monitorspec", "connector"]) else {
                    continue;
                };
                outputs.push(MonitorConfig {
                    connector: mir_connector(connector),
                    x,
                    y,
                    scale,
                    width: child_text(mon, &["mode", "width"])
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    height: child_text(mon, &["mode", "height"])
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    rate: child_text(mon, &["mode", "rate"])
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(60.0),
                    rotation,
                    primary,
                    enabled: true,
                });
            }
        }
        for disabled in conf.children().filter(|c| c.has_tag_name("disabled")) {
            for spec in disabled
                .children()
                .filter(|c| c.has_tag_name("monitorspec"))
            {
                if let Some(connector) = child_text(spec, &["connector"]) {
                    outputs.push(MonitorConfig {
                        connector: mir_connector(connector),
                        enabled: false,
                        ..Default::default()
                    });
                }
            }
        }
        if !outputs.is_empty() {
            layouts.push(outputs);
        }
    }
    Some(layouts)
}

/// The primary monitor GNOME stored in `~/.config/monitors.xml` for this
/// set of connected outputs (Mir names), as mutter picks it on hotplug.
pub(crate) fn stored_primary(connected: &[String]) -> Option<String> {
    let xml = std::fs::read_to_string(config_dir().join("monitors.xml")).ok()?;
    primary_in_layouts(&parse_monitors_xml(&xml)?, connected)
}

fn primary_in_layouts(layouts: &[Vec<MonitorConfig>], connected: &[String]) -> Option<String> {
    let mut want: Vec<&str> = connected.iter().map(String::as_str).collect();
    want.sort_unstable();
    layouts.iter().find_map(|layout| {
        let mut have: Vec<&str> = layout
            .iter()
            .filter(|o| o.enabled)
            .map(|o| o.connector.as_str())
            .collect();
        have.sort_unstable();
        if have != want {
            return None;
        }
        layout
            .iter()
            .find(|o| o.enabled && o.primary)
            .map(|o| o.connector.clone())
    })
}

fn layout_name(outputs: &[MonitorConfig]) -> String {
    let mut names: Vec<_> = outputs.iter().map(|o| o.connector.as_str()).collect();
    names.sort_unstable();
    names.join("+")
}

/// Render Mir's `.display` YAML. The last GNOME configuration (the most
/// recently saved one) also becomes `default`.
pub(crate) fn render_display_yaml(layouts: &[Vec<MonitorConfig>]) -> String {
    let mut y = String::from("# Generated by umoja-mir from ~/.config/monitors.xml\nlayouts:\n");
    let mut emit = |name: &str, outputs: &[MonitorConfig]| {
        let _ = writeln!(y, "  {name}:\n    cards:\n    - card-id: 0");
        for o in outputs {
            let _ = writeln!(y, "      {}:", o.connector);
            if !o.enabled {
                let _ = writeln!(y, "        state: disabled");
                continue;
            }
            let _ = writeln!(y, "        state: enabled");
            if o.width > 0 && o.height > 0 {
                let _ = writeln!(y, "        mode: {}x{}@{:.1}", o.width, o.height, o.rate);
            }
            let _ = writeln!(y, "        position: [{}, {}]", o.x, o.y);
            let _ = writeln!(y, "        orientation: {}", o.rotation);
            let _ = writeln!(y, "        scale: {}", o.scale);
            let _ = writeln!(y, "        group: 0");
        }
    };
    for layout in layouts {
        emit(&format!("'{}'", layout_name(layout)), layout);
    }
    if let Some(last) = layouts.last() {
        emit("default", last);
    }
    y
}

pub(crate) fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

fn newer(a: &Path, b: &Path) -> bool {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (m(a), m(b)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Mutter's connector name for a kernel one (inverse of `mir_connector`).
pub(crate) fn gnome_connector(name: &str) -> String {
    match name.strip_prefix("HDMI-A-") {
        Some(n) => format!("HDMI-{n}"),
        None => name.to_string(),
    }
}

/// Render GNOME's monitors.xml for one configuration, so a layout chosen in
/// Settings while running Umoja also applies when logging into GNOME.
pub(crate) fn render_monitors_xml(outputs: &[MonitorConfig]) -> String {
    let mut x = String::from("<monitors version=\"2\">\n  <configuration>\n");
    for o in outputs.iter().filter(|o| o.enabled) {
        let rotation = match o.rotation {
            "left" => "left",
            "right" => "right",
            "inverted" => "upside_down",
            _ => "normal",
        };
        let _ = write!(
            x,
            "    <logicalmonitor>\n      <x>{}</x>\n      <y>{}</y>\n      <scale>{}</scale>\n{}",
            o.x,
            o.y,
            o.scale,
            if o.primary {
                "      <primary>yes</primary>\n"
            } else {
                ""
            }
        );
        if rotation != "normal" {
            let _ = write!(
                x,
                "      <transform>\n        <rotation>{rotation}</rotation>\n        <flipped>no</flipped>\n      </transform>\n"
            );
        }
        let _ = write!(
            x,
            "      <monitor>\n        <monitorspec>\n          <connector>{}</connector>\n          <vendor>unknown</vendor>\n          <product>unknown</product>\n          <serial>unknown</serial>\n        </monitorspec>\n        <mode>\n          <width>{}</width>\n          <height>{}</height>\n          <rate>{:.3}</rate>\n        </mode>\n      </monitor>\n    </logicalmonitor>\n",
            gnome_connector(&o.connector),
            o.width,
            o.height,
            o.rate
        );
    }
    x.push_str("  </configuration>\n</monitors>\n");
    x
}

/// Mir's port name for a kernel connector (`/sys/class/drm/cardN-<name>`):
/// the kernel says "DP-1", Mir (and its .display files) "DisplayPort-1".
/// Only right for the first card: Mir numbers ports per card and type and
/// adds the card index after the first ("DisplayPort-1-1").
pub(crate) fn mir_port(kernel: &str) -> String {
    let Some((kind, rest)) = kernel.split_once('-') else {
        return kernel.to_string();
    };
    let kind = match kind {
        "DP" => "DisplayPort",
        "Unknown" => "unknown",
        "SVIDEO" => "S-video",
        "DIN" => "9-pin",
        k => k,
    };
    format!("{kind}-{rest}")
}

/// Seed per-panel HiDPI scales for a first session (no GNOME or Umoja
/// layout yet), so a 4K laptop doesn't start at an unreadable 100%.
pub(crate) fn default_hidpi_layout() -> Option<String> {
    seed_layout(&umoja_common::hidpi::connected_panels())
}

fn seed_layout(panels: &[umoja_common::hidpi::Panel]) -> Option<String> {
    if panels.iter().all(|p| p.preferred_scale() == 1.0) {
        return None;
    }
    let mut y = String::from(
        "# Generated by umoja-mir from panel DPI (first run)\nlayouts:\n  default:\n    cards:\n    - card-id: 0\n",
    );
    for p in panels {
        let _ = writeln!(
            y,
            "      {}:\n        state: enabled\n        scale: {}",
            mir_port(&p.connector),
            p.preferred_scale()
        );
    }
    Some(y)
}

/// Import GNOME's monitor layout if it changed since the last import, or
/// seed HiDPI defaults when there is no layout at all.
pub fn import_gnome_monitors(display_file_name: &str) {
    let dir = config_dir();
    let gnome = dir.join("monitors.xml");
    let mir = dir.join(display_file_name);
    if !gnome.exists() && !mir.exists() {
        if let Some(yaml) = default_hidpi_layout() {
            let _ = std::fs::create_dir_all(&dir);
            if std::fs::write(&mir, yaml).is_ok() {
                tracing::info!("seeded HiDPI display scales");
            }
        }
        return;
    }
    if !newer(&gnome, &mir) {
        return;
    }
    let Ok(xml) = std::fs::read_to_string(&gnome) else {
        return;
    };
    match parse_monitors_xml(&xml) {
        Some(layouts) if !layouts.is_empty() => {
            if let Err(e) = std::fs::write(&mir, render_display_yaml(&layouts)) {
                tracing::warn!(error = %e, "could not write display config");
            } else {
                tracing::info!(layouts = layouts.len(), path = %mir.display(), "imported GNOME monitor layout");
            }
        }
        _ => tracing::warn!("could not parse monitors.xml"),
    }
}

#[cfg(test)]
mod tests {

    fn panel(connector: &str, w: u32, h: u32, wmm: u32, hmm: u32) -> umoja_common::hidpi::Panel {
        umoja_common::hidpi::Panel {
            connector: connector.into(),
            width_px: w,
            height_px: h,
            width_mm: wmm,
            height_mm: hmm,
        }
    }

    #[test]
    fn kernel_connectors_get_mir_names() {
        assert_eq!(mir_port("DP-5"), "DisplayPort-5");
        assert_eq!(mir_port("DP-1-1"), "DisplayPort-1-1");
        assert_eq!(mir_port("eDP-1"), "eDP-1");
        assert_eq!(mir_port("HDMI-A-1"), "HDMI-A-1");
    }

    #[test]
    fn hidpi_seed_uses_mir_port_names() {
        let hidpi = [panel("eDP-1", 2880, 1800, 302, 189), panel("DP-1", 1920, 1080, 527, 296)];
        let y = seed_layout(&hidpi).unwrap();
        assert!(y.contains("      eDP-1:\n"));
        assert!(y.contains("      DisplayPort-1:\n        state: enabled\n        scale: 1\n"));
        // All at 100%: leave it to Mir.
        assert!(seed_layout(&[panel("eDP-1", 1920, 1200, 344, 215)]).is_none());
    }

    use super::*;

    const XML: &str = r#"<monitors version="2">
  <configuration>
    <logicalmonitor>
      <x>0</x><y>0</y><scale>1.5</scale><primary>yes</primary>
      <monitor>
        <monitorspec><connector>eDP-1</connector><vendor>BOE</vendor><product>x</product><serial>1</serial></monitorspec>
        <mode><width>2880</width><height>1800</height><rate>120.000</rate></mode>
      </monitor>
    </logicalmonitor>
    <logicalmonitor>
      <x>1920</x><y>0</y><scale>1</scale>
      <transform><rotation>left</rotation><flipped>no</flipped></transform>
      <monitor>
        <monitorspec><connector>HDMI-1</connector><vendor>DEL</vendor><product>y</product><serial>2</serial></monitorspec>
        <mode><width>3840</width><height>2160</height><rate>60.000</rate></mode>
      </monitor>
    </logicalmonitor>
    <disabled><monitorspec><connector>DP-2</connector><vendor>a</vendor><product>b</product><serial>c</serial></monitorspec></disabled>
  </configuration>
</monitors>"#;

    #[test]
    fn stored_primary_matches_the_connected_set() {
        let xml = r#"<monitors version="2">
  <configuration>
    <logicalmonitor><x>0</x><y>0</y><scale>1</scale><primary>yes</primary>
      <monitor><monitorspec><connector>eDP-1</connector><vendor>a</vendor><product>b</product><serial>c</serial></monitorspec>
        <mode><width>1920</width><height>1200</height><rate>60</rate></mode></monitor></logicalmonitor>
  </configuration>
  <configuration>
    <logicalmonitor><x>0</x><y>0</y><scale>1</scale>
      <monitor><monitorspec><connector>eDP-1</connector><vendor>a</vendor><product>b</product><serial>c</serial></monitorspec>
        <mode><width>1920</width><height>1200</height><rate>60</rate></mode></monitor></logicalmonitor>
    <logicalmonitor><x>1920</x><y>0</y><scale>1</scale><primary>yes</primary>
      <monitor><monitorspec><connector>HDMI-1</connector><vendor>a</vendor><product>b</product><serial>c</serial></monitorspec>
        <mode><width>2560</width><height>1440</height><rate>60</rate></mode></monitor></logicalmonitor>
  </configuration>
</monitors>"#;
        let layouts = parse_monitors_xml(xml).unwrap();
        let both = ["HDMI-A-1".to_string(), "eDP-1".to_string()];
        assert_eq!(primary_in_layouts(&layouts, &both).as_deref(), Some("HDMI-A-1"));
        let laptop = ["eDP-1".to_string()];
        assert_eq!(primary_in_layouts(&layouts, &laptop).as_deref(), Some("eDP-1"));
        let other = ["DP-2".to_string()];
        assert_eq!(primary_in_layouts(&layouts, &other), None);
    }

    #[test]
    fn parses_gnome_layout() {
        let layouts = parse_monitors_xml(XML).unwrap();
        assert_eq!(layouts.len(), 1);
        let l = &layouts[0];
        assert_eq!(l[0].connector, "eDP-1");
        assert_eq!(l[0].scale, 1.5);
        assert!(l[0].primary);
        assert_eq!(l[1].connector, "HDMI-A-1");
        assert_eq!(l[1].rotation, "left");
        assert!(!l[2].enabled);
        let yaml = render_display_yaml(&layouts);
        assert!(yaml.contains("  default:"));
        assert!(yaml.contains("mode: 3840x2160@60.0"));
        assert!(yaml.contains("DP-2:\n        state: disabled"));
    }

    #[test]
    fn neighbors() {
        let a = Rect::new(0, 0, 1920, 1080);
        let b = Rect::new(1920, 0, 2560, 1440);
        let c = Rect::new(0, 1080, 1920, 1080);
        let outs = [a, b, c];
        assert_eq!(neighbor(Direction::Right, a, &outs), Some(1));
        assert_eq!(neighbor(Direction::Left, b, &outs), Some(0));
        assert_eq!(neighbor(Direction::Down, a, &outs), Some(2));
        assert_eq!(neighbor(Direction::Left, a, &outs), None);
    }

    #[test]
    fn translate_keeps_relative_position() {
        let from = Rect::new(0, 0, 1000, 1000);
        let to = Rect::new(1000, 0, 2000, 2000);
        let w = Rect::new(500, 250, 400, 300);
        assert_eq!(translate(w, from, to), Rect::new(2000, 500, 400, 300));
        // Too big for the target: shrink and clamp inside it.
        let big = Rect::new(0, 0, 3000, 3000);
        assert_eq!(translate(big, from, to), Rect::new(1000, 0, 2000, 2000));
    }
}
