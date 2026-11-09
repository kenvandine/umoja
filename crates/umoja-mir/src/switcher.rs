//! GNOME's Alt+Tab / Super+Tab switchers, as a pure state machine.
//!
//! The compositor feeds it the windows in most-recently-used order and every
//! key event while it is open; it answers with what to do. umoja-overlay
//! draws the popup from the items and selection this module reports.
//!
//! Behaviour follows gnome-shell's `altTab.js` / `switcherPopup.js`:
//!
//! * `switch-applications` (Alt+Tab, Super+Tab) lists applications, most
//!   recently used first, and starts on the second one; Shift goes back.
//! * `switch-group` (Alt+`, Super+`) cycles windows of the current app, and
//!   inside the app switcher moves through the selected app's windows.
//! * `switch-windows` lists every window.
//! * Releasing the modifier activates the selection, Escape cancels, arrow
//!   keys move, Q quits the selected app (app switcher), W closes the
//!   selected window (window switcher).

/// One window as the compositor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u64,
    pub app_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Applications,
    /// Windows of one application.
    Group,
    Windows,
}

/// An entry in the popup: an app (with its windows, MRU first) or a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub app_id: String,
    pub title: String,
    pub windows: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Key consumed, popup updated.
    Consumed,
    /// Activate this window (and, for an app, raise its other windows).
    Activate {
        window: u64,
        raise: Vec<u64>,
    },
    /// Close these windows and keep the switcher open.
    Close(Vec<u64>),
    Cancel,
}

#[derive(Debug, Clone)]
pub struct Switcher {
    pub mode: Mode,
    pub items: Vec<Item>,
    pub selected: usize,
    /// Window index inside the selected app (app switcher + `), if any.
    pub sub: Option<usize>,
    /// Modifier bits (input::MOD_*) that keep the switcher open.
    pub modifiers: u32,
}

// X keysyms the switcher reacts to.
pub const XK_TAB: u32 = 0xff09;
pub const XK_ISO_LEFT_TAB: u32 = 0xfe20;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_LEFT: u32 = 0xff51;
pub const XK_UP: u32 = 0xff52;
pub const XK_RIGHT: u32 = 0xff53;
pub const XK_DOWN: u32 = 0xff54;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_GRAVE: u32 = 0x60;
pub const XK_ASCIITILDE: u32 = 0x7e;
pub const XK_Q: u32 = 0x71;
pub const XK_W: u32 = 0x77;

const SHIFT: u32 = crate::input::MOD_SHIFT;

fn group_by_app(windows: &[Window]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for w in windows {
        // Windows without an app id each count as their own app.
        let key = if w.app_id.is_empty() {
            format!("window-{}", w.id)
        } else {
            w.app_id.clone()
        };
        match items.iter_mut().find(|i| i.app_id == key) {
            Some(item) => item.windows.push(w.id),
            None => items.push(Item {
                app_id: key,
                title: w.title.clone(),
                windows: vec![w.id],
            }),
        }
    }
    for it in &mut items {
        if it.app_id.starts_with("window-") {
            it.app_id.clear();
        }
    }
    items
}

fn window_items(windows: &[Window]) -> Vec<Item> {
    windows
        .iter()
        .map(|w| Item {
            app_id: w.app_id.clone(),
            title: w.title.clone(),
            windows: vec![w.id],
        })
        .collect()
}

impl Switcher {
    /// Open a switcher. `mru` is most recently used first; the focused window
    /// (if any) is first. Returns `None` when there is nothing to switch to.
    pub fn open(mode: Mode, backward: bool, mru: &[Window], modifiers: u32) -> Option<Self> {
        let items = match mode {
            Mode::Applications => group_by_app(mru),
            Mode::Windows => window_items(mru),
            Mode::Group => {
                let app = &mru.first()?.app_id;
                let same: Vec<Window> = mru.iter().filter(|w| &w.app_id == app).cloned().collect();
                window_items(&same)
            }
        };
        if items.is_empty() {
            return None;
        }
        let n = items.len();
        let selected = if backward { n - 1 } else { 1 % n };
        Some(Self {
            mode,
            items,
            selected,
            sub: None,
            modifiers,
        })
    }

    fn step(&mut self, forward: bool) {
        let n = self.items.len();
        self.sub = None;
        self.selected = if forward {
            (self.selected + 1) % n
        } else {
            (self.selected + n - 1) % n
        };
    }

    fn step_sub(&mut self, forward: bool) {
        let n = self.items[self.selected].windows.len();
        if n == 0 {
            return;
        }
        self.sub = Some(match self.sub {
            None => {
                // The first press goes to the app's second window, as GNOME's.
                if forward { 1 % n } else { n - 1 }
            }
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        });
    }

    /// The selection the popup should highlight (item, window in item).
    pub fn selection(&self) -> (usize, Option<usize>) {
        (self.selected, self.sub)
    }

    /// What releasing the modifier now would do.
    pub fn finish(&self) -> Outcome {
        let item = &self.items[self.selected];
        let window = item.windows[self.sub.unwrap_or(0).min(item.windows.len() - 1)];
        let raise = if self.mode == Mode::Applications && self.sub.is_none() {
            // Like shell_app_activate(): bring the app's other windows up too,
            // least recent first, so the most recent ends on top.
            item.windows
                .iter()
                .rev()
                .copied()
                .filter(|&w| w != window)
                .collect()
        } else {
            Vec::new()
        };
        Outcome::Activate { window, raise }
    }

