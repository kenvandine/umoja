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
mod fade;
mod launch;
mod lock;
mod login;
mod menu_api;
mod myna;
mod network;
mod notifications;
mod polkit;
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
    static MYNA: std::cell::OnceCell<std::rc::Rc<myna::Myna>> =
        const { std::cell::OnceCell::new() };
}

fn build(app: &adw::Application) {
    WALLPAPER.with(|w| {
        w.get_or_init(|| wallpaper::Wallpaper::start(app));
    });
    let center =
        NOTIFICATIONS.with(|n| n.get_or_init(|| notifications::Center::start(app)).clone());

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
    let clock = clock::ClockButton::new(&center).widget();
    bar.set_center_widget(Some(&clock));

    let end = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    // Myna's recording indicator, left of the tray like GNOME's privacy
    // indicators; the same object hosts Myna's HUD pill.
    end.append(&MYNA.with(|m| m.get_or_init(myna::Myna::start).widget()));
    end.append(&sni::Tray::start().widget());
    let quick = quick_settings::button();
    end.append(&quick);
    if let Some(calendar) = clock.downcast_ref::<gtk::MenuButton>() {
        menu_api::own_name(calendar, &quick);
    }
    bar.set_end_widget(Some(&end));

    window.set_child(Some(&bar));
    fade::fade_in_on_map(&window, fade::SESSION_IN_MS);
    window.present();
    lock::init();
    polkit::start();
}

/// GNOME 45+'s Activities button: one dot per workspace, the active one a
/// wider pill. The label stays as the accessible name and tooltip.
fn activities_button() -> gtk::Button {
    // "Activities" comes from gnome-shell's catalog, already translated.
    let label = i18n::shell("Activities");
    let dots = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    dots.set_valign(gtk::Align::Center);
    dots.add_css_class("workspace-dots");
    set_workspace_dots(&dots, 0, 1);
    let button = gtk::Button::builder()
        .child(&dots)
        .tooltip_text(&label)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&label)]);
    button.add_css_class("flat");
    button.add_css_class("activities");
    button.connect_clicked(|_| toggle_overlay());
    watch_workspaces(&dots);
    button
}

fn set_workspace_dots(dots: &gtk::Box, current: u32, count: u32) {
    while let Some(c) = dots.first_child() {
        dots.remove(&c);
    }
    for i in 0..count.max(1) {
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.set_valign(gtk::Align::Center);
        dot.add_css_class("workspace-dot");
        if i == current {
            dot.add_css_class("active");
        }
        dots.append(&dot);
    }
}

/// Follow org.umoja.Compositor's workspaces.
fn watch_workspaces(dots: &gtk::Box) {
    let dots = dots.downgrade();
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let d = dots.clone();
        let id = conn.signal_subscribe(
            Some(dbus::COMPOSITOR_BUS_NAME),
            Some(dbus::COMPOSITOR_INTERFACE),
            Some("WorkspacesChanged"),
            Some(dbus::COMPOSITOR_OBJECT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let (Some(dots), Some((current, count))) =
                    (d.upgrade(), params.get::<(u32, u32)>())
                {
                    set_workspace_dots(&dots, current, count);
                }
            },
        );
        std::mem::forget(id);
        let reply = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                "GetWorkspaces",
                None,
                Some(glib::VariantTy::new("(uu)").unwrap()),
                gio::DBusCallFlags::NONE,
                2000,
            )
            .await;
        if let (Some(dots), Some((current, count))) = (
            dots.upgrade(),
            reply.ok().and_then(|v| v.get::<(u32, u32)>()),
        ) {
            set_workspace_dots(&dots, current, count);
        }
        std::mem::forget(conn);
    });
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
