//! Window-management chrome shared with umoja-overlay.
//!
//! The compositor decides, the overlay draws. This module holds the
//! compositor's half of three GNOME features:
//!
//! * the Alt+Tab / Super+Tab switcher ([`crate::switcher`]),
//! * the tile preview and layout picker shown while dragging
//!   ([`crate::snap`]),
//! * `org.umoja.Compositor` on the session bus, which the overlay uses to
//!   list windows for the overview (window spread) and to activate, close
//!   or tile them.

use crate::input;
use crate::session::overlay_call;
use crate::snap::{self, Chrome, Visibility};
use crate::switcher::{Mode, Outcome, Switcher, Window};
use crate::tiling::{Action, Rect};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::{LazyLock, Mutex};
use zbus::interface;

pub const BUS_NAME: &str = "org.umoja.Compositor";
pub const OBJECT_PATH: &str = "/org/umoja/Compositor";

// ---- switcher ---------------------------------------------------------------

struct Open {
    switcher: Switcher,
    titles: HashMap<u64, String>,
}

static SWITCHER: LazyLock<Mutex<Option<Open>>> = LazyLock::new(Default::default);

fn switcher() -> std::sync::MutexGuard<'static, Option<Open>> {
    SWITCHER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Which switcher a bound action opens, and in which direction.
pub fn switcher_binding(action: u32) -> Option<(Mode, bool)> {
    Some(match Action::from_u32(action) {
        Action::SwitchApplications => (Mode::Applications, false),
        Action::SwitchApplicationsBackward => (Mode::Applications, true),
        Action::SwitchGroup => (Mode::Group, false),
        Action::SwitchGroupBackward => (Mode::Group, true),
        Action::SwitchWindows => (Mode::Windows, false),
        Action::SwitchWindowsBackward => (Mode::Windows, true),
        _ => return None,
    })
}

fn mode_code(m: Mode) -> u32 {
    match m {
        Mode::Applications => 0,
        Mode::Group => 1,
        Mode::Windows => 2,
    }
}

fn announce(open: &Open) {
    let items: Vec<(String, String, Vec<String>)> = open
        .switcher
        .items
        .iter()
        .map(|i| {
            let titles = i
                .windows
                .iter()
                .map(|w| open.titles.get(w).cloned().unwrap_or_default())
                .collect();
            (i.app_id.clone(), i.title.clone(), titles)
        })
        .collect();
    let (sel, sub) = open.switcher.selection();
    overlay_call(
        "ShowSwitcher",
        (
            mode_code(open.switcher.mode),
            items,
            sel as u32,
            sub.map(|s| s as i32).unwrap_or(-1),
        ),
    );
}

fn announce_selection(open: &Open) {
    let (sel, sub) = open.switcher.selection();
    overlay_call(
        "SwitcherSelect",
        (sel as u32, sub.map(|s| s as i32).unwrap_or(-1)),
    );
}

/// Open the switcher for `action` with windows in MRU order.
pub fn open_switcher(action: u32, mods: u32, mru: Vec<Window>) -> bool {
    let Some((mode, backward)) = switcher_binding(action) else {
        return false;
    };
    // Only the modifiers of the shortcut keep it open (Shift is direction).
    let hold = mods & !input::MOD_SHIFT;
    if hold == 0 {
        return false;
    }
    let titles = mru.iter().map(|w| (w.id, w.title.clone())).collect();
    let Some(s) = Switcher::open(mode, backward, &mru, hold) else {
        return false;
    };
    let open = Open {
        switcher: s,
        titles,
    };
    announce(&open);
    *switcher() = Some(open);
    true
}

const XK_SHIFT_L: u32 = 0xffe1;
const XK_SHIFT_R: u32 = 0xffe2;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_CONTROL_R: u32 = 0xffe4;
const XK_META_L: u32 = 0xffe7;
const XK_META_R: u32 = 0xffe8;
const XK_ALT_L: u32 = 0xffe9;
const XK_ALT_R: u32 = 0xffea;
const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;
const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;

fn modifier_bit(keysym: u32) -> u32 {
    match keysym {
        XK_SHIFT_L | XK_SHIFT_R => input::MOD_SHIFT,
        XK_CONTROL_L | XK_CONTROL_R => input::MOD_CTRL,
        XK_ALT_L | XK_ALT_R | XK_META_L | XK_META_R | XK_ISO_LEVEL3_SHIFT => input::MOD_ALT,
        XK_SUPER_L | XK_SUPER_R => input::MOD_SUPER,
        _ => 0,
    }
}

