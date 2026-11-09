//! GNOME's touchscreen gestures, recognised from raw touch frames.
//!
//! gnome-shell (js/ui/swipeTracker.js, edgeDragAction.js, windowManager.js):
//!
//! * three fingers sideways: switch workspace, the workspaces following the
//!   fingers, finishing or snapping back by distance and speed on release;
//! * three fingers up: the overview, then the app grid; down goes back;
//! * one finger from the bottom edge: the overview (then the app grid);
//! * one finger down from the top edge: the left part opens the calendar
//!   and notifications, the right part opens quick settings.
//!
//! The recogniser is pure: it gets every frame (all current points, as Mir
//! sends them) and says whether to keep the frame from the app under the
//! fingers, and what to do. Touches start out with the app; a gesture only
//! claims them once it is sure, and lift-offs always go through so the app
//! never keeps a finger stuck down.

use std::collections::HashMap;

type Pos = (f64, f64);

/// Mir's `MirTouchAction`.
pub const TOUCH_UP: u32 = 0;
pub const TOUCH_DOWN: u32 = 1;

/// How close to the top or bottom a touch must land to be an edge drag.
pub const EDGE_ZONE: f64 = 24.0;
/// How far an edge drag must come in before it fires (gnome-shell's
/// EDGE_THRESHOLD is 20 logical px, plus the zone itself).
pub const EDGE_TRIGGER: f64 = 48.0;
/// All of a three-finger gesture's fingers must land within this long.
pub const LAND_WINDOW_MS: f64 = 300.0;
/// Movement that decides a three-finger gesture's direction
/// (swipeTracker.js uses 16 logical px for touch).
pub const DIRECTION_THRESHOLD: f64 = 16.0;
/// Vertical travel that opens or closes a level of the overview.
pub const VERTICAL_TRIGGER: f64 = 80.0;
/// Fingers that may move before the touch is the app's for good.
pub const SLOP: f64 = 16.0;
/// Release speed (progress per ms) that finishes a swipe whatever its
/// distance, as swipeTracker.js's VELOCITY_THRESHOLD (0.4 px/ms on a
/// ~1300 px wide reference) does.
pub const FLICK_VELOCITY: f64 = 0.0003;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub id: i32,
    pub action: u32,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    Calendar,
    QuickSettings,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Out {
    /// Workspaces start following the fingers.
    SwipeBegin,
    /// How far towards the next workspace (positive) or the previous one.
    SwipeUpdate(f64),
    /// Fingers lifted: -1 previous, 1 next, 0 snap back.
    SwipeEnd(i32),
    OverviewUp,
    OverviewDown,
    OpenMenu(Menu),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Top,
    Bottom,
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Idle,
    /// Touches are with the app, undecided.
    Tracking {
        t0: f64,
        edge: Option<Edge>,
    },
    /// Three or more fingers, direction undecided.
    Multi {
        start: (f64, f64),
    },
    Horizontal {
        start_x: f64,
        progress: f64,
        samples: Vec<(f64, f64)>,
    },
    Vertical {
        start_y: f64,
        fired: bool,
    },
    /// The gesture is over; swallow the rest until the fingers lift.
    Done,
    /// The touches are the app's.
    App,
}

#[derive(Debug, Default)]
pub struct Recognizer {
    state: State,
    /// Where each finger down now first landed, and where it is.
    fingers: HashMap<i32, (Pos, Pos)>,
}

impl Recognizer {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// One touch frame. Returns whether the frame should be kept from apps.
    pub fn feed(
        &mut self,
        points: &[Point],
        t_ms: f64,
        screen: Screen,
        out: &mut Vec<Out>,
    ) -> bool {
        let any_down = points.iter().any(|p| p.action == TOUCH_DOWN);
        let any_up = points.iter().any(|p| p.action == TOUCH_UP);
        for p in points {
            match p.action {
                TOUCH_DOWN => {
                    self.fingers.insert(p.id, ((p.x, p.y), (p.x, p.y)));
                }
                _ => {
                    self.fingers
                        .entry(p.id)
                        .and_modify(|f| f.1 = (p.x, p.y))
                        .or_insert(((p.x, p.y), (p.x, p.y)));
                }
            }
        }
        let count = points.iter().filter(|p| p.action != TOUCH_UP).count();

        if matches!(self.state, State::Idle) && any_down {
            let edge = if count == 1 {
                let p = points.iter().find(|p| p.action == TOUCH_DOWN).unwrap();
                if p.y < screen.y + EDGE_ZONE {
                    Some(Edge::Top)
                } else if p.y >= screen.y + screen.h - EDGE_ZONE {
                    Some(Edge::Bottom)
                } else {
                    None
                }
            } else {
                None
            };
            self.state = State::Tracking { t0: t_ms, edge };
        }

        let consumed = self.step(count, t_ms, screen, out);

        for p in points.iter().filter(|p| p.action == TOUCH_UP) {
            self.fingers.remove(&p.id);
        }
        if count == 0 {
            self.state = State::Idle;
            self.fingers.clear();
        }
        // Lift-offs always reach the app, which may still hold those fingers.
        consumed && !any_up
    }

