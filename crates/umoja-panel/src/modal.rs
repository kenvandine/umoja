//! The shell's password card: a full-screen overlay surface that dims the
//! screen and holds one blurred dark card in the middle, taking the
//! keyboard while it is up. Shared by the PolicyKit, NetworkManager and
//! keyring dialogs, which are gnome-shell's modal dialogs in our style
//! (the `umoja-polkit` styles in panel.css).

use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

pub struct Modal {
    pub window: gtk::Window,
    /// Where the dialog puts its contents, top to bottom.
    pub card: gtk::Box,
}

/// A modal card on the primary monitor's overlay layer. `class` is added
/// to the window next to `umoja-polkit`, for per-dialog styles.
pub fn new(namespace: &str, class: Option<&str>) -> Modal {
    let mut classes = vec!["umoja-polkit"];
    classes.extend(class);
    let window = gtk::Window::builder()
        .css_classes(classes)
        .decorated(false)
        .build();
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(namespace);
    crate::place_on_primary(&window);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    // The card is see-through: blur what is behind it, like the bubbles.
    umoja_common::blur::blur_behind(&window);

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .width_request(400)
        .css_classes(["polkit-card"])
        .build();
    let backdrop = gtk::Box::builder().css_classes(["polkit-backdrop"]).build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&backdrop));
    overlay.add_overlay(&card);
    window.set_child(Some(&overlay));
    Modal { window, card }
}

/// Run `f` when Escape is pressed anywhere in the dialog.
pub fn on_escape(window: &gtk::Window, f: impl Fn() + 'static) {
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            f();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
}

/// The Cancel / OK pair at the bottom of every card.
pub fn button_row(cancel: &gtk::Button, ok: &gtk::Button) -> gtk::Box {
    let buttons = gtk::Box::builder()
        .spacing(12)
        .homogeneous(true)
        .margin_top(12)
        .build();
    buttons.append(cancel);
    buttons.append(ok);
    buttons
}