/// A key event while the switcher may be open. `None` means "not ours".
pub fn switcher_key(keysym: u32, mods: u32, down: bool) -> Option<Outcome> {
    let mut guard = switcher();
    let open = guard.as_mut()?;
    let hold = open.switcher.modifiers;
    let released = if down {
        mods & hold == 0
    } else {
        // The key-up of the held modifier itself; its bit may still be set.
        modifier_bit(keysym) & hold != 0 && (mods & !modifier_bit(keysym)) & hold == 0
    };
    if released {
        let out = open.switcher.finish();
        *guard = None;
        overlay_call("HideSwitcher", ());
        return Some(out);
    }
    if !down || modifier_bit(keysym) != 0 {
        return Some(Outcome::Consumed);
    }
    let binding = match input::lookup(keysym, mods) {
        0 => None,
        a => switcher_binding(a),
    };
    let out = open.switcher.key_down(keysym, mods, binding);
    match &out {
        Outcome::Consumed => announce_selection(open),
        Outcome::Cancel | Outcome::Activate { .. } => {
            *guard = None;
            overlay_call("HideSwitcher", ());
        }
        Outcome::Close(_) => {}
    }
    Some(out)
}

pub fn switcher_pick(index: usize) -> Option<Outcome> {
    let mut guard = switcher();
    let out = guard.as_mut()?.switcher.pick(index);
    if out.is_some() {
        *guard = None;
        overlay_call("HideSwitcher", ());
    }
    out
}

pub fn switcher_window_closed(id: u64) {
    let mut guard = switcher();
    let Some(open) = guard.as_mut() else { return };
    if open.switcher.remove_window(id) {
        announce(open);
    } else {
        *guard = None;
        overlay_call("HideSwitcher", ());
    }
}

pub fn switcher_cancel() {
    if switcher().take().is_some() {
        overlay_call("HideSwitcher", ());
    }
}

// ---- drag chrome ------------------------------------------------------------

static DRAG: LazyLock<Mutex<Option<Chrome>>> = LazyLock::new(Default::default);

fn send_chrome(old: Option<&Chrome>, new: &Chrome, window: Rect) {
    let picker_changed = old.is_none_or(|o| {
        o.picker != new.picker || o.picker_tile != new.picker_tile || o.area != new.area
    });
    if picker_changed {
        overlay_call(
            "LayoutPicker",
            (
                new.picker as u32,
                new.picker_tile as u32,
                snap::picker_x(new.area),
                new.area.y,
            ),
        );
    }
    if old.is_none_or(|o| o.preview != new.preview) {
        let p = new.preview.unwrap_or_default();
        overlay_call(
            "TilePreview",
            (
                new.preview.is_some(),
                p.x,
                p.y,
                p.w,
                p.h,
                window.x,
                window.y,
                window.w,
                window.h,
            ),
        );
    }
}

/// A window move started (picker peeks out under the top bar).
pub fn drag_begin(px: i32, py: i32, window: Rect, area: Rect, monitor: Rect) {
    let use_picker = input::read().layout_picker && input::edge_tiling_enabled();
    let mut chrome = snap::evaluate(px, py, window, area, monitor, use_picker);
    if !input::edge_tiling_enabled() {
        chrome.action = Action::None;
        chrome.preview = None;
    }
    send_chrome(None, &chrome, window);
    *DRAG.lock().unwrap_or_else(|e| e.into_inner()) = Some(chrome);
}

/// Pointer moved during a window move; returns the snap action so far.
pub fn drag_motion(px: i32, py: i32, window: Rect, area: Rect, monitor: Rect) -> Action {
    let mut guard = DRAG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(old) = guard.as_ref() else {
        return Action::None;
    };
    if !input::edge_tiling_enabled() {
        return Action::None;
    }
    let use_picker = old.picker != Visibility::Hidden;
    let chrome = snap::evaluate(px, py, window, area, monitor, use_picker);
    send_chrome(Some(old), &chrome, window);
    let action = chrome.action;
    *guard = Some(chrome);
    action
}