    fn centroid(&self) -> ((f64, f64), (f64, f64)) {
        let n = self.fingers.len().max(1) as f64;
        let (mut s, mut c) = ((0.0, 0.0), (0.0, 0.0));
        for (start, now) in self.fingers.values() {
            s.0 += start.0;
            s.1 += start.1;
            c.0 += now.0;
            c.1 += now.1;
        }
        ((s.0 / n, s.1 / n), (c.0 / n, c.1 / n))
    }

    fn moved_most(&self) -> f64 {
        self.fingers
            .values()
            .map(|(s, c)| ((c.0 - s.0).powi(2) + (c.1 - s.1).powi(2)).sqrt())
            .fold(0.0, f64::max)
    }

    fn step(&mut self, count: usize, t: f64, screen: Screen, out: &mut Vec<Out>) -> bool {
        match std::mem::take(&mut self.state) {
            State::Idle => false,
            State::App => {
                self.state = State::App;
                false
            }
            State::Done => {
                self.state = State::Done;
                true
            }
            State::Tracking { t0, edge } => {
                if count >= 3 {
                    if t - t0 <= LAND_WINDOW_MS {
                        let (_, now) = self.centroid();
                        self.state = State::Multi { start: now };
                        return true;
                    }
                    self.state = State::App;
                    return false;
                }
                if let (Some(edge), 1) = (edge, count) {
                    let &(start, now) = self.fingers.values().next().unwrap();
                    let inward = match edge {
                        Edge::Top => now.1 - start.1,
                        Edge::Bottom => start.1 - now.1,
                    };
                    if inward >= EDGE_TRIGGER {
                        out.push(match edge {
                            Edge::Bottom => Out::OverviewUp,
                            Edge::Top if start.0 >= screen.x + screen.w / 2.0 => {
                                Out::OpenMenu(Menu::QuickSettings)
                            }
                            Edge::Top => Out::OpenMenu(Menu::Calendar),
                        });
                        self.state = State::Done;
                        return true;
                    }
                    // Sideways first: an ordinary drag that started there.
                    if (now.0 - start.0).abs() > EDGE_TRIGGER {
                        self.state = State::App;
                        return false;
                    }
                    self.state = State::Tracking {
                        t0,
                        edge: Some(edge),
                    };
                    return false;
                }
                // Two fingers, or one that moved: a scroll, pinch or drag
                // for the app, unless more fingers are still landing.
                let late = t - t0 > LAND_WINDOW_MS;
                if late || (count == 1 && edge.is_none() && self.moved_most() > SLOP) {
                    self.state = State::App;
                } else {
                    self.state = State::Tracking {
                        t0,
                        edge: if count == 1 { edge } else { None },
                    };
                }
                false
            }
            State::Multi { start } => {
                if count < 3 {
                    self.state = State::Done;
                    return true;
                }
                let (_, now) = self.centroid();
                let (dx, dy) = (now.0 - start.0, now.1 - start.1);
                if dx.abs() >= DIRECTION_THRESHOLD && dx.abs() >= dy.abs() {
                    out.push(Out::SwipeBegin);
                    self.state = State::Horizontal {
                        start_x: now.0,
                        progress: 0.0,
                        samples: vec![(t, 0.0)],
                    };
                } else if dy.abs() >= DIRECTION_THRESHOLD {
                    self.state = State::Vertical {
                        start_y: start.1,
                        fired: false,
                    };
                    return self.step(count, t, screen, out);
                } else {
                    self.state = State::Multi { start };
                }
                true
            }
            State::Horizontal {
                start_x,
                progress,
                mut samples,
            } => {
                if count < 3 {
                    out.push(Out::SwipeEnd(Self::decide(progress, &samples)));
                    self.state = State::Done;
                    return true;
                }
                let (_, now) = self.centroid();
                // Fingers moving left bring in the workspace on the right.
                let p = -(now.0 - start_x) / screen.w.max(1.0);
                if p != progress {
                    out.push(Out::SwipeUpdate(p));
                }
                samples.push((t, p));
                samples.retain(|&(st, _)| t - st <= 100.0);
                self.state = State::Horizontal {
                    start_x,
                    progress: p,
                    samples,
                };
                true
            }
            State::Vertical { start_y, fired } => {
                if count < 3 {
                    self.state = State::Done;
                    return true;
                }
                let (_, now) = self.centroid();
                let dy = now.1 - start_y;
                let fired = fired
                    || if dy <= -VERTICAL_TRIGGER {
                        out.push(Out::OverviewUp);
                        true
                    } else if dy >= VERTICAL_TRIGGER {
                        out.push(Out::OverviewDown);
                        true
                    } else {
                        false
                    };
                self.state = State::Vertical { start_y, fired };
                true
            }
        }
    }

