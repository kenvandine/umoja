//! umoja-dock: an Ubuntu Dock work-alike.
//!
//! Reads the same `org.gnome.shell.extensions.dash-to-dock` keys as Ubuntu
//! Dock, pins apps from `org.gnome.shell favorite-apps`, and shows running
//! windows (with dots) from the compositor's foreign-toplevel protocol.

mod launcher;
mod previews;
mod tracker;

use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use libadwaita::prelude::AnimationExt;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use tracker::{Command, Event, WindowId, WindowInfo};
use umoja_common::gsettings as gs;
use umoja_common::types::DockPosition;
use umoja_common::{dbus, i18n};

const APP_ID: &str = "org.umoja.Dock";
const HIDDEN_SIZE: i32 = 2;
/// Gap between the dock and the screen edges it sits against.
const EDGE_GAP: i32 = 8;

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
}

impl Config {
    fn load() -> Self {
        let d = gs::DASH_TO_DOCK;
        Self {
            position: DockPosition::from_gsettings(&gs::get_string(d, "dock-position", "LEFT")),
            icon_size: gs::get_int(d, "dash-max-icon-size", 48).clamp(16, 128),
            // Ubuntu ships dock-fixed=true; autohide only matters when unfixed.
            fixed: gs::get_bool(d, "dock-fixed", true),
            extend: gs::get_bool(d, "extend-height", true),
            show_apps_at_top: gs::get_bool(d, "show-apps-at-top", false),
            show_windows_preview: gs::get_bool(d, "show-windows-preview", true),
            default_windows_preview_to_open: gs::get_bool(
                d,
                "default-windows-preview-to-open",
                false,
            ),
        }
    }
}

struct Dock {
    window: gtk::ApplicationWindow,
    list: gtk::Box,
    config: RefCell<Config>,
    windows: RefCell<BTreeMap<WindowId, WindowInfo>>,
    commands: Option<async_channel::Sender<Command>>,
    /// A menu or window list is open. Rebuilding would destroy the button it
    /// hangs off (and close it), so rebuilds wait until it closes.
    menu_open: Cell<bool>,
    rebuild_pending: Cell<bool>,
    _settings: Vec<gio::Settings>,
}

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-dock");
    i18n::init();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build);
    app.run()
}

