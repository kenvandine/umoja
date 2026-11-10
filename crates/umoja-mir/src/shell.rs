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

/// What a held-modifier popup is cycling through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Alt+Tab and friends: the popup, activate on release.
    Switcher,
    /// mutter's cycle-windows/cycle-group (Alt+Esc, Alt+F6): no popup, the
    /// selected window is raised as you go (gnome-shell's WindowCycler).
    Cycler { origin: u64 },
    /// Ctrl+Alt+Tab: the top bar and dock (gnome-shell's CtrlAltTabPopup).
    Panels { popup: bool },
}

struct Open {
    switcher: Switcher,
    titles: HashMap<u64, String>,
    kind: Kind,
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
        Action::CycleGroup => (Mode::Group, false),
        Action::CycleGroupBackward => (Mode::Group, true),
        Action::CycleWindows => (Mode::Windows, false),
        Action::CycleWindowsBackward => (Mode::Windows, true),
        Action::SwitchPanels | Action::CyclePanels => (Mode::Windows, false),
        Action::SwitchPanelsBackward | Action::CyclePanelsBackward => (Mode::Windows, true),
        _ => return None,
    })
}

fn kind_for(action: u32, mru: &[Window]) -> Kind {
    match Action::from_u32(action) {
        Action::CycleGroup
        | Action::CycleGroupBackward
        | Action::CycleWindows
        | Action::CycleWindowsBackward => Kind::Cycler {
            origin: mru.first().map(|w| w.id).unwrap_or(0),
        },
        Action::SwitchPanels | Action::SwitchPanelsBackward => Kind::Panels { popup: true },
        Action::CyclePanels | Action::CyclePanelsBackward => Kind::Panels { popup: false },
        _ => Kind::Switcher,
    }
}

fn mode_code(open: &Open) -> u32 {
    if matches!(open.kind, Kind::Panels { .. }) {
        return 3;
    }
    match open.switcher.mode {
        Mode::Applications => 0,
        Mode::Group => 1,
        Mode::Windows => 2,
    }
}

/// Whether this open switcher draws a popup.
fn has_popup(open: &Open) -> bool {
    match open.kind {
        Kind::Switcher => true,
        Kind::Cycler { .. } => false,
        Kind::Panels { popup } => popup,
    }
}

/// The places Ctrl+Alt+Tab moves keyboard focus to, in gnome-shell's
/// order (top bar first, the dock at the bottom of the list). The "window"
/// id is the place's index; the app id carries an icon name for the popup.
pub const PANEL_TOP_BAR: u64 = 0;
pub const PANEL_DOCK: u64 = 1;

fn panel_places() -> Vec<Window> {
    vec![
        Window {
            id: PANEL_TOP_BAR,
            app_id: "focus-top-bar-symbolic".into(),
            // gnome-shell msgids; the overlay translates them.
            title: "Top Bar".into(),
        },
        Window {
            id: PANEL_DOCK,
            app_id: "user-bookmarks-symbolic".into(),
            title: "Dash".into(),
        },
    ]
}

static PANEL_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Give the keyboard to the top bar or the dock.
fn focus_place(place: u64) {
    PANEL_FOCUSED.store(true, std::sync::atomic::Ordering::Relaxed);
    if place == PANEL_DOCK {
        crate::session::dock_call("Focus", ());
        crate::session::panel_call("Unfocus", ());
    } else {
        crate::session::dock_call("Unfocus", ());
        crate::session::panel_call("Focus", ());
    }
}

