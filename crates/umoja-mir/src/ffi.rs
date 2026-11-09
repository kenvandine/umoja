//! The C ABI the MirAL shim calls into. Every entry point catches panics so a
//! bug in policy code can never unwind into Mir's C++ event loop.

use crate::outputs::{self, Direction};
use crate::switcher::{self, Outcome};
use crate::tiling::{self, Action, Rect, TilingState};
use crate::workspaces::{self, Request};
use crate::{gestures, input, session, shell};
use std::ffi::{CStr, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{LazyLock, Mutex};

static TILING: LazyLock<Mutex<TilingState>> = LazyLock::new(Default::default);

#[repr(C)]
pub struct UmojaConfig {
    pub extensions: *const *const c_char,
    pub n_extensions: usize,
    pub enable_x11: bool,
}

unsafe extern "C" {
    pub fn umoja_mir_run(
        argc: c_int,
        argv: *const *const c_char,
        config: *const UmojaConfig,
    ) -> c_int;
}

fn guard<T>(what: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            tracing::error!(what, "panic caught at compositor FFI boundary");
            fallback
        }
    }
}

fn tiling() -> std::sync::MutexGuard<'static, TilingState> {
    TILING.lock().unwrap_or_else(|e| e.into_inner())
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_lookup_binding(keysym: u32, modifiers: u32) -> u32 {
    guard("lookup_binding", 0, || input::lookup(keysym, modifiers))
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_overlay_key_matches(keysym: u32) -> bool {
    guard("overlay_key", false, || input::is_overlay_key(keysym))
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_tile_geometry(action: u32, area: Rect, out: *mut Rect) -> bool {
    guard("tile_geometry", false, || {
        match tiling::geometry(Action::from_u32(action), area) {
            Some(r) if !out.is_null() => {
                unsafe { *out = r };
                true
            }
            _ => false,
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_tiling_register(id: u64, action: u32, rect: Rect) {
    guard("tiling_register", (), || {
        tiling().register(id, Action::from_u32(action), rect)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_tiling_unregister(id: u64) {
    guard("tiling_unregister", (), || tiling().unregister(id))
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_tiling_resized(
    id: u64,
    new_rect: Rect,
    out_ids: *mut u64,
    out_rects: *mut Rect,
    max: usize,
) -> usize {
    guard("tiling_resized", 0, || {
        if out_ids.is_null() || out_rects.is_null() {
            return 0;
        }
        let changes = tiling().resized(id, new_rect);
        let n = changes.len().min(max);
        for (i, (cid, r)) in changes.into_iter().take(n).enumerate() {
            unsafe {
                *out_ids.add(i) = cid;
                *out_rects.add(i) = r;
            }
        }
        n
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_handle_action(action: u32) {
    guard("handle_action", (), || {
        if action >= input::CUSTOM_BASE {
            input::run_custom(action)
        } else {
            session::dispatch(Action::from_u32(action))
        }
    })
}

/// Whether scroll events from this source class (`UMOJA_SCROLL_*`) should be
/// inverted ("natural" scrolling). Called for every scroll event.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_natural_scroll(source: u32) -> bool {
    guard("natural_scroll", false, || {
        crate::peripherals::natural_scroll(source)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_session_started(wayland_display: *const c_char) {
    guard("session_started", (), || {
        let display = if wayland_display.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(wayland_display) }
                .to_string_lossy()
                .into_owned()
        };
        session::started(&display);
    })
}

/// Resolve a workspace action. `kind` is 1 for switch, 2 for move-window.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_workspace_resolve(
    code: u32,
    current: usize,
    count: usize,
    out_kind: *mut u32,
    out_index: *mut usize,
) -> bool {
    guard("workspace_resolve", false, || {
        let Some(req) = workspaces::resolve(code, current, count) else {
            return false;
        };
        if out_kind.is_null() || out_index.is_null() {
            return false;
        }
        let (kind, index) = match req {
            Request::Switch(i) => (1, i),
            Request::MoveWindow(i) => (2, i),
        };
        unsafe {
            *out_kind = kind;
            *out_index = index;
        }
        true
    })
}

/// Given which workspaces hold windows, return how many should exist and
/// write the indices to delete (highest first) into `out_remove`.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_workspace_layout(
    occupied: *const bool,
    n: usize,
    current: usize,
    out_remove: *mut usize,
    max_remove: usize,
    out_n_remove: *mut usize,
) -> usize {
    guard("workspace_layout", n.max(1), || {
        let occ = if occupied.is_null() || n == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(occupied, n) }
        };
        let settings = input::read().workspaces;
        let remove = workspaces::removable(&settings, occ, current);
        let k = remove.len().min(max_remove);
        if !out_remove.is_null() {
            for (i, idx) in remove.iter().take(k).enumerate() {
                unsafe { *out_remove.add(i) = *idx };
            }
        }
        if !out_n_remove.is_null() {
            unsafe { *out_n_remove = if out_remove.is_null() { 0 } else { k } };
        }
        // Count after removals: removed ones were never occupied.
        let remaining: Vec<bool> = occ
            .iter()
            .enumerate()
            .filter(|(i, _)| !remove[..k].contains(i))
            .map(|(_, o)| *o)
            .collect();
        let current_after = current - remove[..k].iter().filter(|&&r| r < current).count();
        workspaces::desired_count(&settings, &remaining, current_after)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_workspaces_only_on_primary() -> bool {
    guard("workspaces_only_on_primary", true, || {
        input::read().workspaces.only_on_primary
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_center_new_windows() -> bool {
    guard("center_new_windows", false, || {
        input::read().center_new_windows
    })
}

/// Index into `rects` of the output next to `from` for a move-to-monitor
/// action, or -1.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_neighbor_output(
    action: u32,
    from: Rect,
    rects: *const Rect,
    n: usize,
) -> i32 {
    guard("neighbor_output", -1, || {
        let dir = match Action::from_u32(action) {
            Action::MoveToMonitorLeft => Direction::Left,
            Action::MoveToMonitorRight => Direction::Right,
            Action::MoveToMonitorUp => Direction::Up,
            Action::MoveToMonitorDown => Direction::Down,
            _ => return -1,
        };
        if rects.is_null() {
            return -1;
        }
        let outs = unsafe { std::slice::from_raw_parts(rects, n) };
        outputs::neighbor(dir, from, outs)
            .map(|i| i as i32)
            .unwrap_or(-1)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_translate_rect(window: Rect, from: Rect, to: Rect) -> Rect {
    guard("translate_rect", window, || {
        outputs::translate(window, from, to)
    })
}

/// Alt+Print: capture just this window's rectangle.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_screenshot_window(rect: Rect) {
    guard("screenshot_window", (), || session::screenshot_area(rect))
}

/// Mir created or changed an output.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_output_update(
    id: i32,
    name: *const c_char,
    extents: Rect,
    scale: f32,
    refresh: f64,
    width_mm: u32,
    height_mm: u32,
    orientation: u32,
) {
    guard("output_update", (), || {
        let name = if name.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        };
        crate::display_config::output_updated(
            id,
            crate::display_config::OutputState {
                name,
                rect: extents,
                scale: scale as f64,
                refresh,
                width_mm,
                height_mm,
                orientation,
            },
        );
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_output_remove(id: i32) {
    guard("output_remove", (), || {
        crate::display_config::output_removed(id)
    })
}

// ---- switcher ---------------------------------------------------------------

#[repr(C)]
pub struct SwitchWindow {
    pub id: u64,
    pub app_id: *const c_char,
    pub title: *const c_char,
}

const SWITCH_MAX: usize = 64;

#[repr(C)]
pub struct SwitchResult {
    pub window: u64,
    pub ids: [u64; SWITCH_MAX],
    pub n_ids: usize,
}

pub const SWITCH_NOT_OURS: u32 = 0;
pub const SWITCH_CONSUMED: u32 = 1;
pub const SWITCH_ACTIVATE: u32 = 2;
pub const SWITCH_CLOSE: u32 = 3;
pub const SWITCH_CANCEL: u32 = 4;

fn lossy(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn write_outcome(out: Option<Outcome>, res: *mut SwitchResult) -> u32 {
    let Some(out) = out else {
        return SWITCH_NOT_OURS;
    };
    let fill = |ids: &[u64]| {
        if res.is_null() {
            return;
        }
        let r = unsafe { &mut *res };
        let n = ids.len().min(SWITCH_MAX);
        r.ids[..n].copy_from_slice(&ids[..n]);
        r.n_ids = n;
    };
    match out {
        Outcome::Consumed => SWITCH_CONSUMED,
        Outcome::Cancel => SWITCH_CANCEL,
        Outcome::Activate { window, raise } => {
            fill(&raise);
            if !res.is_null() {
                unsafe { (*res).window = window };
            }
            SWITCH_ACTIVATE
        }
        Outcome::Close(ids) => {
            fill(&ids);
            SWITCH_CLOSE
        }
    }
}

/// True when `action` is one of the Alt+Tab family.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_is_switcher_action(action: u32) -> bool {
    guard("is_switcher_action", false, || {
        shell::switcher_binding(action).is_some()
    })
}

/// Open a switcher. `windows` is in MRU order (focused first).
#[unsafe(no_mangle)]
pub extern "C" fn umoja_switcher_open(
    action: u32,
    mods: u32,
    windows: *const SwitchWindow,
    n: usize,
) -> bool {
    guard("switcher_open", false, || {
        let list = if windows.is_null() || n == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(windows, n) }
        };
        let mru = list
            .iter()
            .map(|w| switcher::Window {
                id: w.id,
                app_id: lossy(w.app_id),
                title: lossy(w.title),
            })
            .collect();
        shell::open_switcher(action, mods, mru)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_switcher_key(
    keysym: u32,
    mods: u32,
    down: bool,
    out: *mut SwitchResult,
) -> u32 {
    guard("switcher_key", SWITCH_NOT_OURS, || {
        write_outcome(shell::switcher_key(keysym, mods, down), out)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_switcher_pick(index: u32, out: *mut SwitchResult) -> u32 {
    guard("switcher_pick", SWITCH_NOT_OURS, || {
        write_outcome(shell::switcher_pick(index as usize), out)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_switcher_window_closed(id: u64) {
    guard("switcher_window_closed", (), || {
        shell::switcher_window_closed(id)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_switcher_cancel() {
    guard("switcher_cancel", (), shell::switcher_cancel)
}

// ---- drag to snap ----------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn umoja_drag_begin(px: i32, py: i32, window: Rect, area: Rect, monitor: Rect) {
    guard("drag_begin", (), || {
        shell::drag_begin(px, py, window, area, monitor)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_drag_motion(
    px: i32,
    py: i32,
    window: Rect,
    area: Rect,
    monitor: Rect,
) -> u32 {
    guard("drag_motion", 0, || {
        shell::drag_motion(px, py, window, area, monitor) as u32
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_drag_end() -> u32 {
    guard("drag_end", 0, || shell::drag_end() as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_raise_tile_group() -> bool {
    guard("raise_tile_group", true, || input::read().raise_tile_group)
}

#[unsafe(no_mangle)]
pub extern "C" fn umoja_is_tile_action(action: u32) -> bool {
    guard("is_tile_action", false, || {
        tiling::is_tile(Action::from_u32(action))
    })
}

/// A keyboard workspace switch happened: show GNOME's switcher popup.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_workspace_switched(current: usize, count: usize) {
    guard("workspace_switched", (), || {
        session::overlay_call("WorkspaceSwitched", (current as u32, count as u32))
    })
}

/// The workspace list or the active workspace may have changed: tell the
/// top bar's indicator and the overview (deduplicated in [`shell`]).
#[unsafe(no_mangle)]
pub extern "C" fn umoja_workspaces_changed(current: usize, count: usize) {
    guard("workspaces_changed", (), || {
        shell::workspaces_changed(current as u32, count as u32)
    })
}

/// Mir dimmed, blanked or woke the screen (`UMOJA_IDLE_*`).
#[unsafe(no_mangle)]
pub extern "C" fn umoja_idle_state(state: u32) {
    guard("idle_state", (), || crate::idle::mir_state(state))
}

/// One touch point from Mir (`action`: 0 up, 1 down, 2 moved).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UmojaTouchPoint {
    pub id: i32,
    pub action: u32,
    pub x: f32,
    pub y: f32,
}

/// What the shim does with a touch frame. `swipe` is a set of
/// `UMOJA_SWIPE_*` bits, applied begin, update, end.
#[repr(C)]
pub struct UmojaTouchResult {
    pub consume: bool,
    pub swipe: u32,
    pub progress: f64,
    pub dir: i32,
}

pub const SWIPE_BEGIN: u32 = 1;
pub const SWIPE_UPDATE: u32 = 2;
pub const SWIPE_END: u32 = 4;

unsafe extern "C" {
    fn umoja_wm_workspace_swipe(flags: u32, progress: f64, dir: i32);
}

static TOUCHPAD: LazyLock<Mutex<gestures::Touchpad>> = LazyLock::new(Default::default);

/// Touchpad gestures from libinput. Mir drops them; our Mir patch (see
/// packaging/mir) looks this function up in the executable and calls it on
/// its input thread. `kind`: 0 swipe, 1 pinch, 2 hold; `phase`: 0 begin,
/// 1 update, 2 end, 3 cancelled. Exported by build.rs's linker flags.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn mir_touchpad_gesture_hook(
    kind: c_int,
    phase: c_int,
    fingers: c_int,
    dx: f64,
    dy: f64,
    _scale: f64,
    _angle: f64,
    time_usec: u64,
) {
    guard("touchpad_gesture", (), || {
        let mut outs = Vec::new();
        TOUCHPAD.lock().unwrap_or_else(|e| e.into_inner()).gesture(
            kind,
            phase,
            fingers,
            dx,
            dy,
            time_usec as f64 / 1e3,
            &mut outs,
        );
        let (mut flags, mut progress, mut dir) = (0, 0.0, 0);
        for o in outs {
            apply_out(o, &mut flags, &mut progress, &mut dir);
        }
        unsafe { umoja_wm_workspace_swipe(flags, progress, dir) };
    })
}

fn apply_out(o: gestures::Out, flags: &mut u32, progress: &mut f64, dir: &mut i32) {
    match o {
        gestures::Out::SwipeBegin => *flags |= SWIPE_BEGIN,
        gestures::Out::SwipeUpdate(p) => {
            *flags |= SWIPE_UPDATE;
            *progress = p;
        }
        gestures::Out::SwipeEnd(d) => {
            *flags |= SWIPE_END;
            *dir = d;
        }
        gestures::Out::OverviewUp => session::overlay_call("ShiftOverview", (true,)),
        gestures::Out::OverviewDown => session::overlay_call("ShiftOverview", (false,)),
        gestures::Out::OpenMenu(m) => session::panel_call(
            "OpenMenu",
            (match m {
                gestures::Menu::Calendar => "calendar",
                gestures::Menu::QuickSettings => "quick-settings",
            },),
        ),
    }
}

static GESTURES: LazyLock<Mutex<gestures::Recognizer>> = LazyLock::new(Default::default);

/// A touch frame for GNOME's touchscreen gestures ([`gestures`]). `screen`
/// is the output the gesture is on. Returns whether to keep it from apps.
#[unsafe(no_mangle)]
pub extern "C" fn umoja_touch(
    points: *const UmojaTouchPoint,
    n: usize,
    time_ns: u64,
    screen: Rect,
    out: *mut UmojaTouchResult,
) -> bool {
    guard("touch", false, || {
        if points.is_null() || out.is_null() {
            return false;
        }
        let pts: Vec<gestures::Point> = unsafe { std::slice::from_raw_parts(points, n) }
            .iter()
            .map(|p| gestures::Point {
                id: p.id,
                action: p.action,
                x: p.x as f64,
                y: p.y as f64,
            })
            .collect();
        let screen = gestures::Screen {
            x: screen.x as f64,
            y: screen.y as f64,
            w: screen.w as f64,
            h: screen.h as f64,
        };
        let mut outs = Vec::new();
        let consume = GESTURES.lock().unwrap_or_else(|e| e.into_inner()).feed(
            &pts,
            time_ns as f64 / 1e6,
            screen,
            &mut outs,
        );
        let res = unsafe { &mut *out };
        *res = UmojaTouchResult {
            consume,
            swipe: 0,
            progress: 0.0,
            dir: 0,
        };
        for o in outs {
            apply_out(o, &mut res.swipe, &mut res.progress, &mut res.dir);
        }
        consume
    })
}
