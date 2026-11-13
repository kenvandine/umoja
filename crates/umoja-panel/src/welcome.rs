//! `umoja-panel --welcome`: the screen shown while the desktop loads.
//!
//! After login the compositor comes up empty and the wallpaper, top bar and
//! dock then appear one by one as their processes start. This covers that
//! with the login screen's wallpaper, "Welcome, <name>" and a spinner, and
//! crossfades to the finished desktop once the panel and the dock are both
//! up. It is a process of its own (umoja-welcome.service) so its animation
//! never waits on the panel's start-up, and so nothing here can hold the
//! panel back.
//!
//! bin/umoja-session leaves a marker in the runtime directory at login;
//! the unit only starts when it is there and we remove it, so a panel or
//! compositor restart later in the session shows no welcome.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use umoja_common::{dbus, i18n};

/// The greeter's background (crates/umoja-greeter/src/backdrop.rs), so the
/// picture carries on from the login screen.
const WALLPAPER: &str = "/usr/share/backgrounds/warty-final-ubuntu.png";
/// Left in $XDG_RUNTIME_DIR by bin/umoja-session at login.
const MARKER: &str = "umoja-welcome";

/// Fade of the picture and text up from black.
const IN_MS: u32 = 350;
/// Crossfade to the desktop.
const OUT_MS: u32 = 600;
/// Shown at least this long, so a fast login doesn't flash it.
const MIN_SHOWN: Duration = Duration::from_millis(1100);
/// After the panel and dock have their bus names: time for their surfaces
/// to map and finish their own fade (fade::SESSION_IN_MS).
const SETTLE: Duration = Duration::from_millis(800);
/// Never keep the desktop covered longer than this, whatever is missing.
const MAX_SHOWN: Duration = Duration::from_secs(8);

const CSS: &str = "
window.umoja-welcome { background-color: #000000; }
window.umoja-welcome .dim {
  background-image: linear-gradient(to bottom,
    rgba(0,0,0,0.30), rgba(0,0,0,0.40) 55%, rgba(0,0,0,0.62));
}
window.umoja-welcome label.greeting {
  color: #ffffff;
  font-size: 26pt;
  font-weight: 300;
  text-shadow: 0 1px 6px rgba(0,0,0,0.5);
}
window.umoja-welcome spinner { color: rgba(255,255,255,0.85); }
";

pub fn marker_path() -> std::path::PathBuf {
    glib::user_runtime_dir().join(MARKER)
}

struct State {
    app: adw::Application,
    windows: Vec<gtk::Window>,
    contents: Vec<gtk::Widget>,
    /// When the picture and text became visible.
    shown: Cell<Option<Instant>>,
    panel: Cell<bool>,
    dock: Cell<bool>,
    leaving: Cell<bool>,
}

pub fn run() -> glib::ExitCode {
    // Once per login, even if this process is started again.
    if std::fs::remove_file(marker_path()).is_err() {
        return glib::ExitCode::SUCCESS;
    }
    let app = adw::Application::builder()
        .application_id("org.umoja.Welcome")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(build);
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let app = app.clone();
        glib::unix_signal_add_local_once(sig, move || app.quit());
    }
    app.run_with_args::<&str>(&[])
}

fn greeting() -> String {
    let name = glib::real_name().to_string_lossy().trim().to_string();
    // The first name, as other desktops greet: "Welcome, Ken".
    let first = name.split_whitespace().next().unwrap_or_default();
    if first.is_empty() || name == "Unknown" {
        i18n::umoja("Welcome")
    } else {
        i18n::umoja("Welcome, %s").replacen("%s", first, 1)
    }
}

