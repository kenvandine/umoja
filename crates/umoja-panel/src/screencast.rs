//! gnome-shell's screen recording indicator: a red pill in the top bar with
//! a stop icon and the elapsed time while umoja-overlay records the screen.
//! Clicking it stops the recording.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::time::{Duration, Instant};
use umoja_common::{dbus, i18n};

/// "0:07", "12:34", "1:02:03", as gnome-shell's indicator shows it.
pub fn elapsed_label(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn indicator() -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&gtk::Image::from_gicon(&gio::ThemedIcon::from_names(&[
        "screencast-stop-symbolic",
        "media-playback-stop-symbolic",
    ])));
    let label = gtk::Label::new(Some(&elapsed_label(0)));
    label.add_css_class("numeric");
    content.append(&label);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["flat", "screencast-indicator"])
        .tooltip_text(i18n::shell("Stop Screencast"))
        .visible(false)
        .build();
    button.connect_clicked(|_| stop());
    watch(&button, &label);
    button
}

fn stop() {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            "StopScreencast",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |res| {
                if let Err(e) = res {
                    tracing::warn!(error = %e, "could not stop the screencast");
                }
            },
        );
    });
}

/// Show/hide on org.umoja.Overlay's ScreencastChanged, and tick the clock.
fn watch(button: &gtk::Button, label: &gtk::Label) {
    let button = button.downgrade();
    let label = label.downgrade();
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let timer: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>> = Default::default();
        let set = {
            let (button, label, timer) = (button.clone(), label.clone(), timer.clone());
            move |active: bool| {
                let (Some(b), Some(l)) = (button.upgrade(), label.upgrade()) else {
                    return;
                };
                if let Some(id) = timer.borrow_mut().take() {
                    id.remove();
                }
                b.set_visible(active);
                if !active {
                    return;
                }
                let start = Instant::now();
                l.set_text(&elapsed_label(0));
                let lw = l.downgrade();
                let id = glib::timeout_add_local(Duration::from_millis(500), move || {
                    match lw.upgrade() {
                        Some(l) => {
                            l.set_text(&elapsed_label(start.elapsed().as_secs()));
                            glib::ControlFlow::Continue
                        }
                        None => glib::ControlFlow::Break,
                    }
                });
                *timer.borrow_mut() = Some(id);
            }
        };
        let on_signal = set.clone();
        let id = conn.signal_subscribe(
            Some(dbus::OVERLAY_BUS_NAME),
            Some(dbus::OVERLAY_INTERFACE),
            Some("ScreencastChanged"),
            Some(dbus::OVERLAY_OBJECT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let Some((active,)) = params.get::<(bool,)>() {
                    on_signal(active);
                }
            },
        );
        std::mem::forget(id);
        // A panel restarted mid-recording picks the indicator back up.
        let reply = conn
            .call_future(
                Some(dbus::OVERLAY_BUS_NAME),
                dbus::OVERLAY_OBJECT_PATH,
                dbus::OVERLAY_INTERFACE,
                "IsScreencasting",
                None,
                Some(glib::VariantTy::new("(b)").unwrap()),
                gio::DBusCallFlags::NONE,
                2000,
            )
            .await;
        if let Some((true,)) = reply.ok().and_then(|v| v.get::<(bool,)>()) {
            set(true);
        }
        std::mem::forget(conn);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(elapsed_label(0), "0:00");
        assert_eq!(elapsed_label(7), "0:07");
        assert_eq!(elapsed_label(754), "12:34");
        assert_eq!(elapsed_label(3723), "1:02:03");
    }
}
