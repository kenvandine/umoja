//! Drag-to-snap, the way Ubuntu's Tiling Assistant does it.
//!
//! While a window is dragged, the compositor asks [`motion`] where it would
//! land. The answer drives two pieces of chrome drawn by umoja-overlay:
//!
//! * the tile preview, a translucent accent-coloured rectangle under the
//!   window showing the half, quarter or maximized area it will fill;
//! * the layout picker, a small panel that peeks out from under the top bar
//!   as soon as a drag starts and slides fully down when the pointer comes
//!   near it, offering left/right halves, top/bottom halves, quarters and
//!   maximize ("enable-layout-picker", on by default in Ubuntu).
//!
//! Geometry and thresholds follow tiling-assistant@ubuntu.com
//! (`moveHandler.js`, `layoutPicker.js`, `stylesheet.css`), so the edges,
//! corners and picker hit areas behave the same as on Ubuntu's GNOME session.

use crate::tiling::{Action, Rect, target_rect};

/// `vertical-preview-area` / `horizontal-preview-area` (default 15 px).
pub const EDGE: i32 = 15;

// Layout picker metrics, from tiling-assistant's stylesheet.
pub const ICON: i32 = 64;
pub const SPACING: i32 = 15;
pub const INNER_PAD: i32 = 15;
pub const OUTER_PAD: i32 = 20;
pub const PICKER_W: i32 = 4 * ICON + 3 * SPACING + 2 * (INNER_PAD + OUTER_PAD);
pub const PICKER_H: i32 = ICON + 2 * (INNER_PAD + OUTER_PAD);

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Hidden = 0,
    /// Only the bottom padding shows under the top bar.
    Peek = 1,
    Shown = 2,
}

/// `LayoutPickerTileType` in tiling-assistant. The overlay uses the same
/// numbers to highlight the hovered part of an icon.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerTile {
    #[default]
    None = 0,
    Left = 1,
    Right = 2,
    Top = 3,
    Bottom = 4,
    /// Top right.
    Q1 = 5,
    /// Top left.
    Q2 = 6,
    /// Bottom left.
    Q3 = 7,
    /// Bottom right.
    Q4 = 8,
    Maximize = 9,
}

/// Left edge of the picker for a work area (it is centred on it).
pub fn picker_x(area: Rect) -> i32 {
    area.x + (area.w - PICKER_W) / 2
}

/// Icon `i` (0 = top/bottom halves, 1 = left/right halves, 2 = quarters,
/// 3 = maximize) when the picker is fully shown.
pub fn icon_rect(area: Rect, i: i32) -> Rect {
    Rect::new(
        picker_x(area) + OUTER_PAD + INNER_PAD + i * (ICON + SPACING),
        area.y + OUTER_PAD + INNER_PAD,
        ICON,
        ICON,
    )
}

fn contains(r: Rect, x: i32, y: i32) -> bool {
    x >= r.x && x <= r.right() && y >= r.y && y <= r.bottom()
}

/// Picker visibility and hovered tile for a pointer position during a drag.
pub fn picker(px: i32, py: i32, area: Rect, monitor: Rect) -> (Visibility, PickerTile) {
    let x0 = picker_x(area);
    let near = py >= monitor.y
        && py <= area.y + PICKER_H - OUTER_PAD
        && px >= x0 + OUTER_PAD
        && px <= x0 + PICKER_W - OUTER_PAD;
    if !near {
        return (Visibility::Peek, PickerTile::None);
    }
    let left_half = |r: Rect| px < r.x + r.w / 2;
    let top_half = |r: Rect| py <= r.y + r.h / 2;

    let horizontal = icon_rect(area, 1);
    if contains(horizontal, px, py) {
        let t = if left_half(horizontal) {
            PickerTile::Left
        } else {
            PickerTile::Right
        };
        return (Visibility::Shown, t);
    }
    let vertical = icon_rect(area, 0);
    if contains(vertical, px, py) {
        let t = if top_half(vertical) {
            PickerTile::Top
        } else {
            PickerTile::Bottom
        };
        return (Visibility::Shown, t);
    }
    let quarter = icon_rect(area, 2);
    if contains(quarter, px, py) {
        let t = match (top_half(quarter), left_half(quarter)) {
            (true, true) => PickerTile::Q2,
            (true, false) => PickerTile::Q1,
            (false, true) => PickerTile::Q3,
            (false, false) => PickerTile::Q4,
        };
        return (Visibility::Shown, t);
    }
    if contains(icon_rect(area, 3), px, py) {
        return (Visibility::Shown, PickerTile::Maximize);
    }
    (Visibility::Shown, PickerTile::None)
}

/// Where a window dragged with the pointer at `(px, py)` would be tiled.
///
/// `window` is the window's current frame and `area` the work area of the
/// monitor under the pointer. Mirrors `_edgeTilingPreview()` with the layout
/// picker enabled: the top edge maximizes, the bottom edge tiles the bottom
/// half, the sides tile halves, and side + top/bottom gives a quarter. A
/// picker choice always wins over the edges.
pub fn target(px: i32, py: i32, window: Rect, area: Rect, pick: PickerTile) -> Action {
    use PickerTile as P;
    let picking = pick != P::None;
    let at_top = py <= area.y + EDGE || matches!(pick, P::Top | P::Maximize);
    let at_bottom = py >= area.bottom() - EDGE || pick == P::Bottom;
    let at_left = px <= area.x + EDGE || pick == P::Left;
    let at_right = px >= area.right() - EDGE || pick == P::Right;
    // Dragging by the title bar along a side, with the window pinned to the
    // top or near the bottom, also counts as a corner.
    let title_grabbed = py - window.y < 50;
    let window_at_top = title_grabbed && window.y == area.y;
    let window_at_bottom = window.y >= area.bottom() - 75;

    if pick == P::Q2 || !picking && at_left && (at_top || window_at_top) {
        Action::TileTopLeft
    } else if pick == P::Q1 || !picking && at_right && (at_top || window_at_top) {
        Action::TileTopRight
    } else if pick == P::Q3 || !picking && at_left && (at_bottom || window_at_bottom) {
        Action::TileBottomLeft
    } else if pick == P::Q4 || !picking && at_right && (at_bottom || window_at_bottom) {
        Action::TileBottomRight
    } else if at_top {
        if pick == P::Top {
            Action::TileTopHalf
        } else {
            Action::Maximize
        }
    } else if at_bottom {
        Action::TileBottomHalf
    } else if at_left {
        Action::TileLeft
    } else if at_right {
        Action::TileRight
    } else {
        Action::None
    }
}

