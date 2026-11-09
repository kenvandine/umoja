//! org.umoja.Panel: lets the compositor open the top bar's menus for
//! GNOME's top-edge touch swipe (left part: calendar and notifications,
//! right part: quick settings).

use gtk::{gio, prelude::*};
use gtk4 as gtk;
use umoja_common::dbus;

const XML: &str = r#"<node>
  <interface name="org.umoja.Panel">
    <method name="OpenMenu"><arg type="s" name="menu" direction="in"/></method>
  </interface>
</node>"#;

pub fn own_name(calendar: &gtk::MenuButton, quick_settings: &gtk::MenuButton) {
    let calendar = calendar.downgrade();
    let quick_settings = quick_settings.downgrade();
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::PANEL_INTERFACE)
            .expect("interface");
        let (calendar, quick_settings) = (calendar.clone(), quick_settings.clone());
        let res = conn
            .register_object(dbus::PANEL_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                if method != "OpenMenu" {
                    invocation
                        .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                    return;
                }
                let button = match params.get::<(String,)>().as_ref().map(|(m,)| m.as_str()) {
                    Some("calendar") => calendar.upgrade(),
                    Some("quick-settings") => quick_settings.upgrade(),
                    _ => None,
                };
                match button {
                    Some(b) => {
                        b.popup();
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
