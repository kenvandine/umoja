//! umoja-dock: an Ubuntu Dock work-alike.
//!
//! Reads the same `org.gnome.shell.extensions.dash-to-dock` keys as Ubuntu
//! Dock, pins apps from `org.umoja.shell favorite-apps`, and shows running
//! windows (with dots) from the compositor's foreign-toplevel protocol.

mod autohide;
mod dock_layout;
mod drag;
mod keynav;
mod launcher;
mod launcher_entry;
mod placement;
mod previews;
mod slide;
mod tracker;

use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use libadwaita::prelude::AnimationExt;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;
use tracker::{Command, WindowId, WindowInfo};
use umoja_common::gsettings as gs;
use umoja_common::types::DockPosition;
use umoja_common::{dbus, i18n};

const APP_ID: &str = "org.umoja.Dock";
/// Gap between the floating dock and the screen edge it sits against.
/// Panel mode (extend-height) has none: like Ubuntu Dock's, it is a flat
/// bar touching its edge and both ends of it.
const EDGE_GAP: i32 = 2;
/// Pixels between the dock card and its menus, as umoja-panel's menu_gap.
const MENU_GAP: i32 = 6;
/// How long Super is held before the dock shows its Super+N numbers, so a
/// tap for the overview doesn't flash them.
const SUPER_HOLD_MS: u64 = 400;
/// switch-to-application-1..9.
const MAX_APP_KEYS: usize = 9;

#[derive(Clone, Copy, Debug)]
struct Config {
    position: DockPosition,
    icon_size: i32,
    fixed: bool,
    extend: bool,
    show_apps_at_top: bool,
    /// Ubuntu Dock lists windows under an "All Windows" submenu when set.
    show_windows_preview: bool,
    default_windows_preview_to_open: bool,
    /// Auto-hide and intellihide, used when `fixed` is off.
    hide: autohide::HideConfig,
    /// isolate-monitors: each dock shows only its own monitor's windows.
    isolate_monitors: bool,
    /// isolate-workspaces: only windows on the current workspace.
    isolate_workspaces: bool,
    /// What middle, Shift and Shift+middle clicks and scrolling do.
    middle_click: ClickAction,
    shift_click: ClickAction,
    shift_middle_click: ClickAction,
    scroll: ScrollAction,
    /// hot-keys + hotkeys-overlay: holding Super shows each icon's number.
    hotkeys_overlay: bool,
    /// hotkeys-show-dock: a hidden dock comes out while the numbers show.
    hotkeys_show_dock: bool,
}

/// dash-to-dock's click-action values Umoja does.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ClickAction {
    /// focus-or-previews and the other "normal click" choices.
    Focus,
    Launch,
    Minimize,
    Quit,
    Previews,
    Nothing,
}