    /// swipeTracker.js: a quick flick finishes in its direction, otherwise
    /// past half way finishes and short of it snaps back.
    fn decide(progress: f64, samples: &[(f64, f64)]) -> i32 {
        let velocity = match (samples.first(), samples.last()) {
            (Some(a), Some(b)) if b.0 > a.0 => (b.1 - a.1) / (b.0 - a.0),
            _ => 0.0,
        };
        if velocity.abs() >= FLICK_VELOCITY {
            // A flick back towards the start cancels.
            if velocity.signum() != progress.signum() && progress != 0.0 {
                return 0;
            }
            return velocity.signum() as i32;
        }
        if progress.abs() >= 0.5 {
            progress.signum() as i32
        } else {
            0
        }
    }
}

/// Touchpad swipes from libinput (via our Mir patch's gesture hook), as
/// gnome-shell's TouchpadSwipeGesture: three or more fingers sideways
/// switch workspace following the fingers, up and down shift the overview.
/// Deltas are libinput's unaccelerated ones (1000 dpi units).
#[derive(Debug, Default)]
pub struct Touchpad {
    state: Pad,
}

#[derive(Debug, Default)]
enum Pad {
    #[default]
    Idle,
    Undecided {
        acc: Pos,
    },
    Horizontal {
        acc: f64,
        progress: f64,
        samples: Vec<(f64, f64)>,
    },
    Vertical {
        acc: f64,
        fired: bool,
    },
}

/// swipeTracker.js's TOUCHPAD_BASE_WIDTH: finger travel for one workspace.
pub const TOUCHPAD_WIDTH: f64 = 400.0;
/// Vertical travel that opens or closes a level of the overview (a fifth
/// of gnome-shell's TOUCHPAD_BASE_HEIGHT, where its progressive animation
/// would commit on release).
pub const TOUCHPAD_VERTICAL_TRIGGER: f64 = 60.0;

pub const GESTURE_SWIPE: i32 = 0;
pub const PHASE_BEGIN: i32 = 0;
#[allow(dead_code)] // what the hook sends between begin and end
pub const PHASE_UPDATE: i32 = 1;
pub const PHASE_END: i32 = 2;
pub const PHASE_CANCEL: i32 = 3;

