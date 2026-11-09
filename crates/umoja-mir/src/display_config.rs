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
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
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
/// The primary picked in Settings › Displays this session, with the set of
/// outputs it was picked for (a hotplug falls back to monitors.xml).
static CHOSEN_PRIMARY: Mutex<Option<(Vec<String>, String)>> = Mutex::new(None);
/// Output id of the primary monitor, or -1; read by the window manager.
static PRIMARY_ID: AtomicI32 = AtomicI32::new(-1);

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
    let map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let chosen = CHOSEN_PRIMARY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let stored = |names: &[String]| outputs::stored_primary(names);
    let id = primary_id(&map, chosen.as_ref(), stored).unwrap_or(-1);
    if PRIMARY_ID.swap(id, Ordering::SeqCst) != id {
        tracing::info!(
            primary = map.get(&id).map(|o| o.name.as_str()).unwrap_or("none"),
            "primary monitor"
        );
    }
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
                    && !modes.contains(&(w, h))
                {
                    modes.push((w, h));
                }
            }
        }
    }
    modes
}

/// The primary monitor's output id, or -1 with no outputs.
pub fn primary_output_id() -> i32 {
    PRIMARY_ID.load(Ordering::SeqCst)
}

fn connected_names(map: &BTreeMap<i32, OutputState>) -> Vec<String> {
    let mut names: Vec<String> = map.values().map(|o| o.name.clone()).collect();
    names.sort_unstable();
    names
}

/// Pick the primary as mutter does: the one chosen in Settings for these
/// outputs, else the one stored in monitors.xml for them, else the built-in
/// panel, else the monitor at the origin.
fn primary_id(
    map: &BTreeMap<i32, OutputState>,
    chosen: Option<&(Vec<String>, String)>,
    stored: impl FnOnce(&[String]) -> Option<String>,
) -> Option<i32> {
    let names = connected_names(map);
    let by_name = |name: &str| {
        map.iter()
            .find(|(_, o)| o.name == name)
            .map(|(id, _)| *id)
    };
    chosen
        .filter(|(set, _)| *set == names)
        .and_then(|(_, name)| by_name(name))
        .or_else(|| stored(&names).and_then(|n| by_name(&n)))
        .or_else(|| {
            map.iter()
                .find(|(_, o)| hidpi::is_builtin_connector(&o.name))
                .map(|(id, _)| *id)
        })
        .or_else(|| {
            map.iter()
                .find(|(_, o)| {
                    o.rect.x <= 0 && o.rect.y <= 0 && o.rect.right() > 0 && o.rect.bottom() > 0
                })
                .map(|(id, _)| *id)
        })
        .or_else(|| map.keys().next().copied())
}

