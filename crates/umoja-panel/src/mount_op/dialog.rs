//! The mount dialogs, gnome-shell's ShellMountPasswordDialog,
//! ShellMountQuestionDialog and ShellProcessesDialog, drawn as the same
//! blurred dark card as our PolicyKit dialog (it reuses its styles).
//!
//! One window serves a whole mount: when gvfs asks again for the same
//! object (a wrong passphrase, the list of busy apps changing) the card is
//! refilled instead of a new one popping up.

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::i18n;

const REMEMBER_SCHEMA: &str = "org.gnome.shell";
const REMEMBER_KEY: &str = "remember-mount-password";

/// An app keeping a volume busy.
#[derive(Clone)]
pub struct App {
    pub name: String,
    pub icon: Option<gio::Icon>,
}

pub enum Kind {
    Password {
        flags: gio::AskPasswordFlags,
        user: String,
        domain: String,
    },
    Question {
        choices: Vec<String>,
    },
    Processes {
        apps: Vec<App>,
        choices: Vec<String>,
    },
}

#[derive(Default, Clone, Debug)]
pub struct Password {
    pub password: String,
    pub username: String,
    pub domain: String,
    pub anonymous: bool,
    pub remember: bool,
    pub hidden_volume: bool,
    pub system_volume: bool,
    pub pim: u32,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Password(Password),
    Choice(i32),
    Abort,
}

type Callback = Rc<dyn Fn(Reply)>;

/// The password form's widgets, read when the user presses Unlock.
struct Form {
    password: gtk::PasswordEntry,
    user: Option<gtk::Entry>,
    domain: Option<gtk::Entry>,
    anonymous: Option<gtk::CheckButton>,
    remember: Option<gtk::CheckButton>,
    hidden: Option<gtk::CheckButton>,
    system: Option<gtk::CheckButton>,
    keyfiles: Option<gtk::CheckButton>,
    pim: Option<gtk::Entry>,
    error: gtk::Label,
    unlock: gtk::Button,
    spinner: gtk::Spinner,
}

pub struct Dialog {
    window: gtk::Window,
    card: gtk::Box,
    form: RefCell<Option<Form>>,
    on_done: RefCell<Callback>,
    /// Waiting for an answer (false while the passphrase is being tried).
    asking: Cell<bool>,
    closed: Cell<bool>,
}

