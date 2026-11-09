//! Screen and keyboard backlight for the brightness keys, the way
//! gnome-settings-daemon's power plugin does it.
//!
//! Screen: the kernel backlight (firmware, then platform, then raw, like
//! gsd-backlight), stepped by 1/20 of its range and set through logind's
//! `Session.SetBrightness`, which needs no root for the active session.
//! Keyboard: UPower's KbdBacklight. When the firmware changes the keyboard
//! backlight itself (ThinkPads' Fn+Space), UPower says so and the OSD shows
//! it, as in GNOME.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk4::{gdk, glib};

use crate::osd;

const SYSFS: &str = "/sys/class/backlight";
/// While keys are coming in, step from our own target: the kernel may not
/// have caught up with the last one yet.
const KEY_BURST: Duration = Duration::from_millis(800);

const UPOWER: &str = "org.freedesktop.UPower";
const KBD_PATH: &str = "/org/freedesktop/UPower/KbdBacklight";
const KBD_IFACE: &str = "org.freedesktop.UPower.KbdBacklight";

/// gpm-common.h's BRIGHTNESS_STEP_AMOUNT.
pub fn step_amount(range: i64) -> i64 {
    if range < 20 { 1 } else { range / 20 }
}

/// gpm-common.h's ABS_TO_PERCENTAGE.
pub fn to_percent(min: i64, max: i64, value: i64) -> i64 {
    if max <= min {
        return 0;
    }
    (((value - min) * 100) as f64 / (max - min) as f64).round().clamp(0.0, 100.0) as i64
}

#[derive(Debug, Clone)]
struct Backlight {
    name: String,
    dir: PathBuf,
    min: i64,
    max: i64,
}

