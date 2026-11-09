//! The window menu (Alt+Space), gnome-shell's `windowMenu.js`.
//!
//! umoja-mir asks for it with `ShowWindowMenu` and the window's state; the
//! entries call back into `org.umoja.Compositor` with the window's id. The
//! menu is a `gtk::PopoverMenu` hanging off a 1×1 layer surface placed at the
//! window's top-left corner, so it gets GTK's keyboard navigation and closes
//! on a click outside, like any other popup menu.

use gio::prelude::*;
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use umoja_common::{dbus, i18n};

// Keep in sync with umoja-mir's tiling::Action and umoja_shim.h.
const ACTION_MINIMIZE: u32 = 5;
const ACTION_CLOSE: u32 = 11;
const ACTION_TOGGLE_MAXIMIZED: u32 = 13;
const ACTION_MOVE_TO_MONITOR_LEFT: u32 = 21;
const ACTION_MOVE_TO_MONITOR_RIGHT: u32 = 22;
const ACTION_MOVE_TO_MONITOR_UP: u32 = 23;
const ACTION_MOVE_TO_MONITOR_DOWN: u32 = 24;
const ACTION_SCREENSHOT_WINDOW: u32 = 27;
const ACTION_BEGIN_MOVE: u32 = 47;
const ACTION_BEGIN_RESIZE: u32 = 48;
const ACTION_TOGGLE_ABOVE: u32 = 53;
const ACTION_TOGGLE_STICKY: u32 = 54;

// UMOJA_WINDOW_* flags.
const MAXIMIZED: u32 = 1 << 3;
const FULLSCREEN: u32 = 1 << 4;
const TILED: u32 = 1 << 5;
const ABOVE: u32 = 1 << 6;
const STICKY: u32 = 1 << 7;
const RESIZABLE: u32 = 1 << 8;

// Neighbouring monitors.
const MON_LEFT: u32 = 1;
const MON_RIGHT: u32 = 2;
const MON_UP: u32 = 4;
const MON_DOWN: u32 = 8;

/// What the compositor tells us about the window.
#[derive(Debug, Clone, Copy, Default)]
pub struct Target {
    pub id: u64,
    pub x: i32,
    pub y: i32,
    pub flags: u32,
    pub workspace: u32,
    pub n_workspaces: u32,
    pub neighbours: u32,
}

pub struct WindowMenu {
    window: gtk::ApplicationWindow,
    anchor: gtk::Box,
    popover: gtk::PopoverMenu,
    target: Cell<Target>,
    actions: gio::SimpleActionGroup,
}

fn call(method: &'static str, args: glib::Variant) {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        if let Err(e) = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                method,
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                1000,
            )
            .await
        {
            tracing::warn!(method, error = %e, "window menu call failed");
        }
    });
}

fn monitor_at(x: i32, y: i32) -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    let mut first = None;
    for i in 0..monitors.n_items() {
        let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
            continue;
        };
        let g = m.geometry();
        if x >= g.x() && x < g.x() + g.width() && y >= g.y() && y < g.y() + g.height() {
            return Some(m);
        }
        first.get_or_insert(m);
    }
    first
}

/// The menu model for a window, in gnome-shell's order and wording.
fn model(t: &Target) -> gio::Menu {
    let menu = gio::Menu::new();
    let item = |section: &gio::Menu, label: &str, action: &str| {
        section.append(Some(&i18n::shell(label)), Some(action));
    };

    let shot = gio::Menu::new();
    item(&shot, "Take Screenshot", "wm.screenshot");
    menu.append_section(None, &shot);

    let state = gio::Menu::new();
    item(&state, "Hide", "wm.minimize");
    if t.flags & (MAXIMIZED | TILED) != 0 {
        item(&state, "Restore", "wm.maximize");
    } else {
        item(&state, "Maximize", "wm.maximize");
    }
    if t.flags & FULLSCREEN == 0 {
        item(&state, "Move", "wm.move");
        if t.flags & RESIZABLE != 0 {
            item(&state, "Resize", "wm.resize");
        }
    }
    menu.append_section(None, &state);

    let place = gio::Menu::new();
    item(&place, "Always on Top", "wm.above");
    if t.n_workspaces > 0 {
        item(&place, "Always on Visible Workspace", "wm.sticky");
    }
    if t.flags & STICKY == 0 {
        if t.workspace > 0 {
            item(&place, "Move to Workspace Left", "wm.workspace-left");
        }
        if t.workspace + 1 < t.n_workspaces {
            item(&place, "Move to Workspace Right", "wm.workspace-right");
        }
    }
    for (bit, label, action) in [
        (MON_UP, "Move to Monitor Up", "wm.monitor-up"),
        (MON_DOWN, "Move to Monitor Down", "wm.monitor-down"),
        (MON_LEFT, "Move to Monitor Left", "wm.monitor-left"),
        (MON_RIGHT, "Move to Monitor Right", "wm.monitor-right"),
    ] {
        if t.neighbours & bit != 0 {
            item(&place, label, action);
        }
    }
    menu.append_section(None, &place);

    let close = gio::Menu::new();
    item(&close, "Close", "wm.close");
    menu.append_section(None, &close);
    menu
}

