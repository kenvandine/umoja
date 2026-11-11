//! The keyboard shortcuts guide (Super+/).
//!
//! A full-screen, frosted sheet listing every shortcut the desktop answers
//! to, grouped the way people think about them (desktop, apps, windows,
//! tiling, workspaces, screenshots, assistant, their own). The list comes
//! from the compositor (`org.umoja.Compositor.GetShortcuts`), which reads
//! the same GSettings keys it binds, so whatever is shown is what actually
//! works, including shortcuts changed in Settings and GNOME custom
//! shortcuts. Typing filters it; Esc, the shortcut again, or a click
//! outside the cards closes it.

use gio::prelude::*;
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::{dbus, i18n};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Desktop,
    Apps,
    Windows,
    Tiling,
    Workspaces,
    Screenshots,
    Assistant,
    Custom,
    Other,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::Desktop => "Desktop",
            Group::Apps => "Switching",
            Group::Windows => "Windows",
            Group::Tiling => "Tiling",
            Group::Workspaces => "Workspaces",
            Group::Screenshots => "Screenshots",
            Group::Assistant => "Assistant",
            Group::Custom => "Your Shortcuts",
            Group::Other => "Other",
        }
    }

    fn icons(self) -> &'static [&'static str] {
        match self {
            Group::Desktop => &["user-desktop-symbolic", "computer-symbolic"],
            Group::Apps => &["view-app-grid-symbolic", "view-grid-symbolic"],
            Group::Windows => &["focus-windows-symbolic", "window-new-symbolic"],
            Group::Tiling => &["view-grid-symbolic", "view-app-grid-symbolic"],
            Group::Workspaces => &["view-paged-symbolic", "view-dual-symbolic"],
            Group::Screenshots => &["screenshot-recorded-symbolic", "camera-photo-symbolic"],
            Group::Assistant => &["starred-symbolic", "dialog-question-symbolic"],
            Group::Custom => &[
                "preferences-desktop-keyboard-shortcuts-symbolic",
                "input-keyboard-symbolic",
            ],
            Group::Other => &["input-keyboard-symbolic"],
        }
    }

    /// A line under the group's shortcuts.
    fn note(self) -> Option<&'static str> {
        match self {
            Group::Apps => Some("Add Shift to go backwards."),
            Group::Custom => Some("Added in Settings › Keyboard."),
            _ => None,
        }
    }
}