fn build(app: &adw::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .css_classes(["umoja-dock"])
        .build();
    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_namespace("umoja-dock");
    window.set_keyboard_mode(KeyboardMode::None);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    list.add_css_class("dock-list");
    window.set_child(Some(&list));

    let tracker = tracker::start();
    let mut settings = Vec::new();
    for schema in [gs::DASH_TO_DOCK, gs::SHELL] {
        if let Some(s) = gs::settings(schema) {
            settings.push(s);
        }
    }

    let dock = Rc::new(Dock {
        window: window.clone(),
        list,
        config: RefCell::new(Config::load()),
        windows: RefCell::default(),
        commands: tracker.as_ref().map(|t| t.commands.clone()),
        menu_open: Cell::new(false),
        rebuild_pending: Cell::new(false),
        _settings: settings.clone(),
    });

    for s in &settings {
        let weak = Rc::downgrade(&dock);
        s.connect_changed(None, move |_, _| {
            if let Some(d) = weak.upgrade() {
                *d.config.borrow_mut() = Config::load();
                d.apply_layout();
                d.rebuild();
            }
        });
    }

    if let Some(t) = tracker {
        let weak = Rc::downgrade(&dock);
        glib::spawn_future_local(async move {
            while let Ok(ev) = t.events.recv().await {
                let Some(d) = weak.upgrade() else { break };
                match ev {
                    Event::Changed(info) => {
                        d.windows.borrow_mut().insert(info.id, info);
                    }
                    Event::Closed(id) => {
                        d.windows.borrow_mut().remove(&id);
                    }
                }
                d.rebuild();
            }
        });
    }

    // Every handler above holds only a weak reference, so the window owns
    // the dock state. Without this the Dock was freed as soon as build()
    // returned, the tracker's channel closed, and running apps never showed.
    let keep = RefCell::new(Some(dock.clone()));
    window.connect_destroy(move |_| {
        keep.borrow_mut().take();
    });

    dock.install_autohide();
    dock.apply_layout();
    dock.rebuild();
    fade_in_on_map(&window);
    window.present();
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
        self.list.set_orientation(if cfg.position.is_vertical() {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        });
        // Float the rounded dock slightly off the screen edge, and off both
        // ends too when it spans the edge (panel mode).
        let gap = |on: bool| if on { EDGE_GAP } else { 0 };
        let (vertical, along) = (cfg.position.is_vertical(), cfg.extend);
        self.list.set_margin_start(gap(
            cfg.position == DockPosition::Left || (!vertical && along)
        ));
        self.list.set_margin_end(gap(
            cfg.position == DockPosition::Right || (!vertical && along)
        ));
        self.list
            .set_margin_top(gap(cfg.position == DockPosition::Top || (vertical && along)));
        self.list.set_margin_bottom(gap(
            cfg.position == DockPosition::Bottom || (vertical && along)
        ));
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
        let thickness = cfg.icon_size + 16 + EDGE_GAP;
        if cfg.position.is_vertical() {
            self.window.set_default_size(thickness, -1);
        } else {
            self.window.set_default_size(-1, thickness);
        }
        if cfg.fixed {
            self.window.set_exclusive_zone(thickness);
        } else {
            self.window.set_exclusive_zone(0);
        }
        self.list.set_visible(true);
    }

    /// Unfixed dock: collapse to a thin strip until the pointer touches it.
    fn install_autohide(self: &Rc<Self>) {
        let motion = gtk::EventControllerMotion::new();
        let enter = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(d) = enter.upgrade() {
                d.list.set_visible(true);
            }
        });
        let leave = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(d) = leave.upgrade()
                && !d.config.borrow().fixed
            {
                d.list.set_visible(false);
                d.window.set_default_size(HIDDEN_SIZE, HIDDEN_SIZE);
            }
        });
        self.window.add_controller(motion);
    }

    fn rebuild(self: &Rc<Self>) {
        if self.menu_open.get() {
            self.rebuild_pending.set(true);
            return;
        }
        self.rebuild_pending.set(false);
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let cfg = *self.config.borrow();
        if cfg.show_apps_at_top {
            self.list.append(&self.show_apps_button(cfg.icon_size));
        }

        // Favorites first (in order), then running apps that aren't pinned.
        let windows = self.windows.borrow();
        let mut entries: Vec<(gio::DesktopAppInfo, bool, Vec<WindowInfo>)> = launcher::favorites()
            .into_iter()
            .map(|a| (a, true, Vec::new()))
            .collect();
        for w in windows.values() {
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

        for (app, pinned, wins) in entries {
            self.list
                .append(&self.app_button(&app, pinned, wins, cfg.icon_size));
        }
        if !cfg.show_apps_at_top {
            let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
            spacer.set_vexpand(cfg.position.is_vertical());
            spacer.set_hexpand(!cfg.position.is_vertical());
            self.list.append(&spacer);
            self.list.append(&self.show_apps_button(cfg.icon_size));
        }
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
        overlay.set_child(Some(&button));

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

        let commands = self.commands.clone();
        let app_c = app.clone();
        let wins_c = wins.clone();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |b| match (wins_c.as_slice(), &commands) {
            ([], _) | (_, None) => launcher::launch(&app_c),
            // Ubuntu Dock "focus-or-previews": click a focused single window to minimize it.
            ([w], Some(tx)) => {
                let _ = tx.try_send(if w.activated {
                    Command::Minimize(w.id)
                } else {
                    Command::Activate(w.id)
                });
            }
            (many, Some(tx)) => {
                if let Some(d) = weak.upgrade() {
                    d.show_popover(&previews::popover(many, tx.clone()), b);
                }
            }
        });

        let menu_click = gtk::GestureClick::builder().button(3).build();
        let weak = Rc::downgrade(self);
        let app_m = app.clone();
        menu_click.connect_pressed(move |g, _, _, _| {
            let (Some(d), Some(widget)) = (weak.upgrade(), g.widget()) else {
                return;
            };
            d.show_popover(&d.context_menu(&app_m, pinned, &wins), &widget);
        });
        button.add_controller(menu_click);
        overlay.upcast()
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
                let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
                for w in wins {
                    list.append(&window_button(w));
                }
                let expander = gtk::Expander::builder()
                    .label(i18n::dock("All Windows"))
                    .child(&list)
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
        if gs::settings(gs::SHELL).is_some_and(|s| s.is_writable("favorite-apps")) {
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
        popover.set_position(self.menu_side());
        popover.set_parent(parent);
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
        b.connect_clicked(|_| toggle_overlay());
        b.upcast()
    }
}

fn toggle_overlay() {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            "Toggle",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |_| {},
        );
    });
}

fn load_css() {
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