impl WindowMenu {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-chrome"])
            .default_width(1)
            .default_height(1)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-window-menu");
        window.set_exclusive_zone(-1);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        let anchor = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        anchor.set_size_request(1, 1);
        window.set_child(Some(&anchor));

        let popover = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        popover.set_has_arrow(false);
        popover.set_position(gtk::PositionType::Bottom);
        popover.set_halign(gtk::Align::Start);
        popover.set_parent(&anchor);

        let menu = Rc::new(Self {
            window,
            anchor,
            popover,
            target: Cell::new(Target::default()),
            actions: gio::SimpleActionGroup::new(),
        });
        menu.install_actions();
        {
            let weak = Rc::downgrade(&menu);
            menu.popover.connect_closed(move |_| {
                if let Some(m) = weak.upgrade() {
                    m.window.set_visible(false);
                }
            });
        }
        menu
    }

    fn install_actions(self: &Rc<Self>) {
        let group = &self.actions;
        let simple = |name: &str, f: Box<dyn Fn(Target)>| {
            let a = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(m) = weak.upgrade() {
                    f(m.target.get());
                }
            });
            group.add_action(&a);
        };
        let tile = |action: u32| -> Box<dyn Fn(Target)> {
            Box::new(move |t: Target| call("TileWindow", (t.id, action).to_variant()))
        };
        // The menu must be gone from the picture first.
        simple(
            "screenshot",
            Box::new(|t: Target| {
                glib::timeout_add_local_once(Duration::from_millis(200), move || {
                    call("TileWindow", (t.id, ACTION_SCREENSHOT_WINDOW).to_variant())
                });
            }),
        );
        simple("minimize", tile(ACTION_MINIMIZE));
        simple("maximize", tile(ACTION_TOGGLE_MAXIMIZED));
        simple("move", tile(ACTION_BEGIN_MOVE));
        simple("resize", tile(ACTION_BEGIN_RESIZE));
        simple("close", tile(ACTION_CLOSE));
        simple("monitor-left", tile(ACTION_MOVE_TO_MONITOR_LEFT));
        simple("monitor-right", tile(ACTION_MOVE_TO_MONITOR_RIGHT));
        simple("monitor-up", tile(ACTION_MOVE_TO_MONITOR_UP));
        simple("monitor-down", tile(ACTION_MOVE_TO_MONITOR_DOWN));
        simple(
            "workspace-left",
            Box::new(|t: Target| {
                call(
                    "SendWindowToWorkspace",
                    (t.id, t.workspace.saturating_sub(1)).to_variant(),
                )
            }),
        );
        simple(
            "workspace-right",
            Box::new(|t: Target| {
                call(
                    "SendWindowToWorkspace",
                    (t.id, t.workspace + 1).to_variant(),
                )
            }),
        );
        // Check items: the state comes from the compositor each time.
        for (name, action) in [
            ("above", ACTION_TOGGLE_ABOVE),
            ("sticky", ACTION_TOGGLE_STICKY),
        ] {
            let a = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
            let weak = Rc::downgrade(self);
            a.connect_activate(move |a, _| {
                let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                a.set_state(&on.to_variant());
                if let Some(m) = weak.upgrade() {
                    call("TileWindow", (m.target.get().id, action).to_variant());
                }
            });
            group.add_action(&a);
        }
        self.window.insert_action_group("wm", Some(group));
    }

    fn set_check(&self, name: &str, on: bool) {
        if let Some(a) = self
            .actions
            .lookup_action(name)
            .and_downcast::<gio::SimpleAction>()
        {
            a.set_state(&on.to_variant());
        }
    }

    pub fn show(self: &Rc<Self>, t: Target) {
        self.target.set(t);
        self.set_check("above", t.flags & ABOVE != 0);
        self.set_check("sticky", t.flags & STICKY != 0);
        self.popover.set_menu_model(Some(&model(&t)));
        if let Some(m) = monitor_at(t.x, t.y) {
            let g = m.geometry();
            self.window.set_monitor(&m);
            self.window.set_margin(Edge::Left, (t.x - g.x()).max(0));
            self.window.set_margin(Edge::Top, (t.y - g.y()).max(0));
        }
        self.window.present();
        // Pop up once the surface is mapped, or the popup has no parent.
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(m) = weak.upgrade() {
                let _ = &m.anchor;
                m.popover.popup();
            }
        });
    }

    pub fn hide(&self) {
        self.popover.popdown();
        self.window.set_visible(false);
    }
}
