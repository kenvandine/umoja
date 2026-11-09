//! Ubuntu-style window tiling: half/quarter snaps and group resizing.
//!
//! Pure geometry, no Mir types, so it is unit tested directly. The C++ shim
//! calls into this through `ffi.rs`.

use std::collections::HashMap;

/// Mirrors the `UMOJA_ACTION_*` enum in `shim/umoja_shim.h`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    None = 0,
    TileLeft = 1,
    TileRight = 2,
    Maximize = 3,
    Unmaximize = 4,
    Minimize = 5,
    ToggleOverlay = 6,
    TileTopLeft = 7,
    TileTopRight = 8,
    TileBottomLeft = 9,
    TileBottomRight = 10,
    Close = 11,
    ToggleDictation = 12,
    ToggleMaximized = 13,
    ToggleFullscreen = 14,
    WorkspacePrev = 15,
    WorkspaceNext = 16,
    WorkspaceLast = 17,
    MoveToWorkspacePrev = 18,
    MoveToWorkspaceNext = 19,
    MoveToWorkspaceLast = 20,
    MoveToMonitorLeft = 21,
    MoveToMonitorRight = 22,
    MoveToMonitorUp = 23,
    MoveToMonitorDown = 24,
    ScreenshotUi = 25,
    Screenshot = 26,
    ScreenshotWindow = 27,
    TileTopHalf = 28,
    TileBottomHalf = 29,
    SwitchApplications = 30,
    SwitchApplicationsBackward = 31,
    SwitchGroup = 32,
    SwitchGroupBackward = 33,
    SwitchWindows = 34,
    SwitchWindowsBackward = 35,
    ToggleOverview = 36,
    AskAboutScreen = 37,
    ClipboardHistory = 38,
    /// gnome-shell's shift-overview-up/down: desktop → overview → app grid.
    ShiftOverviewUp = 39,
    ShiftOverviewDown = 40,
    /// mutter's cycle-group / cycle-windows (Alt+F6, Alt+Esc): switch
    /// directly, no popup.
    CycleGroup = 41,
    CycleGroupBackward = 42,
    CycleWindows = 43,
    CycleWindowsBackward = 44,
    /// gnome-shell's Ctrl+Alt+Tab: move keyboard focus to the top bar/dock.
    SwitchPanels = 45,
    SwitchPanelsBackward = 46,
    /// Alt+F7 / Alt+F8: move or resize the window with the keyboard.
    BeginMove = 47,
    BeginResize = 48,
    /// Alt+Space: the window menu.
    WindowMenu = 49,
    /// Super+V: the notification list.
    ToggleMessageTray = 50,
    CyclePanels = 51,
    CyclePanelsBackward = 52,
    /// Window menu entries.
    ToggleAbove = 53,
    ToggleSticky = 54,
    /// gnome-shell's show-screen-recording-ui (Ctrl+Shift+Alt+R).
    ScreenRecordingUi = 55,
    // 100+n / 200+n: switch-to / move-to workspace n (see workspaces.rs).
}

