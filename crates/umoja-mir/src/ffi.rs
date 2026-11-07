//! The C ABI the MirAL shim calls into. Every entry point catches panics so a
//! bug in policy code can never unwind into Mir's C++ event loop.

use crate::outputs::{self, Direction};
use crate::tiling::{self, Action, Rect, TilingState};
use crate::workspaces::{self, Request};
use crate::{input, session};
use std::ffi::{CStr, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{LazyLock, Mutex};

static TILING: LazyLock<Mutex<TilingState>> = LazyLock::new(Default::default);

/// Pixels from a screen edge at which a dropped window snaps.
const SNAP_THRESHOLD: i32 = 8;
/// Height of the corner band that turns a side snap into a quarter snap.
const CORNER_BAND: i32 = 96;

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
pub extern "C" fn umoja_edge_zone(px: i32, py: i32, area: Rect) -> u32 {
    guard("edge_zone", 0, || {
        if !input::edge_tiling_enabled() {
            return 0;
        }
        tiling::edge_zone(px, py, area, SNAP_THRESHOLD, CORNER_BAND) as u32
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
        session::dispatch(Action::from_u32(action))
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
