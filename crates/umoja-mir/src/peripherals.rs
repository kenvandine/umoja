//! Pointer settings from GNOME's `org.gnome.desktop.peripherals.*` schemas.
//!
//! Mir has no natural-scrolling switch (its libinput backend never calls
//! `libinput_device_config_scroll_set_natural_scroll_enabled`), so the
//! shim flips scroll events itself in an event filter and asks us, per
//! event, whether the device class that produced it scrolls "naturally".
//! As in mutter, touchpads follow the touchpad key (on by default) and
//! mice, trackballs and pointing sticks follow the mouse key (off by
//! default). Changes made in Settings apply immediately.
//!
//! Everything else (tap-to-click, click method, scroll method, disable
//! while typing, speed, acceleration profile, primary button) Mir's libinput
//! backend does support, but leaves at libinput's defaults, where tapping is
//! off. We pass GNOME's values to the shim's MirAL InputConfiguration
//! (`input_config.cpp`), which applies them to every pointer device,
//! including ones plugged in later. Mir has no switches for tap-and-drag,
//! drag lock or the tap button map, so those stay at libinput's defaults
//! (on, off and left/right/middle), which are also GNOME's.

use gio::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use umoja_common::gsettings as gs;

pub const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
pub const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";

/// Scroll source classes passed from the shim. Keep in sync with
/// `UMOJA_SCROLL_*` in umoja_shim.h.
pub const SCROLL_TOUCHPAD: u32 = 1;
pub const SCROLL_MOUSE: u32 = 2;

// GNOME's schema defaults.
static TOUCHPAD_NATURAL: AtomicBool = AtomicBool::new(true);
static MOUSE_NATURAL: AtomicBool = AtomicBool::new(false);

/// `UmojaInputSettings` in umoja_shim.h.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputSettings {
    pub tap_to_click: bool,
    pub disable_while_typing: bool,
    pub disable_with_external_mouse: bool,
    pub middle_click_emulation: bool,
    pub click_method: i32,
    pub scroll_method: i32,
    pub touchpad_speed: f64,
    pub touchpad_accel_profile: i32,
    pub mouse_speed: f64,
    pub mouse_accel_profile: i32,
    pub left_handed: bool,
}

// UMOJA_CLICK_*, UMOJA_SCROLL_METHOD_*, UMOJA_ACCEL_* in umoja_shim.h.
pub const CLICK_DEFAULT: i32 = 0;
pub const CLICK_NONE: i32 = 1;
pub const CLICK_AREAS: i32 = 2;
pub const CLICK_FINGERS: i32 = 3;
pub const SCROLL_METHOD_NONE: i32 = 0;
pub const SCROLL_METHOD_TWO_FINGER: i32 = 1;
pub const SCROLL_METHOD_EDGE: i32 = 2;
pub const ACCEL_DEFAULT: i32 = 0;
pub const ACCEL_FLAT: i32 = 1;
pub const ACCEL_ADAPTIVE: i32 = 2;

unsafe extern "C" {
    fn umoja_input_apply(settings: *const InputSettings);
}

pub fn click_method(value: &str) -> i32 {
    match value {
        "none" => CLICK_NONE,
        "areas" => CLICK_AREAS,
        "fingers" => CLICK_FINGERS,
        _ => CLICK_DEFAULT,
    }
}

/// As mutter: two-finger scrolling wins when both are on.
pub fn scroll_method(two_finger: bool, edge: bool) -> i32 {
    if two_finger {
        SCROLL_METHOD_TWO_FINGER
    } else if edge {
        SCROLL_METHOD_EDGE
    } else {
        SCROLL_METHOD_NONE
    }
}

pub fn accel_profile(value: &str) -> i32 {
    match value {
        "flat" => ACCEL_FLAT,
        "adaptive" => ACCEL_ADAPTIVE,
        _ => ACCEL_DEFAULT,
    }
}

fn get_double(schema: &str, key: &str, default: f64) -> f64 {
    match gs::settings(schema) {
        Some(s) if gs::has_key(&s, key) => s.double(key).clamp(-1.0, 1.0),
        _ => default,
    }
}

