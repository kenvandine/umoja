//! Where each app's icon is on the docks, for the minimize animation
//! (GNOME minimizes a window into its dock icon). umoja-dock sends the icon
//! rectangles of each dock in that dock's surface coordinates, with the
//! monitor it is on (org.umoja.Compositor.SetDockIcons); the shim adds the
//! dock surface's position, which only it knows.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::tiling::Rect;

type Monitor = (i32, i32, i32, i32);
type Icons = HashMap<Monitor, Vec<(String, Rect)>>;

static ICONS: LazyLock<Mutex<Icons>> = LazyLock::new(Mutex::default);

/// The dock on `monitor` now shows these (app id, rectangle) icons. App
/// ids are the window app ids (Wayland app_id, X11 WM_CLASS) the icon
/// stands for, and the desktop id without `.desktop`.
pub fn set(monitor: Monitor, icons: Vec<(String, Rect)>) {
    let mut all = ICONS.lock().unwrap_or_else(|e| e.into_inner());
    if icons.is_empty() {
        all.remove(&monitor);
    } else {
        all.insert(monitor, icons);
    }
}

/// The icon for `app_id` on the dock on `monitor`, in that dock's surface.
pub fn find(app_id: &str, monitor: Monitor) -> Option<Rect> {
    let all = ICONS.lock().unwrap_or_else(|e| e.into_inner());
    all.get(&monitor)?
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(app_id))
        .map(|(_, r)| *r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_by_monitor_and_app() {
        let r = Rect { x: 4, y: 100, w: 48, h: 48 };
        set((0, 0, 1920, 1080), vec![("org.gnome.Ptyxis".into(), r)]);
        assert_eq!(find("org.gnome.ptyxis", (0, 0, 1920, 1080)), Some(r));
        assert_eq!(find("org.gnome.Ptyxis", (1920, 0, 1920, 1080)), None);
        set((0, 0, 1920, 1080), Vec::new());
        assert_eq!(find("org.gnome.Ptyxis", (0, 0, 1920, 1080)), None);
    }
}
