//! GNOME's pressure barrier (`Layout.PressureBarrier`, what dash-to-dock
//! uses to bring back an auto-hidden dock): the pointer has to *push*
//! against a screen edge, about 100 px of motion into it within a second,
//! before the dock shows. Just reaching the edge, as when aiming at a button
//! at the bottom of a fullscreen game, does nothing.
//!
//! The shim reports every pointer motion with the outer screen edges the
//! pointer sits on (edges with no other monitor beyond them, where the
//! pointer is held back). On a push past the threshold this emits
//! `org.umoja.Compositor.EdgePressure`, once until the pointer leaves the
//! edge again, as the barrier's `trigger`/`barrier-left` do.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use gio::prelude::*;
use umoja_common::gsettings as gs;

pub const LEFT: u32 = 1;
pub const RIGHT: u32 = 2;
pub const TOP: u32 = 4;
pub const BOTTOM: u32 = 8;

/// dash-to-dock's `pressure-threshold` default, in pixels.
const THRESHOLD: f64 = 100.0;
/// GNOME's pressure timeout (dash-to-dock's PRESSURE_TIMEOUT).
const TIMEOUT: Duration = Duration::from_millis(1000);

static THRESHOLD_SETTING: LazyLock<f64> = LazyLock::new(|| {
    gs::settings(gs::DASH_TO_DOCK)
        .filter(|s| gs::has_key(s, "pressure-threshold"))
        .map(|s| s.double("pressure-threshold"))
        .filter(|t| t.is_finite() && *t > 0.0)
        .unwrap_or(THRESHOLD)
});

static BARRIER: LazyLock<Mutex<Barrier>> = LazyLock::new(Default::default);

#[derive(Default)]
pub struct Barrier {
    /// The edge being pushed (one of LEFT..BOTTOM), 0 for none.
    edge: u32,
    events: VecDeque<(Instant, f64)>,
    pressure: f64,
    /// Fired already; quiet until the pointer leaves the edge.
    triggered: bool,
}

impl Barrier {
    fn reset(&mut self, edge: u32) {
        self.edge = edge;
        self.events.clear();
        self.pressure = 0.0;
        self.triggered = false;
    }

    /// One pointer motion by (dx, dy) that left the pointer on the outer
    /// `edges`. Returns the edge pushed past `threshold`, if any.
    pub fn motion(
        &mut self,
        now: Instant,
        dx: f64,
        dy: f64,
        edges: u32,
        threshold: f64,
    ) -> Option<u32> {
        // How far this motion pushed into each edge the pointer is on.
        let pushes = [
            (LEFT, -dx, dy.abs()),
            (RIGHT, dx, dy.abs()),
            (TOP, -dy, dx.abs()),
            (BOTTOM, dy, dx.abs()),
        ];
        let push = pushes
            .into_iter()
            .filter(|(e, into, _)| edges & e != 0 && *into > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1));

        if self.edge != 0 && edges & self.edge == 0 {
            // barrier-left: off the edge, start over next time.
            self.reset(0);
        }
        let (edge, distance, slide) = push?;
        if edge != self.edge {
            self.reset(edge);
        }
        if self.triggered {
            return None;
        }
        if distance >= threshold {
            self.triggered = true;
            return Some(edge);
        }
        // GNOME ignores motion that mostly slides along the barrier.
        if slide > distance {
            return None;
        }
        while let Some(&(t, d)) = self.events.front() {
            if now.duration_since(t) <= TIMEOUT {
                break;
            }
            self.events.pop_front();
            self.pressure -= d;
        }
        self.events.push_back((now, distance));
        self.pressure += distance;
        if self.pressure >= threshold {
            self.triggered = true;
            return Some(edge);
        }
        None
    }
}

/// From the shim, for every pointer motion.
pub fn motion(x: i32, y: i32, dx: f64, dy: f64, edges: u32) {
    if edges == 0 {
        let mut b = BARRIER.lock().unwrap_or_else(|e| e.into_inner());
        if b.edge != 0 {
            b.reset(0);
        }
        return;
    }
    let threshold = *THRESHOLD_SETTING;
    let fired = BARRIER.lock().unwrap_or_else(|e| e.into_inner()).motion(
        Instant::now(),
        dx,
        dy,
        edges,
        threshold,
    );
    if let Some(edge) = fired {
        tracing::debug!(edge, x, y, "edge pressure");
        crate::shell::edge_pressure(edge, x, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaching_the_edge_is_not_enough() {
        let mut b = Barrier::default();
        let t = Instant::now();
        // Arrive with a 30 px move, then rest.
        assert_eq!(b.motion(t, 0.0, 30.0, BOTTOM, 100.0), None);
        assert_eq!(b.motion(t, 2.0, 0.0, BOTTOM, 100.0), None);
    }

    #[test]
    fn pushing_fires_once_until_the_pointer_leaves() {
        let mut b = Barrier::default();
        let t = Instant::now();
        let mut fired = 0;
        for i in 0..20 {
            let now = t + Duration::from_millis(i * 10);
            if b.motion(now, 0.0, 10.0, BOTTOM, 100.0).is_some() {
                fired += 1;
            }
        }
        assert_eq!(fired, 1);
        // Moving up off the edge re-arms it.
        assert_eq!(b.motion(t, 0.0, -5.0, 0, 100.0), None);
        let mut again = None;
        for i in 0..12 {
            again =
                again.or(b.motion(t + Duration::from_millis(300 + i), 0.0, 10.0, BOTTOM, 100.0));
        }
        assert_eq!(again, Some(BOTTOM));
    }

    #[test]
    fn slow_pushes_time_out_and_grazing_does_not_count() {
        let mut b = Barrier::default();
        let t = Instant::now();
        for i in 0..10 {
            // 20 px every 600 ms: never 100 px within a second.
            let now = t + Duration::from_millis(i * 600);
            assert_eq!(b.motion(now, 0.0, 20.0, BOTTOM, 100.0), None);
        }
        let mut b = Barrier::default();
        for _ in 0..50 {
            // Sliding along the bottom edge while drifting down a little.
            assert_eq!(b.motion(t, 30.0, 5.0, BOTTOM, 100.0), None);
        }
    }

    #[test]
    fn moving_inward_on_the_edge_does_not_count() {
        let mut b = Barrier::default();
        let t = Instant::now();
        for _ in 0..50 {
            assert_eq!(b.motion(t, 50.0, 0.0, LEFT, 100.0), None);
        }
        assert_eq!(b.motion(t, -150.0, 0.0, LEFT, 100.0), Some(LEFT));
    }
}
