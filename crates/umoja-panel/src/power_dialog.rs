//! The Power Off and Log Out confirmations, like gnome-shell's end-session
//! dialog: a card over a dimmed screen that acts by itself after 60
//! seconds, with Cancel, Restart and Power Off, or Cancel and Log Out.

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

/// What the dialog ends.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    PowerOff,
    LogOut,
}

/// What a button (or the countdown running out) does.
#[derive(Clone, Copy)]
enum Action {
    /// A logind Manager method: "PowerOff" or "Reboot".
    Logind(&'static str),
    LogOut,
}

impl Action {
    fn perform(self) {
        match self {
            Action::Logind(method) => {
                tracing::info!(method, "end session confirmed");
                crate::login::manager_call(method);
            }
            Action::LogOut => {
                tracing::info!("log out confirmed");
                crate::fade::log_out();
            }
        }
    }
}

// What gnome-session does: ask logind directly over D-Bus.
const POWER_OFF: Action = Action::Logind("PowerOff");
const REBOOT: Action = Action::Logind("Reboot");

/// The user's full name, when they have one (gnome-shell names them in the
/// Log Out countdown).
fn real_name() -> Option<String> {
    let name = glib::real_name().to_string_lossy().trim().to_string();
    (!name.is_empty() && name != "Unknown").then_some(name)
}

fn countdown_text(kind: Kind, left: u32) -> String {
    let text = match kind {
        Kind::PowerOff => i18n::shell_n(
            "The system will power off automatically in %d second.",
            "The system will power off automatically in %d seconds.",
            left,
        ),
        Kind::LogOut => match real_name() {
            Some(name) => i18n::shell_n(
                "%s will be logged out automatically in %d second.",
                "%s will be logged out automatically in %d seconds.",
                left,
            )
            .replacen("%s", &name, 1),
            None => i18n::shell_n(
                "You will be logged out automatically in %d second.",
                "You will be logged out automatically in %d seconds.",
                left,
            ),
        },
    };
    text.replace("%d", &left.to_string())
}

/// Shows the Power Off dialog (or brings the open one back to the front).
pub fn open() {
    show(Kind::PowerOff);
}

/// Shows the Log Out dialog (or brings the open one back to the front).
pub fn open_log_out() {
    show(Kind::LogOut);
}

fn show(kind: Kind) {
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
        .width_request(420)
        .css_classes(["end-session-card"])
        .build();
    card.append(
        &gtk::Label::builder()
            .label(match kind {
                Kind::PowerOff => i18n::shell("Power Off"),
                Kind::LogOut => i18n::shell("Log Out"),
            })
            .css_classes(["end-session-title"])
            .build(),
    );
    let body = gtk::Label::builder()
        .label(countdown_text(kind, SECONDS))
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
    buttons.append(&cancel);
    // The other buttons, the last one being the default (also what the
    // countdown does).
    let choices: &[(String, Action)] = &match kind {
        Kind::PowerOff => vec![
            (i18n::shell("Restart"), REBOOT),
            (i18n::shell("Power Off"), POWER_OFF),
        ],
        Kind::LogOut => vec![(i18n::shell("Log Out"), Action::LogOut)],
    };
    let default_action = choices.last().map(|(_, a)| *a).unwrap_or(POWER_OFF);
    let choice_buttons: Vec<(gtk::Button, Action)> = choices
        .iter()
        .enumerate()
        .map(|(i, (label, action))| {
            let last = i + 1 == choices.len();
            let b = button(label, if last { &["suggested-action"] } else { &[] });
            buttons.append(&b);
            (b, *action)
        })
        .collect();
    let default_button = choice_buttons.last().map(|(b, _)| b.clone());
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
        move |action: Option<Action>| {
            if done.replace(true) {
                return;
            }
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
            OPEN.with(|o| o.borrow_mut().take());
            window.destroy();
            match action {
                Some(action) => action.perform(),
                None => tracing::info!(?kind, "end session cancelled"),
            }
        }
    };
    let finish = Rc::new(finish);
    cancel.connect_clicked({
        let f = finish.clone();
        move |_| f(None)
    });
    for (b, action) in choice_buttons {
        let f = finish.clone();
        b.connect_clicked(move |_| f(Some(action)));
    }
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
                f(Some(default_action));
                return glib::ControlFlow::Break;
            }
            body.set_label(&countdown_text(kind, n));
            glib::ControlFlow::Continue
        }
    });
    *timer.borrow_mut() = Some(id);

    OPEN.with(|o| *o.borrow_mut() = Some(window.clone()));
    window.present();
    if let Some(b) = default_button {
        b.grab_focus();
    }
}
