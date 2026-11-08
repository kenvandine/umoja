//! umoja-overlay: app grid, window-switching entry point and AI search.
//!
//! Exposes `org.umoja.Overlay` on the session bus so the compositor (Super
//! key), panel (Activities) and dock (Show Apps) can toggle it.

mod lemonade;
mod model;
mod model_picker;
mod myna;
mod screenshot;
mod view;

use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use std::rc::Rc;
use umoja_common::{dbus, i18n};

const APP_ID: &str = "org.umoja.OverlayApp";

const IFACE_XML: &str = r#"
<node>
  <interface name="org.umoja.Overlay">
    <method name="Toggle"/>
    <method name="Show"/>
    <method name="Hide"/>
    <method name="ToggleDictation"/>
    <method name="ShowScreenshotUI"/>
    <method name="ScreenshotScreen"/>
    <method name="ScreenshotArea">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
      <arg name="width" type="i" direction="in"/>
      <arg name="height" type="i" direction="in"/>
    </method>
    <property name="Visible" type="b" access="read"/>
  </interface>
</node>"#;

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-overlay");
    i18n::init();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| load_css());
    app.connect_activate(|app| {
        let overlay = view::Overlay::new(app);
        let shots = screenshot::ScreenshotUi::new(app);
        // Fetch the assistant's model in the background at login.
        let weak = std::rc::Rc::downgrade(&overlay);
        std::mem::forget(model::ModelWatcher::start(move |status| {
            if let Some(o) = weak.upgrade() {
                o.set_model_status(status);
            }
        }));
        export(overlay, shots);
        // Keep the process alive while the overlay window is hidden.
        std::mem::forget(app.hold());
    });
    app.run()
}

fn export(overlay: Rc<view::Overlay>, shots: Rc<screenshot::ScreenshotUi>) {
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(IFACE_XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::OVERLAY_INTERFACE)
            .expect("interface");
        let methods = overlay.clone();
        let props = overlay.clone();
        let shots = shots.clone();
        let res = conn
            .register_object(dbus::OVERLAY_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                match method {
                    "ShowScreenshotUI" => {
                        methods.hide();
                        shots.open();
                    }
                    "ScreenshotScreen" => shots.capture_screen(),
                    "ScreenshotArea" => match params.get::<(i32, i32, i32, i32)>() {
                        Some((x, y, w, h)) => shots.capture_area(x, y, w, h),
                        None => {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (iiii)",
                            );
                            return;
                        }
                    },
                    "Toggle" => methods.toggle(),
                    "Show" => methods.show(),
                    "Hide" => methods.hide(),
                    "ToggleDictation" => methods.toggle_dictation(),
                    _ => {
                        invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                        return;
                    }
                }
                invocation.return_value(None);
            })
            .property(move |_, _, _, _, name| match name {
                "Visible" => props.is_visible().to_variant(),
                _ => false.to_variant(),
            })
            .build();
        if let Err(e) = res {
            tracing::error!(error = %e, "could not export org.umoja.Overlay");
        }
    };
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        dbus::OVERLAY_BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE,
        on_bus,
        |_, _| tracing::info!("overlay ready"),
        |_, _| tracing::warn!("lost org.umoja.Overlay"),
    );
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("overlay.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