impl ClickAction {
    fn parse(s: &str) -> Self {
        match s {
            "launch" => Self::Launch,
            "minimize" => Self::Minimize,
            "quit" => Self::Quit,
            "previews" => Self::Previews,
            "skip" | "none" => Self::Nothing,
            _ => Self::Focus,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ScrollAction {
    Nothing,
    CycleWindows,
    SwitchWorkspace,
}

impl Config {
    fn load() -> Self {
        let d = gs::DASH_TO_DOCK;
        Self {
            position: DockPosition::from_gsettings(&gs::get_string(d, "dock-position", "LEFT")),
            icon_size: gs::get_int(d, "dash-max-icon-size", 48).clamp(16, 128),
            // Ubuntu ships dock-fixed=true; autohide only matters when unfixed.
            fixed: gs::get_bool(d, "dock-fixed", true),
            // data/90_umoja.gschema.override makes Umoja default to the floating dock.
            extend: gs::get_bool(d, "extend-height", false),
            show_apps_at_top: gs::get_bool(d, "show-apps-at-top", false),
            show_windows_preview: gs::get_bool(d, "show-windows-preview", true),
            default_windows_preview_to_open: gs::get_bool(
                d,
                "default-windows-preview-to-open",
                false,
            ),
            hide: autohide::HideConfig::load(),
            isolate_monitors: gs::get_bool(d, "isolate-monitors", false),
            isolate_workspaces: gs::get_bool(d, "isolate-workspaces", false),
            middle_click: ClickAction::parse(&gs::get_string(d, "middle-click-action", "launch")),
            shift_click: ClickAction::parse(&gs::get_string(d, "shift-click-action", "minimize")),
            shift_middle_click: ClickAction::parse(&gs::get_string(
                d,
                "shift-middle-click-action",
                "launch",
            )),
            scroll: match gs::get_string(d, "scroll-action", "do-nothing").as_str() {
                "cycle-windows" => ScrollAction::CycleWindows,
                "switch-workspace" => ScrollAction::SwitchWorkspace,
                _ => ScrollAction::Nothing,
            },
            hotkeys_overlay: gs::get_bool(d, "hot-keys", true)
                && gs::get_bool(d, "hotkeys-overlay", true),
            hotkeys_show_dock: gs::get_bool(d, "hotkeys-show-dock", true),
        }
    }
}

struct Dock {
    window: gtk::ApplicationWindow,
    list: gtk::Box,
    /// The list's layout (glides icons, floats the dragged one).
    layout: dock_layout::DockLayout,
    /// Holds `list`; slides it off the edge while auto-hiding.
    slide: slide::Slide,
    config: RefCell<Config>,
    /// Every window the tracker knows, shared by the docks on each monitor.
    windows: Rc<RefCell<BTreeMap<WindowId, WindowInfo>>>,
    /// The monitor this dock is on.
    monitor: gtk::gdk::Monitor,
    /// Which monitors each window is on (see placement::Homes).
    homes: Rc<placement::Homes>,
    commands: Option<tracker::Commands>,
    /// A menu or window list is open. Rebuilding would destroy the button it
    /// hangs off (and close it), so rebuilds wait until it closes.
    menu_open: Cell<bool>,
    /// An icon is being dragged (see drag.rs); rebuilds wait for the drop.
    dragging: Cell<bool>,
    rebuild_pending: Cell<bool>,
    /// The app icons in dock order, and Show Apps, for dragging.
    drag: RefCell<drag::Targets>,
    /// Unity LauncherEntry counts, progress and urgency, by dock key.
    entries: OnceCell<Rc<launcher_entry::Entries>>,
    /// The badge widgets of the icons now in the dock, by dock key, so an
    /// update (a download's progress ticks often) needs no rebuild.
    badges: RefCell<HashMap<String, Badge>>,
    hide: autohide::Hide,
    /// When each window last had focus (a counter), most recent highest.
    mru: Rc<RefCell<HashMap<WindowId, u64>>>,
    /// The app icons in order, for Super+1..9.
    apps: RefCell<Vec<(gio::DesktopAppInfo, gtk::Button, Vec<WindowInfo>)>>,
    last_scroll: Cell<i64>,
    /// The Super+1..9 number labels of the first nine icons.
    numbers: RefCell<Vec<gtk::Label>>,
    /// Super is down (umoja-mir's SuperKey), and the numbers are showing.
    super_held: Cell<bool>,
    numbers_shown: Cell<bool>,
    numbers_timer: RefCell<Option<glib::SourceId>>,
}

struct Badge {
    button: gtk::Button,
    count: gtk::Label,
    progress: gtk::ProgressBar,
}

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-dock");
    i18n::init();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| load_css());
    // Map nothing while the screen is blanked: Mir has no active output
    // then (see umoja_common::screen).
    app.connect_activate(|app| {
        let a = app.clone();
        umoja_common::screen::when_on(app, move || build(&a));
    });
    app.run()
}

fn build(app: &adw::Application) {
    // Learn the primary monitor before the first dock maps.
    umoja_common::monitors::start();
    placement::Docks::start(app);
}

/// The state every dock shares, whichever monitor it is on.
struct Shared {
    windows: Rc<RefCell<BTreeMap<WindowId, WindowInfo>>>,
    homes: Rc<placement::Homes>,
    commands: Option<tracker::Commands>,
    entries: Rc<launcher_entry::Entries>,
    mru: Rc<RefCell<HashMap<WindowId, u64>>>,
}

/// A dock window on `monitor`.
fn new_dock(app: &adw::Application, shared: &Shared, monitor: &gtk::gdk::Monitor) -> Rc<Dock> {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .css_classes(["umoja-dock"])
        .build();
    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_namespace("umoja-dock");
    window.set_monitor(monitor);
    umoja_common::blur::blur_behind(&window);
    window.set_keyboard_mode(KeyboardMode::None);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    list.add_css_class("dock-list");
    let layout = dock_layout::DockLayout::new(4);
    list.set_layout_manager(Some(layout.clone()));
    let slide = slide::Slide::new(&list);
    window.set_child(Some(&slide));

    let dock = Rc::new(Dock {
        window: window.clone(),
        list,
        layout,
        slide,
        config: RefCell::new(Config::load()),
        windows: shared.windows.clone(),
        monitor: monitor.clone(),
        homes: shared.homes.clone(),
        commands: shared.commands.clone(),
        menu_open: Cell::new(false),
        dragging: Cell::new(false),
        rebuild_pending: Cell::new(false),
        drag: RefCell::default(),
        entries: OnceCell::new(),
        badges: RefCell::default(),
        hide: autohide::Hide::new(),
        mru: shared.mru.clone(),
        apps: RefCell::default(),
        last_scroll: Cell::new(0),
        numbers: RefCell::default(),
        super_held: Cell::new(false),
        numbers_shown: Cell::new(false),
        numbers_timer: RefCell::default(),
    });
    let _ = dock.entries.set(shared.entries.clone());

    dock.install_autohide();
    keynav::attach(&dock);
    dock.apply_layout();
    dock.rebuild();
    dock.refresh_autohide();
    fade_in_on_map(&window);
    window.present();
    dock
}

/// Fade the dock up from transparent the first time it maps, in step with
/// umoja-panel's wallpaper and top bar after login.
fn fade_in_on_map(window: &gtk::ApplicationWindow) {
    window.set_opacity(0.0);
    let handler = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
    let h = handler.clone();
    handler.set(Some(window.connect_map(move |w| {
        if let Some(id) = h.take() {
            w.disconnect(id);
        }
        let target = adw::PropertyAnimationTarget::new(w, "opacity");
        let anim = adw::TimedAnimation::new(w, 0.0, 1.0, 700, target);
        anim.set_easing(adw::Easing::EaseInOutCubic);
        anim.play();
    })));
}

impl Dock {
    fn apply_layout(&self) {
        let cfg = *self.config.borrow();
        let (main, ends) = match cfg.position {
            DockPosition::Left => (Edge::Left, [Edge::Top, Edge::Bottom]),
            DockPosition::Right => (Edge::Right, [Edge::Top, Edge::Bottom]),
            DockPosition::Top => (Edge::Top, [Edge::Left, Edge::Right]),
            DockPosition::Bottom => (Edge::Bottom, [Edge::Left, Edge::Right]),
        };
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            self.window
                .set_anchor(edge, edge == main || (cfg.extend && ends.contains(&edge)));
        }
        self.layout.set_vertical(cfg.position.is_vertical());
        // Float the rounded dock slightly off the screen edge; in panel
        // mode it sits flush against the edge and both its ends.
        let gap = |edge: DockPosition| {
            if cfg.position == edge && !cfg.extend {
                EDGE_GAP
            } else {
                0
            }
        };
        self.list.set_margin_start(gap(DockPosition::Left));
        self.list.set_margin_end(gap(DockPosition::Right));
        self.list.set_margin_top(gap(DockPosition::Top));
        self.list.set_margin_bottom(gap(DockPosition::Bottom));
        if cfg.extend {
            self.window.add_css_class("panel-mode");
        } else {
            self.window.remove_css_class("panel-mode");
        }
        for (pos, class) in [
            (DockPosition::Left, "left"),
            (DockPosition::Right, "right"),
            (DockPosition::Top, "top"),
            (DockPosition::Bottom, "bottom"),
        ] {
            if pos == cfg.position {
                self.window.add_css_class(class);
            } else {
                self.window.remove_css_class(class);
            }
        }
        self.list.set_visible(self.hide.shown());
        self.update_reserved_space();
        self.place_slide();
    }