fn current_state() -> (u32, Vec<Monitor>, Vec<LogicalMonitor>, Props) {
    let map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let primary = Some(primary_output_id()).filter(|id| map.contains_key(id));
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

/// Connectors the `default` layout of a `.display` file configures, and
/// whether each is switched on. Only reads the shape umoja-mir writes
/// (outputs at six spaces under a card, their keys at eight).
fn default_layout_outputs(yaml: &str) -> Option<Vec<(String, bool)>> {
    let mut lines = yaml.lines().skip_while(|l| l.trim_end() != "  default:");
    lines.next()?;
    let mut names: Vec<(String, bool)> = Vec::new();
    for l in lines {
        if l.starts_with("  ") && !l.starts_with("   ") {
            break; // next layout
        }
        if let Some(rest) = l.strip_prefix("      ")
            && !rest.starts_with(' ')
            && let Some(name) = rest.trim_end().strip_suffix(':')
        {
            names.push((name.to_string(), true));
        } else if l.trim() == "state: disabled"
            && let Some(last) = names.last_mut()
        {
            last.1 = false;
        }
    }
    Some(names)
}

/// When a monitor the saved layout doesn't know about is connected (a new
/// monitor, or one on a second GPU), Mir leaves it at 0,0 on top of the
/// others, which looks like mirroring. Like mutter, put new monitors to the
/// right of the known ones and keep everything else where it is.
fn place_new_outputs(
    map: &BTreeMap<i32, OutputState>,
    listed: &[(String, bool)],
) -> Option<Vec<MonitorConfig>> {
    let is_listed = |name: &str| listed.iter().any(|(n, _)| n == name);
    let (known, new): (Vec<&OutputState>, Vec<&OutputState>) =
        map.values().partition(|o| is_listed(&o.name));
    if new.is_empty() {
        return None;
    }
    let config = |o: &OutputState, x: i32, y: i32| {
        let (width, height) = o.pixel_size();
        MonitorConfig {
            connector: o.name.clone(),
            x,
            y,
            scale: o.scale,
            width,
            height,
            rate: o.refresh,
            rotation: rotation_of(transform_of(o.orientation)),
            primary: false,
            enabled: true,
        }
    };
    let mut out: Vec<MonitorConfig> = known.iter().map(|o| config(o, o.rect.x, o.rect.y)).collect();
    let mut right = known.iter().map(|o| o.rect.x + o.rect.w).max().unwrap_or(0);
    let top = known.iter().map(|o| o.rect.y).min().unwrap_or(0);
    for o in new {
        out.push(config(o, right, top));
        right += o.rect.w;
    }
    // Keep monitors the file switched off switched off.
    for (name, enabled) in listed {
        if !enabled && !out.iter().any(|c| &c.connector == name) {
            out.push(MonitorConfig {
                connector: name.clone(),
                enabled: false,
                ..Default::default()
            });
        }
    }
    Some(out)
}

/// Rewrite the `.display` file if a connected monitor is missing from it.
/// Mir reloads the file and moves the monitor. No file means Mir's own
/// side-by-side default is already in charge.
fn ensure_new_outputs_placed(display_file: &str) {
    let path = outputs::config_dir().join(display_file);
    let Ok(yaml) = std::fs::read_to_string(&path) else {
        return;
    };
    let Some(listed) = default_layout_outputs(&yaml) else {
        return;
    };
    let map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(configs) = place_new_outputs(&map, &listed) else {
        return;
    };
    match std::fs::write(&path, outputs::render_display_yaml(std::slice::from_ref(&configs))) {
        Ok(()) => tracing::info!(
            outputs = ?configs.iter().map(|c| (&c.connector, c.x, c.y)).collect::<Vec<_>>(),
            "placed new monitors to the right of the saved layout"
        ),
        Err(e) => tracing::warn!(error = %e, "could not update display config"),
    }
}

/// The login screen's arrangement: every screen side by side, top-aligned,
/// built-in panel first, at the scales Mir already uses. `None` when they
/// already are (or there is only one).
fn greeter_layout(map: &BTreeMap<i32, OutputState>) -> Option<Vec<MonitorConfig>> {
    if map.len() < 2 {
        return None;
    }
    let mut outs: Vec<&OutputState> = map.values().collect();
    outs.sort_by(|a, b| {
        hidpi::is_builtin_connector(&b.name)
            .cmp(&hidpi::is_builtin_connector(&a.name))
            .then(a.name.cmp(&b.name))
    });
    let mut x = 0;
    let mut in_place = true;
    let mut configs = Vec::new();
    for o in outs {
        in_place &= o.rect.x == x && o.rect.y == 0;
        let (width, height) = o.pixel_size();
        configs.push(MonitorConfig {
            connector: o.name.clone(),
            x,
            y: 0,
            scale: o.scale,
            width,
            height,
            rate: o.refresh,
            rotation: rotation_of(transform_of(o.orientation)),
            primary: false,
            enabled: true,
        });
        x += o.rect.w;
    }
    (!in_place).then_some(configs)
}

/// Whether the greeter's arrangement is in place: Mir has reported at least
/// `expected` screens and they already sit side by side.
pub fn greeter_layout_settled(expected: usize) -> bool {
    let map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner());
    !map.is_empty() && map.len() >= expected && greeter_layout(&map).is_none()
}

