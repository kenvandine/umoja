//! `org.gnome.Mutter.DisplayConfig` for gnome-control-center's Displays panel.
//!
//! Settings › Displays talks to this D-Bus API on GNOME. Implementing it here
//! means the stock panel works unchanged under Umoja: it lists monitors and
//! modes, offers the same fractional scales as mutter (100%–400% in 25%
//! steps), and applies arrangement, scale, rotation and refresh rate.
//!
//! Applying writes Mir's `~/.config/umoja-mir.display` (Mir reloads it live)
//! and, for persistent changes, GNOME's `~/.config/monitors.xml`, so the
//! layout carries over between the Umoja and GNOME sessions.

use crate::outputs::{self, MonitorConfig};
use crate::tiling::Rect;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};
use tokio::sync::Notify;
use umoja_common::hidpi;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{fdo, interface};

pub const BUS_NAME: &str = "org.gnome.Mutter.DisplayConfig";
pub const OBJECT_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

#[derive(Debug, Clone, PartialEq)]
pub struct OutputState {
    pub name: String,
    /// Logical extents (already divided by scale), as Mir reports them.
    pub rect: Rect,
    pub scale: f64,
    pub refresh: f64,
    pub width_mm: u32,
    pub height_mm: u32,
    /// Degrees clockwise: 0, 90, 180, 270.
    pub orientation: u32,
}

impl OutputState {
    fn pixel_size(&self) -> (u32, u32) {
        let w = (self.rect.w as f64 * self.scale).round() as u32;
        let h = (self.rect.h as f64 * self.scale).round() as u32;
        if self.orientation == 90 || self.orientation == 270 {
            (h, w)
        } else {
            (w, h)
        }
    }
}

static OUTPUTS: LazyLock<Mutex<BTreeMap<i32, OutputState>>> = LazyLock::new(Default::default);
static SERIAL: AtomicU32 = AtomicU32::new(1);
static CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

pub fn output_updated(id: i32, state: OutputState) {
    let mut map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner());
    if map.get(&id) != Some(&state) {
        map.insert(id, state);
        drop(map);
        changed();
    }
}

pub fn output_removed(id: i32) {
    if OUTPUTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
        .is_some()
    {
        changed();
    }
}

fn changed() {
    SERIAL.fetch_add(1, Ordering::SeqCst);
    CHANGED.notify_one();
}

type Props = HashMap<String, OwnedValue>;
type MonitorSpec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Monitor = (MonitorSpec, Vec<Mode>, Props);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, Props);
type MonitorAssignment = (String, String, Props);
type LogicalMonitorConfig = (i32, i32, f64, u32, bool, Vec<MonitorAssignment>);

fn prop<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values convert")
}

pub fn mode_id(w: u32, h: u32, rate: f64) -> String {
    format!("{w}x{h}@{rate:.3}")
}

pub fn parse_mode_id(id: &str) -> Option<(u32, u32, f64)> {
    let (size, rate) = id.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, rate.parse().ok()?))
}

/// Mutter transform enum <-> Mir orientation.
fn transform_of(degrees: u32) -> u32 {
    match degrees {
        90 => 1,
        180 => 2,
        270 => 3,
        _ => 0,
    }
}

fn rotation_of(transform: u32) -> &'static str {
    match transform {
        1 => "left",
        2 => "inverted",
        3 => "right",
        _ => "normal",
    }
}

/// Modes the kernel advertises for a connector (sysfs lists "WxH" lines).
fn kernel_modes(connector: &str) -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut modes = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.split_once('-').map(|(_, c)| c) != Some(connector) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(e.path().join("modes")) {
            for line in text.lines() {
                if let Some((w, h)) = line.trim().trim_end_matches('i').split_once('x')
                    && let (Ok(w), Ok(h)) = (w.parse(), h.parse())
                        && !modes.contains(&(w, h)) {
                            modes.push((w, h));
                        }
            }
        }
    }
    modes
}

fn primary_id(map: &BTreeMap<i32, OutputState>) -> Option<i32> {
    map.iter()
        .find(|(_, o)| o.rect.x <= 0 && o.rect.y <= 0 && o.rect.right() > 0 && o.rect.bottom() > 0)
        .or_else(|| map.iter().next())
        .map(|(id, _)| *id)
}

