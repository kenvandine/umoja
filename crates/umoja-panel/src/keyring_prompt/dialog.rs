//! The keyring dialog: gnome-shell's KeyringDialog (keyringPrompt.js) in
//! our blurred dark card. One dialog serves every prompt of one caller:
//! gnome-keyring asks again on the same dialog when the password was
//! wrong, with a warning, and in between the dialog waits, insensitive.

use super::Props;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::i18n;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Password,
    Confirm,
}

/// What the user answered.
pub enum Reply {
    /// The password typed (password prompts) or Continue (confirm, `None`).
    Continue(Option<String>),
    Cancel,
}

pub struct Dialog {
    window: gtk::Window,
    title: gtk::Label,
    description: gtk::Label,
    entry: gtk::PasswordEntry,
    confirm: gtk::PasswordEntry,
    warning: gtk::Label,
    choice: gtk::CheckButton,
    cancel: gtk::Button,
    ok: gtk::Button,
    ok_label: gtk::Label,
    spinner: gtk::Spinner,
    /// The prompt now on screen, `None` while waiting for the next one.
    kind: Cell<Option<Kind>>,
    password_new: Cell<bool>,
    /// Strength of the last new password submitted.
    strength: Cell<i32>,
    closed: Cell<bool>,
    on_reply: RefCell<Box<dyn Fn(Reply)>>,
}

fn label(classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(40)
        .css_classes(classes)
        .build()
}

fn entry(placeholder: &str) -> gtk::PasswordEntry {
    gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text(placeholder)
        .css_classes(["polkit-entry"])
        .build()
}

impl Dialog {
    pub fn new(on_reply: impl Fn(Reply) + 'static) -> Rc<Self> {
        let crate::modal::Modal { window, card } =
            crate::modal::new("umoja-keyring", Some("umoja-keyring"));
        card.append(
            &gtk::Image::builder()
                .icon_name("dialog-password-symbolic")
                .pixel_size(48)
                .margin_top(4)
                .build(),
        );
        let title = label(&["polkit-title"]);
        card.append(&title);
        let description = label(&["polkit-body"]);
        card.append(&description);

        let entry = entry(&i18n::shell("Password"));
        entry.set_margin_top(4);
        card.append(&entry);
        let confirm = entry_confirm();
        card.append(&confirm);
        let warning = label(&["polkit-message"]);
        card.append(&warning);
        let choice = gtk::CheckButton::builder()
            .halign(gtk::Align::Center)
            .css_classes(["keyring-choice"])
            .build();
        card.append(&choice);

        let cancel = gtk::Button::builder().css_classes(["pill"]).build();
        let ok = gtk::Button::builder()
            .css_classes(["pill", "suggested-action"])
            .build();
        let ok_box = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let ok_label = gtk::Label::new(None);
        ok_box.append(&spinner);
        ok_box.append(&ok_label);
        ok.set_child(Some(&ok_box));
        card.append(&crate::modal::button_row(&cancel, &ok));

        let dialog = Rc::new(Self {
            window,
            title,
            description,
            entry,
            confirm,
            warning,
            choice,
            cancel,
            ok,
            ok_label,
            spinner,
            kind: Cell::new(None),
            password_new: Cell::new(false),
            strength: Cell::new(0),
            closed: Cell::new(false),
            on_reply: RefCell::new(Box::new(on_reply)),
        });
        let weak = Rc::downgrade(&dialog);
        let submit = {
            let weak = weak.clone();
            move || {
                if let Some(d) = weak.upgrade() {
                    d.submit();
                }
            }
        };
        dialog.ok.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        // Enter in the first field moves on to "Type again" when there is one.
        dialog.entry.connect_activate({
            let (weak, submit) = (weak.clone(), submit.clone());
            move |_| match weak.upgrade() {
                Some(d) if d.confirm.is_visible() && d.confirm.text().is_empty() => {
                    d.confirm.grab_focus();
                }
                _ => submit(),
            }
        });
        dialog.confirm.connect_activate(move |_| submit());
        let cancel = {
            let weak = weak.clone();
            move || {
                if let Some(d) = weak.upgrade() {
                    d.answer(Reply::Cancel);
                }
            }
        };
        dialog.cancel.connect_clicked({
            let cancel = cancel.clone();
            move |_| cancel()
        });
        crate::modal::on_escape(&dialog.window, cancel);
        dialog
    }

