//! gnome-shell's screen sharing indicator: an orange pill in the top bar
//! while the screen or a window is shared through the ScreenCast portal (a
//! video call, OBS, a remote desktop tool). Clicking it stops the sharing,
//! as the indicator's "Turn off" does in GNOME.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use umoja_common::{dbus, i18n};

pub fn indicator() -> gtk::Button {
    let button = gtk::Button::builder()
        .child(&gtk::Image::from_gicon(&gio::ThemedIcon::from_names(&[
            "screen-shared-symbolic",
            "video-display-symbolic",
        ])))
        .css_classes(["flat", "screen-sharing-indicator"])
        .tooltip_text(i18n::shell("Screen is being shared"))
        .visible(false)
        .build();
    button.connect_clicked(|b| {
        b.set_visible(false);
        stop(b.downgrade());
    });
    watch(&button);
    button
}

fn stop(button: glib::WeakRef<gtk::Button>) {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let res = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                "StopScreenSharing",
                None,
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "could not stop screen sharing");
            if let Some(b) = button.upgrade() {
                b.set_visible(true);
            }
        }
    });
}

/// Follow org.umoja.Compositor's ScreenSharingChanged.
fn watch(button: &gtk::Button) {
    let button = button.downgrade();
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let on_signal = button.clone();
        let id = conn.signal_subscribe(
            Some(dbus::COMPOSITOR_BUS_NAME),
            Some(dbus::COMPOSITOR_INTERFACE),
            Some("ScreenSharingChanged"),
            Some(dbus::COMPOSITOR_OBJECT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let (Some((sharing,)), Some(b)) = (params.get::<(bool,)>(), on_signal.upgrade()) {
                    b.set_visible(sharing);
                }
            },
        );
        std::mem::forget(id);
        // A panel restarted mid-call picks the indicator back up.
        let reply = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                "IsScreenSharing",
                None,
                Some(glib::VariantTy::new("(b)").unwrap()),
                gio::DBusCallFlags::NONE,
                2000,
            )
            .await;
        if let (Some((true,)), Some(b)) = (reply.ok().and_then(|v| v.get::<(bool,)>()), button.upgrade()) {
            b.set_visible(true);
        }
        std::mem::forget(conn);
    });
}
