//! The network password dialog: gnome-shell's NetworkSecretDialog, drawn
//! as the same blurred dark card as our PolicyKit dialog (it reuses its
//! styles). One entry per secret, Connect only once every entry is valid.

use super::content::{Content, Secret};
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::Cell;
use std::rc::Rc;
use umoja_common::i18n;

/// What the user typed, by setting key.
pub type Answer = Vec<(String, String)>;

pub struct Dialog {
    window: gtk::Window,
    done: Cell<bool>,
    on_done: Box<dyn Fn(Option<Answer>)>,
}

impl Dialog {
    pub fn new(content: Content, on_done: impl Fn(Option<Answer>) + 'static) -> Rc<Self> {
        let crate::modal::Modal { window, card } =
            crate::modal::new("umoja-network-agent", Some("umoja-network-agent"));
        card.append(
            &gtk::Image::builder()
                .icon_name("network-wireless-encrypted-symbolic")
                .pixel_size(48)
                .margin_top(4)
                .build(),
        );
        card.append(
            &gtk::Label::builder()
                .label(&content.title)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .css_classes(["polkit-title"])
                .build(),
        );
        if let Some(message) = &content.message {
            card.append(
                &gtk::Label::builder()
                    .label(message)
                    .wrap(true)
                    .justify(gtk::Justification::Center)
                    .max_width_chars(40)
                    .css_classes(["polkit-body"])
                    .build(),
            );
        }

        let connect = gtk::Button::builder()
            .label(i18n::shell("Connect"))
            .css_classes(["pill", "suggested-action"])
            .build();
        let cancel = gtk::Button::builder()
            .label(i18n::shell("Cancel"))
            .css_classes(["pill"])
            .build();

        // (secret, entry text getter, valid now)
        let fields: Rc<Vec<(Secret, gtk::Editable, Cell<bool>)>> = Rc::new(
            content
                .secrets
                .into_iter()
                .map(|s| {
                    let editable: gtk::Editable = if s.password {
                        gtk::PasswordEntry::builder()
                            .show_peek_icon(true)
                            .placeholder_text(&s.label)
                            .css_classes(["polkit-entry"])
                            .build()
                            .upcast()
                    } else {
                        gtk::Entry::builder()
                            .placeholder_text(&s.label)
                            .css_classes(["polkit-entry"])
                            .build()
                            .upcast()
                    };
                    editable.set_text(&s.value);
                    // gnome-shell shows key-less secrets (the user name of
                    // an 802.1X login) without letting them be edited.
                    if s.key.is_none() {
                        editable.set_sensitive(false);
                        editable.set_tooltip_text(Some(&s.label));
                    }
                    let valid = Cell::new(s.key.is_none() || s.validate.check(&s.value));
                    (s, editable, valid)
                })
                .collect(),
        );
        let update = {
            let (fields, connect) = (fields.clone(), connect.clone());
            move || connect.set_sensitive(fields.iter().all(|(_, _, v)| v.get()))
        };
        for (i, (_, editable, _)) in fields.iter().enumerate() {
            card.append(editable);
            let (fields_c, update_c) = (Rc::downgrade(&fields), update.clone());
            editable.connect_changed(move |e| {
                let Some(fields) = fields_c.upgrade() else {
                    return;
                };
                let (s, _, valid) = &fields[i];
                if s.key.is_some() {
                    valid.set(s.validate.check(&e.text()));
                }
                update_c();
            });
        }
        if content.wps {
            card.append(
                &gtk::Label::builder()
                    .label(i18n::shell(
                        "Alternatively you can connect by pushing the “WPS” button on your router",
                    ))
                    .wrap(true)
                    .justify(gtk::Justification::Center)
                    .max_width_chars(40)
                    .css_classes(["polkit-body"])
                    .build(),
            );
        }
        update();

        card.append(&crate::modal::button_row(&cancel, &connect));

        let dialog = Rc::new(Self {
            window,
            done: Cell::new(false),
            on_done: Box::new(on_done),
        });
        let weak = Rc::downgrade(&dialog);
        let ok = {
            let (weak, fields) = (weak.clone(), fields.clone());
            move || {
                let Some(d) = weak.upgrade() else { return };
                if !fields.iter().all(|(_, _, v)| v.get()) {
                    return; // as gnome-shell: Enter does nothing until valid
                }
                let answer = fields
                    .iter()
                    .filter_map(|(s, e, _)| Some((s.key.clone()?, e.text().to_string())))
                    .collect();
                d.finish(Some(answer));
            }
        };
        connect.connect_clicked({
            let ok = ok.clone();
            move |_| ok()
        });
        for (_, editable, _) in fields.iter() {
            if let Some(entry) = editable.downcast_ref::<gtk::Entry>() {
                let ok = ok.clone();
                entry.connect_activate(move |_| ok());
            } else if let Some(entry) = editable.downcast_ref::<gtk::PasswordEntry>() {
                let ok = ok.clone();
                entry.connect_activate(move |_| ok());
            }
        }
        cancel.connect_clicked({
            let weak = weak.clone();
            move |_| {
                if let Some(d) = weak.upgrade() {
                    d.finish(None);
                }
            }
        });
        crate::modal::on_escape(&dialog.window, move || {
            if let Some(d) = weak.upgrade() {
                d.finish(None);
            }
        });

        dialog.window.present();
        if let Some((_, e, _)) = fields.iter().find(|(s, _, _)| s.key.is_some()) {
            e.grab_focus();
        }
        dialog
    }

    fn finish(&self, answer: Option<Answer>) {
        if self.done.replace(true) {
            return;
        }
        self.window.destroy();
        (self.on_done)(answer);
    }

    /// NetworkManager withdrew the request: close without answering.
    pub fn close(&self) {
        if !self.done.replace(true) {
            self.window.destroy();
        }
    }
}