    /// Select an item directly (clicked in the popup).
    pub fn pick(&mut self, index: usize) -> Option<Outcome> {
        if index >= self.items.len() {
            return None;
        }
        self.selected = index;
        self.sub = None;
        Some(self.finish())
    }

    /// Drop windows that went away; returns false when nothing is left.
    pub fn remove_window(&mut self, id: u64) -> bool {
        for it in &mut self.items {
            it.windows.retain(|&w| w != id);
        }
        let before = self.items.len();
        let sel_id = self.items.get(self.selected).map(|i| i.app_id.clone());
        self.items.retain(|i| !i.windows.is_empty());
        if self.items.is_empty() {
            return false;
        }
        if self.items.len() != before {
            self.selected = sel_id
                .and_then(|a| self.items.iter().position(|i| i.app_id == a))
                .unwrap_or(self.selected.min(self.items.len() - 1));
        }
        self.sub = None;
        true
    }

    /// A key went down while the switcher is open.
    ///
    /// `binding` is the switcher action the key is bound to, if any, so a
    /// repeated Alt+Tab or Alt+` steps the same way the shortcut opened it.
    pub fn key_down(&mut self, keysym: u32, mods: u32, binding: Option<(Mode, bool)>) -> Outcome {
        let shift = mods & SHIFT != 0;
        if let Some((mode, backward)) = binding {
            if mode == Mode::Group && self.mode == Mode::Applications {
                self.step_sub(!backward);
            } else {
                self.step(!backward);
            }
            return Outcome::Consumed;
        }
        match keysym {
            XK_TAB => self.step(!shift),
            XK_ISO_LEFT_TAB => self.step(false),
            XK_ESCAPE => return Outcome::Cancel,
            XK_RETURN => return self.finish(),
            XK_LEFT => self.step(false),
            XK_RIGHT => self.step(true),
            XK_GRAVE | XK_ASCIITILDE if self.mode == Mode::Applications => {
                self.step_sub(!shift && keysym == XK_GRAVE)
            }
            XK_DOWN if self.mode == Mode::Applications => self.step_sub(true),
            XK_UP if self.mode == Mode::Applications => self.sub = None,
            XK_Q if self.mode == Mode::Applications => {
                return Outcome::Close(self.items[self.selected].windows.clone());
            }
            XK_W if self.mode != Mode::Applications => {
                return Outcome::Close(self.items[self.selected].windows.clone());
            }
            _ => {}
        }
        Outcome::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: u64, app: &str) -> Window {
        Window {
            id,
            app_id: app.into(),
            title: format!("{app} {id}"),
        }
    }

    fn mru() -> Vec<Window> {
        vec![w(1, "term"), w(2, "files"), w(3, "term"), w(4, "editor")]
    }

    #[test]
    fn apps_grouped_and_second_selected() {
        let s = Switcher::open(Mode::Applications, false, &mru(), 4).unwrap();
        let apps: Vec<_> = s.items.iter().map(|i| i.app_id.as_str()).collect();
        assert_eq!(apps, ["term", "files", "editor"]);
        assert_eq!(s.items[0].windows, [1, 3]);
        assert_eq!(s.selected, 1);
        assert_eq!(
            s.finish(),
            Outcome::Activate {
                window: 2,
                raise: vec![]
            }
        );
    }

    #[test]
    fn backward_starts_at_end_and_wraps() {
        let mut s = Switcher::open(Mode::Applications, true, &mru(), 4).unwrap();
        assert_eq!(s.selected, 2);
        s.key_down(XK_TAB, 0, None);
        assert_eq!(s.selected, 0);
        s.key_down(XK_ISO_LEFT_TAB, SHIFT, None);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn activating_app_raises_its_other_windows() {
        let mut s = Switcher::open(Mode::Applications, false, &mru(), 4).unwrap();
        s.key_down(XK_LEFT, 0, None);
        assert_eq!(
            s.finish(),
            Outcome::Activate {
                window: 1,
                raise: vec![3]
            }
        );
    }

    #[test]
    fn grave_cycles_windows_of_selected_app() {
        let mut s = Switcher::open(Mode::Applications, false, &mru(), 4).unwrap();
        s.key_down(XK_LEFT, 0, None); // term
        s.key_down(0, 4, Some((Mode::Group, false)));
        assert_eq!(s.selection(), (0, Some(1)));
        assert_eq!(
            s.finish(),
            Outcome::Activate {
                window: 3,
                raise: vec![]
            }
        );
    }

    #[test]
    fn group_mode_lists_only_current_app() {
        let s = Switcher::open(Mode::Group, false, &mru(), 4).unwrap();
        assert_eq!(s.items.len(), 2);
        assert_eq!(
            s.finish(),
            Outcome::Activate {
                window: 3,
                raise: vec![]
            }
        );
    }

    #[test]
    fn escape_cancels_and_q_closes_app() {
        let mut s = Switcher::open(Mode::Applications, false, &mru(), 4).unwrap();
        assert_eq!(s.key_down(XK_ESCAPE, 4, None), Outcome::Cancel);
        assert_eq!(s.key_down(XK_Q, 4, None), Outcome::Close(vec![2]));
        assert!(s.remove_window(2));
        assert_eq!(s.items.len(), 2);
        assert!(s.selected < 2);
    }

    #[test]
    fn single_window_still_opens() {
        let s = Switcher::open(Mode::Applications, false, &[w(9, "a")], 4).unwrap();
        assert_eq!(s.selected, 0);
        assert!(Switcher::open(Mode::Applications, false, &[], 4).is_none());
    }
}