    /// How far the dock reaches in from its screen edge: the list's real
    /// size across the dock (icons, item and list padding, border) plus the
    /// gap it floats off the edge. Measured rather than computed from the
    /// icon size so CSS changes can't let maximized windows slide under it.
    fn thickness(&self) -> i32 {
        let cfg = *self.config.borrow();
        let fallback = cfg.icon_size + 16 + if cfg.extend { 0 } else { EDGE_GAP };
        if !self.list.is_visible() || self.list.first_child().is_none() {
            return fallback;
        }
        let across = if cfg.position.is_vertical() {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        };
        // measure() includes the widget margins, i.e. the EDGE_GAP.
        let (_, natural, _, _) = self.list.measure(across, -1);
        natural.max(fallback)
    }

    /// Size the dock and, when it is fixed, reserve exactly the space it
    /// covers so maximized and tiled windows start right after it, like
    /// Ubuntu Dock.
    fn update_reserved_space(&self) {
        let cfg = *self.config.borrow();
        let thickness = self.thickness();
        self.remember_extent(thickness);
        if self.hide.shown() {
            if cfg.position.is_vertical() {
                self.window.set_default_size(thickness, -1);
            } else {
                self.window.set_default_size(-1, thickness);
            }
        } else {
            self.collapse();
        }
        // An auto-hiding dock floats over windows and reserves nothing.
        let thickness = if cfg.fixed { thickness } else { 0 };
        if self.window.exclusive_zone() != thickness {
            tracing::debug!(thickness, "dock exclusive zone");
            self.window.set_exclusive_zone(thickness);
        }
    }