/// What the overlay should be showing for the current drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Chrome {
    pub picker: Visibility,
    pub picker_tile: PickerTile,
    /// Work area the picker hangs from.
    pub area: Rect,
    pub action: Action,
    pub preview: Option<Rect>,
}

impl Default for Action {
    fn default() -> Self {
        Action::None
    }
}

/// Full drag evaluation: picker state, snap action and preview rectangle.
pub fn evaluate(
    px: i32,
    py: i32,
    window: Rect,
    area: Rect,
    monitor: Rect,
    use_picker: bool,
) -> Chrome {
    let (picker_vis, pick) = if use_picker {
        picker(px, py, area, monitor)
    } else {
        (Visibility::Hidden, PickerTile::None)
    };
    let action = target(px, py, window, area, pick);
    Chrome {
        picker: picker_vis,
        picker_tile: pick,
        area,
        action,
        preview: target_rect(action, area),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: Rect = Rect::new(0, 0, 1920, 1080);
    const AREA: Rect = Rect::new(0, 32, 1920, 1048);
    const WIN: Rect = Rect::new(600, 300, 800, 600);

    #[test]
    fn edges_and_corners() {
        assert_eq!(
            target(0, 500, WIN, AREA, PickerTile::None),
            Action::TileLeft
        );
        assert_eq!(
            target(1919, 500, WIN, AREA, PickerTile::None),
            Action::TileRight
        );
        assert_eq!(
            target(960, 40, WIN, AREA, PickerTile::None),
            Action::Maximize
        );
        assert_eq!(
            target(960, 1075, WIN, AREA, PickerTile::None),
            Action::TileBottomHalf
        );
        assert_eq!(
            target(3, 40, WIN, AREA, PickerTile::None),
            Action::TileTopLeft
        );
        assert_eq!(
            target(1919, 1079, WIN, AREA, PickerTile::None),
            Action::TileBottomRight
        );
        assert_eq!(target(960, 500, WIN, AREA, PickerTile::None), Action::None);
    }

    #[test]
    fn title_bar_drag_along_side_with_window_at_top_is_a_quarter() {
        let w = Rect::new(0, AREA.y, 800, 600);
        assert_eq!(
            target(2, AREA.y + 20, w, AREA, PickerTile::None),
            Action::TileTopLeft
        );
    }

    #[test]
    fn picker_choice_wins() {
        assert_eq!(
            target(960, 100, WIN, AREA, PickerTile::Right),
            Action::TileRight
        );
        assert_eq!(
            target(960, 100, WIN, AREA, PickerTile::Left),
            Action::TileLeft
        );
        assert_eq!(
            target(960, 100, WIN, AREA, PickerTile::Top),
            Action::TileTopHalf
        );
        assert_eq!(
            target(960, 100, WIN, AREA, PickerTile::Q3),
            Action::TileBottomLeft
        );
        assert_eq!(
            target(960, 100, WIN, AREA, PickerTile::Maximize),
            Action::Maximize
        );
    }

    #[test]
    fn picker_peeks_until_pointer_is_near() {
        assert_eq!(
            picker(100, 500, AREA, MON),
            (Visibility::Peek, PickerTile::None)
        );
        let r = icon_rect(AREA, 1);
        assert_eq!(
            picker(r.x + 5, r.y + 30, AREA, MON),
            (Visibility::Shown, PickerTile::Left)
        );
        assert_eq!(
            picker(r.right() - 5, r.y + 30, AREA, MON),
            (Visibility::Shown, PickerTile::Right)
        );
        let q = icon_rect(AREA, 2);
        assert_eq!(picker(q.x + 2, q.y + 2, AREA, MON).1, PickerTile::Q2);
        assert_eq!(
            picker(q.right() - 2, q.bottom() - 2, AREA, MON).1,
            PickerTile::Q4
        );
        let m = icon_rect(AREA, 3);
        assert_eq!(
            picker(m.x + 30, m.y + 30, AREA, MON).1,
            PickerTile::Maximize
        );
        // Over the top bar, above the picker, it is shown but nothing picked.
        let x = picker_x(AREA) + PICKER_W / 2;
        assert_eq!(
            picker(x, 5, AREA, MON),
            (Visibility::Shown, PickerTile::None)
        );
    }

    #[test]
    fn evaluate_reports_preview() {
        let c = evaluate(0, 500, WIN, AREA, MON, true);
        assert_eq!(c.action, Action::TileLeft);
        assert_eq!(c.preview, Some(Rect::new(0, 32, 960, 1048)));
        assert_eq!(c.picker, Visibility::Peek);
        let c = evaluate(960, 500, WIN, AREA, MON, false);
        assert_eq!(c.preview, None);
        assert_eq!(c.picker, Visibility::Hidden);
    }
}
