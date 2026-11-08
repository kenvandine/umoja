//! umoja-panel: the top bar.
//!
//! Activities button on the left, clock/calendar in the middle, tray icons
//! and the Quick Settings menu on the right. Runs as its own process
//! (`umoja-panel.service`); if it crashes, systemd restarts it and the rest
//! of the session carries on.

mod battery;
mod bluetooth;
mod bus;
mod clock;
mod dbusmenu;
mod launch;
mod lock;
mod login;
mod network;
mod notifications;
mod quick_settings;
mod sni;
mod volume;
mod wallpaper;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use libadwaita as adw;
use umoja_common::{dbus, i18n};

const APP_ID: &str = "org.umoja.Panel";
const PANEL_HEIGHT: i32 = 32;

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-panel");
    i18n::init();

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build);
    app.run()
}

thread_local! {
    static WALLPAPER: std::cell::OnceCell<std::rc::Rc<wallpaper::Wallpaper>> =
        const { std::cell::OnceCell::new() };
    static NOTIFICATIONS: std::cell::OnceCell<std::rc::Rc<notifications::Center>> =
        const { std::cell::OnceCell::new() };
}

fn build(app: &adw::Application) {
    WALLPAPER.with(|w| {
        w.get_or_init(|| wallpaper::Wallpaper::start(app));
    });
    let center = NOTIFICATIONS.with(|n| n.get_or_init(|| notifications::Center::start(app)).clone());

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .default_height(PANEL_HEIGHT)
        .css_classes(["umoja-panel"])
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_namespace("umoja-panel");
    for edge in [Edge::Top, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.auto_exclusive_zone_enable();

    let bar = gtk::CenterBox::new();
    bar.set_start_widget(Some(&activities_button()));
    bar.set_center_widget(Some(&clock::ClockButton::new(&center).widget()));

    let end = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    end.append(&sni::Tray::start().widget());
    end.append(&quick_settings::button());
    bar.set_end_widget(Some(&end));

    window.set_child(Some(&bar));
    window.present();
    lock::init();
}

fn activities_button() -> gtk::Button {
    // "Activities" comes from gnome-shell's catalog, already translated.
    let button = gtk::Button::with_label(&i18n::shell("Activities"));
    button.add_css_class("flat");
    button.add_css_class("activities");
    button.connect_clicked(|_| toggle_overlay());
    button
}

/// Activities: ask umoja-overlay to show/hide the overview (window spread).
pub fn toggle_overlay() {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            "ToggleOverview",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |res| {
                if let Err(e) = res {
                    tracing::warn!(error = %e, "could not toggle overlay");
                }
            },
        );
    });
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("panel.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
