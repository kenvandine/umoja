//! Pointer settings from GNOME's `org.gnome.desktop.peripherals.*` schemas.
//!
//! Mir has no natural-scrolling switch (its libinput backend never calls
//! `libinput_device_config_scroll_set_natural_scroll_enabled`), so the
//! shim flips scroll events itself in an event filter and asks us, per
//! event, whether the device class that produced it scrolls "naturally".
//! As in mutter, touchpads follow the touchpad key (on by default) and
//! mice, trackballs and pointing sticks follow the mouse key (off by
//! default). Changes made in Settings apply immediately.

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

pub fn reload() {
    let touchpad = gs::get_bool(TOUCHPAD, "natural-scroll", true);
    let mouse = gs::get_bool(MOUSE, "natural-scroll", false);
    TOUCHPAD_NATURAL.store(touchpad, Ordering::Relaxed);
    MOUSE_NATURAL.store(mouse, Ordering::Relaxed);
    tracing::info!(touchpad, mouse, "natural scrolling");
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
                        s.connect_changed(Some("natural-scroll"), |_, _| reload());
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
}