impl Action {
    pub fn from_u32(v: u32) -> Self {
        use Action::*;
        match v {
            1 => TileLeft,
            2 => TileRight,
            3 => Maximize,
            4 => Unmaximize,
            5 => Minimize,
            6 => ToggleOverlay,
            7 => TileTopLeft,
            8 => TileTopRight,
            9 => TileBottomLeft,
            10 => TileBottomRight,
            11 => Close,
            12 => ToggleDictation,
            13 => ToggleMaximized,
            14 => ToggleFullscreen,
            15 => WorkspacePrev,
            16 => WorkspaceNext,
            17 => WorkspaceLast,
            18 => MoveToWorkspacePrev,
            19 => MoveToWorkspaceNext,
            20 => MoveToWorkspaceLast,
            21 => MoveToMonitorLeft,
            22 => MoveToMonitorRight,
            23 => MoveToMonitorUp,
            24 => MoveToMonitorDown,
            25 => ScreenshotUi,
            26 => Screenshot,
            27 => ScreenshotWindow,
            28 => TileTopHalf,
            29 => TileBottomHalf,
            30 => SwitchApplications,
            31 => SwitchApplicationsBackward,
            32 => SwitchGroup,
            33 => SwitchGroupBackward,
            34 => SwitchWindows,
            35 => SwitchWindowsBackward,
            36 => ToggleOverview,
            37 => AskAboutScreen,
            38 => ClipboardHistory,
            39 => ShiftOverviewUp,
            40 => ShiftOverviewDown,
            41 => CycleGroup,
            42 => CycleGroupBackward,
            43 => CycleWindows,
            44 => CycleWindowsBackward,
            45 => SwitchPanels,
            46 => SwitchPanelsBackward,
            47 => BeginMove,
            48 => BeginResize,
            49 => WindowMenu,
            50 => ToggleMessageTray,
            51 => CyclePanels,
            52 => CyclePanelsBackward,
            53 => ToggleAbove,
            54 => ToggleSticky,
            55 => ScreenRecordingUi,
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    fn overlaps_vertically(&self, o: &Rect) -> bool {
        self.y < o.bottom() && o.y < self.bottom()
    }
    fn overlaps_horizontally(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right()
    }
}

/// Target rectangle for a tiling action within the usable work area
/// (the output minus panel/dock exclusive zones).
pub fn geometry(action: Action, area: Rect) -> Option<Rect> {
    let half_w = area.w / 2;
    let half_h = area.h / 2;
    let (rx, ry) = (area.x + half_w, area.y + half_h);
    let (rw, rh) = (area.w - half_w, area.h - half_h);
    Some(match action {
        Action::TileLeft => Rect::new(area.x, area.y, half_w, area.h),
        Action::TileRight => Rect::new(rx, area.y, rw, area.h),
        Action::TileTopLeft => Rect::new(area.x, area.y, half_w, half_h),
        Action::TileTopRight => Rect::new(rx, area.y, rw, half_h),
        Action::TileBottomLeft => Rect::new(area.x, ry, half_w, rh),
        Action::TileBottomRight => Rect::new(rx, ry, rw, rh),
        Action::TileTopHalf => Rect::new(area.x, area.y, area.w, half_h),
        Action::TileBottomHalf => Rect::new(area.x, ry, area.w, rh),
        _ => return None,
    })
}

/// Where a window would end up for `action`, including maximize (the whole
/// work area). Used for the snap preview.
pub fn target_rect(action: Action, area: Rect) -> Option<Rect> {
    match action {
        Action::Maximize => Some(area),
        a => geometry(a, area),
    }
}

/// True for the actions that put a window into a tile (half or quarter).
pub fn is_tile(action: Action) -> bool {
    matches!(
        action,
        Action::TileLeft
            | Action::TileRight
            | Action::TileTopLeft
            | Action::TileTopRight
            | Action::TileBottomLeft
            | Action::TileBottomRight
            | Action::TileTopHalf
            | Action::TileBottomHalf
    )
}

/// Tracks tiled windows so dragging the border between two tiles resizes both.
#[derive(Default)]
pub struct TilingState {
    windows: HashMap<u64, (Action, Rect)>,
}

impl TilingState {
    pub fn register(&mut self, id: u64, action: Action, rect: Rect) {
        self.windows.insert(id, (action, rect));
    }

    pub fn unregister(&mut self, id: u64) {
        self.windows.remove(&id);
    }

    /// Window `id` was resized to `new`. Returns the neighbours that shared a
    /// border with it and their new rectangles, and records all new sizes.
    pub fn resized(&mut self, id: u64, new: Rect) -> Vec<(u64, Rect)> {
        let Some(&(action, old)) = self.windows.get(&id) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for (&other_id, &(_, o)) in &self.windows {
            if other_id == id {
                continue;
            }
            let mut n = o;
            // Shared vertical border, `id` on the left.
            if old.right() == o.x && old.overlaps_vertically(&o) && new.right() != old.right() {
                n.x = new.right();
                n.w = o.right() - n.x;
            }
            // Shared vertical border, `id` on the right.
            if old.x == o.right() && old.overlaps_vertically(&o) && new.x != old.x {
                n.w = new.x - o.x;
            }
            // Shared horizontal border, `id` above.
            if old.bottom() == o.y && old.overlaps_horizontally(&o) && new.bottom() != old.bottom()
            {
                n.y = new.bottom();
                n.h = o.bottom() - n.y;
            }
            // Shared horizontal border, `id` below.
            if old.y == o.bottom() && old.overlaps_horizontally(&o) && new.y != old.y {
                n.h = new.y - o.y;
            }
            if n != o && n.w > 0 && n.h > 0 {
                changes.push((other_id, n));
            }
        }
        self.windows.insert(id, (action, new));
        for (other_id, r) in &changes {
            if let Some(entry) = self.windows.get_mut(other_id) {
                entry.1 = *r;
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 32, 1920, 1048);

    #[test]
    fn halves_and_quarters_cover_area() {
        let l = geometry(Action::TileLeft, AREA).unwrap();
        let r = geometry(Action::TileRight, AREA).unwrap();
        assert_eq!(l.right(), r.x);
        assert_eq!(l.w + r.w, AREA.w);
        let tl = geometry(Action::TileTopLeft, AREA).unwrap();
        let br = geometry(Action::TileBottomRight, AREA).unwrap();
        assert_eq!(tl.bottom(), br.y);
        assert_eq!(br.bottom(), AREA.bottom());
        assert!(geometry(Action::Maximize, AREA).is_none());
    }

    #[test]
    fn odd_sizes_leave_no_gap() {
        let area = Rect::new(0, 0, 1001, 777);
        let l = geometry(Action::TileLeft, area).unwrap();
        let r = geometry(Action::TileRight, area).unwrap();
        assert_eq!(r.right(), area.right());
        assert_eq!(l.right(), r.x);
    }

    #[test]
    fn group_resize_moves_shared_border() {
        let mut s = TilingState::default();
        let l = geometry(Action::TileLeft, AREA).unwrap();
        let r = geometry(Action::TileRight, AREA).unwrap();
        s.register(1, Action::TileLeft, l);
        s.register(2, Action::TileRight, r);

        let grown = Rect { w: l.w + 100, ..l };
        let changes = s.resized(1, grown);
        assert_eq!(
            changes,
            vec![(
                2,
                Rect {
                    x: r.x + 100,
                    w: r.w - 100,
                    ..r
                }
            )]
        );

        // And back from the other side.
        let r2 = changes[0].1;
        let shrunk = Rect {
            x: r2.x + 50,
            w: r2.w - 50,
            ..r2
        };
        let changes = s.resized(2, shrunk);
        assert_eq!(
            changes,
            vec![(
                1,
                Rect {
                    w: grown.w + 50,
                    ..grown
                }
            )]
        );
    }

    #[test]
    fn unrelated_windows_untouched() {
        let mut s = TilingState::default();
        let tl = geometry(Action::TileTopLeft, AREA).unwrap();
        let br = geometry(Action::TileBottomRight, AREA).unwrap();
        s.register(1, Action::TileTopLeft, tl);
        s.register(2, Action::TileBottomRight, br);
        // Only corners touch, no shared edge segment.
        assert!(s.resized(1, Rect { w: tl.w + 10, ..tl }).is_empty());
    }
}