impl InputSettings {
    /// From GSettings, falling back to GNOME's schema defaults.
    pub fn read() -> Self {
        Self {
            tap_to_click: gs::get_bool(TOUCHPAD, "tap-to-click", false),
            disable_while_typing: gs::get_bool(TOUCHPAD, "disable-while-typing", true),
            disable_with_external_mouse: gs::get_string(TOUCHPAD, "send-events", "enabled")
                == "disabled-on-external-mouse",
            middle_click_emulation: gs::get_bool(TOUCHPAD, "middle-click-emulation", false),
            click_method: click_method(&gs::get_string(TOUCHPAD, "click-method", "default")),
            scroll_method: scroll_method(
                gs::get_bool(TOUCHPAD, "two-finger-scrolling-enabled", true),
                gs::get_bool(TOUCHPAD, "edge-scrolling-enabled", false),
            ),
            touchpad_speed: get_double(TOUCHPAD, "speed", 0.0),
            touchpad_accel_profile: accel_profile(&gs::get_string(
                TOUCHPAD,
                "accel-profile",
                "default",
            )),
            mouse_speed: get_double(MOUSE, "speed", 0.0),
            mouse_accel_profile: accel_profile(&gs::get_string(MOUSE, "accel-profile", "default")),
            left_handed: gs::get_bool(MOUSE, "left-handed", false),
        }
    }
}

pub fn reload() {
    let touchpad = gs::get_bool(TOUCHPAD, "natural-scroll", true);
    let mouse = gs::get_bool(MOUSE, "natural-scroll", false);
    TOUCHPAD_NATURAL.store(touchpad, Ordering::Relaxed);
    MOUSE_NATURAL.store(mouse, Ordering::Relaxed);
    tracing::info!(touchpad, mouse, "natural scrolling");

    let input = InputSettings::read();
    tracing::info!(?input, "pointer settings");
    // SAFETY: the shim copies the struct before returning.
    unsafe { umoja_input_apply(&input) };
}

/// True when scroll events from `source` should be inverted.
pub fn natural_scroll(source: u32) -> bool {
    match source {
        SCROLL_TOUCHPAD => TOUCHPAD_NATURAL.load(Ordering::Relaxed),
        SCROLL_MOUSE => MOUSE_NATURAL.load(Ordering::Relaxed),
        _ => false,
    }
}

/// Load once, then keep watching for changes on a helper thread.
pub fn start_watching() {
    reload();
    std::thread::Builder::new()
        .name("umoja-peripherals".into())
        .spawn(|| {
            let ctx = glib::MainContext::new();
            let _guard = ctx.acquire();
            let _ = ctx.with_thread_default(|| {
                let keep: Vec<gio::Settings> = [TOUCHPAD, MOUSE]
                    .into_iter()
                    .filter_map(gs::settings)
                    .inspect(|s| {
                        s.connect_changed(None, |_, _| reload());
                    })
                    .collect();
                glib::MainLoop::new(Some(&ctx), false).run();
                drop(keep);
            });
        })
        .expect("spawn peripherals thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_sources_are_never_inverted() {
        TOUCHPAD_NATURAL.store(true, Ordering::Relaxed);
        MOUSE_NATURAL.store(true, Ordering::Relaxed);
        assert!(!natural_scroll(0));
        assert!(!natural_scroll(99));
        assert!(natural_scroll(SCROLL_TOUCHPAD));
        assert!(natural_scroll(SCROLL_MOUSE));
        MOUSE_NATURAL.store(false, Ordering::Relaxed);
        assert!(!natural_scroll(SCROLL_MOUSE));
    }

    #[test]
    fn gnome_values_map_to_shim_constants() {
        assert_eq!(click_method("areas"), CLICK_AREAS);
        assert_eq!(click_method("fingers"), CLICK_FINGERS);
        assert_eq!(click_method("none"), CLICK_NONE);
        assert_eq!(click_method("default"), CLICK_DEFAULT);
        assert_eq!(scroll_method(true, true), SCROLL_METHOD_TWO_FINGER);
        assert_eq!(scroll_method(false, true), SCROLL_METHOD_EDGE);
        assert_eq!(scroll_method(false, false), SCROLL_METHOD_NONE);
        assert_eq!(accel_profile("flat"), ACCEL_FLAT);
        assert_eq!(accel_profile("adaptive"), ACCEL_ADAPTIVE);
        assert_eq!(accel_profile("default"), ACCEL_DEFAULT);
    }
}
