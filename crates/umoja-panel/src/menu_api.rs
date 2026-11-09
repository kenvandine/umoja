//! org.umoja.Panel: lets the compositor drive the top bar.
//!
//! ```text
//! OpenMenu(s menu)     open "calendar" or "quick-settings" (GNOME's
//!                      top-edge touch swipe)
//! ToggleMenu(s menu)   open or close it (Super+V: the notification list)
//! Focus()              give the top bar the keyboard (Ctrl+Alt+Tab):
//!                      Tab/arrows move between its buttons, Escape leaves
//! Unfocus()            hand the keyboard back (a click elsewhere)
//! LogOut()             Log Out as the Quick Settings button does
//!                      (the assistant's session tool)
//! ```

use std::cell::Cell;

use gtk::{gio, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use umoja_common::dbus;

const XML: &str = r#"<node>
  <interface name="org.umoja.Panel">
    <method name="OpenMenu"><arg type="s" name="menu" direction="in"/></method>
    <method name="ToggleMenu"><arg type="s" name="menu" direction="in"/></method>
    <method name="Focus"/>
    <method name="Unfocus"/>
    <method name="LogOut"/>
  </interface>
</node>"#;

thread_local! {
    /// The top bar has the keyboard from Ctrl+Alt+Tab. Menus closing keep
    /// it then, so you can go on to the next button.
    static KEYBOARD_NAV: Cell<bool> = const { Cell::new(false) };
}

pub fn keyboard_nav() -> bool {
    KEYBOARD_NAV.with(Cell::get)
}

/// Ctrl+Alt+Tab: focus the first button of the top bar.
pub fn focus(window: &gtk::Window) {
    KEYBOARD_NAV.with(|k| k.set(true));
    if window.is_layer_window() {
        window.set_keyboard_mode(KeyboardMode::Exclusive);
    }
    window.set_focus_visible(true);
    GtkWindowExt::set_focus(window, None::<&gtk::Widget>);
    window.child_focus(gtk::DirectionType::TabForward);
}

/// Leave keyboard navigation (Escape, a click elsewhere, Activities).
pub fn unfocus(window: &gtk::Window) {
    if !KEYBOARD_NAV.with(|k| k.replace(false)) {
        return;
    }
    GtkWindowExt::set_focus(window, None::<&gtk::Widget>);
    // A menu that is open keeps the keyboard until it closes.
    let menu_open = popover_open(window.upcast_ref());
    if window.is_layer_window() && !menu_open {
        window.set_keyboard_mode(KeyboardMode::None);
    }
}

fn popover_open(w: &gtk::Widget) -> bool {
    let mut child = w.first_child();
    while let Some(c) = child {
        if let Some(b) = c.downcast_ref::<gtk::MenuButton>()
            && b.popover().is_some_and(|p| p.is_visible())
        {
            return true;
        }
        if popover_open(&c) {
            return true;
        }
        child = c.next_sibling();
    }
    false
}

/// Escape leaves keyboard navigation, as in gnome-shell.
pub fn install_escape(window: &gtk::Window) {
    let keys = gtk::EventControllerKey::new();
    let weak = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape
            && keyboard_nav()
            && let Some(w) = weak.upgrade()
            && !popover_open(w.upcast_ref())
        {
            unfocus(&w);
            return gtk::glib::Propagation::Stop;
        }
        gtk::glib::Propagation::Proceed
    });
    window.add_controller(keys);
}

pub fn own_name(
    window: &gtk::Window,
    calendar: &gtk::MenuButton,
    quick_settings: &gtk::MenuButton,
) {
    install_escape(window);
    let window = window.downgrade();
    let calendar = calendar.downgrade();
    let quick_settings = quick_settings.downgrade();
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::PANEL_INTERFACE)
            .expect("interface");
        let (window, calendar, quick_settings) =
            (window.clone(), calendar.clone(), quick_settings.clone());
        let res = conn
            .register_object(dbus::PANEL_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                match method {
                    "Focus" | "Unfocus" => {
                        if let Some(w) = window.upgrade() {
                            if method == "Focus" {
                                focus(&w);
                            } else {
                                unfocus(&w);
                            }
                        }
                        invocation.return_value(None);
                        return;
                    }
                    "LogOut" => {
                        invocation.return_value(None);
                        // After the reply, so the caller isn't left waiting.
                        gtk::glib::idle_add_local_once(crate::fade::log_out);
                        return;
                    }
                    "OpenMenu" | "ToggleMenu" => {}
                    _ => {
                        invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                        return;
                    }
                }
                let button = match params.get::<(String,)>().as_ref().map(|(m,)| m.as_str()) {
                    Some("calendar") => calendar.upgrade(),
                    Some("quick-settings") => quick_settings.upgrade(),
                    _ => None,
                };
                match button {
                    Some(b) => {
                        let open = b.popover().is_some_and(|p| p.is_visible());
                        if method == "ToggleMenu" && open {
                            b.popdown();
                        } else if !open {
                            b.popup();
                        }
                        invocation.return_value(None);
                    }
                    None => invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "no such menu",
                    ),
                }
            })
            .build();
        if let Err(e) = res {
            tracing::error!(error = %e, "could not export org.umoja.Panel");
        }
    };
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        dbus::PANEL_BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        on_bus,
        |_, _| {},
        |_, _| tracing::warn!("could not own {}", dbus::PANEL_BUS_NAME),
    );
}
