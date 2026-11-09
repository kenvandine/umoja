//! org.umoja.Dock: Ctrl+Alt+Tab keyboard focus for the dock (Ubuntu Dock
//! registers itself as "Dash" with gnome-shell's Ctrl+Alt+Tab list).
//!
//! ```text
//! Focus()     take the keyboard, focus the first icon; Escape leaves
//! Unfocus()   hand the keyboard back (a click elsewhere, the top bar)
//! ```

use std::cell::Cell;
use std::rc::Rc;

use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use umoja_common::dbus;

use crate::Dock;

const XML: &str = r#"<node>
  <interface name="org.umoja.Dock">
    <method name="Focus"/>
    <method name="Unfocus"/>
  </interface>
</node>"#;

thread_local! {
    static FOCUSED: Cell<bool> = const { Cell::new(false) };
}

fn focus(dock: &Rc<Dock>) {
    FOCUSED.with(|f| f.set(true));
    // An autohidden dock shows itself while it has the keyboard.
    dock.update_visibility();
    dock.window.set_keyboard_mode(KeyboardMode::Exclusive);
    dock.window.set_focus_visible(true);
    GtkWindowExt::set_focus(&dock.window, None::<&gtk::Widget>);
    dock.window.child_focus(gtk::DirectionType::TabForward);
}

fn unfocus(dock: &Rc<Dock>) {
    if !FOCUSED.with(|f| f.replace(false)) {
        return;
    }
    GtkWindowExt::set_focus(&dock.window, None::<&gtk::Widget>);
    if !dock.menu_open.get() {
        dock.window.set_keyboard_mode(KeyboardMode::None);
    }
    dock.update_visibility();
}

/// Whether Ctrl+Alt+Tab gave the dock the keyboard (menus closing keep it).
pub fn focused() -> bool {
    FOCUSED.with(Cell::get)
}

pub fn start(dock: &Rc<Dock>) {
    let keys = gtk::EventControllerKey::new();
    let weak = Rc::downgrade(dock);
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape
            && focused()
            && let Some(d) = weak.upgrade()
            && !d.menu_open.get()
        {
            unfocus(&d);
            return glib::Propagation::Stop;
        }
        // Enter/Space starts or raises the app: the keyboard goes with it.
        if matches!(
            key,
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::space
        ) && focused()
        {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(d) = weak.upgrade()
                    && !d.menu_open.get()
                {
                    unfocus(&d);
                }
            });
        }
        glib::Propagation::Proceed
    });
    dock.window.add_controller(keys);

    let weak = Rc::downgrade(dock);
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::DOCK_INTERFACE)
            .expect("interface");
        let weak = weak.clone();
        let res = conn
            .register_object(dbus::DOCK_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, _, invocation| {
                let Some(d) = weak.upgrade() else {
                    invocation.return_value(None);
                    return;
                };
                match method {
                    "Focus" => focus(&d),
                    "Unfocus" => unfocus(&d),
                    _ => {
                        invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                        return;
                    }
                }
                invocation.return_value(None);
            })
            .build();
        if let Err(e) = res {
            tracing::error!(error = %e, "could not export org.umoja.Dock");
        }
    };
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        dbus::DOCK_BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        on_bus,
        |_, _| {},
        |_, _| tracing::warn!("could not own {}", dbus::DOCK_BUS_NAME),
    );
}