fn read_int(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_str(path: &Path) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

impl Backlight {
    /// firmware → platform → raw, as gsd-backlight picks.
    fn find() -> Option<Self> {
        let mut found: Vec<(String, PathBuf, String)> = std::fs::read_dir(SYSFS)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| {
                let dir = e.path();
                let kind = read_str(&dir.join("type"));
                (e.file_name().to_string_lossy().into_owned(), dir, kind)
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        let (name, dir, kind) = ["firmware", "platform", "raw"]
            .iter()
            .find_map(|t| found.iter().find(|(_, _, k)| k == t).cloned())?;
        let max = read_int(&dir.join("max_brightness"))?;
        let mut min = ((max as f64 * 0.01) as i64).max(1);
        // A raw interface with fewer than 100 steps: 0 isn't "off".
        if max < 99 && kind == "raw" {
            min = 0;
        }
        (min < max).then_some(Self { name, dir, min, max })
    }

    fn current(&self) -> Option<i64> {
        read_int(&self.dir.join("brightness"))
    }
}

pub struct Brightness {
    screen: Option<Backlight>,
    target: Cell<Option<(i64, Instant)>>,
    kbd_max: Cell<i32>,
    /// The level before the keyboard backlight toggle turned it off.
    kbd_before_off: Cell<i32>,
}

thread_local! {
    static BRIGHTNESS: Rc<Brightness> = Brightness::start();
}

impl Brightness {
    pub fn get() -> Rc<Self> {
        BRIGHTNESS.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let screen = Backlight::find();
        tracing::info!(backlight = ?screen.as_ref().map(|b| &b.name), "screen backlight");
        let this = Rc::new(Self {
            screen,
            target: Cell::default(),
            kbd_max: Cell::new(0),
            kbd_before_off: Cell::new(-1),
        });
        this.watch_keyboard();
        this
    }

    /// Screen brightness keys.
    pub fn step_screen(&self, up: bool) {
        let Some(b) = &self.screen else {
            tracing::debug!("no backlight to change");
            return;
        };
        let now = match self.target.get() {
            Some((t, at)) if at.elapsed() < KEY_BURST => Some(t),
            _ => b.current(),
        };
        let Some(now) = now else { return };
        let step = step_amount(b.max - b.min + 1);
        let new = if up { (now + step).min(b.max) } else { (now - step).max(b.min) };
        self.target.set(Some((new, Instant::now())));
        if new != now {
            set_brightness("backlight", &b.name, new as u32);
        }
        osd::show(
            osd::Request::icon("display-brightness-symbolic")
                .level(to_percent(b.min, b.max, new) as f64 / 100.0, 1.0)
                .connector(builtin_connector()),
        );
    }

    fn kbd_conn() -> Option<gio::DBusConnection> {
        crate::bus::system_bus()
    }

    fn kbd_call(&self, method: &str, args: Option<&glib::Variant>) -> Option<glib::Variant> {
        let conn = Self::kbd_conn()?;
        conn.call_sync(
            Some(UPOWER),
            KBD_PATH,
            KBD_IFACE,
            method,
            args,
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        )
        .map_err(|e| tracing::debug!(method, error = %e, "UPower KbdBacklight"))
        .ok()
    }

    fn kbd_get(&self) -> Option<i32> {
        self.kbd_call("GetBrightness", None)?.get::<(i32,)>().map(|(v,)| v)
    }

    fn kbd_set(&self, v: i32) {
        self.kbd_call("SetBrightness", Some(&(v,).to_variant()));
    }

    fn kbd_osd(&self, v: i32) {
        let max = self.kbd_max.get();
        if max <= 0 {
            return;
        }
        osd::show(
            osd::Request::icon("keyboard-brightness-symbolic")
                .level(to_percent(0, max as i64, v as i64) as f64 / 100.0, 1.0),
        );
    }

    /// Keyboard backlight keys. `up`: Some(true/false) steps, None toggles.
    pub fn keyboard(&self, up: Option<bool>) {
        if self.kbd_max.get() <= 0 {
            let max = self
                .kbd_call("GetMaxBrightness", None)
                .and_then(|v| v.get::<(i32,)>())
                .map(|(v,)| v)
                .unwrap_or(0);
            self.kbd_max.set(max);
        }
        let max = self.kbd_max.get();
        if max <= 0 {
            return;
        }
        let Some(now) = self.kbd_get() else { return };
        let new = match up {
            Some(up) => {
                let step = step_amount(max as i64) as i32;
                self.kbd_before_off.set(-1);
                if up { (now + step).min(max) } else { (now - step).max(0) }
            }
            None => {
                let before = self.kbd_before_off.get();
                if now > 0 {
                    self.kbd_before_off.set(now);
                    0
                } else {
                    self.kbd_before_off.set(-1);
                    if before > 0 { before } else { max }
                }
            }
        };
        if new != now {
            self.kbd_set(new);
        }
        self.kbd_osd(new);
    }

    /// Show the OSD when the firmware changes the keyboard backlight.
    fn watch_keyboard(self: &Rc<Self>) {
        let Some(conn) = Self::kbd_conn() else { return };
        let weak = Rc::downgrade(self);
        let id = conn.signal_subscribe(
            Some(UPOWER),
            Some(KBD_IFACE),
            Some("BrightnessChangedWithSource"),
            Some(KBD_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                let Some(this) = weak.upgrade() else { return };
                let Some((value, source)) = params.get::<(i32, String)>() else { return };
                if source != "internal" {
                    return;
                }
                if this.kbd_max.get() <= 0 {
                    let max = this
                        .kbd_call("GetMaxBrightness", None)
                        .and_then(|v| v.get::<(i32,)>())
                        .map(|(v,)| v)
                        .unwrap_or(0);
                    this.kbd_max.set(max);
                }
                this.kbd_osd(value);
            },
        );
        // Lives as long as the panel.
        std::mem::forget(id);
    }
}

/// logind's Session.SetBrightness(subsystem, name, value), asynchronously.
fn set_brightness(subsystem: &str, name: &str, value: u32) {
    let Some(conn) = crate::bus::system_bus() else { return };
    conn.call(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1/session/auto",
        "org.freedesktop.login1.Session",
        "SetBrightness",
        Some(&(subsystem, name, value).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
        |res| {
            if let Err(e) = res {
                tracing::warn!(error = %e, "could not set the backlight");
            }
        },
    );
}

/// The built-in panel's connector, where GNOME shows the brightness OSD.
fn builtin_connector() -> Option<String> {
    let display = gdk::Display::default()?;
    display
        .monitors()
        .iter::<gdk::Monitor>()
        .filter_map(Result::ok)
        .filter_map(|m| m.connector().map(|c| c.to_string()))
        .find(|c| c.starts_with("eDP") || c.starts_with("LVDS") || c.starts_with("DSI"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_like_gsd() {
        assert_eq!(step_amount(3), 1);
        assert_eq!(step_amount(256), 12);
        assert_eq!(step_amount(65535), 3276);
        assert_eq!(to_percent(2, 255, 255), 100);
        assert_eq!(to_percent(2, 255, 2), 0);
        assert_eq!(to_percent(0, 2, 1), 50);
    }
}
