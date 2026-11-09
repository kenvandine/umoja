//! `org.umoja.MediaKeys`: what gnome-settings-daemon's media-keys plugin
//! does in GNOME. umoja-mir grabs the keys from the same schema and calls
//! `Press` with the schema key's name ("volume-up", "mic-mute"...); this
//! changes the volume or brightness, drives MPRIS players, starts apps, and
//! shows the OSD. `ShowOSD` takes gnome-shell's `ShowOSD` dictionary, for
//! anything else that wants to show one.

use gio::prelude::*;
use gtk4::{glib, prelude::*};
use umoja_common::dbus;

use crate::brightness::Brightness;
use crate::volume::{Key, Volume};
use crate::{launch, mpris, osd};

const XML: &str = r#"
<node>
  <interface name="org.umoja.MediaKeys">
    <method name="Press">
      <arg type="s" name="key" direction="in"/>
    </method>
    <method name="ShowOSD">
      <arg type="a{sv}" name="params" direction="in"/>
    </method>
  </interface>
</node>"#;

pub fn own_name() {
    let on_bus = |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::MEDIA_KEYS_INTERFACE)
            .expect("interface");
        let res = conn
            .register_object(dbus::MEDIA_KEYS_OBJECT_PATH, &iface)
            .method_call(|_, _, _, _, method, params, invocation| match method {
                "Press" => match params.get::<(String,)>() {
                    Some((key,)) => {
                        invocation.return_value(None);
                        press(&key);
                    }
                    None => invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "s"),
                },
                "ShowOSD" => {
                    let dict = glib::VariantDict::new(Some(&params.child_value(0)));
                    match osd::Request::from_dict(&dict) {
                        Some(r) => {
                            invocation.return_value(None);
                            osd::show(r);
                        }
                        None => invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "icon is required"),
                    }
                }
                _ => invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
            })
            .build();
        if let Err(e) = res {
            tracing::warn!(error = %e, "could not export org.umoja.MediaKeys");
        }
    };
    let id = gio::bus_own_name(
        gio::BusType::Session,
        dbus::MEDIA_KEYS_BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        on_bus,
        |_, _| {},
        |_, name| tracing::warn!(name, "lost bus name"),
    );
    std::mem::forget(id);
}

/// One media-keys schema key.
pub fn press(key: &str) {
    tracing::debug!(key, "media key");
    let vol = || Volume::get();
    match key {
        "volume-up" => vol().key(Key::Up, false, false),
        "volume-down" => vol().key(Key::Down, false, false),
        "volume-mute" => vol().key(Key::Mute, false, false),
        "volume-up-quiet" => vol().key(Key::Up, true, false),
        "volume-down-quiet" => vol().key(Key::Down, true, false),
        "volume-mute-quiet" => vol().key(Key::Mute, true, false),
        "volume-up-precise" => vol().key(Key::Up, false, true),
        "volume-down-precise" => vol().key(Key::Down, false, true),
        "mic-mute" => vol().toggle_mic_mute(),
        "screen-brightness-up" => Brightness::get().step_screen(true),
        "screen-brightness-down" => Brightness::get().step_screen(false),
        "keyboard-brightness-up" => Brightness::get().keyboard(Some(true)),
        "keyboard-brightness-down" => Brightness::get().keyboard(Some(false)),
        "keyboard-brightness-toggle" => Brightness::get().keyboard(None),
        "play" => mpris::send("PlayPause"),
        "pause" => mpris::send("Pause"),
        "stop" => mpris::send("Stop"),
        "previous" => mpris::send("Previous"),
        "next" => mpris::send("Next"),
        "calculator" => launch_desktop(&["org.gnome.Calculator.desktop", "gnome-calculator.desktop"]),
        "control-center" => crate::quick_settings::open_settings(""),
        "www" => launch_default(gio::AppInfo::default_for_uri_scheme("http"), None),
        "email" => launch_default(gio::AppInfo::default_for_uri_scheme("mailto"), None),
        "home" => launch_default(
            gio::AppInfo::default_for_type("inode/directory", false),
            Some(glib::home_dir().to_string_lossy().into_owned()),
        ),
        // GNOME's search key opens the overview, ready to type.
        "search" => crate::toggle_overlay(),
        "screensaver" => crate::lock::locker().lock(),
        other => tracing::debug!(key = other, "unhandled media key"),
    }
}

fn launch_desktop(ids: &[&str]) {
    match ids.iter().find_map(|id| gio::DesktopAppInfo::new(id)) {
        Some(info) => launch_info(info.upcast(), None),
        None => tracing::info!(?ids, "app not installed"),
    }
}

fn launch_default(info: Option<gio::AppInfo>, arg: Option<String>) {
    match info {
        Some(info) => launch_info(info, arg),
        None => tracing::info!("no default app"),
    }
}

/// Start an app in its own scope, like the rest of the panel's launches.
fn launch_info(info: gio::AppInfo, arg: Option<String>) {
    let Some(cmd) = info.commandline() else { return };
    let Ok(words) = glib::shell_parse_argv(cmd.as_os_str()) else { return };
    let mut argv: Vec<String> = words
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .filter(|w| !(w.len() == 2 && w.starts_with('%')))
        .collect();
    if let Some(arg) = arg {
        argv.push(arg);
    }
    let id = info.id().map(|s| s.trim_end_matches(".desktop").to_string()).unwrap_or_default();
    // An activation token, so the compositor focuses the new window.
    let token = gtk4::gdk::Display::default()
        .and_then(|d| d.app_launch_context().startup_notify_id(Some(&info), &[]))
        .map(|t| t.to_string());
    launch::spawn_app(&id, &argv, token.as_deref());
}