/// (id, group, label) in display order. Ids are the GSettings keys the
/// compositor reads; `#` stands for a workspace number. Ids that share a
/// label in a group become one row with each accelerator as an alternative.
const CATALOG: &[(&str, Group, &str)] = &[
    ("ask-umoja", Group::Desktop, "Ask Umoja"),
    ("overlay-key", Group::Desktop, "Activities overview"),
    ("toggle-overview", Group::Desktop, "Activities overview"),
    ("toggle-application-view", Group::Desktop, "Show apps"),
    (
        "shift-overview-up",
        Group::Desktop,
        "Step up to overview, then apps",
    ),
    (
        "shift-overview-down",
        Group::Desktop,
        "Step back down to the desktop",
    ),
    (
        "toggle-message-tray",
        Group::Desktop,
        "Notifications and calendar",
    ),
    ("switch-panels", Group::Desktop, "Focus the top bar or dock"),
    (
        "cycle-panels",
        Group::Desktop,
        "Cycle focus through top bar and dock",
    ),
    ("screensaver", Group::Desktop, "Lock the screen"),
    (
        "shortcuts-guide-hotkey",
        Group::Desktop,
        "Keyboard shortcuts",
    ),
    ("switch-applications", Group::Apps, "Switch apps"),
    ("switch-group", Group::Apps, "Switch windows of the app"),
    ("switch-windows", Group::Apps, "Switch windows"),
    (
        "cycle-group",
        Group::Apps,
        "Cycle the app's windows, no popup",
    ),
    ("cycle-windows", Group::Apps, "Cycle all windows, no popup"),
    ("close", Group::Windows, "Close"),
    ("minimize", Group::Windows, "Hide"),
    ("maximize", Group::Windows, "Maximize"),
    ("unmaximize", Group::Windows, "Restore"),
    ("toggle-maximized", Group::Windows, "Maximize or restore"),
    ("toggle-fullscreen", Group::Windows, "Fullscreen"),
    ("activate-window-menu", Group::Windows, "Window menu"),
    ("begin-move", Group::Windows, "Move with the keyboard"),
    ("begin-resize", Group::Windows, "Resize with the keyboard"),
    ("always-on-top", Group::Windows, "Always on top"),
    (
        "toggle-on-all-workspaces",
        Group::Windows,
        "On every workspace",
    ),
    (
        "move-to-monitor-left",
        Group::Windows,
        "Move to the display on the left",
    ),
    (
        "move-to-monitor-right",
        Group::Windows,
        "Move to the display on the right",
    ),
    (
        "move-to-monitor-up",
        Group::Windows,
        "Move to the display above",
    ),
    (
        "move-to-monitor-down",
        Group::Windows,
        "Move to the display below",
    ),
    ("toggle-tiled-left", Group::Tiling, "Left half"),
    ("tile-left-half", Group::Tiling, "Left half"),
    ("toggle-tiled-right", Group::Tiling, "Right half"),
    ("tile-right-half", Group::Tiling, "Right half"),
    ("tile-top-half", Group::Tiling, "Top half"),
    ("tile-bottom-half", Group::Tiling, "Bottom half"),
    ("tile-topleft-quarter", Group::Tiling, "Top-left quarter"),
    ("tile-top-left", Group::Tiling, "Top-left quarter"),
    ("tile-topright-quarter", Group::Tiling, "Top-right quarter"),
    ("tile-top-right", Group::Tiling, "Top-right quarter"),
    (
        "tile-bottomleft-quarter",
        Group::Tiling,
        "Bottom-left quarter",
    ),
    ("tile-bottom-left", Group::Tiling, "Bottom-left quarter"),
    (
        "tile-bottomright-quarter",
        Group::Tiling,
        "Bottom-right quarter",
    ),
    ("tile-bottom-right", Group::Tiling, "Bottom-right quarter"),
    ("tile-maximize", Group::Tiling, "Maximize"),
    ("restore-window", Group::Tiling, "Restore"),
    (
        "switch-to-workspace-left",
        Group::Workspaces,
        "Previous workspace",
    ),
    (
        "switch-to-workspace-up",
        Group::Workspaces,
        "Previous workspace",
    ),
    (
        "switch-to-workspace-right",
        Group::Workspaces,
        "Next workspace",
    ),
    (
        "switch-to-workspace-down",
        Group::Workspaces,
        "Next workspace",
    ),
    ("switch-to-workspace-#", Group::Workspaces, "Workspace #"),
    (
        "switch-to-workspace-last",
        Group::Workspaces,
        "Last workspace",
    ),
    (
        "move-to-workspace-left",
        Group::Workspaces,
        "Move window to previous workspace",
    ),
    (
        "move-to-workspace-up",
        Group::Workspaces,
        "Move window to previous workspace",
    ),
    (
        "move-to-workspace-right",
        Group::Workspaces,
        "Move window to next workspace",
    ),
    (
        "move-to-workspace-down",
        Group::Workspaces,
        "Move window to next workspace",
    ),
    (
        "move-to-workspace-#",
        Group::Workspaces,
        "Move window to workspace #",
    ),
    (
        "move-to-workspace-last",
        Group::Workspaces,
        "Move window to last workspace",
    ),
    (
        "show-screenshot-ui",
        Group::Screenshots,
        "Take a screenshot",
    ),
    (
        "screenshot",
        Group::Screenshots,
        "Screenshot the whole screen",
    ),
    (
        "screenshot-window",
        Group::Screenshots,
        "Screenshot the current window",
    ),
    (
        "show-screen-recording-ui",
        Group::Screenshots,
        "Record the screen",
    ),
    (
        "copy-text-hotkey",
        Group::Screenshots,
        "Copy text from the screen",
    ),
    (
        "ask-about-screen-hotkey",
        Group::Assistant,
        "Ask about the screen",
    ),
    (
        "clipboard-history-hotkey",
        Group::Assistant,
        "Clipboard history",
    ),
    (
        "ai-agent-hotkey",
        Group::Assistant,
        "Open your coding agent",
    ),
    ("dictation-hotkey", Group::Assistant, "Dictate with Myna"),
];