    fn rebuild(self: &Rc<Self>) {
        if self.menu_open.get() || self.dragging.get() {
            self.rebuild_pending.set(true);
            return;
        }
        self.rebuild_pending.set(false);
        self.badges.borrow_mut().clear();
        self.apps.borrow_mut().clear();
        self.numbers.borrow_mut().clear();
        *self.drag.borrow_mut() = drag::Targets::default();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let cfg = *self.config.borrow();
        // Ask Umoja sits next to Show Apps, on the side facing the apps
        // (just above it at the dock's end). It is a fixed launcher, not a
        // pin, so it has no menu and can't be unpinned.
        if cfg.show_apps_at_top {
            self.list.append(&self.show_apps_button(cfg.icon_size));
            self.list.append(&self.ask_umoja_button(cfg.icon_size));
        }

        // Favorites first (in order), then running apps that aren't pinned.
        let windows = self.windows.borrow();
        let mut entries: Vec<(gio::DesktopAppInfo, bool, Vec<WindowInfo>)> = launcher::favorites()
            .into_iter()
            .map(|a| (a, true, Vec::new()))
            .collect();
        let isolate = cfg.isolate_monitors;
        let isolate_ws = cfg.isolate_workspaces;
        for w in windows.values().filter(|w| {
            (!isolate || self.homes.shows_on(w.id, &self.monitor))
                && (!isolate_ws || self.homes.on_current_workspace(w.id))
        }) {
            let key = launcher::app_for_window(&w.app_id).map(|a| launcher::key_for(&a));
            match entries
                .iter_mut()
                .find(|(a, _, _)| Some(launcher::key_for(a)) == key)
            {
                Some(entry) => entry.2.push(w.clone()),
                None => match launcher::app_for_window(&w.app_id) {
                    Some(app) => entries.push((app, false, vec![w.clone()])),
                    None => tracing::debug!(app_id = w.app_id, "no desktop file for window"),
                },
            }
        }
        drop(windows);

        // Pins can be dragged to reorder them, and running apps dragged in
        // among them, unless favorite-apps is locked down.
        let draggable = umoja_common::apps::favorites_writable();
        for (app, pinned, wins) in entries {
            let widget = self.app_button(&app, pinned, wins, cfg.icon_size);
            self.list.append(&widget);
            if let (true, Some(id)) = (draggable, app.id()) {
                drag::attach(self, &widget);
                self.drag.borrow_mut().items.push(drag::Item {
                    widget,
                    id: id.to_string(),
                    pinned,
                    icon: app.icon(),
                });
            }
        }
        if !cfg.show_apps_at_top {
            let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
            spacer.set_vexpand(cfg.position.is_vertical());
            spacer.set_hexpand(!cfg.position.is_vertical());
            self.list.append(&spacer);
            self.list.append(&self.ask_umoja_button(cfg.icon_size));
            self.list.append(&self.show_apps_button(cfg.icon_size));
        }
        self.update_reserved_space();
    }