impl Dialog {
    pub fn new(
        message: &str,
        icon: &str,
        kind: Kind,
        on_done: impl Fn(Reply) + 'static,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .css_classes(["umoja-polkit", "umoja-mount-op"])
            .decorated(false)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-mount-op");
        crate::place_on_primary(&window);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        umoja_common::blur::blur_behind(&window);

        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .width_request(400)
            .css_classes(["polkit-card"])
            .build();
        let backdrop = gtk::Box::builder().css_classes(["polkit-backdrop"]).build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&backdrop));
        overlay.add_overlay(&card);
        window.set_child(Some(&overlay));

        let dialog = Rc::new(Self {
            window,
            card,
            form: RefCell::new(None),
            on_done: RefCell::new(Rc::new(on_done)),
            asking: Cell::new(true),
            closed: Cell::new(false),
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&dialog);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(d) = weak.upgrade() {
                    d.respond(Reply::Abort);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dialog.window.add_controller(keys);
        dialog.fill(message, icon, kind);
        dialog.window.present();
        dialog.focus();
        dialog
    }

    /// Ask again in the same card: a new question for the same mount.
    pub fn update(
        self: &Rc<Self>,
        message: &str,
        icon: &str,
        kind: Kind,
        on_done: impl Fn(Reply) + 'static,
    ) {
        if self.closed.get() {
            return;
        }
        *self.on_done.borrow_mut() = Rc::new(on_done);
        self.asking.set(true);
        self.fill(message, icon, kind);
        self.focus();
    }

    /// The passphrase was wrong: say so and ask again, keeping the
    /// options the user picked, as gnome-shell's reaskPassword does.
    pub fn reask(&self, on_done: impl Fn(Reply) + 'static) {
        if self.closed.get() {
            return;
        }
        *self.on_done.borrow_mut() = Rc::new(on_done);
        self.asking.set(true);
        if let Some(f) = self.form.borrow().as_ref() {
            f.password.set_text("");
            f.error
                .set_label(&i18n::shell("Sorry, that didn’t work. Please try again."));
            f.error.set_visible(true);
        }
        self.set_busy(false);
        self.focus();
    }

    /// Whether the card is asking for a password (not a question).
    pub fn is_password(&self) -> bool {
        self.form.borrow().is_some()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// Take the card down without answering.
    pub fn close(&self) {
        if !self.closed.replace(true) {
            self.window.destroy();
        }
    }

    fn respond(self: &Rc<Self>, reply: Reply) {
        // Escape while the passphrase is being tried still cancels.
        if self.closed.get() || (!self.asking.get() && !matches!(reply, Reply::Abort)) {
            return;
        }
        self.asking.set(false);
        let password = matches!(reply, Reply::Password(_));
        let cb = self.on_done.borrow().clone();
        if password {
            // Stay up while udisks tries it; the caller closes or reasks.
            self.set_busy(true);
        } else {
            self.close();
        }
        cb(reply);
    }

    fn set_busy(&self, busy: bool) {
        if let Some(f) = self.form.borrow().as_ref() {
            f.password.set_sensitive(!busy);
            f.unlock.set_sensitive(!busy);
            f.spinner.set_visible(busy);
            f.spinner.set_spinning(busy);
        }
    }

    fn focus(&self) {
        let form = self.form.borrow();
        let Some(f) = form.as_ref() else { return };
        match &f.user {
            Some(u) if u.text().is_empty() && u.is_sensitive() => {
                u.grab_focus();
            }
            _ => {
                f.password.grab_focus();
            }
        }
    }

    fn fill(self: &Rc<Self>, message: &str, icon: &str, kind: Kind) {
        while let Some(child) = self.card.first_child() {
            self.card.remove(&child);
        }
        self.form.borrow_mut().take();
        let card = &self.card;

        let (title, description) = split_message(message);
        let image = gtk::Image::builder().pixel_size(48).margin_top(4).build();
        match gio::Icon::for_string(icon)
            .ok()
            .filter(|_| !icon.is_empty())
        {
            Some(gicon) => image.set_from_gicon(&gicon),
            None => image.set_icon_name(Some(match kind {
                Kind::Password { .. } => "dialog-password-symbolic",
                _ => "drive-harddisk-symbolic",
            })),
        }
        card.append(&image);
        card.append(
            &gtk::Label::builder()
                .label(&title)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .max_width_chars(36)
                .css_classes(["polkit-title"])
                .build(),
        );
        if !description.is_empty() {
            card.append(
                &gtk::Label::builder()
                    .label(&description)
                    .wrap(true)
                    .justify(gtk::Justification::Center)
                    .max_width_chars(40)
                    .css_classes(["polkit-body"])
                    .build(),
            );
        }

        match kind {
            Kind::Password {
                flags,
                user,
                domain,
            } => self.fill_password(flags, &user, &domain),
            Kind::Question { choices } => self.fill_choices(&choices),
            Kind::Processes { apps, choices } => {
                if !apps.is_empty() {
                    card.append(&apps_list(&apps));
                }
                self.fill_choices(&choices);
            }
        }
    }

    fn fill_password(self: &Rc<Self>, flags: gio::AskPasswordFlags, user: &str, domain: &str) {
        let card = &self.card;
        let network = flags
            .intersects(gio::AskPasswordFlags::NEED_USERNAME | gio::AskPasswordFlags::NEED_DOMAIN);

        // A network share that allows guests: GTK's "Connect As" choice.
        let anonymous = flags
            .contains(gio::AskPasswordFlags::ANONYMOUS_SUPPORTED)
            .then(|| {
                let row = gtk::Box::builder()
                    .spacing(12)
                    .halign(gtk::Align::Center)
                    .build();
                let guest = gtk::CheckButton::with_label(&strip_mnemonic(&i18n::gtk("_Anonymous")));
                let registered =
                    gtk::CheckButton::with_label(&strip_mnemonic(&i18n::gtk("Registe_red User")));
                registered.set_group(Some(&guest));
                registered.set_active(true);
                row.append(&gtk::Label::new(Some(&i18n::gtk("Connect As"))));
                row.append(&guest);
                row.append(&registered);
                card.append(&row);
                guest
            });

        let field = |label: &str, value: &str| {
            let e = gtk::Entry::builder()
                .placeholder_text(strip_mnemonic(label))
                .text(value)
                .css_classes(["polkit-entry"])
                .build();
            card.append(&e);
            e
        };
        let user_entry = flags
            .contains(gio::AskPasswordFlags::NEED_USERNAME)
            .then(|| field(&i18n::gtk("_Username"), user));
        let domain_entry = flags
            .contains(gio::AskPasswordFlags::NEED_DOMAIN)
            .then(|| field(&i18n::gtk("_Domain"), domain));

        let password = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .placeholder_text(i18n::shell("Password"))
            .css_classes(["polkit-entry"])
            .build();
        card.append(&password);

        // VeraCrypt volumes: gnome-shell's extra options.
        let tcrypt = flags.contains(gio::AskPasswordFlags::TCRYPT);
        let check = |label: &str| {
            let c = gtk::CheckButton::with_label(label);
            card.append(&c);
            c
        };
        let pim = tcrypt.then(|| {
            let e = gtk::Entry::builder()
                .placeholder_text(i18n::shell("PIM Number"))
                .input_purpose(gtk::InputPurpose::Digits)
                .css_classes(["polkit-entry"])
                .build();
            card.append(&e);
            e
        });
        let hidden = tcrypt.then(|| check(&i18n::shell("Hidden Volume")));
        let system = tcrypt.then(|| check(&i18n::shell("Windows System Volume")));
        let keyfiles = tcrypt.then(|| check(&i18n::shell("Uses Keyfiles")));
        let keyfiles_note = gtk::Label::builder()
            .use_markup(true)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(40)
            .visible(false)
            .css_classes(["polkit-body"])
            .build();
        keyfiles_note.set_markup(
            &i18n::shell(
                "To unlock a volume that uses keyfiles, use the <i>%s</i> utility instead.",
            )
            .replace("%s", &glib::markup_escape_text(&disks_name())),
        );
        card.append(&keyfiles_note);

        let remember = flags
            .contains(gio::AskPasswordFlags::SAVING_SUPPORTED)
            .then(|| {
                let c = check(&i18n::shell("Remember Password"));
                c.set_active(umoja_common::gsettings::get_bool(
                    REMEMBER_SCHEMA,
                    REMEMBER_KEY,
                    false,
                ));
                c
            });

        let error = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .visible(false)
            .css_classes(["polkit-message"])
            .build();
        card.append(&error);

        let buttons = gtk::Box::builder()
            .spacing(12)
            .homogeneous(true)
            .margin_top(12)
            .build();
        let cancel = gtk::Button::builder()
            .label(i18n::shell("Cancel"))
            .css_classes(["pill"])
            .build();
        let unlock = gtk::Button::builder()
            .css_classes(["pill", "suggested-action"])
            .build();
        let unlock_box = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        unlock_box.append(&spinner);
        let unlock_label = gtk::Label::new(Some(&i18n::shell(if network {
            "Connect"
        } else {
            "Unlock"
        })));
        unlock_box.append(&unlock_label);
        unlock.set_child(Some(&unlock_box));
        buttons.append(&cancel);
        buttons.append(&unlock);
        card.append(&buttons);

        // Guests need no login.
        if let Some(guest) = &anonymous {
            let widgets: Vec<gtk::Widget> = [
                user_entry.as_ref().map(|e| e.clone().upcast()),
                domain_entry.as_ref().map(|e| e.clone().upcast()),
                Some(password.clone().upcast()),
            ]
            .into_iter()
            .flatten()
            .collect();
            guest.connect_toggled(move |g| {
                for w in &widgets {
                    w.set_sensitive(!g.is_active());
                }
            });
        }
        // Keyfiles can't be given here: point at Disks, like gnome-shell.
        if let Some(k) = &keyfiles {
            let hide: Vec<gtk::Widget> = [
                Some(password.clone().upcast::<gtk::Widget>()),
                pim.as_ref().map(|e| e.clone().upcast()),
                remember.as_ref().map(|e| e.clone().upcast()),
            ]
            .into_iter()
            .flatten()
            .collect();
            let (note, label) = (keyfiles_note.clone(), unlock_label.clone());
            k.connect_toggled(move |k| {
                let on = k.is_active();
                for w in &hide {
                    w.set_visible(!on);
                }
                note.set_visible(on);
                label.set_label(&if on {
                    disks_name()
                } else {
                    i18n::shell("Unlock")
                });
            });
        }

        *self.form.borrow_mut() = Some(Form {
            password: password.clone(),
            user: user_entry.clone(),
            domain: domain_entry.clone(),
            anonymous,
            remember,
            hidden,
            system,
            keyfiles,
            pim,
            error,
            unlock: unlock.clone(),
            spinner,
        });

        let weak = Rc::downgrade(self);
        let submit = move || {
            if let Some(d) = weak.upgrade() {
                d.submit();
            }
        };
        unlock.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        password.connect_activate({
            let submit = submit.clone();
            move |_| submit()
        });
        for e in [&user_entry, &domain_entry].into_iter().flatten() {
            let submit = submit.clone();
            e.connect_activate(move |_| submit());
        }
        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(d) = weak.upgrade() {
                d.respond(Reply::Abort);
            }
        });
    }

    fn submit(self: &Rc<Self>) {
        let answer = {
            let form = self.form.borrow();
            let Some(f) = form.as_ref() else { return };
            if f.keyfiles.as_ref().is_some_and(|k| k.is_active()) {
                drop(form);
                launch_disks();
                self.respond(Reply::Abort);
                return;
            }
            let pim_text = f
                .pim
                .as_ref()
                .map(|e| e.text().trim().to_string())
                .unwrap_or_default();
            let pim = if pim_text.is_empty() {
                0
            } else if let Ok(n) = pim_text.parse::<u32>() {
                n
            } else {
                f.error
                    .set_label(&i18n::shell("The PIM must be a number or empty."));
                f.error.set_visible(true);
                if let Some(e) = &f.pim {
                    e.set_text("");
                }
                return;
            };
            let remember = f.remember.as_ref().is_some_and(|c| c.is_active());
            if let Some(s) = umoja_common::gsettings::settings(REMEMBER_SCHEMA)
                && f.remember.is_some()
                && umoja_common::gsettings::has_key(&s, REMEMBER_KEY)
            {
                let _ = s.set_boolean(REMEMBER_KEY, remember);
            }
            f.error.set_visible(false);
            Password {
                password: f.password.text().to_string(),
                username: f
                    .user
                    .as_ref()
                    .map(|e| e.text().to_string())
                    .unwrap_or_default(),
                domain: f
                    .domain
                    .as_ref()
                    .map(|e| e.text().to_string())
                    .unwrap_or_default(),
                anonymous: f.anonymous.as_ref().is_some_and(|c| c.is_active()),
                remember,
                hidden_volume: f.hidden.as_ref().is_some_and(|c| c.is_active()),
                system_volume: f.system.as_ref().is_some_and(|c| c.is_active()),
                pim,
            }
        };
        self.respond(Reply::Password(answer));
    }

    /// One button per choice. gnome-shell puts the first choice (the
    /// action, e.g. "Unmount Anyway") rightmost, where the default goes.
    fn fill_choices(self: &Rc<Self>, choices: &[String]) {
        let buttons = gtk::Box::builder()
            .spacing(12)
            .homogeneous(true)
            .margin_top(12)
            .build();
        let mut first = None;
        for (i, label) in choices.iter().enumerate().rev() {
            let b = gtk::Button::builder()
                .label(label)
                .css_classes(["pill"])
                .build();
            if i == 0 {
                b.add_css_class("suggested-action");
                first = Some(b.clone());
            }
            let weak = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.respond(Reply::Choice(i as i32));
                }
            });
            buttons.append(&b);
        }
        if choices.is_empty() {
            let b = gtk::Button::builder()
                .label(i18n::shell("Cancel"))
                .css_classes(["pill"])
                .build();
            let weak = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.respond(Reply::Abort);
                }
            });
            buttons.append(&b);
        }
        self.card.append(&buttons);
        if let Some(b) = first {
            b.grab_focus();
        }
    }
}