/// Reverse directions: the group's note says "add Shift" instead.
const HIDDEN: &[&str] = &[
    "switch-applications-backward",
    "switch-group-backward",
    "switch-windows-backward",
    "cycle-group-backward",
    "cycle-windows-backward",
    "switch-panels-backward",
    "cycle-panels-backward",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    /// Alternatives, each as its keycaps.
    pub keys: Vec<Vec<String>>,
    order: (usize, u32),
}

/// Group, label (in English, translated when shown) and sort order of a
/// shortcut id.
fn classify(id: &str, seq: u32) -> (Group, String, (usize, u32)) {
    if let Some(name) = id.strip_prefix("custom:") {
        return (Group::Custom, name.to_string(), (usize::MAX - 1, seq));
    }
    for (i, (key, group, label)) in CATALOG.iter().enumerate() {
        if let Some(prefix) = key.strip_suffix('#') {
            if let Some(n) = id.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok()) {
                return (*group, label.replace('#', &n.to_string()), (i, n));
            }
        } else if *key == id {
            return (*group, label.to_string(), (i, 0));
        }
    }
    // A key we don't know yet: "volume-up" reads as "Volume up".
    let mut label = id.replace('-', " ");
    if let Some(first) = label.get(..1) {
        label = first.to_uppercase() + &label[1..];
    }
    (Group::Other, label, (usize::MAX, seq))
}

const MOD_SUPER: u8 = 1;
const MOD_CTRL: u8 = 2;
const MOD_ALT: u8 = 4;
const MOD_SHIFT: u8 = 8;

/// Split a GTK accelerator (`<Super><Shift>Page_Up`) into modifiers and
/// the key's name.
fn parse(accel: &str) -> Option<(u8, String)> {
    let mut mods = 0;
    let mut rest = accel.trim();
    while let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?;
        mods |= match stripped[..end].to_ascii_lowercase().as_str() {
            "super" | "mod4" | "meta" => MOD_SUPER,
            "shift" => MOD_SHIFT,
            "ctrl" | "control" | "primary" => MOD_CTRL,
            "alt" | "mod1" => MOD_ALT,
            _ => return None,
        };
        rest = &stripped[end + 1..];
    }
    if rest.is_empty() || rest.eq_ignore_ascii_case("disabled") {
        return None;
    }
    let key = match rest {
        "Above_Tab" => "grave",
        // The overlay key is a whole key on its own.
        "Super_L" | "Super_R" | "Super" => {
            mods |= MOD_SUPER;
            ""
        }
        k => k,
    };
    Some((mods, key.to_string()))
}

/// How a key is printed on its cap.
fn key_label(name: &str) -> String {
    let s = match name {
        "Left" => "←",
        "Right" => "→",
        "Up" => "↑",
        "Down" => "↓",
        "Page_Up" => "Page Up",
        "Page_Down" => "Page Down",
        "Escape" => "Esc",
        "space" => "Space",
        "Return" => "Enter",
        "BackSpace" => "Backspace",
        "Print" => "Print Screen",
        "grave" => "`",
        "Alt_L" => "Alt",
        "slash" => "/",
        "backslash" => "\\",
        "question" => "?",
        "comma" => ",",
        "period" => ".",
        "minus" => "-",
        "equal" => "=",
        "plus" => "+",
        "semicolon" => ";",
        "apostrophe" => "'",
        "bracketleft" => "[",
        "bracketright" => "]",
        _ => "",
    };
    if !s.is_empty() {
        return s.to_string();
    }
    if let Some(n) = name.strip_prefix("KP_") {
        return format!("Keypad {n}");
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return c.to_uppercase().to_string();
    }
    name.replace('_', " ")
}