fn current_state() -> (u32, Vec<Monitor>, Vec<LogicalMonitor>, Props) {
    let map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let primary = primary_id(&map);
    let mut monitors = Vec::new();
    let mut logical = Vec::new();
    for (id, o) in &map {
        let spec: MonitorSpec = (
            outputs::gnome_connector(&o.name),
            "unknown".into(),
            o.name.clone(),
            "unknown".into(),
        );
        let (cw, ch) = o.pixel_size();
        let builtin = hidpi::is_builtin_connector(&o.name);
        let mut sizes = kernel_modes(&o.name);
        if !sizes.contains(&(cw, ch)) {
            sizes.insert(0, (cw, ch));
        }
        let modes: Vec<Mode> = sizes
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| {
                let current = (w, h) == (cw, ch);
                let rate = if current { o.refresh } else { 60.0 };
                let mut props = Props::new();
                props.insert("is-current".into(), prop(current));
                // The kernel lists the preferred mode first.
                props.insert("is-preferred".into(), prop(i == 0));
                (
                    mode_id(w, h, rate),
                    w as i32,
                    h as i32,
                    rate,
                    hidpi::preferred_scale(w, h, o.width_mm, o.height_mm, builtin),
                    hidpi::supported_scales(w, h),
                    props,
                )
            })
            .collect();
        let mut mprops = Props::new();
        mprops.insert("is-builtin".into(), prop(builtin));
        mprops.insert(
            "display-name".into(),
            prop(if builtin {
                "Built-in display".to_string()
            } else {
                o.name.clone()
            }),
        );
        mprops.insert("width-mm".into(), prop(o.width_mm as i32));
        mprops.insert("height-mm".into(), prop(o.height_mm as i32));
        monitors.push((spec.clone(), modes, mprops));
        logical.push((
            o.rect.x,
            o.rect.y,
            o.scale,
            transform_of(o.orientation),
            Some(*id) == primary,
            vec![spec],
            Props::new(),
        ));
    }
    let mut props = Props::new();
    props.insert("layout-mode".into(), prop(1u32)); // logical
    props.insert("supports-changing-layout-mode".into(), prop(false));
    props.insert("global-scale-required".into(), prop(false));
    props.insert("legacy-ui-scaling-factor".into(), prop(1i32));
    (SERIAL.load(Ordering::SeqCst), monitors, logical, props)
}

/// Turn an ApplyMonitorsConfig request into per-output configs.
pub fn configs_from_request(logical: &[LogicalMonitorConfig]) -> fdo::Result<Vec<MonitorConfig>> {
    let mut out = Vec::new();
    for (x, y, scale, transform, primary, monitors) in logical {
        if !(hidpi::MIN_SCALE..=hidpi::MAX_SCALE).contains(scale) {
            return Err(fdo::Error::InvalidArgs(format!(
                "scale {scale} out of range"
            )));
        }
        for (connector, mode, _) in monitors {
            let (width, height, rate) = parse_mode_id(mode)
                .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad mode id {mode}")))?;
            if !hidpi::supported_scales(width, height)
                .iter()
                .any(|s| (s - scale).abs() < 0.01)
            {
                return Err(fdo::Error::InvalidArgs(format!(
                    "scale {scale} not supported for {mode}"
                )));
            }
            out.push(MonitorConfig {
                connector: outputs::mir_connector(connector),
                x: *x,
                y: *y,
                scale: *scale,
                width,
                height,
                rate,
                rotation: rotation_of(*transform),
                primary: *primary,
                enabled: true,
            });
        }
    }
    if out.is_empty() {
        return Err(fdo::Error::InvalidArgs("no monitors enabled".into()));
    }
    // Monitors left out of the request are switched off, as in mutter.
    let known: Vec<String> = OUTPUTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(|o| o.name.clone())
        .collect();
    for name in known {
        if !out.iter().any(|c| c.connector == name) {
            out.push(MonitorConfig {
                connector: name,
                enabled: false,
                ..Default::default()
            });
        }
    }
    Ok(out)
}