/// gnome-shell's _setLabelsForMessage: the first line is the title, the
/// rest the description.
fn split_message(message: &str) -> (String, String) {
    let mut lines = message.splitn(2, '\n');
    let title = lines.next().unwrap_or("").trim().to_string();
    let rest = lines.next().unwrap_or("").trim().to_string();
    (title, rest)
}

fn strip_mnemonic(s: &str) -> String {
    s.replace('_', "")
}

fn apps_list(apps: &[App]) -> gtk::Widget {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(4)
        .halign(gtk::Align::Center)
        .build();
    for app in apps {
        let row = gtk::Box::builder().spacing(10).build();
        let image = gtk::Image::builder().pixel_size(32).build();
        match &app.icon {
            Some(i) => image.set_from_gicon(i),
            None => image.set_icon_name(Some("application-x-executable")),
        }
        row.append(&image);
        row.append(
            &gtk::Label::builder()
                .label(&app.name)
                .xalign(0.0)
                .css_classes(["polkit-user"])
                .build(),
        );
        list.append(&row);
    }
    if apps.len() <= 5 {
        return list.upcast();
    }
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(200)
        .max_content_height(200)
        .child(&list)
        .build()
        .upcast()
}

const DISKS: &str = "org.gnome.DiskUtility.desktop";

fn disks_name() -> String {
    gio::DesktopAppInfo::new(DISKS)
        .map(|a| a.name().to_string())
        .unwrap_or_else(|| "Disks".into())
}

fn launch_disks() {
    if let Some(app) = gio::DesktopAppInfo::new(DISKS) {
        crate::launch::launch_app_info(app.upcast_ref(), &[]);
    }
}
