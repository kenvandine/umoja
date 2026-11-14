//! umoja-panel: the top bar.
//!
//! Activities button on the left, clock/calendar in the middle, tray icons
//! and the Quick Settings menu on the right. Runs as its own process
//! (`umoja-panel.service`); if it crashes, systemd restarts it and the rest
//! of the session carries on.

mod automount;
mod battery;
mod bluetooth;
mod brightness;
mod bus;
mod captive_portal;
mod clock;
mod crash_watch;
mod dbusmenu;
mod downloads;
mod event_sound;
mod events;
mod fade;
mod keyring_prompt;
mod launch;
mod location;
mod lock;
mod login;
mod media_keys;
mod menu_api;
mod menu_gap;
mod modal;
mod mount_op;
mod mpris;
mod myna;
mod network;
mod nm_agent;
mod notifications;
mod osd;
mod polkit;
mod power_dialog;
mod quick_settings;
mod screencast;
mod sharing;
mod sni;
mod sound;
mod volume;
mod wallpaper;
mod welcome;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use libadwaita as adw;
use umoja_common::{dbus, i18n, monitors};

const APP_ID: &str = "org.umoja.Panel";
const PANEL_HEIGHT: i32 = 32;

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-panel");
    i18n::init();
    // The welcome screen shown while the desktop loads (welcome.rs).
    if std::env::args().any(|a| a == "--welcome") {
        return welcome::run();
    }

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| load_css());
    // Map nothing while the screen is blanked: Mir has no active output
    // then (see umoja_common::screen).
    app.connect_activate(|app| {
        // Own org.kde.StatusNotifierWatcher straight away, before waiting
        // for the screen and monitors (that took ~11s after a compositor
        // restart). Electron apps (Claude Desktop, Fluxer, Discord) look for
        // the watcher once when they start and never retry, and autostart
        // apps start as soon as umoja-tray-ready.service sees the name.
        TRAY.with(|t| {
            t.get_or_init(sni::Tray::start);
        });
        let a = app.clone();
        umoja_common::screen::when_on(app, move || build(&a));
    });
    // systemd stops the panel with SIGTERM: quit cleanly so Myna's HUD,
    // which `snap run` puts in a scope of its own, goes with it.
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let app = app.clone();
        glib::unix_signal_add_local_once(sig, move || app.quit());
    }
    app.connect_shutdown(|_| {
        MYNA.with(|m| {
            if let Some(m) = m.get() {
                m.shutdown();
            }
        })
    });
    app.run()
}

thread_local! {
    static WALLPAPER: std::cell::OnceCell<std::rc::Rc<wallpaper::Wallpaper>> =
        const { std::cell::OnceCell::new() };
    static NOTIFICATIONS: std::cell::OnceCell<std::rc::Rc<notifications::Center>> =
        const { std::cell::OnceCell::new() };
    static MYNA: std::cell::OnceCell<std::rc::Rc<myna::Myna>> =
        const { std::cell::OnceCell::new() };
    static TRAY: std::cell::OnceCell<std::rc::Rc<sni::Tray>> =
        const { std::cell::OnceCell::new() };
}

/// The shell's notification center, once the panel is up.
pub fn notification_center() -> Option<std::rc::Rc<notifications::Center>> {
    NOTIFICATIONS.with(|n| n.get().cloned())
}

fn build(app: &adw::Application) {
    // Learn the primary monitor before the first surface maps.
    monitors::start();
    WALLPAPER.with(|w| {
        w.get_or_init(|| wallpaper::Wallpaper::start(app));
    });
    let center =
        NOTIFICATIONS.with(|n| n.get_or_init(|| notifications::Center::start(app)).clone());
    crash_watch::start(center.clone());

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
    umoja_common::blur::blur_behind(&window);
    // GNOME's top bar is on the primary monitor only.
    keep_on_primary(window.upcast_ref());

    let bar = gtk::CenterBox::new();
    bar.set_start_widget(Some(&activities_button()));
    let clock = clock::ClockButton::new(&center).widget();
    bar.set_center_widget(Some(&clock));

    // No spacing: the buttons' transparent borders (panel.css) fill the gaps,
    // so a click between two items still hits one of them.
    let end = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    // gnome-shell's screen recording indicator comes first on the right.
    end.append(&screencast::indicator());
    // And its screen sharing indicator (portal screen casts).
    end.append(&sharing::indicator());
    // Myna's recording indicator, left of the tray like GNOME's privacy
    // indicators; the same object hosts Myna's HUD pill.
    end.append(&MYNA.with(|m| m.get_or_init(myna::Myna::start).widget()));
    // Background downloads (the assistant's model, Piper voices), only
    // while there are some.
    end.append(&downloads::indicator());
    end.append(&TRAY.with(|t| t.get_or_init(sni::Tray::start).widget()));
    let quick = quick_settings::button();
    end.append(&quick);
    if let Some(calendar) = clock.downcast_ref::<gtk::MenuButton>() {
        menu_api::own_name(window.upcast_ref(), calendar, &quick);
    }
    bar.set_end_widget(Some(&end));

    window.set_child(Some(&bar));
    fade::fade_in_on_map(&window, fade::SESSION_IN_MS);
    window.present();
    lock::init();
    polkit::start();
    // Wi-Fi, VPN and 802.1X password prompts for NetworkManager.
    nm_agent::start();
    // "Sign in to Network" and the Hotspot Login window for captive portals.
    captive_portal::start();
    // Ask before apps get the location (GeoClue's agent).
    location::start();
    // Drive passphrases, network share logins and "drive is busy" for
    // every app (org.gtk.MountOperationHandler), and automounting drives.
    mount_op::start();
    automount::start();
    // gnome-keyring's unlock and new-password prompts.
    keyring_prompt::start();
    // Volume, brightness and playback keys, and their on-screen display.
    osd::init(app);
    media_keys::own_name();
    brightness::Brightness::get();
}

/// Open a new layer surface (a dialog, a popup) on the primary monitor,
/// as gnome-shell does. Call before presenting it.
pub fn place_on_primary(window: &gtk::Window) {
    if let Some(m) = monitors::primary() {
        window.set_monitor(&m);
    }
}

/// Put a long-lived layer surface on the primary monitor and move it when
/// the primary changes or its monitor is unplugged.
pub fn keep_on_primary(window: &gtk::Window) {
    place_on_primary(window);
    let weak = window.downgrade();
    monitors::connect_changed(move || {
        let Some(w) = weak.upgrade() else { return };
        if monitors::primary().is_none_or(|m| w.monitor().as_ref() == Some(&m)) {
            return;
        }
        let Some(app) = gio::Application::default() else {
            return;
        };
        // Moving remaps the surface: never while the screen is off.
        let weak = w.downgrade();
        umoja_common::screen::when_on(&app, move || {
            let (Some(w), Some(m)) = (weak.upgrade(), monitors::primary()) else {
                return;
            };
            if w.monitor().as_ref() != Some(&m) {
                tracing::info!(
                    surface = %w.namespace().unwrap_or_default(),
                    monitor = m.connector().as_deref().unwrap_or("?"),
                    "moving to the primary monitor"
                );
                w.set_monitor(&m);
            }
        });
    });
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
    button.connect_clicked(|b| {
        // The overview takes the keyboard from Ctrl+Alt+Tab navigation.
        if let Some(w) = b.root().and_downcast::<gtk::Window>() {
            menu_api::unfocus(&w);
        }
        toggle_overlay()
    });
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
    // The shell chrome is always dark, like the notification bubbles,
    // whatever Settings > Appearance says; apps keep following it.
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    umoja_common::icons::add_shell_icons();
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
