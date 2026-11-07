//! GNOME-style workspaces.
//!
//! Follows mutter's model and settings: dynamic workspaces by default
//! (`org.gnome.mutter dynamic-workspaces`), always one empty workspace at the
//! end, a fixed count from `org.gnome.desktop.wm.preferences num-workspaces`
//! otherwise, and optionally workspaces only on the primary monitor
//! (`org.gnome.mutter workspaces-only-on-primary`, Ubuntu's default).
//!
//! The C++ shim owns the actual `miral::Workspace` objects; this module only
//! decides indices and counts, so it is fully unit tested.

use crate::tiling::Action;

/// Highest workspace number with its own keybinding (switch-to-workspace-N).
pub const MAX_NUMBERED: u32 = 12;
pub const SWITCH_BASE: u32 = 100;
pub const MOVE_BASE: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub dynamic: bool,
    pub fixed_count: usize,
    pub only_on_primary: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dynamic: true,
            fixed_count: 4,
            only_on_primary: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Switch(usize),
    MoveWindow(usize),
}

pub fn switch_code(n: u32) -> u32 {
    SWITCH_BASE + n
}

pub fn move_code(n: u32) -> u32 {
    MOVE_BASE + n
}

/// Translate a bound action code into a target workspace index (0-based).
/// `last_used` is the previously active workspace (for `switch-to-workspace-last`
/// GNOME means the final workspace, not the previous one, so it's unused there).
pub fn resolve(code: u32, current: usize, count: usize) -> Option<Request> {
    if count == 0 {
        return None;
    }
    let last = count - 1;
    // Wrapping is off in GNOME by default (Ubuntu dock-style horizontal row).
    let prev = current.saturating_sub(1);
    let next = (current + 1).min(last);
    let numbered = |base: u32| -> Option<usize> {
        let n = code.checked_sub(base)?;
        (1..=MAX_NUMBERED)
            .contains(&n)
            .then(|| (n as usize - 1).min(last))
    };
    Some(match code {
        c if c == Action::WorkspacePrev as u32 => Request::Switch(prev),
        c if c == Action::WorkspaceNext as u32 => Request::Switch(next),
        c if c == Action::WorkspaceLast as u32 => Request::Switch(last),
        c if c == Action::MoveToWorkspacePrev as u32 => Request::MoveWindow(prev),
        c if c == Action::MoveToWorkspaceNext as u32 => Request::MoveWindow(next),
        c if c == Action::MoveToWorkspaceLast as u32 => Request::MoveWindow(last),
        c if (SWITCH_BASE + 1..=SWITCH_BASE + MAX_NUMBERED).contains(&c) => {
            Request::Switch(numbered(SWITCH_BASE)?)
        }
        c if (MOVE_BASE + 1..=MOVE_BASE + MAX_NUMBERED).contains(&c) => {
            Request::MoveWindow(numbered(MOVE_BASE)?)
        }
        _ => return None,
    })
}

/// How many workspaces should exist given which ones hold windows.
///
/// Dynamic mode: every occupied workspace, plus exactly one trailing empty
/// one, and never fewer than `current + 1` so the user isn't yanked away
/// from an empty workspace they just switched to. Interior empties are
/// removed by the shim via [`removable`].
pub fn desired_count(settings: &Settings, occupied: &[bool], current: usize) -> usize {
    if !settings.dynamic {
        return settings.fixed_count.max(1);
    }
    let last_occupied = occupied
        .iter()
        .rposition(|o| *o)
        .map(|i| i + 1)
        .unwrap_or(0);
    (last_occupied + 1).max(current + 1)
}

/// Indices of empty workspaces that dynamic mode should delete (not the
/// current one and not the trailing spare), highest first so the shim can
/// erase them in order without shifting indices it still needs.
pub fn removable(settings: &Settings, occupied: &[bool], current: usize) -> Vec<usize> {
    if !settings.dynamic || occupied.len() < 2 {
        return Vec::new();
    }
    let spare = occupied.len() - 1;
    (0..occupied.len())
        .rev()
        .filter(|&i| !occupied[i] && i != current && i != spare)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_navigation_clamps() {
        assert_eq!(
            resolve(Action::WorkspaceNext as u32, 0, 3),
            Some(Request::Switch(1))
        );
        assert_eq!(
            resolve(Action::WorkspaceNext as u32, 2, 3),
            Some(Request::Switch(2))
        );
        assert_eq!(
            resolve(Action::WorkspacePrev as u32, 0, 3),
            Some(Request::Switch(0))
        );
        assert_eq!(
            resolve(Action::WorkspaceLast as u32, 0, 3),
            Some(Request::Switch(2))
        );
        assert_eq!(resolve(switch_code(2), 0, 3), Some(Request::Switch(1)));
        assert_eq!(resolve(switch_code(9), 0, 3), Some(Request::Switch(2)));
        assert_eq!(resolve(move_code(1), 2, 3), Some(Request::MoveWindow(0)));
        assert_eq!(resolve(Action::Maximize as u32, 0, 3), None);
    }

    #[test]
    fn dynamic_keeps_one_spare() {
        let s = Settings::default();
        assert_eq!(desired_count(&s, &[false], 0), 1);
        assert_eq!(desired_count(&s, &[true], 0), 2);
        assert_eq!(desired_count(&s, &[true, true, false], 1), 3);
        assert_eq!(desired_count(&s, &[true, false, false, false], 0), 2);
        // Sitting on an empty workspace keeps it alive.
        assert_eq!(desired_count(&s, &[true, false, false], 2), 3);
    }

    #[test]
    fn dynamic_removes_interior_empties() {
        let s = Settings::default();
        assert_eq!(removable(&s, &[true, false, true, false], 0), vec![1]);
        assert_eq!(
            removable(&s, &[true, false, true, false], 1),
            Vec::<usize>::new()
        );
        let fixed = Settings {
            dynamic: false,
            ..s
        };
        assert!(removable(&fixed, &[true, false, true, false], 0).is_empty());
        assert_eq!(desired_count(&fixed, &[true], 0), 4);
    }
}
