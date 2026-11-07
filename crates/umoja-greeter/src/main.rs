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

/// LightDM runs greeters on X11, where GTK only scales when told to. Pick
/// the same integer scale mutter would round to for the main panel, so the
/// login screen isn't tiny on a HiDPI laptop. Must run before GTK starts.
fn apply_hidpi_scale() {
    if std::env::var_os("GDK_SCALE").is_some() {
        return;
    }
    let Some(panel) = umoja_common::hidpi::connected_panels().into_iter().next() else {
        return;
    };
    let scale = panel.preferred_scale();
    if scale >= 1.75 {
        tracing::info!(
            connector = panel.connector,
            scale,
            "using 2x scale for the greeter"
        );
        // SAFETY: single-threaded, before GTK or any other thread starts.
        unsafe { std::env::set_var("GDK_SCALE", "2") };
    }
}