/// The keycaps of an accelerator: modifiers first (Super, Ctrl, Alt,
/// Shift), then the key.
pub fn keycaps(accel: &str) -> Option<Vec<String>> {
    let (mods, key) = parse(accel)?;
    let mut caps = Vec::new();
    for (bit, name) in [
        (MOD_SUPER, "Super"),
        (MOD_CTRL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
    ] {
        if mods & bit != 0 {
            caps.push(name.to_string());
        }
    }
    if !key.is_empty() {
        caps.push(key_label(&key));
    }
    Some(caps)
}

/// Group the compositor's (id, accelerator) list into rows. Hardware keys
/// (`XF86…`: volume, brightness, media) are left out; they say what they
/// do on the key.
pub fn model(shortcuts: &[(String, String)]) -> Vec<(Group, Vec<Row>)> {
    let mut groups: Vec<(Group, Vec<Row>)> = Vec::new();
    for (seq, (id, accel)) in shortcuts.iter().enumerate() {
        if HIDDEN.contains(&id.as_str()) || id.is_empty() {
            continue;
        }
        let Some((_, key)) = parse(accel) else {
            continue;
        };
        if key.starts_with("XF86") {
            continue;
        }
        let Some(caps) = keycaps(accel) else { continue };
        let (group, label, order) = classify(id, seq as u32);
        let rows = match groups.iter_mut().find(|(g, _)| *g == group) {
            Some((_, rows)) => rows,
            None => {
                groups.push((group, Vec::new()));
                &mut groups.last_mut().expect("just pushed").1
            }
        };
        match rows.iter_mut().find(|r| r.label == label) {
            Some(row) => {
                let dup = row.keys.contains(&caps);
                if !dup {
                    row.keys.push(caps);
                }
                row.order = row.order.min(order);
            }
            None => rows.push(Row {
                label,
                keys: vec![caps],
                order,
            }),
        }
    }
    for (_, rows) in &mut groups {
        rows.sort_by_key(|r| r.order);
    }
    groups.sort_by_key(|(g, _)| *g);
    groups
}

async fn fetch() -> Vec<(String, String)> {
    let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
        return Vec::new();
    };
    let reply = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            "GetShortcuts",
            None,
            glib::VariantTy::new("(a(ss))").ok(),
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await;
    match reply {
        Ok(v) => v
            .get::<(Vec<(String, String)>,)>()
            .map(|(l,)| l)
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read shortcuts from the compositor");
            Vec::new()
        }
    }
}

/// One group's card and its rows, with the text the search matches.
struct Card {
    widget: gtk::Box,
    title: String,
    rows: Vec<(gtk::Box, String)>,
}

/// Cards are laid out in columns about this wide (logical pixels).
const COLUMN_WIDTH: i32 = 380;
const MAX_COLUMNS: i32 = 4;

pub struct ShortcutsGuide {
    window: gtk::ApplicationWindow,
    search: gtk::SearchEntry,
    columns: gtk::Box,
    empty: gtk::Label,
    cards: RefCell<Vec<Card>>,
    n_columns: Cell<i32>,
    width: Cell<i32>,
    /// Bumped on every open/close, so a slow fetch can't reopen a guide
    /// the user already closed.
    generation: Cell<u64>,
}

impl ShortcutsGuide {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-shortcuts"])
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-shortcuts");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        umoja_common::blur::blur_behind(&window);

        let title = gtk::Label::builder()
            .label(i18n::umoja("Keyboard Shortcuts"))
            .css_classes(["shortcuts-title"])
            .build();
        let subtitle = gtk::Label::builder()
            .label(i18n::umoja(
                "Press Esc to close. Change shortcuts in Settings › Keyboard.",
            ))
            .css_classes(["shortcuts-subtitle"])
            .build();
        let search = gtk::SearchEntry::builder()
            .placeholder_text(i18n::umoja("Type to find a shortcut"))
            .width_request(360)
            .halign(gtk::Align::Center)
            .css_classes(["shortcuts-search"])
            .build();
        search.set_key_capture_widget(Some(&window));
        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.set_halign(gtk::Align::Center);
        header.append(&title);
        header.append(&subtitle);
        header.append(&search);
        search.set_margin_top(14);

