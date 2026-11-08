//! umoja-greeter: a GTK4 LightDM greeter in the style of Ubuntu's login screen.

mod auth;
mod backdrop;
mod ui;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use umoja_common::i18n;

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-greeter");
    i18n::init();
    apply_hidpi_scale();
    apply_desktop_defaults();

    let app = adw::Application::builder()
        .application_id("org.umoja.Greeter")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("greeter.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    // `umoja-greeter --preview` shows the UI with sample users, no LightDM.
    let preview = std::env::args().any(|a| a == "--preview");
    app.connect_activate(move |app| {
        match if preview {
            Ok(auth::Greeter::preview())
        } else {
            auth::Greeter::connect()
        } {
            Ok(greeter) => ui::build(app, greeter),
            Err(e) => {
                // Without the daemon there is nothing useful to show.
                tracing::error!(error = %e, "could not connect to LightDM");
                app.quit();
            }
        }
    });
    app.run_with_args::<&str>(&[])
}

/// LightDM runs greeters on X11, where GTK only scales by whole numbers and
/// only when told to. Round the panel's preferred (mutter) scale: 150% and
/// up becomes 2x, so e.g. a 166⅔% OLED laptop isn't tiny. Must run before
/// GTK starts.
fn apply_hidpi_scale() {
    if std::env::var_os("GDK_SCALE").is_some() {
        return;
    }
    let Some(panel) = umoja_common::hidpi::connected_panels().into_iter().next() else {
        return;
    };
    let scale = panel.preferred_scale();
    if scale >= 1.5 {
        tracing::info!(
            connector = panel.connector,
            scale,
            "using 2x scale for the greeter"
        );
        // SAFETY: single-threaded, before GTK or any other thread starts.
        unsafe { std::env::set_var("GDK_SCALE", "2") };
    }
}

/// LightDM starts greeters with a bare environment and no settings daemon,
/// so GTK would fall back to upstream defaults. Ask for Ubuntu's (Yaru,
/// Ubuntu Sans, the Yaru cursor at the right size) the way the session does.
fn apply_desktop_defaults() {
    // SAFETY: single-threaded, before GTK or any other thread starts.
    unsafe {
        if std::env::var_os("XDG_CURRENT_DESKTOP").is_none() {
            std::env::set_var("XDG_CURRENT_DESKTOP", "Umoja:ubuntu:GNOME");
        }
        if std::env::var_os("XCURSOR_THEME").is_none() {
            let theme = umoja_common::gsettings::get_string(
                umoja_common::gsettings::DESKTOP_INTERFACE,
                "cursor-theme",
                "Yaru",
            );
            std::env::set_var("XCURSOR_THEME", theme);
        }
        if std::env::var_os("XCURSOR_SIZE").is_none() {
            let base = umoja_common::gsettings::get_int(
                umoja_common::gsettings::DESKTOP_INTERFACE,
                "cursor-size",
                24,
            );
            let scale: i32 = std::env::var("GDK_SCALE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            std::env::set_var("XCURSOR_SIZE", (base * scale).to_string());
        }
    }
}