/// On the login screen, put the screens side by side, as GDM does (Mir's
/// own default mirrors them, so a wide monitor showed the laptop-sized
/// login screen in its corner). Uses the names Mir reports, which only it
/// knows (ports are numbered per card: "DisplayPort-1-1").
pub fn start_greeter(display_file: &'static str) {
    crate::session::RUNTIME.spawn(async move {
        loop {
            CHANGED.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let map = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let Some(configs) = greeter_layout(&map) else {
                continue;
            };
            let path = outputs::config_dir().join(display_file);
            match std::fs::write(&path, outputs::render_display_yaml(std::slice::from_ref(&configs))) {
                Ok(()) => tracing::info!(
                    outputs = ?configs.iter().map(|c| (&c.connector, c.x)).collect::<Vec<_>>(),
                    "greeter: screens side by side"
                ),
                Err(e) => tracing::warn!(error = %e, "greeter: could not write display layout"),
            }
        }
    });
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
        if let Some(p) = configs.iter().find(|c| c.enabled && c.primary) {
            let mut set: Vec<String> = configs
                .iter()
                .filter(|c| c.enabled)
                .map(|c| c.connector.clone())
                .collect();
            set.sort_unstable();
            *CHOSEN_PRIMARY.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((set, p.connector.clone()));
            changed();
        }
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
        crate::session::keep_name(conn.clone(), BUS_NAME);
        loop {
            CHANGED.notified().await;
            // Coalesce bursts (hotplug sends several updates).
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            ensure_new_outputs_placed(display_file);
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
    fn greeter_puts_screens_side_by_side() {
        let out = |name: &str, x: i32, w: i32, h: i32| OutputState {
            name: name.into(),
            rect: Rect::new(x, 0, w, h),
            scale: 1.0,
            refresh: 60.0,
            width_mm: 0,
            height_mm: 0,
            orientation: 0,
        };
        // Mir's default: both at 0,0 (cloned).
        let mut map = BTreeMap::new();
        map.insert(1, out("DisplayPort-1-1", 0, 3440, 1440));
        map.insert(2, out("eDP-1", 0, 1920, 1200));
        let l = greeter_layout(&map).unwrap();
        assert_eq!((l[0].connector.as_str(), l[0].x), ("eDP-1", 0));
        assert_eq!((l[1].connector.as_str(), l[1].x), ("DisplayPort-1-1", 1920));
        assert_eq!((l[1].width, l[1].height), (3440, 1440));
        // Already arranged: nothing to write.
        map.insert(1, out("DisplayPort-1-1", 1920, 3440, 1440));
        assert!(greeter_layout(&map).is_none());
        // One screen: nothing to do.
        map.remove(&1);
        assert!(greeter_layout(&map).is_none());
    }

    #[test]
    fn primary_follows_choice_then_monitors_xml_then_builtin() {
        let out = |name: &str, x: i32| OutputState {
            name: name.into(),
            rect: Rect::new(x, 0, 1920, 1080),
            scale: 1.0,
            refresh: 60.0,
            width_mm: 0,
            height_mm: 0,
            orientation: 0,
        };
        let mut map = BTreeMap::new();
        map.insert(1, out("HDMI-A-1", 0));
        map.insert(2, out("eDP-1", 1920));
        let none = |_: &[String]| None;
        // No config: the laptop panel, even though HDMI is at the origin.
        assert_eq!(primary_id(&map, None, none), Some(2));
        // monitors.xml says HDMI.
        assert_eq!(
            primary_id(&map, None, |_: &[String]| Some("HDMI-A-1".into())),
            Some(1)
        );
        // A Settings choice for this exact set wins over monitors.xml...
        let chosen = (vec!["HDMI-A-1".into(), "eDP-1".into()], "eDP-1".to_string());
        assert_eq!(
            primary_id(&map, Some(&chosen), |_: &[String]| Some("HDMI-A-1".into())),
            Some(2)
        );
        // ...but not once the set of monitors changed.
        let stale = (vec!["DP-1".into(), "eDP-1".into()], "DP-1".to_string());
        assert_eq!(primary_id(&map, Some(&stale), none), Some(2));
        // No built-in panel: the one at the origin.
        map.remove(&2);
        map.insert(3, out("DP-1", 1920));
        assert_eq!(primary_id(&map, None, none), Some(1));
        assert_eq!(primary_id(&BTreeMap::new(), None, none), None);
    }

    #[test]
    fn new_monitor_goes_to_the_right() {
        let yaml = outputs::render_display_yaml(&[vec![MonitorConfig {
            connector: "eDP-1".into(),
            scale: 1.0,
            width: 1920,
            height: 1200,
            rate: 60.0,
            rotation: "normal",
            enabled: true,
            ..Default::default()
        }]]);
        let listed = default_layout_outputs(&yaml).unwrap();
        assert_eq!(listed, vec![("eDP-1".to_string(), true)]);
        let out = |name: &str, w: i32, h: i32| OutputState {
            name: name.into(),
            rect: Rect::new(0, 0, w, h),
            scale: 1.0,
            refresh: 60.0,
            width_mm: 0,
            height_mm: 0,
            orientation: 0,
        };
        let mut map = BTreeMap::new();
        map.insert(1, out("eDP-1", 1920, 1200));
        assert_eq!(place_new_outputs(&map, &listed), None);
        map.insert(2, out("DP-5", 2560, 1440));
        let cfg = place_new_outputs(&map, &listed).unwrap();
        let dp = cfg.iter().find(|c| c.connector == "DP-5").unwrap();
        assert_eq!((dp.x, dp.y, dp.width, dp.height), (1920, 0, 2560, 1440));
        let edp = cfg.iter().find(|c| c.connector == "eDP-1").unwrap();
        assert_eq!((edp.x, edp.y), (0, 0));
        // Written back, both are known: nothing more to do.
        let again = outputs::render_display_yaml(&[cfg]);
        let listed = default_layout_outputs(&again).unwrap();
        assert_eq!(place_new_outputs(&map, &listed), None);

        // A monitor switched off in Settings stays off and isn't "new".
        let off = outputs::render_display_yaml(&[vec![
            MonitorConfig {
                connector: "eDP-1".into(),
                scale: 1.0,
                rotation: "normal",
                enabled: true,
                ..Default::default()
            },
            MonitorConfig {
                connector: "DP-5".into(),
                enabled: false,
                ..Default::default()
            },
        ]]);
        let listed = default_layout_outputs(&off).unwrap();
        assert_eq!(
            listed,
            vec![("eDP-1".to_string(), true), ("DP-5".to_string(), false)]
        );
        map.remove(&2);
        assert_eq!(place_new_outputs(&map, &listed), None);
        map.insert(3, out("HDMI-A-1", 1920, 1080));
        let cfg = place_new_outputs(&map, &listed).unwrap();
        assert!(cfg.iter().any(|c| c.connector == "DP-5" && !c.enabled));
        let hdmi = cfg.iter().find(|c| c.connector == "HDMI-A-1").unwrap();
        assert_eq!(hdmi.x, 1920);
    }

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
        // Settings sends back the 166⅔% it was offered, as a float.
        let ok = vec![(
            0,
            0,
            1.666_666_7,
            0u32,
            true,
            vec![(
                "eDP-1".to_string(),
                "2880x1800@120.000".to_string(),
                Props::new(),
            )],
        )];
        let cfg = configs_from_request(&ok).unwrap();
        assert!((cfg[0].scale - 1.666_666_7).abs() < 1e-6);
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
