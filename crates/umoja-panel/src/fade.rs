//! Fades for the session's start and end, so logging in and out pass
//! smoothly through black instead of cutting between screens.

use gtk::{gdk, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;

/// How long the desktop takes to fade up after login.
pub const SESSION_IN_MS: u32 = 700;
/// How long the screen takes to fade to black on Log Out.
pub const SESSION_OUT_MS: u32 = 450;

/// Animates `widget`'s opacity to `to`, then runs `done`. libadwaita skips
/// the animation (running `done` at once) when animations are disabled.
pub fn fade(widget: &impl IsA<gtk::Widget>, to: f64, ms: u32, done: impl Fn() + 'static) {
    let widget = widget.as_ref();
    let target = adw::PropertyAnimationTarget::new(widget, "opacity");
    let anim = adw::TimedAnimation::new(widget, widget.opacity(), to, ms, target);
    anim.set_easing(adw::Easing::EaseInOutCubic);
    anim.connect_done(move |_| done());
    anim.play();
}

/// Makes `widget` start invisible and fade up once it is first on screen.
pub fn fade_in_on_map(widget: &impl IsA<gtk::Widget>, ms: u32) {
    let widget = widget.as_ref();
    widget.set_opacity(0.0);
    let id = std::rc::Rc::new(std::cell::Cell::new(None::<gtk::glib::SignalHandlerId>));
    let handler = {
        let id = id.clone();
        widget.connect_map(move |w| {
            if let Some(id) = id.take() {
                w.disconnect(id);
            }
            fade(w, 1.0, ms, || {});
        })
    };
    id.set(Some(handler));
}

/// Covers every monitor with black, fading in, then runs `then`. Used for
/// Log Out so the desktop dims away before the session ends; the greeter
/// then fades up from black on the other side.
pub fn to_black(app: &gtk::Application, then: fn()) {
    let Some(display) = gdk::Display::default() else {
        then();
        return;
    };
    let monitors = display.monitors();
    let mut started = false;
    // Whatever happens to the animation, never leave the user stuck.
    let fallback = gtk::glib::timeout_add_local_once(
        std::time::Duration::from_millis(SESSION_OUT_MS as u64 + 1500),
        then,
    );
    let fallback = std::rc::Rc::new(std::cell::Cell::new(Some(fallback)));
    for i in 0..monitors.n_items() {
        let Some(monitor) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
            continue;
        };
        let window = gtk::Window::builder()
            .application(app)
            .decorated(false)
            .css_classes(["umoja-curtain"])
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-curtain");
        window.set_monitor(&monitor);
        // Swallow input while the session winds down.
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.set_exclusive_zone(-1);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        // Black window (see panel.css), faded in as a whole.
        window.set_opacity(0.0);
        // Only the first monitor's fade ends the session.
        let lead = !started;
        started = true;
        let fallback = fallback.clone();
        window.connect_map(move |w| {
            let fallback = fallback.clone();
            fade(w, 1.0, SESSION_OUT_MS, move || {
                if lead && let Some(id) = fallback.take() {
                    id.remove();
                    then();
                }
            });
        });
        window.present();
    }
    if !started && let Some(id) = fallback.take() {
        id.remove();
        then();
    }
}

/// Log Out from the panel: fade the screen to black, then end the session.
pub fn log_out() {
    use gtk::gio;
    match gio::Application::default().and_downcast::<gtk::Application>() {
        Some(app) => to_black(&app, crate::login::terminate_session),
        None => crate::login::terminate_session(),
    }
}