        let columns = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .homogeneous(true)
            .valign(gtk::Align::Start)
            .build();
        let empty = gtk::Label::builder()
            .label(i18n::umoja("No shortcuts match."))
            .css_classes(["shortcuts-empty"])
            .visible(false)
            .margin_top(48)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&columns);
        body.append(&empty);
        let clamp = adw::Clamp::builder()
            .maximum_size(MAX_COLUMNS * (COLUMN_WIDTH + 18))
            .tightening_threshold(COLUMN_WIDTH)
            .child(&body)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .css_classes(["shortcuts-scroller"])
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 28);
        root.add_css_class("shortcuts-root");
        root.append(&header);
        root.append(&scroller);
        window.set_child(Some(&root));

        let guide = Rc::new(Self {
            window,
            search,
            columns,
            empty,
            cards: RefCell::default(),
            n_columns: Cell::new(0),
            width: Cell::new(0),
            generation: Cell::new(0),
        });
        guide.connect_signals(&root);
        guide
    }

    fn connect_signals(self: &Rc<Self>, root: &gtk::Box) {
        // Esc clears the search first, then closes.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            if let Some(g) = weak.upgrade() {
                if g.search.text().is_empty() {
                    g.hide();
                } else {
                    g.search.set_text("");
                }
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);

        // A click anywhere but on a card or the search closes it.
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        let root_weak = root.downgrade();
        click.connect_released(move |_, _, x, y| {
            let (Some(g), Some(root)) = (weak.upgrade(), root_weak.upgrade()) else {
                return;
            };
            let mut hit = root.pick(x, y, gtk::PickFlags::DEFAULT);
            while let Some(w) = hit {
                if w.has_css_class("shortcut-group")
                    || w.has_css_class("shortcuts-search")
                    || w.is::<gtk::Scrollbar>()
                {
                    return;
                }
                hit = w.parent();
            }
            g.hide();
        });
        root.add_controller(click);

        let weak = Rc::downgrade(self);
        self.search.connect_search_changed(move |_| {
            if let Some(g) = weak.upgrade() {
                g.filter();
            }
        });

        // Lay the cards out again when the sheet's width changes columns.
        let weak = Rc::downgrade(self);
        self.window.connect_realize(move |w| {
            let Some(surface) = w.surface() else { return };
            let weak = weak.clone();
            surface.connect_layout(move |_, width, _| {
                let Some(g) = weak.upgrade() else { return };
                if g.width.replace(width) != width {
                    let weak = Rc::downgrade(&g);
                    glib::idle_add_local_once(move || {
                        if let Some(g) = weak.upgrade() {
                            g.relayout(false);
                        }
                    });
                }
            });
        });
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        self.generation.set(self.generation.get() + 1);
        self.window.set_visible(false);
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.is_visible() {
            self.hide();
        } else {
            self.open();
        }
    }

    /// Read the shortcuts afresh (they may have changed in Settings) and
    /// show the guide.
    pub fn open(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let shortcuts = fetch().await;
            let Some(g) = weak.upgrade() else { return };
            if g.generation.get() != generation {
                return;
            }
            g.populate(&model(&shortcuts));
            g.search.set_text("");
            g.window.present();
        });
    }

    fn populate(&self, groups: &[(Group, Vec<Row>)]) {
        for card in self.cards.borrow_mut().drain(..) {
            detach(card.widget.upcast_ref());
        }
        let mut cards = Vec::new();
        for (group, rows) in groups {
            cards.push(card(*group, rows));
        }
        *self.cards.borrow_mut() = cards;
        if groups.is_empty() {
            self.empty.set_label(&i18n::umoja(
                "Couldn't read the shortcuts from the desktop.",
            ));
        } else {
            self.empty.set_label(&i18n::umoja("No shortcuts match."));
        }
        self.relayout(true);
    }

    fn filter(&self) {
        let query = self.search.text().to_lowercase();
        let query = query.trim();
        for card in self.cards.borrow().iter() {
            let whole = !query.is_empty() && card.title.contains(query);
            let mut any = false;
            for (row, text) in &card.rows {
                let show = query.is_empty() || whole || text.contains(query);
                row.set_visible(show);
                any |= show;
            }
            card.widget.set_visible(any);
        }
        self.relayout(true);
    }

    /// Spread the visible cards over as many columns as fit, each card
    /// going to the shortest column so far (a masonry layout).
    fn relayout(&self, force: bool) {
        let width = match self.width.get() {
            0 => gdk::Display::default()
                .and_then(|d| d.monitors().item(0))
                .and_downcast::<gdk::Monitor>()
                .map(|m| m.geometry().width())
                .unwrap_or(1600),
            w => w,
        };
        let n = ((width - 96) / (COLUMN_WIDTH + 18)).clamp(1, MAX_COLUMNS);
        if !force && n == self.n_columns.get() {
            return;
        }
        self.n_columns.set(n);
        let cards = self.cards.borrow();
        for card in cards.iter() {
            detach(card.widget.upcast_ref());
        }
        while let Some(child) = self.columns.first_child() {
            self.columns.remove(&child);
        }
        let cols: Vec<gtk::Box> = (0..n)
            .map(|_| {
                let col = gtk::Box::new(gtk::Orientation::Vertical, 18);
                self.columns.append(&col);
                col
            })
            .collect();
        let mut heights = vec![0usize; n as usize];
        let mut shown = 0;
        for card in cards.iter().filter(|c| c.widget.is_visible()) {
            let rows = card.rows.iter().filter(|(r, _)| r.is_visible()).count();
            let (i, _) = heights
                .iter()
                .enumerate()
                .min_by_key(|(_, h)| **h)
                .expect("at least one column");
            cols[i].append(&card.widget);
            heights[i] += rows + 3;
            shown += 1;
        }
        self.empty.set_visible(shown == 0);
    }
}

