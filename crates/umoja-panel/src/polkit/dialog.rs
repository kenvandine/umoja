//! The password dialog: a card over a dimmed screen, like gnome-shell's
//! modal "Authentication Required" dialog, with the user's avatar, polkit's
//! message, and a password field. It takes the keyboard while it is up.

use super::{Request, ffi};
use adw::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::i18n;

pub struct Dialog {
    window: gtk::Window,
    avatar: adw::Avatar,
    name: gtk::Label,
    entry: gtk::Entry,
    message: gtk::Label,
    authenticate: gtk::Button,
    spinner: gtk::Spinner,
    cookie: String,
    uids: Vec<u32>,
    selected: Cell<usize>,
    session: RefCell<Option<ffi::Session>>,
    /// Bumped for every new session, so a replaced one's late signals are
    /// ignored.
    generation: Cell<u32>,
    /// PAM is waiting for the text in the entry.
    awaiting: Cell<bool>,
    /// The last session ended without success; Authenticate starts another.
    needs_restart: Cell<bool>,
    done: Cell<bool>,
    on_done: Box<dyn Fn(bool)>,
}

impl Dialog {
    pub fn new(req: &Request, on_done: impl Fn(bool) + 'static) -> Rc<Self> {
        let window = gtk::Window::builder()
            .css_classes(["umoja-polkit"])
            .decorated(false)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-polkit");
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
            .width_request(400)
            .css_classes(["polkit-card"])
            .build();
        card.append(
            &gtk::Label::builder()
                .label(i18n::shell("Authentication Required"))
                .css_classes(["polkit-title"])
                .build(),
        );
        card.append(
            &gtk::Label::builder()
                .label(&req.message)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .max_width_chars(40)
                .tooltip_text(&req.action_id)
                .css_classes(["polkit-body"])
                .build(),
        );

        let avatar = adw::Avatar::new(64, None, true);
        avatar.set_margin_top(12);
        card.append(&avatar);
        let name = gtk::Label::builder().css_classes(["polkit-user"]).build();
        let names: Vec<String> = req.uids.iter().map(|&u| user_info(u).1).collect();
        // Several administrators: pick who types their password.
        let users = (names.len() > 1).then(|| {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let users = gtk::DropDown::from_strings(&refs);
            users.set_halign(gtk::Align::Center);
            card.append(&users);
            users
        });
        if users.is_none() {
            card.append(&name);
        }

        let entry = gtk::Entry::builder()
            .visibility(false)
            .input_purpose(gtk::InputPurpose::Password)
            .placeholder_text(i18n::shell("Password"))
            .sensitive(false)
            .css_classes(["polkit-entry"])
            .build();
        entry.set_margin_top(12);
        card.append(&entry);
        let message = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["polkit-message"])
            .build();
        card.append(&message);

        let buttons = gtk::Box::builder()
            .spacing(12)
            .homogeneous(true)
            .margin_top(12)
            .build();
        let cancel = gtk::Button::builder()
            .label(i18n::shell("Cancel"))
            .css_classes(["pill"])
            .build();
        let authenticate = gtk::Button::builder()
            .css_classes(["pill", "suggested-action"])
            .sensitive(false)
            .build();
        let auth_label = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        auth_label.append(&spinner);
        auth_label.append(&gtk::Label::new(Some(&i18n::shell("Authenticate"))));
        authenticate.set_child(Some(&auth_label));
        buttons.append(&cancel);
        buttons.append(&authenticate);
        card.append(&buttons);