fn build(app: &adw::Application) {
    let Some(display) = gdk::Display::default() else {
        app.quit();
        return;
    };
    let monitors = display.monitors();
    let mut windows = Vec::new();
    let mut contents = Vec::new();
    let mut pictures = Vec::new();
    for i in 0..monitors.n_items() {
        let Some(monitor) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
            continue;
        };
        let window = gtk::Window::builder()
            .application(app)
            .decorated(false)
            .css_classes(["umoja-welcome"])
            .build();
        window.init_layer_shell();
        // Above the panel, dock and any app that starts meanwhile.
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-welcome");
        window.set_monitor(&monitor);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }

        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_can_shrink(true);
        let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dim.add_css_class("dim");

        let label = gtk::Label::new(Some(&greeting()));
        label.add_css_class("greeting");
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.set_halign(gtk::Align::Center);
        let centre = gtk::Box::new(gtk::Orientation::Vertical, 22);
        centre.set_halign(gtk::Align::Center);
        // In the lower third, clear of the mascot in the middle of
        // Ubuntu's wallpaper, where the login card was.
        centre.set_valign(gtk::Align::End);
        centre.set_margin_bottom(monitor.geometry().height() / 6);
        centre.append(&label);
        centre.append(&spinner);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&dim);
        overlay.add_overlay(&centre);
        // Up from black once the picture has loaded.
        overlay.set_opacity(0.0);
        window.set_child(Some(&overlay));
        window.present();

        windows.push(window);
        contents.push(overlay.upcast::<gtk::Widget>());
        pictures.push(picture);
    }
    if windows.is_empty() {
        app.quit();
        return;
    }
    let state = Rc::new(State {
        app: app.clone(),
        windows,
        contents,
        shown: Cell::new(None),
        panel: Cell::new(false),
        dock: Cell::new(false),
        leaving: Cell::new(false),
    });

    // Decoding a 4K PNG takes a moment: do it off the main loop and fade
    // the whole screen in when it is there (or without it, if it is not).
    glib::spawn_future_local({
        let state = state.clone();
        async move {
            let texture = gio::spawn_blocking(|| gdk::Texture::from_filename(WALLPAPER).ok())
                .await
                .ok()
                .flatten();
            for p in &pictures {
                p.set_paintable(texture.as_ref());
            }
            state.shown.set(Some(Instant::now()));
            for c in &state.contents {
                crate::fade::fade(c, 1.0, IN_MS, || {});
            }
            state.maybe_leave();
        }
    });

    for (name, panel) in [(dbus::PANEL_BUS_NAME, true), (dbus::DOCK_BUS_NAME, false)] {
        let state = state.clone();
        gio::bus_watch_name(
            gio::BusType::Session,
            name,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if panel {
                    state.panel.set(true);
                } else {
                    state.dock.set(true);
                }
                let state = state.clone();
                glib::timeout_add_local_once(SETTLE, move || state.maybe_leave());
            },
            |_, _| {},
        );
    }

    let s = state.clone();
    glib::timeout_add_local_once(MAX_SHOWN, move || {
        if !s.leaving.get() {
            tracing::warn!(
                panel = s.panel.get(),
                dock = s.dock.get(),
                "desktop not ready in time; leaving the welcome screen"
            );
            s.leave();
        }
    });
}

impl State {
    /// Leave once the panel and dock are up and we have been seen for
    /// long enough.
    fn maybe_leave(self: &Rc<Self>) {
        if self.leaving.get() || !(self.panel.get() && self.dock.get()) {
            return;
        }
        let Some(shown) = self.shown.get() else {
            return;
        };
        let seen = shown.elapsed();
        if seen < MIN_SHOWN {
            let s = self.clone();
            glib::timeout_add_local_once(MIN_SHOWN - seen, move || s.maybe_leave());
            return;
        }
        self.leave();
    }

    /// Crossfade to the desktop underneath, then exit.
    fn leave(self: &Rc<Self>) {
        if self.leaving.replace(true) {
            return;
        }
        tracing::info!("desktop ready; leaving the welcome screen");
        let app = self.app.clone();
        // Whatever happens to the animation, never leave the desktop covered.
        let a = app.clone();
        glib::timeout_add_local_once(Duration::from_millis(OUT_MS as u64 + 1000), move || {
            a.quit()
        });
        for (i, w) in self.windows.iter().enumerate() {
            // Let clicks through to the desktop as soon as it shows.
            if let Some(surface) = w.surface() {
                surface.set_input_region(&gtk::cairo::Region::create());
            }
            let app = app.clone();
            crate::fade::fade(w, 0.0, OUT_MS, move || {
                if i == 0 {
                    app.quit();
                }
            });
        }
    }
}