impl Touchpad {
    #[allow(clippy::too_many_arguments)]
    pub fn gesture(
        &mut self,
        kind: i32,
        phase: i32,
        fingers: i32,
        dx: f64,
        dy: f64,
        t_ms: f64,
        out: &mut Vec<Out>,
    ) {
        if kind != GESTURE_SWIPE {
            return;
        }
        if phase == PHASE_BEGIN {
            self.state = if fingers >= 3 {
                Pad::Undecided { acc: (0.0, 0.0) }
            } else {
                Pad::Idle
            };
            return;
        }
        let ending = phase == PHASE_END || phase == PHASE_CANCEL;
        match std::mem::take(&mut self.state) {
            Pad::Idle => {}
            Pad::Undecided { acc } => {
                if ending {
                    return;
                }
                let acc = (acc.0 + dx, acc.1 + dy);
                if acc.0.abs() >= DIRECTION_THRESHOLD && acc.0.abs() >= acc.1.abs() {
                    out.push(Out::SwipeBegin);
                    let progress = -acc.0 / TOUCHPAD_WIDTH;
                    out.push(Out::SwipeUpdate(progress));
                    self.state = Pad::Horizontal {
                        acc: acc.0,
                        progress,
                        samples: vec![(t_ms, progress)],
                    };
                } else if acc.1.abs() >= DIRECTION_THRESHOLD {
                    self.state = Pad::Vertical {
                        acc: acc.1,
                        fired: false,
                    };
                    self.vertical_check(out);
                } else {
                    self.state = Pad::Undecided { acc };
                }
            }
            Pad::Horizontal {
                acc,
                progress,
                mut samples,
            } => {
                if ending {
                    let dir = if phase == PHASE_CANCEL {
                        0
                    } else {
                        Recognizer::decide(progress, &samples)
                    };
                    out.push(Out::SwipeEnd(dir));
                    return;
                }
                // Fingers moving left bring in the workspace on the right.
                let acc = acc + dx;
                let progress = -acc / TOUCHPAD_WIDTH;
                out.push(Out::SwipeUpdate(progress));
                samples.push((t_ms, progress));
                samples.retain(|&(st, _)| t_ms - st <= 100.0);
                self.state = Pad::Horizontal {
                    acc,
                    progress,
                    samples,
                };
            }
            Pad::Vertical { acc, fired } => {
                if ending {
                    return;
                }
                self.state = Pad::Vertical {
                    acc: acc + dy,
                    fired,
                };
                self.vertical_check(out);
            }
        }
    }