/// Take the keyboard back from the top bar/dock (a click elsewhere, or a
/// window activated with Alt+Tab).
pub fn unfocus_places() {
    if PANEL_FOCUSED.swap(false, std::sync::atomic::Ordering::Relaxed) {
        crate::session::panel_call("Unfocus", ());
        crate::session::dock_call("Unfocus", ());
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
    if !has_popup(open) {
        return;
    }
    overlay_call(
        "ShowSwitcher",
        (
            mode_code(open),
            items,
            sel as u32,
            sub.map(|s| s as i32).unwrap_or(-1),
        ),
    );
}

fn announce_selection(open: &Open) {
    if !has_popup(open) {
        return;
    }
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
    let kind = kind_for(action, &mru);
    let mru = if matches!(kind, Kind::Panels { .. }) {
        panel_places()
    } else {
        mru
    };
    let titles = mru.iter().map(|w| (w.id, w.title.clone())).collect();
    let Some(mut s) = Switcher::open(mode, backward, &mru, hold) else {
        return false;
    };
    if matches!(kind, Kind::Panels { .. }) && !backward {
        // Ctrl+Alt+Tab starts on the top bar.
        s.selected = 0;
    }
    let open = Open {
        switcher: s,
        titles,
        kind,
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
        let kind = open.kind;
        let popup = has_popup(open);
        *guard = None;
        if popup {
            overlay_call("HideSwitcher", ());
        }
        return Some(settle(kind, out));
    }
    if !down || modifier_bit(keysym) != 0 {
        return Some(Outcome::Consumed);
    }
    let binding = match input::lookup(keysym, mods) {
        0 => None,
        a => switcher_binding(a),
    };
    let mut out = open.switcher.key_down(keysym, mods, binding);
    let kind = open.kind;
    if kind != Kind::Switcher && matches!(out, Outcome::Close(_)) {
        // Q/W only close things in the Alt+Tab popups.
        out = Outcome::Consumed;
    }
    match &out {
        Outcome::Consumed => {
            announce_selection(open);
            if let Kind::Cycler { .. } = kind {
                return Some(Outcome::Preview(current_window(open)));
            }
        }
        Outcome::Cancel | Outcome::Activate { .. } | Outcome::Preview(_) => {
            let popup = has_popup(open);
            *guard = None;
            if popup {
                overlay_call("HideSwitcher", ());
            }
        }
        Outcome::Close(_) => {}
    }
    Some(settle(kind, out))
}

fn current_window(open: &Open) -> u64 {
    let (sel, sub) = open.switcher.selection();
    let item = &open.switcher.items[sel];
    item.windows[sub.unwrap_or(0).min(item.windows.len() - 1)]
}

/// Turn the switcher's answer into what the compositor should do for the
/// cycler and the Ctrl+Alt+Tab popup.
fn settle(kind: Kind, out: Outcome) -> Outcome {
    match (kind, out) {
        // Escape puts the window you started from back on top.
        (Kind::Cycler { origin }, Outcome::Cancel) if origin != 0 => Outcome::Activate {
            window: origin,
            raise: Vec::new(),
        },
        (Kind::Panels { .. }, Outcome::Activate { window, .. }) => {
            focus_place(window);
            Outcome::Consumed
        }
        (_, out) => out,
    }
}

/// The window a cycler shows right now (0 when none is open).
pub fn switcher_preview() -> u64 {
    match switcher().as_ref() {
        Some(open) if matches!(open.kind, Kind::Cycler { .. }) => current_window(open),
        _ => 0,
    }
}

pub fn switcher_pick(index: usize) -> Option<Outcome> {
    let mut guard = switcher();
    let open = guard.as_mut()?;
    let kind = open.kind;
    let out = open.switcher.pick(index);
    if out.is_some() {
        *guard = None;
        overlay_call("HideSwitcher", ());
    }
    out.map(|o| settle(kind, o))
}

pub fn switcher_window_closed(id: u64) {
    let mut guard = switcher();
    let Some(open) = guard.as_mut() else { return };
    if matches!(open.kind, Kind::Panels { .. }) {
        return;
    }
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
    fn umoja_wm_move_pointer(x: i32, y: i32);
    fn umoja_wm_script_drag(
        id: u64,
        phase: u32,
        x: i32,
        y: i32,
        out_x: *mut i32,
        out_y: *mut i32,
    ) -> bool;
    fn umoja_wm_workspaces(current: *mut u32, count: *mut u32);
    fn umoja_wm_switch_workspace(index: u32);
    fn umoja_wm_move_to_workspace(id: u64, index: u32);
    fn umoja_wm_show_workspace(index: u32);
    fn umoja_wm_send_to_workspace(id: u64, index: u32);
    fn umoja_wm_workspace_settings_changed();
}

/// Ease-in-out position `t` (0..=1) of a scripted drag.
fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The steps of a scripted drag from `from` to `to`, one per ~16 ms frame.
fn drag_path(from: (i32, i32), to: (i32, i32), duration_ms: u32) -> Vec<(i32, i32)> {
    let steps = (duration_ms / 16).clamp(8, 600);
    (1..=steps)
        .map(|i| {
            let e = ease(f64::from(i) / f64::from(steps));
            (
                from.0 + (f64::from(to.0 - from.0) * e).round() as i32,
                from.1 + (f64::from(to.1 - from.1) * e).round() as i32,
            )
        })
        .collect()
}

fn scripted_drag(id: u64, x: i32, y: i32, duration_ms: u32) -> bool {
    let (mut gx, mut gy) = (0i32, 0i32);
    if !unsafe { umoja_wm_script_drag(id, 0, 0, 0, &mut gx, &mut gy) } {
        return false;
    }
    let frame = std::time::Duration::from_millis(16);
    for (px, py) in drag_path((gx, gy), (x, y), duration_ms) {
        std::thread::sleep(frame);
        if !unsafe { umoja_wm_script_drag(id, 1, px, py, std::ptr::null_mut(), std::ptr::null_mut()) } {
            break;
        }
    }
    // Hold still a moment so the tile preview settles, as a person would.
    std::thread::sleep(std::time::Duration::from_millis(350));
    unsafe { umoja_wm_script_drag(id, 2, 0, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    true
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

    /// Drag a window by its title bar so the grab point glides to (x, y)
    /// over `duration_ms`, then drop it, exactly as a pointer drag would:
    /// the tile preview and layout picker follow it and it snaps to the
    /// half, quarter or maximized area under (x, y). For demos
    /// (tools/demo). False if the window can't be dragged now.
    async fn drag_window(&self, id: u64, x: i32, y: i32, duration_ms: u32) -> bool {
        tokio::task::spawn_blocking(move || scripted_drag(id, x, y, duration_ms))
            .await
            .unwrap_or(false)
    }

    /// Put the pointer at (x, y) in layout pixels, out of the way of
    /// whatever is being shown (tools/demo). It is a warp: no motion or
    /// button events reach any client.
    async fn move_pointer(&self, x: i32, y: i32) {
        let _ = tokio::task::spawn_blocking(move || unsafe { umoja_wm_move_pointer(x, y) }).await;
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

    /// (id, accelerator) for every keyboard shortcut the compositor
    /// handles, as currently configured. The id is the GSettings key the
    /// accelerator came from (`toggle-tiled-left`, `screensaver`, ...),
    /// `custom:<name>` for a GNOME custom shortcut, or `overlay-key`.
    async fn get_shortcuts(&self) -> Vec<(String, String)> {
        crate::input::shortcuts()
    }

    /// Start the screensaver now, as idling would (`umoja-screensaver
    /// --activate`). False when the session is locked or it isn't installed.
    async fn activate_screensaver(&self) -> bool {
        crate::idle::activate_screensaver()
    }

    /// The pointer pushed against an outer screen `edge` (1 left, 2 right,
    /// 4 top, 8 bottom) at (x, y), like GNOME's pressure barrier: an
    /// auto-hidden dock on that edge comes back.
    #[zbus(signal)]
    async fn edge_pressure(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        edge: u32,
        x: i32,
        y: i32,
    ) -> zbus::Result<()>;

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

static CONNECTION: std::sync::OnceLock<zbus::Connection> = std::sync::OnceLock::new();

/// Emit EdgePressure (from the input thread; sent on the runtime).
pub fn edge_pressure(edge: u32, x: i32, y: i32) {
    let Some(conn) = CONNECTION.get().cloned() else {
        return;
    };
    crate::session::RUNTIME.spawn(async move {
        let sent = match zbus::object_server::SignalEmitter::new(&conn, OBJECT_PATH) {
            Ok(e) => Compositor::edge_pressure(&e, edge, x, y).await,
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            tracing::debug!(error = %e, "EdgePressure not sent");
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
                let _ = CONNECTION.set(conn.clone());
                std::mem::forget(conn);
            }
            Err(e) => tracing::warn!(error = %e, "org.umoja.Compositor unavailable"),
        }
    });
}

#[cfg(test)]
mod drag_tests {
    use super::drag_path;

    #[test]
    fn drag_path_ends_on_target_and_eases() {
        let p = drag_path((100, 100), (900, 500), 800);
        assert_eq!(p.len(), 50);
        assert_eq!(*p.last().unwrap(), (900, 500));
        // Slow start: the first step covers far less than an even share.
        assert!(p[0].0 - 100 < 800 / 50);
        let q = drag_path((0, 0), (10, 10), 0);
        assert_eq!(q.len(), 8);
    }
}