/// The move ended; returns the action to apply on drop.
pub fn drag_end() -> Action {
    let Some(chrome) = DRAG.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return Action::None;
    };
    if chrome.picker != Visibility::Hidden {
        overlay_call("LayoutPicker", (0u32, 0u32, 0i32, 0i32));
    }
    overlay_call(
        "TilePreview",
        (false, 0i32, 0i32, 0i32, 0i32, 0i32, 0i32, 0i32, 0i32),
    );
    chrome.action
}

// ---- org.umoja.Compositor ---------------------------------------------------

/// One window as listed over D-Bus.
#[repr(C)]
pub struct WindowDesc {
    pub id: u64,
    pub app_id: *const c_char,
    pub title: *const c_char,
    pub rect: Rect,
    pub flags: u32,
    /// 0-based, or -1 for a window on every workspace.
    pub workspace: i32,
}

// Flags: 1 focused, 2 minimized, 4 on the current workspace, 8 maximized,
// 16 fullscreen, 32 tiled (UMOJA_WINDOW_* in the shim header).

unsafe extern "C" {
    fn umoja_wm_list_windows(
        cb: extern "C" fn(*mut c_void, *const WindowDesc),
        ctx: *mut c_void,
        mru: bool,
    );
    fn umoja_wm_activate(id: u64);
    fn umoja_wm_close(id: u64);
    fn umoja_wm_switcher_pick(index: u32);
    fn umoja_wm_tile(id: u64, action: u32);
    fn umoja_wm_workspaces(current: *mut u32, count: *mut u32);
    fn umoja_wm_switch_workspace(index: u32);
    fn umoja_wm_move_to_workspace(id: u64, index: u32);
    fn umoja_wm_show_workspace(index: u32);
    fn umoja_wm_send_to_workspace(id: u64, index: u32);
    fn umoja_wm_workspace_settings_changed();
}

/// Re-apply GNOME's workspace settings after they changed.
pub fn workspace_settings_changed() {
    unsafe { umoja_wm_workspace_settings_changed() }
}

pub type WindowRow = (u64, String, String, i32, i32, i32, i32, u32);

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

extern "C" fn collect_workspaces(ctx: *mut c_void, desc: *const WindowDesc) {
    if ctx.is_null() || desc.is_null() {
        return;
    }
    let rows = unsafe { &mut *(ctx as *mut Vec<(u64, i32)>) };
    let d = unsafe { &*desc };
    rows.push((d.id, d.workspace));
}

/// (window id, workspace) for every application window, -1 = all workspaces.
pub fn list_window_workspaces() -> Vec<(u64, i32)> {
    let mut rows: Vec<(u64, i32)> = Vec::new();
    unsafe {
        umoja_wm_list_windows(
            collect_workspaces,
            &mut rows as *mut _ as *mut c_void,
            false,
        )
    };
    rows
}

extern "C" fn collect(ctx: *mut c_void, desc: *const WindowDesc) {
    if ctx.is_null() || desc.is_null() {
        return;
    }
    let rows = unsafe { &mut *(ctx as *mut Vec<WindowRow>) };
    let d = unsafe { &*desc };
    rows.push((
        d.id,
        cstr(d.app_id),
        cstr(d.title),
        d.rect.x,
        d.rect.y,
        d.rect.w,
        d.rect.h,
        d.flags,
    ));
}

/// Application windows, bottom to top in stacking order (or MRU first).
pub fn list_windows(mru: bool) -> Vec<WindowRow> {
    let mut rows: Vec<WindowRow> = Vec::new();
    unsafe { umoja_wm_list_windows(collect, &mut rows as *mut _ as *mut c_void, mru) };
    rows
}

struct Compositor;

#[interface(name = "org.umoja.Compositor")]
impl Compositor {
    /// (id, app_id, title, x, y, width, height, flags), bottom to top.
    async fn get_windows(&self) -> Vec<WindowRow> {
        tokio::task::spawn_blocking(|| list_windows(false))
            .await
            .unwrap_or_default()
    }

    /// Same, most recently used first.
    async fn get_windows_mru(&self) -> Vec<WindowRow> {
        tokio::task::spawn_blocking(|| list_windows(true))
            .await
            .unwrap_or_default()
    }

    async fn activate_window(&self, id: u64) {
        let _ = tokio::task::spawn_blocking(move || unsafe { umoja_wm_activate(id) }).await;
    }

    async fn close_window(&self, id: u64) {
        let _ = tokio::task::spawn_blocking(move || unsafe { umoja_wm_close(id) }).await;
    }