    fn vertical_check(&mut self, out: &mut Vec<Out>) {
        if let Pad::Vertical {
            acc,
            fired: fired @ false,
        } = &mut self.state
        {
            if *acc <= -TOUCHPAD_VERTICAL_TRIGGER {
                out.push(Out::OverviewUp);
                *fired = true;
            } else if *acc >= TOUCHPAD_VERTICAL_TRIGGER {
                out.push(Out::OverviewDown);
                *fired = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Screen = Screen {
        x: 0.0,
        y: 0.0,
        w: 1000.0,
        h: 800.0,
    };

    fn pt(id: i32, action: u32, x: f64, y: f64) -> Point {
        Point { id, action, x, y }
    }

    /// Three fingers land at (x, y) spaced 50 px, then the frames move them.
    fn three(r: &mut Recognizer, x: f64, y: f64, out: &mut Vec<Out>) -> Vec<bool> {
        vec![
            r.feed(&[pt(1, TOUCH_DOWN, x, y)], 0.0, S, out),
            r.feed(
                &[pt(1, 2, x, y), pt(2, TOUCH_DOWN, x + 50.0, y)],
                20.0,
                S,
                out,
            ),
            r.feed(
                &[
                    pt(1, 2, x, y),
                    pt(2, 2, x + 50.0, y),
                    pt(3, TOUCH_DOWN, x + 100.0, y),
                ],
                40.0,
                S,
                out,
            ),
        ]
    }

    fn mv(r: &mut Recognizer, x: f64, y: f64, t: f64, out: &mut Vec<Out>) -> bool {
        r.feed(
            &[
                pt(1, 2, x, y),
                pt(2, 2, x + 50.0, y),
                pt(3, 2, x + 100.0, y),
            ],
            t,
            S,
            out,
        )
    }

    fn lift(r: &mut Recognizer, x: f64, y: f64, t: f64, out: &mut Vec<Out>) -> bool {
        let a = r.feed(
            &[
                pt(1, TOUCH_UP, x, y),
                pt(2, 2, x + 50.0, y),
                pt(3, 2, x + 100.0, y),
            ],
            t,
            S,
            out,
        );
        r.feed(
            &[pt(2, TOUCH_UP, x + 50.0, y), pt(3, 2, x + 100.0, y)],
            t + 5.0,
            S,
            out,
        );
        r.feed(&[pt(3, TOUCH_UP, x + 100.0, y)], t + 10.0, S, out);
        a
    }

    #[test]
    fn three_finger_swipe_left_goes_to_next_workspace() {
        let mut r = Recognizer::new();
        let mut out = vec![];
        let c = three(&mut r, 500.0, 400.0, &mut out);
        assert_eq!(c, vec![false, false, true]);
        assert!(mv(&mut r, 480.0, 400.0, 60.0, &mut out));
        assert_eq!(out, vec![Out::SwipeBegin]);
        out.clear();
        let mut t = 60.0;
        for x in (100..480).rev().step_by(20) {
            t += 100.0; // slow, so distance decides
            mv(&mut r, x as f64, 400.0, t, &mut out);
        }
        assert!(
            matches!(out.last(), Some(Out::SwipeUpdate(p)) if (*p - 0.361).abs() < 0.01),
            "{out:?}"
        );
        out.clear();
        mv(&mut r, -50.0, 400.0, t + 300.0, &mut out);
        out.clear();
        assert!(
            !lift(&mut r, -50.0, 400.0, t + 600.0, &mut out),
            "lift-offs reach the app"
        );
        assert_eq!(out, vec![Out::SwipeEnd(1)]);
        assert!(matches!(r.state, State::Idle));
    }

    #[test]
    fn short_slow_swipe_snaps_back() {
        let mut r = Recognizer::new();
        let mut out = vec![];
        three(&mut r, 500.0, 400.0, &mut out);
        mv(&mut r, 520.0, 400.0, 60.0, &mut out);
        mv(&mut r, 700.0, 400.0, 1000.0, &mut out);
        mv(&mut r, 700.0, 400.0, 1500.0, &mut out);
        out.clear();
        lift(&mut r, 700.0, 400.0, 1600.0, &mut out);
        assert_eq!(out, vec![Out::SwipeEnd(0)]);
    }

    #[test]
    fn quick_flick_finishes() {
        let mut r = Recognizer::new();
        let mut out = vec![];
        three(&mut r, 500.0, 400.0, &mut out);
        mv(&mut r, 520.0, 400.0, 60.0, &mut out);
        mv(&mut r, 560.0, 400.0, 80.0, &mut out);
        mv(&mut r, 620.0, 400.0, 100.0, &mut out);
        out.clear();
        lift(&mut r, 620.0, 400.0, 110.0, &mut out);
        assert_eq!(out, vec![Out::SwipeEnd(-1)]);
    }

    #[test]
    fn three_fingers_up_and_down_shift_the_overview() {
        let mut r = Recognizer::new();
        let mut out = vec![];
        three(&mut r, 400.0, 500.0, &mut out);
        mv(&mut r, 400.0, 480.0, 60.0, &mut out);
        mv(&mut r, 400.0, 400.0, 80.0, &mut out);
        mv(&mut r, 400.0, 300.0, 100.0, &mut out);
        assert_eq!(out, vec![Out::OverviewUp], "fires once");
        out.clear();
        lift(&mut r, 400.0, 300.0, 120.0, &mut out);
        three(&mut r, 400.0, 300.0, &mut out);
        mv(&mut r, 400.0, 420.0, 60.0, &mut out);
        assert_eq!(out, vec![Out::OverviewDown]);
    }

    #[test]
    fn edge_drags() {
        let mut out = vec![];
        let mut r = Recognizer::new();
        assert!(!r.feed(&[pt(1, TOUCH_DOWN, 500.0, 795.0)], 0.0, S, &mut out));
        assert!(!r.feed(&[pt(1, 2, 500.0, 770.0)], 10.0, S, &mut out));
        assert!(r.feed(&[pt(1, 2, 500.0, 740.0)], 20.0, S, &mut out));
        assert!(r.feed(&[pt(1, 2, 500.0, 600.0)], 30.0, S, &mut out));
        assert!(!r.feed(&[pt(1, TOUCH_UP, 500.0, 600.0)], 40.0, S, &mut out));
        assert_eq!(out, vec![Out::OverviewUp]);

        out.clear();
        r.feed(&[pt(1, TOUCH_DOWN, 900.0, 5.0)], 0.0, S, &mut out);
        r.feed(&[pt(1, 2, 900.0, 80.0)], 10.0, S, &mut out);
        r.feed(&[pt(1, TOUCH_UP, 900.0, 80.0)], 20.0, S, &mut out);
        r.feed(&[pt(1, TOUCH_DOWN, 100.0, 5.0)], 100.0, S, &mut out);
        r.feed(&[pt(1, 2, 100.0, 80.0)], 110.0, S, &mut out);
        r.feed(&[pt(1, TOUCH_UP, 100.0, 80.0)], 120.0, S, &mut out);
        assert_eq!(
            out,
            vec![
                Out::OpenMenu(Menu::QuickSettings),
                Out::OpenMenu(Menu::Calendar)
            ]
        );
    }

    #[test]
    fn taps_and_app_drags_are_left_alone() {
        let mut out = vec![];
        let mut r = Recognizer::new();
        // A tap on the top bar.
        assert!(!r.feed(&[pt(1, TOUCH_DOWN, 100.0, 5.0)], 0.0, S, &mut out));
        assert!(!r.feed(&[pt(1, TOUCH_UP, 100.0, 5.0)], 50.0, S, &mut out));
        // A one-finger drag in the middle, even a long downward one.
        for (i, y) in [400.0, 420.0, 500.0, 700.0].iter().enumerate() {
            let a = if i == 0 { TOUCH_DOWN } else { 2 };
            assert!(!r.feed(&[pt(1, a, 400.0, *y)], i as f64 * 10.0, S, &mut out));
        }
        r.feed(&[pt(1, TOUCH_UP, 400.0, 700.0)], 50.0, S, &mut out);
        // A slow third finger joining a pinch stays with the app.
        r.feed(&[pt(1, TOUCH_DOWN, 300.0, 300.0)], 0.0, S, &mut out);
        r.feed(
            &[pt(1, 2, 300.0, 300.0), pt(2, TOUCH_DOWN, 400.0, 300.0)],
            10.0,
            S,
            &mut out,
        );
        let c = r.feed(
            &[
                pt(1, 2, 300.0, 300.0),
                pt(2, 2, 400.0, 300.0),
                pt(3, TOUCH_DOWN, 500.0, 300.0),
            ],
            900.0,
            S,
            &mut out,
        );
        assert!(!c);
        assert!(!mv(&mut r, 100.0, 300.0, 950.0, &mut out));
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn touchpad_three_finger_swipe() {
        let mut t = Touchpad::default();
        let mut out = vec![];
        t.gesture(0, PHASE_BEGIN, 3, 0.0, 0.0, 0.0, &mut out);
        t.gesture(0, PHASE_UPDATE, 3, -10.0, 1.0, 10.0, &mut out);
        assert!(out.is_empty());
        t.gesture(0, PHASE_UPDATE, 3, -10.0, 1.0, 20.0, &mut out);
        assert_eq!(out, vec![Out::SwipeBegin, Out::SwipeUpdate(0.05)]);
        out.clear();
        let mut ts = 20.0;
        for _ in 0..20 {
            ts += 100.0;
            t.gesture(0, PHASE_UPDATE, 3, -10.0, 0.0, ts, &mut out);
        }
        out.clear();
        t.gesture(0, PHASE_END, 3, 0.0, 0.0, ts + 200.0, &mut out);
        assert_eq!(out, vec![Out::SwipeEnd(1)], "past half way");
    }

    #[test]
    fn touchpad_cancel_and_two_fingers() {
        let mut t = Touchpad::default();
        let mut out = vec![];
        t.gesture(0, PHASE_BEGIN, 4, 0.0, 0.0, 0.0, &mut out);
        t.gesture(0, PHASE_UPDATE, 4, 300.0, 0.0, 10.0, &mut out);
        out.clear();
        t.gesture(0, PHASE_CANCEL, 4, 0.0, 0.0, 20.0, &mut out);
        assert_eq!(out, vec![Out::SwipeEnd(0)]);
        out.clear();
        t.gesture(0, PHASE_BEGIN, 2, 0.0, 0.0, 0.0, &mut out);
        t.gesture(0, PHASE_UPDATE, 2, 300.0, 0.0, 10.0, &mut out);
        t.gesture(1, PHASE_BEGIN, 3, 0.0, 0.0, 0.0, &mut out);
        t.gesture(1, PHASE_UPDATE, 3, 300.0, 0.0, 10.0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn touchpad_vertical_shifts_overview_once() {
        let mut t = Touchpad::default();
        let mut out = vec![];
        t.gesture(0, PHASE_BEGIN, 3, 0.0, 0.0, 0.0, &mut out);
        for i in 0..10 {
            t.gesture(0, PHASE_UPDATE, 3, 1.0, -20.0, i as f64, &mut out);
        }
        t.gesture(0, PHASE_END, 3, 0.0, 0.0, 20.0, &mut out);
        assert_eq!(out, vec![Out::OverviewUp]);
        out.clear();
        t.gesture(0, PHASE_BEGIN, 3, 0.0, 0.0, 0.0, &mut out);
        t.gesture(0, PHASE_UPDATE, 3, 0.0, 90.0, 1.0, &mut out);
        assert_eq!(out, vec![Out::OverviewDown]);
    }
}
