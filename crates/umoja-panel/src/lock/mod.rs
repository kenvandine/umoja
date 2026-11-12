//! The lock screen, in-session like gnome-shell's: an `ext-session-lock-v1`
//! surface on every monitor showing the blurred wallpaper and a large
//! clock. A key press or click brings up the unlock prompt, which checks
//! the password with PAM. Users with enrolled fingers can also touch the
//! fingerprint reader, on the clock or at the prompt (see [`fingerprint`]).
//!
//! Triggers: the Quick Settings Lock button, logind's Lock signal
//! (`loginctl lock-session`), and suspend when
//! `org.gnome.desktop.screensaver lock-enabled` /
//! `ubuntu-lock-on-suspend` say so. If the panel restarts while logind's
//! LockedHint is set, it locks again straight away.

mod backdrop;
mod fingerprint;
mod pam;

use crate::login;
use adw::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use gtk4_session_lock as session_lock;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use umoja_common::{gsettings as gs, i18n};

const SCREENSAVER: &str = "org.gnome.desktop.screensaver";

#[derive(Default)]
pub struct Locker {
    instance: RefCell<Option<session_lock::Instance>>,
    windows: RefCell<Vec<(gdk::Monitor, gtk::Window)>>,
    monitors_handler: RefCell<Option<glib::SignalHandlerId>>,
    /// The unlock prompt's widgets, on the first monitor's lock surface.
    ui: RefCell<Option<PromptUi>>,
    fingerprint: RefCell<Option<Rc<fingerprint::Fingerprint>>>,
}

#[derive(Clone)]
struct PromptUi {
    stack: gtk::Stack,
    entry: gtk::PasswordEntry,
    message: gtk::Label,
    hint: gtk::Label,
}

thread_local! {
    static LOCKER: Rc<Locker> = Rc::new(Locker::default());
}

pub fn locker() -> Rc<Locker> {
    LOCKER.with(Rc::clone)
}

/// Hook up logind and relock after a panel restart.
pub fn init() {
    login::watch(|signal, params| match signal {
        "Lock" => locker().lock(),
        "Unlock" => locker().unlock(),
        "PrepareForSleep" => {
            let going_down = params.try_child_value(0).and_then(|v| v.get::<bool>()) == Some(true);
            let wanted = gs::get_bool(SCREENSAVER, "lock-enabled", true)
                && gs::get_bool(SCREENSAVER, "ubuntu-lock-on-suspend", true);
            if going_down && wanted {
                locker().lock();
            }
            // The reader may have been reset over suspend.
            if !going_down && let Some(fp) = locker().fingerprint.borrow().clone() {
                fp.reset();
            }
        }
        _ => {}
    });
    if let Some(conn) = crate::bus::system_bus()
        && let Some(session) = login::session_path(&conn)
        && login::locked_hint(&conn, &session)
    {
        tracing::info!("session was locked when the panel started; locking again");
        locker().lock();
    }
}

impl Locker {
    pub fn is_locked(&self) -> bool {
        self.instance.borrow().is_some()
    }

    pub fn lock(self: &Rc<Self>) {
        if self.is_locked() {
            return;
        }
        if !session_lock::is_supported() {
            tracing::warn!("compositor has no ext-session-lock-v1; cannot lock");
            return;
        }
        let instance = session_lock::Instance::new();
        instance.connect_locked(|_| {
            tracing::info!("screen locked");
            login::set_locked_hint(true);
        });
        instance.connect_failed({
            let weak = Rc::downgrade(self);
            move |_| {
                tracing::warn!("could not lock the screen");
                if let Some(l) = weak.upgrade() {
                    l.teardown();
                }
            }
        });
        if !instance.lock() {
            tracing::warn!("lock request refused");
            return;
        }
        *self.instance.borrow_mut() = Some(instance);
        self.cover_monitors();
        self.start_fingerprint();

        // Monitors plugged in while locked get a lock surface too.
        if let Some(display) = gdk::Display::default() {
            let weak = Rc::downgrade(self);
            let id = display.monitors().connect_items_changed(move |_, _, _, _| {
                if let Some(l) = weak.upgrade() {
                    l.cover_monitors();
                }
            });
            *self.monitors_handler.borrow_mut() = Some(id);
        }
    }

