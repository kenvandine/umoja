//! "Allow location access": gnome-shell's GeoClue permission dialog, drawn
//! as the same card over a dimmed screen as the PolicyKit dialog (it reuses
//! that dialog's styles, `.umoja-polkit` in panel.css).

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use umoja_common::i18n;

/// Ask whether `app` may know where the user is. Escape means no.
pub async fn ask(app: &gio::DesktopAppInfo) -> bool {
    let (tx, rx) = async_channel::bounded::<bool>(1);
    let window = build(app, tx);
    window.present();
    let granted = rx.recv().await.unwrap_or(false);
    window.destroy();
    granted
}

fn build(app: &gio::DesktopAppInfo, tx: async_channel::Sender<bool>) -> gtk::Window {
    let window = gtk::Window::builder()
        .css_classes(["umoja-polkit", "umoja-location"])
        .decorated(false)
        .build();
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace("umoja-location");
    crate::place_on_primary(&window);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    umoja_common::blur::blur_behind(&window);

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .width_request(400)
        .css_classes(["polkit-card"])
        .build();
    let icon = gtk::Image::builder()
        .pixel_size(64)
        .margin_top(4)
        .css_classes(["icon-dropshadow"])
        .build();
    match app.icon() {
        Some(gicon) => icon.set_from_gicon(&gicon),
        None => icon.set_icon_name(Some("application-x-executable")),
    }
    card.append(&icon);
    card.append(
        &gtk::Label::builder()
            .label(i18n::shell("Allow location access"))
            .css_classes(["polkit-title"])
            .build(),
    );
    let body = |text: &str| {
        gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(40)
            .css_classes(["polkit-body"])
            .build()
    };
    let name = app.display_name();
    card.append(&body(
        &i18n::shell("The app %s wants to access your location").replace("%s", &name),
    ));
    // Apps can say why, in their desktop file (X-Geoclue-Reason).
    if let Some(reason) = app
        .locale_string("X-Geoclue-Reason")
        .filter(|r| !r.trim().is_empty())
    {
        card.append(&body(&reason));
    }
    let note = body(&i18n::shell(
        "Location access can be changed at any time from the privacy settings.",
    ));
    note.add_css_class("location-note");
    card.append(&note);

    let buttons = gtk::Box::builder()
        .spacing(12)
        .homogeneous(true)
        .margin_top(12)
        .build();
    let deny = gtk::Button::builder()
        .label(i18n::shell("Deny Access"))
        .css_classes(["pill"])
        .build();
    let grant = gtk::Button::builder()
        .label(i18n::shell("Grant Access"))
        .css_classes(["pill", "suggested-action"])
        .build();
    buttons.append(&deny);
    buttons.append(&grant);
    card.append(&buttons);

    let backdrop = gtk::Box::builder().css_classes(["polkit-backdrop"]).build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&backdrop));
    overlay.add_overlay(&card);
    window.set_child(Some(&overlay));

    let answer = move |granted: bool| {
        let tx = tx.clone();
        move || {
            let _ = tx.try_send(granted);
        }
    };
    deny.connect_clicked({
        let a = answer(false);
        move |_| a()
    });
    grant.connect_clicked({
        let a = answer(true);
        move |_| a()
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let a = answer(false);
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                a();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
    // Grant is the default button, as in gnome-shell.
    window.set_default_widget(Some(&grant));
    window.connect_map(move |_| {
        grant.grab_focus();
    });
    window
}