        let backdrop = gtk::Box::builder().css_classes(["polkit-backdrop"]).build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&backdrop));
        overlay.add_overlay(&card);
        window.set_child(Some(&overlay));

        let dialog = Rc::new(Self {
            window,
            avatar,
            name,
            entry,
            message,
            authenticate,
            spinner,
            cookie: req.cookie.clone(),
            uids: req.uids.clone(),
            selected: Cell::new(0),
            session: RefCell::new(None),
            generation: Cell::new(0),
            awaiting: Cell::new(false),
            needs_restart: Cell::new(false),
            done: Cell::new(false),
            on_done: Box::new(on_done),
        });
        let weak = Rc::downgrade(&dialog);
        if let Some(users) = users {
            let weak = weak.clone();
            users.connect_selected_notify(move |d| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.select(d.selected() as usize);
                }
            });
        }
        cancel.connect_clicked({
            let weak = weak.clone();
            move |_| {
                if let Some(d) = weak.upgrade() {
                    d.cancel();
                }
            }
        });
        let submit = {
            let weak = weak.clone();
            move || {
                if let Some(d) = weak.upgrade() {
                    d.submit();
                }
            }
        };
        dialog.authenticate.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        dialog.entry.connect_activate(move |_| submit());
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let weak = weak.clone();
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if let Some(d) = weak.upgrade() {
                        d.cancel();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        dialog.window.add_controller(keys);

        dialog.show_user();
        dialog.window.present();
        dialog.start_session();
        dialog
    }

    /// Close without replying; the agent does that.
    pub fn close(&self) {
        self.done.set(true);
        self.end_session();
        self.window.destroy();
    }

    /// The user or polkitd gave up.
    pub fn cancel(&self) {
        if self.done.replace(true) {
            return;
        }
        self.end_session();
        (self.on_done)(false);
    }

    fn end_session(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(s) = self.session.borrow_mut().take() {
            s.cancel();
        }
    }

    fn uid(&self) -> u32 {
        self.uids[self.selected.get().min(self.uids.len() - 1)]
    }

    fn select(self: &Rc<Self>, i: usize) {
        if i == self.selected.get() || i >= self.uids.len() {
            return;
        }
        self.selected.set(i);
        self.message.set_label("");
        self.show_user();
        self.start_session();
    }

    fn show_user(&self) {
        let (login, name) = user_info(self.uid());
        self.name.set_label(&name);
        self.avatar.set_text(Some(&name));
        let icon = format!("/var/lib/AccountsService/icons/{login}");
        self.avatar
            .set_custom_image(gdk::Texture::from_filename(&icon).ok().as_ref());
    }

    fn start_session(self: &Rc<Self>) {
        self.end_session();
        self.awaiting.set(false);
        self.needs_restart.set(false);
        self.set_busy(true);
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        let session = ffi::Session::new(self.uid(), &self.cookie, move |event| {
            let Some(d) = weak.upgrade() else { return };
            if d.done.get() || d.generation.get() != generation {
                return;
            }
            d.on_event(event);
        });
        match session {
            Some(s) => {
                *self.session.borrow_mut() = Some(s);
                if let Some(s) = self.session.borrow().as_ref() {
                    s.initiate();
                }
            }
            None => self.failed(&i18n::shell("Sorry, that didn’t work. Please try again.")),
        }
    }

    fn on_event(self: &Rc<Self>, event: ffi::Event) {
        match event {
            ffi::Event::Request(prompt, echo) => {
                self.awaiting.set(true);
                self.set_busy(false);
                self.entry.set_visibility(echo);
                let p = prompt.trim().trim_end_matches(':').trim();
                let placeholder = if p.is_empty() || p.eq_ignore_ascii_case("password") {
                    i18n::shell("Password")
                } else {
                    p.to_string()
                };
                self.entry.set_placeholder_text(Some(&placeholder));
                self.entry.grab_focus();
            }
            ffi::Event::Error(text) | ffi::Event::Info(text) => {
                self.message.set_label(text.trim());
            }
            ffi::Event::Completed(true) => {
                self.done.set(true);
                // Out of the session's signal emission before it's dropped.
                let d = self.clone();
                glib::idle_add_local_once(move || (d.on_done)(true));
            }
            ffi::Event::Completed(false) => {
                // Wrong password: say so and ask again, as gnome-shell does.
                let d = self.clone();
                glib::idle_add_local_once(move || {
                    if d.done.get() {
                        return;
                    }
                    d.message
                        .set_label(&i18n::shell("Sorry, that didn’t work. Please try again."));
                    d.start_session();
                });
            }
        }
    }

    /// The session couldn't start; let the user try again by hand.
    fn failed(&self, text: &str) {
        self.message.set_label(text);
        self.needs_restart.set(true);
        self.set_busy(false);
        self.entry.set_sensitive(false);
        self.authenticate.set_sensitive(true);
    }

    fn submit(self: &Rc<Self>) {
        if self.done.get() {
            return;
        }
        if self.needs_restart.get() {
            self.start_session();
            return;
        }
        if !self.awaiting.replace(false) {
            return;
        }
        let text = self.entry.text().to_string();
        self.entry.set_text("");
        self.message.set_label("");
        self.set_busy(true);
        if let Some(s) = self.session.borrow().as_ref() {
            s.respond(&text);
        }
    }

    fn set_busy(&self, busy: bool) {
        self.entry.set_sensitive(!busy);
        self.authenticate.set_sensitive(!busy);
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        if !busy {
            self.entry.grab_focus();
        }
    }
}

/// Login name and the name to show (the real name if there is one).
fn user_info(uid: u32) -> (String, String) {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return (uid.to_string(), uid.to_string());
    }
    let cstr = |p: *const libc::c_char| {
        if p.is_null() {
            String::new()
        } else {
            unsafe { std::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let login = cstr(pwd.pw_name);
    let gecos = cstr(pwd.pw_gecos);
    let real = gecos.split(',').next().unwrap_or("").trim().to_string();
    let shown = if real.is_empty() { login.clone() } else { real };
    (login, shown)
}