    fn cover_monitors(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let instance = self.instance.borrow();
        let Some(instance) = instance.as_ref() else {
            return;
        };
        let monitors = display.monitors();
        let mut windows = self.windows.borrow_mut();
        windows.retain(|(m, w)| {
            let alive = m.is_valid();
            if !alive {
                w.destroy();
            }
            alive
        });
        for i in 0..monitors.n_items() {
            let Some(monitor) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
                continue;
            };
            if windows.iter().any(|(m, _)| *m == monitor) {
                continue;
            }
            // The unlock prompt goes on the first monitor, like gnome-shell's
            // primary; the others just show the curtain.
            let (window, ui) = lock_window(windows.is_empty());
            if ui.is_some() {
                *self.ui.borrow_mut() = ui;
            }
            instance.assign_window_to_monitor(&window, &monitor);
            window.present();
            windows.push((monitor, window));
        }
    }

    fn start_fingerprint(&self) {
        let fp = fingerprint::Fingerprint::new(fingerprint::Callbacks {
            hint: Box::new(|text| {
                if let Some(ui) = locker().ui.borrow().clone() {
                    ui.hint.set_label(text.as_deref().unwrap_or(""));
                    ui.hint.set_visible(text.is_some());
                }
            }),
            error: Box::new(|text| {
                if let Some(ui) = locker().ui.borrow().clone() {
                    // Show it even if the touch came while on the clock.
                    if ui.stack.visible_child_name().as_deref() != Some("prompt") {
                        ui.stack.set_visible_child_name("prompt");
                        ui.entry.grab_focus();
                    }
                    ui.message.set_label(&text);
                }
            }),
            matched: Box::new(|| locker().unlock()),
        });
        fp.start();
        *self.fingerprint.borrow_mut() = Some(fp);
    }

    /// The curtain was lifted: let the reader listen again if it gave up.
    fn rearm_fingerprint(&self) {
        if let Some(fp) = self.fingerprint.borrow().clone() {
            fp.rearm();
        }
    }

    pub fn unlock(&self) {
        let Some(instance) = self.instance.borrow_mut().take() else {
            return;
        };
        instance.unlock();
        self.teardown();
        login::set_locked_hint(false);
        tracing::info!("screen unlocked");
    }

    fn teardown(&self) {
        self.instance.borrow_mut().take();
        if let Some(fp) = self.fingerprint.borrow_mut().take() {
            fp.stop();
        }
        self.ui.borrow_mut().take();
        if let (Some(id), Some(display)) = (
            self.monitors_handler.borrow_mut().take(),
            gdk::Display::default(),
        ) {
            display.monitors().disconnect(id);
        }
        for (_, w) in self.windows.borrow_mut().drain(..) {
            w.destroy();
        }
    }
}