    fn app_button(
        self: &Rc<Self>,
        app: &gio::DesktopAppInfo,
        pinned: bool,
        wins: Vec<WindowInfo>,
        size: i32,
    ) -> gtk::Widget {
        let overlay = gtk::Overlay::new();
        let image = match app.icon() {
            Some(icon) => gtk::Image::from_gicon(&icon),
            None => gtk::Image::from_icon_name("application-x-executable"),
        };
        image.set_pixel_size(size);
        let button = gtk::Button::builder()
            .child(&image)
            .css_classes(["flat", "dock-item"])
            .build();
        button.set_tooltip_text(Some(&app.display_name()));
        if wins.iter().any(|w| w.activated) {
            button.add_css_class("focused");
        }
        // A window of the app asked for attention (umoja-mir's "is ready").
        if wins.iter().any(|w| !w.activated && self.homes.wants_attention(w.id)) {
            button.add_css_class("attention");
        }
        overlay.set_child(Some(&button));
        self.apps
            .borrow_mut()
            .push((app.clone(), button.clone(), wins.clone()));

        if !wins.is_empty() {
            // Pips sit on the screen-edge side of the icon, as in Ubuntu Dock.
            let (orientation, halign, valign) = match self.config.borrow().position {
                DockPosition::Left => (
                    gtk::Orientation::Vertical,
                    gtk::Align::Start,
                    gtk::Align::Center,
                ),
                DockPosition::Right => (
                    gtk::Orientation::Vertical,
                    gtk::Align::End,
                    gtk::Align::Center,
                ),
                DockPosition::Top => (
                    gtk::Orientation::Horizontal,
                    gtk::Align::Center,
                    gtk::Align::Start,
                ),
                DockPosition::Bottom => (
                    gtk::Orientation::Horizontal,
                    gtk::Align::Center,
                    gtk::Align::End,
                ),
            };
            let dots = gtk::Box::new(orientation, 2);
            dots.set_halign(halign);
            dots.set_valign(valign);
            dots.set_can_target(false);
            for _ in 0..wins.len().min(4) {
                let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                dot.add_css_class("running-dot");
                dots.append(&dot);
            }
            overlay.add_overlay(&dots);
        }
        self.add_badges(&launcher::key_for(app), &overlay, &button);
        self.add_number(&overlay);

        // The modifiers held when the button was pressed (clicked has none).
        let mods = Rc::new(Cell::new(gtk::gdk::ModifierType::empty()));
        let press = gtk::GestureClick::builder().button(1).build();
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let mods = mods.clone();
            press.connect_pressed(move |g, _, _, _| mods.set(g.current_event_state()));
        }
        button.add_controller(press);
        let app_c = app.clone();
        let wins_c = wins.clone();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |b| {
            let Some(d) = weak.upgrade() else { return };
            let m = mods.replace(gtk::gdk::ModifierType::empty());
            let shift = m.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let ctrl = m.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            // Ctrl+click opens a new window, as in GNOME's dash.
            let action = if ctrl {
                ClickAction::Launch
            } else if shift {
                d.config.borrow().shift_click
            } else {
                ClickAction::Focus
            };
            d.click(&app_c, &wins_c, b, action);
        });

        // Middle-click (and Shift+middle-click): a new window by default.
        let middle = gtk::GestureClick::builder().button(2).build();
        let weak = Rc::downgrade(self);
        let (app_m2, wins_m2, button_m2) = (app.clone(), wins.clone(), button.clone());
        middle.connect_released(move |g, _, _, _| {
            let Some(d) = weak.upgrade() else { return };
            let shift = g
                .current_event_state()
                .contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let action = if shift {
                d.config.borrow().shift_middle_click
            } else {
                d.config.borrow().middle_click
            };
            d.click(&app_m2, &wins_m2, &button_m2, action);
        });
        button.add_controller(middle);

        // Scrolling over a running app cycles its windows (scroll-action).
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        let wins_s = wins.clone();
        scroll.connect_scroll(move |_, _, dy| {
            let Some(d) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let action = d.config.borrow().scroll;
            if action == ScrollAction::Nothing || dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let now = glib::monotonic_time();
            if now - d.last_scroll.get() < 250_000 {
                return glib::Propagation::Stop;
            }
            d.last_scroll.set(now);
            match action {
                ScrollAction::CycleWindows => d.cycle(&wins_s, dy > 0.0),
                ScrollAction::SwitchWorkspace => step_workspace(if dy > 0.0 { 1 } else { -1 }),
                ScrollAction::Nothing => {}
            }
            glib::Propagation::Stop
        });
        button.add_controller(scroll);

        let menu_click = gtk::GestureClick::builder().button(3).build();
        let weak = Rc::downgrade(self);
        let app_m = app.clone();
        menu_click.connect_pressed(move |g, _, _, _| {
            let (Some(d), Some(widget)) = (weak.upgrade(), g.widget()) else {
                return;
            };
            d.show_popover(&d.context_menu(&app_m, pinned, &d.fresh(&wins)), &widget);
        });
        button.add_controller(menu_click);
        overlay.upcast()
    }

    /// What a click on an app's icon does.
    fn click(
        self: &Rc<Self>,
        app: &gio::DesktopAppInfo,
        wins: &[WindowInfo],
        anchor: &gtk::Button,
        action: ClickAction,
    ) {
        // Titles change without a rebuild: use the windows as they are now.
        let wins = self.fresh(wins);
        let Some(tx) = self.commands.clone() else {
            launcher::launch(app);
            return;
        };
        match action {
            ClickAction::Nothing => {}
            ClickAction::Launch => {
                if wins.is_empty() {
                    launcher::launch(app);
                } else {
                    launcher::new_window(app);
                }
            }
            ClickAction::Minimize => {
                for w in &wins {
                    tx.try_send(Command::Minimize(w.id));
                }
            }
            ClickAction::Quit => {
                for w in &wins {
                    tx.try_send(Command::Close(w.id));
                }
            }
            ClickAction::Previews if !wins.is_empty() => {
                self.show_popover(&previews::popover(&wins, tx), anchor);
            }
            ClickAction::Previews | ClickAction::Focus => match wins.as_slice() {
                [] => launcher::launch(app),
                // Ubuntu Dock "focus-or-previews": click a focused single
                // window to minimize it.
                [w] => tx.try_send(if w.activated {
                    Command::Minimize(w.id)
                } else {
                    Command::Activate(w.id)
                }),
                // Several: focus the app first (its most recent window);
                // only a click on the app that already has focus opens the
                // previews.
                many if !many.iter().any(|w| w.activated) => {
                    if let Some(w) = self.most_recent(many) {
                        tx.try_send(Command::Activate(w));
                    }
                }
                many => self.show_popover(&previews::popover(many, tx), anchor),
            },
        }
    }

    /// The window of `wins` that had focus last.
    fn most_recent(&self, wins: &[WindowInfo]) -> Option<WindowId> {
        let mru = self.mru.borrow();
        wins.iter()
            .max_by_key(|w| mru.get(&w.id).copied().unwrap_or(0))
            .map(|w| w.id)
    }

    /// Scroll: the next (or previous) of the app's windows, in the order
    /// they last had focus.
    fn cycle(&self, wins: &[WindowInfo], forward: bool) {
        let wins = self.fresh(wins);
        let Some(tx) = &self.commands else { return };
        if wins.is_empty() {
            return;
        }
        let mut order: Vec<&WindowInfo> = wins.iter().collect();
        {
            let mru = self.mru.borrow();
            order.sort_by_key(|w| std::cmp::Reverse(mru.get(&w.id).copied().unwrap_or(0)));
        }
        let at = order.iter().position(|w| w.activated);
        let next = match (at, forward) {
            (None, _) => 0,
            (Some(i), true) => (i + 1) % order.len(),
            (Some(i), false) => (i + order.len() - 1) % order.len(),
        };
        tx.try_send(Command::Activate(order[next].id));
    }

    /// Super+1..9 (switch-to-application-N): the app at `index` as a click
    /// would, or a new window of it (Super+Ctrl, gnome-shell and Ubuntu Dock).
    pub fn activate_app(self: &Rc<Self>, index: u32, new_window: bool) {
        let entry = self.apps.borrow().get(index as usize).cloned();
        let Some((app, button, wins)) = entry else {
            return;
        };
        if new_window {
            self.click(&app, &wins, &button, ClickAction::Launch);
            return;
        }
        // Super+N while Super is held: show the numbers now, not after the wait.
        if self.super_held.get() {
            if let Some(id) = self.numbers_timer.borrow_mut().take() {
                id.remove();
            }
            self.show_numbers(true);
        }
        // The keyboard never minimizes: a focused app stays focused.
        let wins_now = self.fresh(&wins);
        if wins_now.iter().any(|w| w.activated) && wins_now.len() == 1 {
            return;
        }
        self.click(&app, &wins, &button, ClickAction::Focus);
    }

    /// Super went down or up (umoja-mir). Ubuntu Dock's hotkeys-overlay:
    /// held a moment, each of the first nine icons shows its Super+N
    /// number until Super goes up. A tap (the overview) shows nothing.
    pub fn super_key(self: &Rc<Self>, held: bool) {
        self.super_held.set(held);
        if let Some(id) = self.numbers_timer.borrow_mut().take() {
            id.remove();
        }
        if !held {
            self.show_numbers(false);
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(SUPER_HOLD_MS), move || {
            if let Some(d) = weak.upgrade() {
                d.numbers_timer.borrow_mut().take();
                if d.super_held.get() {
                    d.show_numbers(true);
                }
            }
        });
        *self.numbers_timer.borrow_mut() = Some(id);
    }

    fn show_numbers(self: &Rc<Self>, show: bool) {
        let show = show && self.config.borrow().hotkeys_overlay;
        if show == self.numbers_shown.get() {
            return;
        }
        self.numbers_shown.set(show);
        for label in self.numbers.borrow().iter() {
            label.set_visible(show);
        }
        self.update_visibility();
    }

    /// Whether the numbers bring out a hidden dock (hotkeys-show-dock).
    pub fn numbers_reveal(&self) -> bool {
        self.numbers_shown.get() && self.config.borrow().hotkeys_show_dock
    }

    /// The Super+N number on the icon (the first nine icons).
    fn add_number(&self, overlay: &gtk::Overlay) {
        let n = self.apps.borrow().len();
        if n == 0 || n > MAX_APP_KEYS {
            return;
        }
        let label = gtk::Label::builder()
            .label(n.to_string())
            .css_classes(["dock-number"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .visible(self.numbers_shown.get())
            .build();
        overlay.add_overlay(&label);
        self.numbers.borrow_mut().push(label);
    }

    /// For tools/test/umoja-wm-test: the dock's edge, its surface size, and
    /// each app icon (desktop id, windows, focused, attention, rectangle in
    /// the surface).
    #[allow(clippy::type_complexity)]
    pub fn test_icons(&self) -> (String, (i32, i32), Vec<(String, u32, bool, bool, i32, i32, i32, i32)>) {
        let position = format!("{:?}", self.config.borrow().position).to_lowercase();
        let size = (self.window.width(), self.window.height());
        let icons = self
            .apps
            .borrow()
            .iter()
            .filter_map(|(app, button, wins)| {
                let b = button.compute_bounds(&self.window)?;
                let wins = self.fresh(wins);
                Some((
                    app.id().map(|s| s.to_string()).unwrap_or_default(),
                    wins.len() as u32,
                    button.has_css_class("focused"),
                    button.has_css_class("attention"),
                    b.x() as i32,
                    b.y() as i32,
                    b.width() as i32,
                    b.height() as i32,
                ))
            })
            .collect();
        (position, size, icons)
    }

    /// `wins` as the tracker knows them now (titles and focus may have
    /// changed since the button was built; closed windows drop out).
    fn fresh(&self, wins: &[WindowInfo]) -> Vec<WindowInfo> {
        let now = self.windows.borrow();
        wins.iter().filter_map(|w| now.get(&w.id).cloned()).collect()
    }

    /// Unread count (top corner) and progress bar (across the bottom of
    /// the icon) from the app's LauncherEntry, like Ubuntu Dock's.
    fn add_badges(&self, key: &str, overlay: &gtk::Overlay, button: &gtk::Button) {
        let count = gtk::Label::builder()
            .css_classes(["dock-badge"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .can_target(false)
            .visible(false)
            .build();
        let progress = gtk::ProgressBar::builder()
            .css_classes(["dock-progress"])
            .valign(gtk::Align::End)
            .margin_start(10)
            .margin_end(10)
            .margin_bottom(8)
            .can_target(false)
            .visible(false)
            .build();
        overlay.add_overlay(&progress);
        overlay.add_overlay(&count);
        self.badges.borrow_mut().insert(
            key.to_string(),
            Badge {
                button: button.clone(),
                count,
                progress,
            },
        );
        self.update_badge(key);
    }

    fn update_badge(&self, key: &str) {
        let badges = self.badges.borrow();
        let Some(badge) = badges.get(key) else { return };
        let entry = self
            .entries
            .get()
            .and_then(|e| e.get(key))
            .unwrap_or_default();
        match entry.badge() {
            Some(text) => {
                badge.count.set_label(&text);
                badge.count.set_visible(true);
            }
            None => badge.count.set_visible(false),
        }
        match entry.progress() {
            Some(f) => {
                badge.progress.set_fraction(f);
                badge.progress.set_visible(true);
            }
            None => badge.progress.set_visible(false),
        }
        if entry.urgent {
            badge.button.add_css_class("urgent");
        } else {
            badge.button.remove_css_class("urgent");
        }
    }

    /// Ubuntu Dock's app menu, in its order: the app name, its windows
    /// ("All Windows" submenu or an "Open Windows" list), New Window, the
    /// app's desktop actions, Pin to Dock / Unpin, App Details and Quit.
    fn context_menu(
        &self,
        app: &gio::DesktopAppInfo,
        pinned: bool,
        wins: &[WindowInfo],
    ) -> gtk::Popover {
        let cfg = *self.config.borrow();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        col.add_css_class("dock-menu");
        let popover = gtk::Popover::builder()
            .child(&col)
            .position(self.menu_side())
            .has_arrow(false)
            .build();
        let weak_pop = popover.downgrade();
        let button = |label: &str, action: Box<dyn Fn()>| {
            let b = gtk::Button::builder()
                .child(&gtk::Label::builder().label(label).xalign(0.0).build())
                .css_classes(["flat"])
                .build();
            let pop = weak_pop.clone();
            b.connect_clicked(move |_| {
                if let Some(p) = pop.upgrade() {
                    p.popdown();
                }
                action();
            });
            b
        };
        let item = |label: &str, action: Box<dyn Fn()>| col.append(&button(label, action));
        let separator = || col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let heading = |text: &str| {
            let l = gtk::Label::builder().label(text).xalign(0.0).build();
            l.add_css_class("dock-menu-heading");
            col.append(&l);
        };

        heading(&app.display_name());

        if let (false, Some(tx)) = (wins.is_empty(), &self.commands) {
            let window_button = |w: &WindowInfo| {
                let title = if w.title.is_empty() {
                    app.display_name().to_string()
                } else {
                    w.title.clone()
                };
                let (tx, id) = (tx.clone(), w.id);
                button(
                    &title,
                    Box::new(move || {
                        let _ = tx.try_send(Command::Activate(id));
                    }),
                )
            };
            if cfg.show_windows_preview {
                // Ubuntu Dock's previews: a live thumbnail over each title.
                let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
                list.add_css_class("dock-window-list");
                let mut cards = Vec::new();
                for w in wins {
                    let title = if w.title.is_empty() {
                        app.display_name().to_string()
                    } else {
                        w.title.clone()
                    };
                    let (card, holder) = previews::card(&title);
                    let b = gtk::Button::builder()
                        .child(&card)
                        .css_classes(["flat", "dock-window"])
                        .build();
                    let (tx, id, pop) = (tx.clone(), w.id, weak_pop.clone());
                    b.connect_clicked(move |_| {
                        if let Some(p) = pop.upgrade() {
                            p.popdown();
                        }
                        tx.try_send(Command::Activate(id));
                    });
                    list.append(&b);
                    cards.push((w.clone(), holder));
                }
                let cards = RefCell::new(cards);
                popover.connect_map(move |p| previews::load(p, cards.take()));
                let expander = gtk::Expander::builder()
                    .label(i18n::dock("All Windows"))
                    .child(&previews::scroller(&list, wins.len()))
                    .expanded(cfg.default_windows_preview_to_open)
                    .build();
                expander.add_css_class("dock-menu-submenu");
                col.append(&expander);
            } else {
                heading(&i18n::shell("Open Windows"));
                for w in wins {
                    col.append(&window_button(w));
                }
            }
        }

        separator();
        let actions = app.list_actions();
        if umoja_common::apps::can_open_new_window(app)
            && !actions.iter().any(|a| a == "new-window")
        {
            let a = app.clone();
            item(
                &i18n::shell("New Window"),
                Box::new(move || launcher::launch(&a)),
            );
            separator();
        }
        for action in actions {
            let label = app.action_name(&action).to_string();
            let a = app.clone();
            item(
                &label,
                Box::new(move || launcher::launch_action(&a, &action)),
            );
        }

        let id = app.id().map(|s| s.to_string()).unwrap_or_default();
        if umoja_common::apps::favorites_writable() {
            separator();
            let label = if pinned {
                i18n::shell("Unpin")
            } else {
                i18n::dock("Pin to Dock")
            };
            item(
                &label,
                Box::new(move || umoja_common::apps::set_favorite(&id, !pinned)),
            );
        }
        if let Some(details) = umoja_common::apps::details_for(app) {
            separator();
            item(
                &i18n::shell("App Details"),
                Box::new(move || umoja_common::apps::show_details(&details)),
            );
        }

        if let (false, Some(tx)) = (wins.is_empty(), &self.commands) {
            separator();
            let tx = tx.clone();
            let ids: Vec<_> = wins.iter().map(|w| w.id).collect();
            let label = if ids.len() == 1 {
                i18n::shell("Quit")
            } else {
                i18n::dock_n("Quit %d Window", "Quit %d Windows", ids.len() as u32)
                    .replace("%d", &ids.len().to_string())
            };
            item(
                &label,
                Box::new(move || {
                    for id in &ids {
                        let _ = tx.try_send(Command::Close(*id));
                    }
                }),
            );
        }
        popover
    }

    /// Menus open away from the screen edge the dock sits on.
    fn menu_side(&self) -> gtk::PositionType {
        match self.config.borrow().position {
            DockPosition::Left => gtk::PositionType::Right,
            DockPosition::Right => gtk::PositionType::Left,
            DockPosition::Top => gtk::PositionType::Bottom,
            DockPosition::Bottom => gtk::PositionType::Top,
        }
    }

    /// Attach a transient popover, hold rebuilds while it is open, and drop
    /// it again once it closes.
    fn show_popover(self: &Rc<Self>, popover: &gtk::Popover, parent: &impl IsA<gtk::Widget>) {
        let side = self.menu_side();
        popover.set_position(side);
        popover.set_parent(parent);
        // Open MENU_GAP clear of the dock card itself, not of the icon, so
        // the right-click menu sits like the panel's menus do under the bar.
        if let Some(at) = parent
            .as_ref()
            .compute_point(&self.list, &gtk::graphene::Point::new(0.0, 0.0))
        {
            let (w, h) = (parent.as_ref().width(), parent.as_ref().height());
            let rect = match side {
                gtk::PositionType::Left | gtk::PositionType::Right => {
                    gtk::gdk::Rectangle::new(-(at.x() as i32), 0, self.list.width(), h)
                }
                _ => gtk::gdk::Rectangle::new(0, -(at.y() as i32), w, self.list.height()),
            };
            popover.set_pointing_to(Some(&rect));
            let (dx, dy) = match side {
                gtk::PositionType::Right => (MENU_GAP, 0),
                gtk::PositionType::Left => (-MENU_GAP, 0),
                gtk::PositionType::Bottom => (0, MENU_GAP),
                _ => (0, -MENU_GAP),
            };
            popover.set_offset(dx, dy);
        }
        umoja_common::blur::blur_behind(popover);
        self.menu_open.set(true);
        let weak = Rc::downgrade(self);
        popover.connect_closed(move |p| {
            let p = p.clone();
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                p.unparent();
                if let Some(d) = weak.upgrade() {
                    d.menu_open.set(false);
                    if d.rebuild_pending.get() {
                        d.rebuild();
                    }
                    d.update_visibility();
                }
            });
        });
        popover.popup();
    }

    fn show_apps_button(&self, size: i32) -> gtk::Widget {
        let image = gtk::Image::from_icon_name("view-app-grid-symbolic");
        image.set_pixel_size(size * 2 / 3);
        let b = gtk::Button::builder()
            .child(&image)
            .css_classes(["flat", "dock-item", "show-apps"])
            .build();
        b.set_tooltip_text(Some(&i18n::shell("Show Apps")));
        b.connect_clicked(|_| call_overlay("Toggle"));
        // Dropping a pinned app here unpins it, as in GNOME's dash.
        self.drag.borrow_mut().show_apps = Some((b.clone().upcast(), image));
        b.upcast()
    }

    fn ask_umoja_button(&self, size: i32) -> gtk::Widget {
        // Shipped with Umoja (data/icons), so no fallback names: GTK tries
        // every name in a theme before moving on to the next theme, so a
        // fallback that Yaru has would always beat our hicolor icon.
        let image = gtk::Image::from_icon_name("umoja-ask-symbolic");
        // Full app-icon size, like the pinned apps around it (Show Apps
        // stays smaller, as in Ubuntu Dock).
        image.set_pixel_size(size);
        let b = gtk::Button::builder()
            .child(&image)
            .css_classes(["flat", "dock-item", "show-apps", "ask-umoja"])
            .build();
        b.set_tooltip_text(Some(&i18n::umoja("Ask Umoja")));
        b.connect_clicked(|_| call_overlay("ToggleAsk"));
        b.upcast()
    }
}

/// The next or previous workspace (scroll-action switch-workspace).
fn step_workspace(delta: i32) {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let call = |method: &'static str, args: Option<glib::Variant>| {
            conn.call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                method,
                args.as_ref(),
                None,
                gio::DBusCallFlags::NONE,
                1000,
            )
        };
        let Some((current, count)) = call("GetWorkspaces", None)
            .await
            .ok()
            .and_then(|v| v.get::<(u32, u32)>())
        else {
            return;
        };
        let to = (current as i32 + delta).clamp(0, count.max(1) as i32 - 1) as u32;
        if to != current {
            let _ = call("SwitchWorkspace", Some((to,).to_variant())).await;
        }
    });
}

/// Call a no-argument method on the overlay ("Toggle" for the app grid,
/// "ToggleAsk" for Ask Umoja).
fn call_overlay(method: &'static str) {
    call_overlay_with(method, None);
}

/// Call an overlay method with arguments (drag.rs: AppDrag).
pub fn call_overlay_with(method: &'static str, args: Option<glib::Variant>) {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            method,
            args.as_ref(),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |_| {},
        );
    });
}

fn load_css() {
    // The shell chrome is always dark, like the notification bubbles,
    // whatever Settings > Appearance says; apps keep following it.
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    umoja_common::icons::add_shell_icons();
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("dock.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
