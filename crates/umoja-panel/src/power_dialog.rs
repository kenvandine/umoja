//! The Power Off confirmation, like gnome-shell's end-session dialog: a
//! card over a dimmed screen that powers off by itself after 60 seconds,
//! with Cancel, Restart and Power Off.

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::i18n;

/// gnome-shell's endSessionDialog waits this long before acting.
const SECONDS: u32 = 60;

thread_local! {
    /// The dialog on screen, so a second click doesn't stack another.
    static OPEN: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

fn countdown_text(left: u32) -> String {
    i18n::shell_n(
        "The system will power off automatically in %d second.",
        "The system will power off automatically in %d seconds.",
        left,
    )
    .replace("%d", &left.to_string())
}

/// Shows the dialog (or brings the open one back to the front).
pub fn open() {
    if let Some(w) = OPEN.with(|o| o.borrow().clone()) {
        w.present();
        return;
    }
    let window = gtk::Window::builder()
        .css_classes(["umoja-end-session"])
        .decorated(false)
        .build();
    if let Some(app) = gtk::gio::Application::default().and_downcast::<gtk::Application>() {
        window.set_application(Some(&app));
    }
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace("umoja-end-session");
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::Exclusive);

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .width_request(420)
        .css_classes(["end-session-card"])
        .build();
    card.append(
        &gtk::Label::builder()
            .label(i18n::shell("Power Off"))
            .css_classes(["end-session-title"])
            .build(),
    );
    let body = gtk::Label::builder()
        .label(countdown_text(SECONDS))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(40)
        .css_classes(["end-session-body"])
        .build();
    card.append(&body);

    let buttons = gtk::Box::builder()
        .spacing(12)
        .homogeneous(true)
        .margin_top(12)
        .build();
    let button = |label: &str, classes: &[&str]| {
        gtk::Button::builder()
            .label(label)
            .css_classes(classes.iter().copied().chain(["pill"]).collect::<Vec<_>>())
            .build()
    };
    let cancel = button(&i18n::shell("Cancel"), &[]);
    let restart = button(&i18n::shell("Restart"), &[]);
    let power_off = button(&i18n::shell("Power Off"), &["suggested-action"]);
    buttons.append(&cancel);
    buttons.append(&restart);
    buttons.append(&power_off);
    card.append(&buttons);

    let backdrop = gtk::Box::builder()
        .css_classes(["end-session-backdrop"])
        .build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&backdrop));
    overlay.add_overlay(&card);
    window.set_child(Some(&overlay));

    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let done = Rc::new(Cell::new(false));
    // Closes the dialog once, then runs the chosen action.
    let finish = {
        let (window, timer, done) = (window.clone(), timer.clone(), done.clone());
        move |argv: Option<&'static [&'static str]>| {
            if done.replace(true) {
                return;
            }
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
            OPEN.with(|o| o.borrow_mut().take());
            window.destroy();
            match argv {
                Some(argv) => {
                    tracing::info!(cmd = ?argv, "end session confirmed");
                    crate::quick_settings::run(argv);
                }
                None => tracing::info!("power off cancelled"),
            }
        }
    };
    let finish = Rc::new(finish);
    const POWER_OFF: &[&str] = &["systemctl", "poweroff"];
    const REBOOT: &[&str] = &["systemctl", "reboot"];

    cancel.connect_clicked({
        let f = finish.clone();
        move |_| f(None)
    });
    restart.connect_clicked({
        let f = finish.clone();
        move |_| f(Some(REBOOT))
    });
    power_off.connect_clicked({
        let f = finish.clone();
        move |_| f(Some(POWER_OFF))
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let f = finish.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                f(None);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);

    let left = Cell::new(SECONDS);
    let id = glib::timeout_add_seconds_local(1, {
        let (f, timer) = (finish.clone(), timer.clone());
        move || {
            let n = left.get().saturating_sub(1);
            left.set(n);
            if n == 0 {
                // The source ends by returning Break; don't remove it twice.
                timer.borrow_mut().take();
                f(Some(POWER_OFF));
                return glib::ControlFlow::Break;
            }
            body.set_label(&countdown_text(n));
            glib::ControlFlow::Continue
        }
    });
    *timer.borrow_mut() = Some(id);

    OPEN.with(|o| *o.borrow_mut() = Some(window.clone()));
    window.present();
    power_off.grab_focus();
}
