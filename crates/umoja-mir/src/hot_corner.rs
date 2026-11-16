//! GNOME's hot corner (`Layout.HotCorner`): with
//! `org.gnome.desktop.interface enable-hot-corners` on, pushing the pointer
//! into the top-left corner of a monitor opens the overview (Ubuntu ships the
//! setting off; Settings > Multitasking and Ask Umoja turn it on).
//!
//! As in gnome-shell it is a pressure barrier, not a plain "pointer reached
//! the corner": two barriers run from the corner along the top and the left
//! edge, a panel's height long, and pushing about 100 px into them within a
//! second toggles the overview, once until the pointer leaves the corner.
//! The shim only reports a corner whose two edges have no monitor beyond
//! them, and none while a fullscreen window covers that monitor, which is
//! when gnome-shell turns its hot corners off too.

use crate::edge_pressure::{Barrier, LEFT, TOP};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// gnome-shell's HOT_CORNER_PRESSURE_THRESHOLD, in pixels.
const THRESHOLD: f64 = 100.0;

static BARRIER: LazyLock<Mutex<Barrier>> = LazyLock::new(Default::default);

/// One pointer motion near a hot corner. `on_left`/`on_top`: the pointer
/// is on the corner's left or top barrier (0 for neither, which resets).
pub fn motion(dx: f64, dy: f64, on_left: bool, on_top: bool) {
    let mut edges = 0;
    if on_left {
        edges |= LEFT;
    }
    if on_top {
        edges |= TOP;
    }
    let fired = BARRIER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .motion(Instant::now(), dx, dy, edges, THRESHOLD);
    if fired.is_none() || crate::greeter::enabled() || !crate::input::read().hot_corners {
        return;
    }
    tracing::debug!("hot corner");
    crate::session::dispatch(crate::tiling::Action::ToggleOverview);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pushing_into_the_corner_fires_once() {
        let mut b = Barrier::default();
        let t = Instant::now();
        let mut fired = 0;
        for i in 0..40 {
            if b
                .motion(t + Duration::from_millis(i * 10), -6.0, -4.0, LEFT | TOP, THRESHOLD)
                .is_some()
            {
                fired += 1;
            }
        }
        assert_eq!(fired, 1);
        // Leaving the corner re-arms it.
        assert!(b.motion(t, 5.0, 5.0, 0, THRESHOLD).is_none());
        let mut again = false;
        for i in 0..40 {
            again |= b
                .motion(t + Duration::from_millis(500 + i * 10), -6.0, 0.0, LEFT, THRESHOLD)
                .is_some();
        }
        assert!(again);
    }
}