fn lock_window(with_prompt: bool) -> (gtk::Window, Option<PromptUi>) {
    let window = gtk::Window::builder().css_classes(["umoja-lock"]).build();
    let overlay = gtk::Overlay::new();
    let backdrop = backdrop::Backdrop::new(36.0);
    let wallpaper =
        gs::wallpaper_path(gs::prefers_dark()).map(|p| p.to_string_lossy().into_owned());
    backdrop.set_wallpaper(wallpaper.as_deref());
    overlay.set_child(Some(&backdrop));

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideUp)
        .transition_duration(250)
        .build();
    stack.add_named(&clock(), Some("clock"));
    overlay.add_overlay(&stack);
    window.set_child(Some(&overlay));

    if !with_prompt {
        return (window, None);
    }
    let (prompt, entry, message, hint) = prompt();
    stack.add_named(&prompt, Some("prompt"));
    let ui = PromptUi {
        stack: stack.clone(),
        entry: entry.clone(),
        message,
        hint,
    };

    // Any key or click lifts the curtain; the key that did it is typed
    // into the password field rather than lost.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let (stack, entry) = (stack.clone(), entry.clone());
        move |_, key, _, _| {
            if stack.visible_child_name().as_deref() == Some("prompt") {
                if key == gdk::Key::Escape {
                    entry.set_text("");
                    stack.set_visible_child_name("clock");
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }
            stack.set_visible_child_name("prompt");
            entry.grab_focus();
            locker().rearm_fingerprint();
            if let Some(c) = key.to_unicode().filter(|c| !c.is_control()) {
                entry.set_text(&c.to_string());
                entry.set_position(-1);
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);
    let click = gtk::GestureClick::new();
    click.connect_pressed({
        let (stack, entry) = (stack.clone(), entry.clone());
        move |_, _, _, _| {
            if stack.visible_child_name().as_deref() == Some("clock") {
                stack.set_visible_child_name("prompt");
                entry.grab_focus();
                locker().rearm_fingerprint();
            }
        }
    });
    window.add_controller(click);
    (window, Some(ui))
}

/// Big time and date, gnome-shell's lock screen "curtain".
fn clock() -> gtk::Box {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["lock-clock"])
        .build();
    let time = gtk::Label::builder().css_classes(["lock-time"]).build();
    let date = gtk::Label::builder().css_classes(["lock-date"]).build();
    b.append(&time);
    b.append(&date);
    let hint = gtk::Label::builder()
        .label(i18n::shell("Click or press a key to unlock"))
        .css_classes(["lock-hint"])
        .build();
    b.append(&hint);

    let update = move || {
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        let twelve = umoja_common::clock::twelve_hour();
        let t = now.format(if twelve { "%l:%M" } else { "%H:%M" });
        if let Ok(t) = t {
            time.set_label(t.trim());
        }
        if let Ok(d) = now.format("%A %B %-e") {
            date.set_label(&d);
        }
    };
    update();
    let weak = b.downgrade();
    glib::timeout_add_seconds_local(1, move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        update();
        glib::ControlFlow::Continue
    });
    b
}

/// Avatar, name, password field, fingerprint hint and message line.
fn prompt() -> (gtk::Box, gtk::PasswordEntry, gtk::Label, gtk::Label) {
    let user = glib::user_name().to_string_lossy().into_owned();
    let real = glib::real_name().to_string_lossy().into_owned();
    let name = if real.is_empty() || real == "Unknown" {
        user.clone()
    } else {
        real
    };

    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .width_request(320)
        .css_classes(["lock-prompt"])
        .build();
    let avatar = adw::Avatar::new(128, Some(&name), true);
    let icon = format!("/var/lib/AccountsService/icons/{user}");
    if let Ok(tex) = gdk::Texture::from_filename(&icon) {
        avatar.set_custom_image(Some(&tex));
    }
    b.append(&avatar);
    b.append(
        &gtk::Label::builder()
            .label(&name)
            .css_classes(["lock-name"])
            .build(),
    );
    let entry = gtk::PasswordEntry::builder()
        .placeholder_text(i18n::shell("Password"))
        .show_peek_icon(true)
        .css_classes(["lock-entry"])
        .build();
    b.append(&entry);
    // "(or place finger on reader)" while the fingerprint reader listens.
    let hint = gtk::Label::builder()
        .css_classes(["lock-fingerprint-hint"])
        .wrap(true)
        .visible(false)
        .build();
    b.append(&hint);
    let message = gtk::Label::builder()
        .css_classes(["lock-message"])
        .wrap(true)
        .build();
    b.append(&message);
    let message_out = message.clone();

    entry.connect_activate(move |e| {
        let password = e.text().to_string();
        if password.is_empty() {
            return;
        }
        e.set_sensitive(false);
        message.set_label("");
        let (e, message, user) = (e.clone(), message.clone(), user.clone());
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || pam::authenticate(&user, &password))
                .await
                .unwrap_or_else(|_| Err("authentication thread failed".into()));
            e.set_sensitive(true);
            e.set_text("");
            match result {
                Ok(()) => locker().unlock(),
                Err(err) => {
                    tracing::info!(error = err, "unlock failed");
                    message.set_label(&i18n::shell("Sorry, that didn’t work. Please try again."));
                    e.grab_focus();
                }
            }
        });
    });
    (b, entry, message_out, hint)
}