pub struct DisplayConfig {
    display_file: &'static str,
}

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayConfig {
    async fn get_current_state(&self) -> (u32, Vec<Monitor>, Vec<LogicalMonitor>, Props) {
        current_state()
    }

    /// method: 0 verify, 1 temporary, 2 persistent.
    async fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<LogicalMonitorConfig>,
        _properties: Props,
    ) -> fdo::Result<()> {
        if serial != SERIAL.load(Ordering::SeqCst) {
            return Err(fdo::Error::AccessDenied(
                "The requested configuration is based on stale information".into(),
            ));
        }
        let configs = configs_from_request(&logical_monitors)?;
        if method == 0 {
            return Ok(());
        }
        let dir = outputs::config_dir();
        let yaml = outputs::render_display_yaml(std::slice::from_ref(&configs));
        std::fs::write(dir.join(self.display_file), yaml)
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        if method == 2 {
            std::fs::write(
                dir.join("monitors.xml"),
                outputs::render_monitors_xml(&configs),
            )
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        }
        tracing::info!(
            method,
            outputs = configs.len(),
            "applied monitor configuration"
        );
        Ok(())
    }

    #[zbus(signal)]
    async fn monitors_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn apply_monitors_config_allowed(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn night_light_supported(&self) -> bool {
        false // TODO(phase 4): gamma ramps through Mir
    }

    #[zbus(property)]
    fn panel_orientation_managed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn power_save_mode(&self) -> i32 {
        0
    }
}

/// Serve DisplayConfig on the session bus and emit MonitorsChanged.
pub fn start(display_file: &'static str) {
    crate::session::RUNTIME.spawn(async move {
        let conn = match zbus::connection::Builder::session()
            .and_then(|b| b.name(BUS_NAME))
            .and_then(|b| b.serve_at(OBJECT_PATH, DisplayConfig { display_file }))
        {
            Ok(b) => match b.build().await {
                Ok(c) => c,
                Err(e) => return tracing::warn!(error = %e, "DisplayConfig unavailable"),
            },
            Err(e) => return tracing::warn!(error = %e, "DisplayConfig unavailable"),
        };
        loop {
            CHANGED.notified().await;
            // Coalesce bursts (hotplug sends several updates).
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) {
                let _ = DisplayConfig::monitors_changed(&emitter).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_ids_round_trip() {
        assert_eq!(mode_id(2880, 1800, 120.0), "2880x1800@120.000");
        assert_eq!(
            parse_mode_id("2880x1800@120.000"),
            Some((2880, 1800, 120.0))
        );
        assert_eq!(parse_mode_id("junk"), None);
    }

    #[test]
    fn apply_request_validation() {
        let ok = vec![(
            0,
            0,
            1.75,
            0u32,
            true,
            vec![(
                "eDP-1".to_string(),
                "2880x1800@120.000".to_string(),
                Props::new(),
            )],
        )];
        let cfg = configs_from_request(&ok).unwrap();
        assert_eq!(cfg[0].scale, 1.75);
        assert_eq!(cfg[0].width, 2880);

        let bad_scale = vec![(
            0,
            0,
            3.0,
            0u32,
            true,
            vec![(
                "HDMI-1".to_string(),
                "1920x1080@60.000".to_string(),
                Props::new(),
            )],
        )];
        assert!(configs_from_request(&bad_scale).is_err());

        let rotated = vec![(
            0,
            0,
            1.0,
            1u32,
            true,
            vec![(
                "HDMI-1".to_string(),
                "1920x1080@60.000".to_string(),
                Props::new(),
            )],
        )];
        let cfg = configs_from_request(&rotated).unwrap();
        assert_eq!(cfg[0].connector, "HDMI-A-1");
        assert_eq!(cfg[0].rotation, "left");
    }

    #[test]
    fn pixel_size_accounts_for_scale_and_rotation() {
        let o = OutputState {
            name: "eDP-1".into(),
            rect: Rect::new(0, 0, 1646, 1029),
            scale: 1.75,
            refresh: 120.0,
            width_mm: 302,
            height_mm: 189,
            orientation: 0,
        };
        assert_eq!(o.pixel_size(), (2881, 1801));
        let r = OutputState {
            orientation: 90,
            rect: Rect::new(0, 0, 1080, 1920),
            scale: 1.0,
            ..o
        };
        assert_eq!(r.pixel_size(), (1920, 1080));
    }
}