    /// An item in the Alt+Tab popup was clicked.
    async fn switcher_pick(&self, index: u32) {
        let _ = tokio::task::spawn_blocking(move || unsafe { umoja_wm_switcher_pick(index) }).await;
    }

    /// Tile a window: 1 left, 2 right, 3 maximize, 7–10 quarters, 28/29
    /// top/bottom half (the `tiling::Action` numbers).
    async fn tile_window(&self, id: u64, action: u32) {
        let _ = tokio::task::spawn_blocking(move || unsafe { umoja_wm_tile(id, action) }).await;
    }

    /// (current, count): the active workspace (0-based) and how many exist.
    async fn get_workspaces(&self) -> (u32, u32) {
        tokio::task::spawn_blocking(|| {
            let (mut current, mut count) = (0u32, 0u32);
            unsafe { umoja_wm_workspaces(&mut current, &mut count) };
            (current, count)
        })
        .await
        .unwrap_or_default()
    }

    /// Switch to workspace `index` (0-based), showing the switcher popup.
    async fn switch_workspace(&self, index: u32) {
        let _ =
            tokio::task::spawn_blocking(move || unsafe { umoja_wm_switch_workspace(index) }).await;
    }

    /// Move a window to workspace `index` (0-based) and follow it there.
    async fn move_window_to_workspace(&self, id: u64, index: u32) {
        let _ =
            tokio::task::spawn_blocking(move || unsafe { umoja_wm_move_to_workspace(id, index) })
                .await;
    }

    /// (window id, workspace) for every window in `GetWindows`; the
    /// workspace is 0-based, or -1 for a window on every workspace.
    async fn get_window_workspaces(&self) -> Vec<(u64, i32)> {
        tokio::task::spawn_blocking(list_window_workspaces)
            .await
            .unwrap_or_default()
    }

    /// Switch to workspace `index` without the switcher popup (overview).
    async fn show_workspace(&self, index: u32) {
        let _ =
            tokio::task::spawn_blocking(move || unsafe { umoja_wm_show_workspace(index) }).await;
    }

    /// Put a window on workspace `index` without following it (overview
    /// drag and drop).
    async fn send_window_to_workspace(&self, id: u64, index: u32) {
        let _ =
            tokio::task::spawn_blocking(move || unsafe { umoja_wm_send_to_workspace(id, index) })
                .await;
    }

    /// Emitted when the active workspace or the number of workspaces changes.
    #[zbus(signal)]
    async fn workspaces_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        current: u32,
        count: u32,
    ) -> zbus::Result<()>;
}

/// The latest (current, count). One task emits WorkspacesChanged from it,
/// so signals go out in order and a burst collapses into its final state
/// (separately spawned emits could overtake each other and leave the top
/// bar showing a workspace that was already left).
static WORKSPACES: LazyLock<tokio::sync::watch::Sender<(u32, u32)>> =
    LazyLock::new(|| tokio::sync::watch::channel((0, 1)).0);

/// Called from the shim (under the WM lock) after every workspace change.
pub fn workspaces_changed(current: u32, count: u32) {
    WORKSPACES.send_if_modified(|v| {
        let changed = *v != (current, count);
        *v = (current, count);
        changed
    });
}

fn emit_workspaces(conn: zbus::Connection) {
    let mut rx = WORKSPACES.subscribe();
    crate::session::RUNTIME.spawn(async move {
        loop {
            let (current, count) = *rx.borrow_and_update();
            let emitted = match zbus::object_server::SignalEmitter::new(&conn, OBJECT_PATH) {
                Ok(e) => Compositor::workspaces_changed(&e, current, count).await,
                Err(e) => Err(e),
            };
            if let Err(e) = emitted {
                tracing::debug!(error = %e, "WorkspacesChanged not sent");
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
}

pub fn start() {
    crate::session::RUNTIME.spawn(async move {
        let built = match zbus::connection::Builder::session()
            .and_then(|b| b.name(BUS_NAME))
            .and_then(|b| b.serve_at(OBJECT_PATH, Compositor))
        {
            Ok(b) => b.build().await,
            Err(e) => Err(e),
        };
        match built {
            // Keep the connection alive for the life of the compositor.
            Ok(conn) => {
                crate::session::keep_name(conn.clone(), BUS_NAME);
                emit_workspaces(conn.clone());
                std::mem::forget(conn);
            }
            Err(e) => tracing::warn!(error = %e, "org.umoja.Compositor unavailable"),
        }
    });
}