    /// Show (or reshow) the dialog for one prompt.
    pub fn prompt(&self, kind: Kind, props: &Props) {
        if self.closed.get() {
            return;
        }
        let password = kind == Kind::Password;
        self.kind.set(Some(kind));
        self.password_new.set(password && props.password_new);
        // gnome-shell puts the message in the dialog's title and the
        // description under it; the window title has no window here.
        let heading = [&props.message, &props.title]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default();
        self.title.set_label(&heading);
        self.title.set_visible(!heading.is_empty());
        self.description.set_label(&props.description);
        self.description.set_visible(!props.description.is_empty());
        self.warning.set_label(&props.warning);
        self.warning.set_visible(!props.warning.is_empty());

        self.entry.set_visible(password);
        self.entry.set_text("");
        self.confirm.set_visible(self.password_new.get());
        self.confirm.set_text("");
        self.choice.set_label(Some(&props.choice_label));
        self.choice.set_visible(!props.choice_label.is_empty());
        self.choice.set_active(props.choice_chosen);

        let or = |s: &str, default: &str| {
            if s.is_empty() {
                i18n::shell(default)
            } else {
                s.to_string()
            }
        };
        self.cancel.set_label(&or(&props.cancel_label, "Cancel"));
        self.ok_label
            .set_label(&or(&props.continue_label, "Continue"));
        self.set_busy(false);
        self.window.present();
        if password {
            self.entry.grab_focus();
        } else {
            self.ok.grab_focus();
        }
    }

    /// The caller is done with us (or went away).
    pub fn close(&self) {
        if !self.closed.replace(true) {
            self.kind.set(None);
            self.wipe();
            self.window.destroy();
        }
    }

    /// Whether the "remember" choice is ticked, if it is offered.
    pub fn choice_chosen(&self) -> Option<bool> {
        self.choice.is_visible().then(|| self.choice.is_active())
    }

    /// 0 for an empty new password, more for longer and more varied ones,
    /// as gnome-shell's prompt reports it (gnome-keyring only checks > 0).
    pub fn password_strength(&self) -> Option<i32> {
        self.password_new.get().then(|| self.strength.get())
    }

    /// Type into the dialog and press its default button (live test).
    #[cfg(test)]
    pub fn type_and_continue(&self, password: &str, confirm: &str) -> bool {
        if self.kind.get().is_none() {
            return false;
        }
        self.entry.set_text(password);
        self.confirm.set_text(confirm);
        self.choice.set_active(true);
        self.submit();
        true
    }

    #[cfg(test)]
    pub fn warning_text(&self) -> String {
        self.warning.label().to_string()
    }

    fn submit(&self) {
        let Some(kind) = self.kind.get() else { return };
        if kind == Kind::Confirm {
            self.answer(Reply::Continue(None));
            return;
        }
        let password = self.entry.text().to_string();
        if self.password_new.get() && password != self.confirm.text().as_str() {
            self.warning
                .set_label(&i18n::shell("Passwords do not match."));
            self.warning.set_visible(true);
            self.confirm.set_text("");
            self.confirm.grab_focus();
            return;
        }
        self.strength.set(strength(&password));
        self.answer(Reply::Continue(Some(password)));
    }

    fn answer(&self, reply: Reply) {
        if self.kind.take().is_none() {
            return;
        }
        let cancelled = matches!(reply, Reply::Cancel);
        if cancelled {
            // Gone at once; a further prompt from the caller reopens it.
            self.window.set_visible(false);
        } else {
            self.set_busy(true);
        }
        self.wipe();
        (self.on_reply.borrow())(reply);
    }

    fn set_busy(&self, busy: bool) {
        for w in [
            self.entry.upcast_ref::<gtk::Widget>(),
            self.confirm.upcast_ref(),
            self.choice.upcast_ref(),
            self.ok.upcast_ref(),
        ] {
            w.set_sensitive(!busy);
        }
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
    }

    fn wipe(&self) {
        self.entry.set_text("");
        self.confirm.set_text("");
    }
}

fn entry_confirm() -> gtk::PasswordEntry {
    let e = entry(&i18n::shell("Type again"));
    e.set_visible(false);
    e
}

/// gnome-shell's calculate_password_strength (shell-keyring-prompt.c),
/// scaled to 0-100.
fn strength(password: &str) -> i32 {
    if password.is_empty() {
        return 0;
    }
    let length = password.chars().count().min(5) as f64;
    let count = |f: fn(&char) -> bool| (password.chars().filter(f).count().min(3)) as f64;
    let digits = count(char::is_ascii_digit);
    let upper = count(char::is_ascii_uppercase);
    let misc = count(|c| !c.is_alphanumeric());
    let s = (length * 0.1 - 0.2) + digits * 0.1 + misc * 0.15 + upper * 0.1;
    ((s.clamp(0.0, 1.0) * 100.0) as i32).max(1)
}

#[cfg(test)]
mod tests {
    use super::strength;

    #[test]
    fn strength_grows_with_variety() {
        assert_eq!(strength(""), 0);
        assert!(strength("a") > 0);
        assert!(strength("Abc12!xyz") > strength("abcdefghi"));
        assert!(strength("A1!B2@C3#") <= 100);
    }
}