fn detach(w: &gtk::Widget) {
    if let Some(parent) = w.parent().and_downcast::<gtk::Box>() {
        parent.remove(w);
    }
}

fn card(group: Group, rows: &[Row]) -> Card {
    let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
    widget.add_css_class("shortcut-group");
    let title = i18n::umoja(group.title());
    let icon = gtk::Image::from_gicon(&gio::ThemedIcon::from_names(group.icons()));
    icon.add_css_class("group-icon");
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    heading.add_css_class("group-heading");
    heading.append(&icon);
    heading.append(
        &gtk::Label::builder()
            .label(&title)
            .xalign(0.0)
            .css_classes(["group-title"])
            .build(),
    );
    widget.append(&heading);

    let mut out = Vec::new();
    for row in rows {
        let label = if group == Group::Custom || group == Group::Other {
            row.label.clone()
        } else {
            i18n::umoja(&row.label)
        };
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        line.add_css_class("shortcut-row");
        line.append(
            &gtk::Label::builder()
                .label(&label)
                .xalign(0.0)
                .hexpand(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .css_classes(["shortcut-label"])
                .build(),
        );
        let alts = gtk::Box::new(gtk::Orientation::Vertical, 6);
        alts.set_halign(gtk::Align::End);
        alts.set_valign(gtk::Align::Center);
        for caps in &row.keys {
            alts.append(&keys_widget(caps));
        }
        line.append(&alts);
        widget.append(&line);
        let text = format!(
            "{} {} {}",
            label,
            row.label,
            row.keys
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        )
        .to_lowercase();
        out.push((line, text));
    }
    if let Some(note) = group.note() {
        widget.append(
            &gtk::Label::builder()
                .label(i18n::umoja(note))
                .xalign(0.0)
                .wrap(true)
                .css_classes(["group-note"])
                .build(),
        );
    }
    Card {
        widget,
        title: title.to_lowercase(),
        rows: out,
    }
}

fn keys_widget(caps: &[String]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    b.set_halign(gtk::Align::End);
    for (i, cap) in caps.iter().enumerate() {
        if i > 0 {
            b.append(
                &gtk::Label::builder()
                    .label("+")
                    .css_classes(["keycap-plus"])
                    .build(),
            );
        }
        let wide = cap.chars().count() > 1;
        let l = gtk::Label::builder()
            .label(cap)
            .css_classes(["keycap"])
            .build();
        if !wide {
            l.add_css_class("single");
        }
        b.append(&l);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, accel: &str) -> (String, String) {
        (id.to_string(), accel.to_string())
    }

    #[test]
    fn keycaps_read_like_keys() {
        assert_eq!(
            keycaps("<Shift><Super>Page_Up").unwrap(),
            ["Super", "Shift", "Page Up"]
        );
        assert_eq!(keycaps("<Super>Left").unwrap(), ["Super", "←"]);
        assert_eq!(keycaps("<Alt>Above_Tab").unwrap(), ["Alt", "`"]);
        assert_eq!(
            keycaps("<Primary><Alt>Delete").unwrap(),
            ["Ctrl", "Alt", "Delete"]
        );
        assert_eq!(keycaps("<Super>slash").unwrap(), ["Super", "/"]);
        assert_eq!(keycaps("<Super>KP_7").unwrap(), ["Super", "Keypad 7"]);
        assert_eq!(keycaps("<Super>h").unwrap(), ["Super", "H"]);
        assert_eq!(keycaps("Super_L").unwrap(), ["Super"]);
        assert_eq!(keycaps("Print").unwrap(), ["Print Screen"]);
        assert_eq!(keycaps(""), None);
        assert_eq!(keycaps("disabled"), None);
    }

    #[test]
    fn groups_merge_and_order() {
        let m = model(&[
            s("overlay-key", "Super_L"),
            s("toggle-tiled-left", "<Super>Left"),
            s("tile-left-half", "<Super>Left"),
            s("tile-left-half", "<Super>KP_4"),
            s("switch-to-workspace-left", "<Super>Page_Up"),
            s("switch-to-workspace-up", "<Super>Page_Up"),
            s("switch-to-workspace-up", "<Control><Alt>Up"),
            s("switch-to-workspace-1", "<Super>Home"),
            s("switch-applications", "<Super>Tab"),
            s("switch-applications-backward", "<Shift><Super>Tab"),
            s("volume-up", "XF86AudioRaiseVolume"),
            s("screensaver", "<Super>l"),
            s("screensaver", "XF86ScreenSaver"),
            s("custom:Myna", "<Super>j"),
            s("some-new-key", "<Super>y"),
            s("toggle-overview", "<Super>s"),
        ]);
        let groups: Vec<Group> = m.iter().map(|(g, _)| *g).collect();
        assert_eq!(
            groups,
            [
                Group::Desktop,
                Group::Apps,
                Group::Tiling,
                Group::Workspaces,
                Group::Custom,
                Group::Other
            ]
        );
        let rows = |g: Group| m.iter().find(|(x, _)| *x == g).unwrap().1.clone();
        let desktop = rows(Group::Desktop);
        assert_eq!(desktop[0].label, "Activities overview");
        assert_eq!(desktop[0].keys, vec![vec!["Super"], vec!["Super", "S"]]);
        assert_eq!(desktop[1].label, "Lock the screen");
        assert_eq!(desktop[1].keys.len(), 1, "hardware key left out");
        let tiling = rows(Group::Tiling);
        assert_eq!(tiling.len(), 1);
        assert_eq!(
            tiling[0].keys.len(),
            2,
            "same key from two schemas shown once"
        );
        let ws = rows(Group::Workspaces);
        assert_eq!(ws[0].label, "Previous workspace");
        assert_eq!(ws[0].keys.len(), 2);
        assert_eq!(ws[1].label, "Workspace 1");
        assert_eq!(rows(Group::Apps).len(), 1, "backward variants are a note");
        assert_eq!(rows(Group::Custom)[0].label, "Myna");
        assert_eq!(rows(Group::Other)[0].label, "Some new key");
    }
}
