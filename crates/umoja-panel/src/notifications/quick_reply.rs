//! The quick-reply window: opened from a banner's Reply button or
//! `org.umoja.Notifications.OpenReply`.
//!
//! A popover can't be used for these. A popup needs the serial of the input
//! event that opened it, and neither a click on the banner surface nor a
//! D-Bus call gives the panel one, so Mir dismissed the clock menu at once
//! (or, without a grab, left it open with no keyboard and no way to close
//! it). This is its own layer surface instead: centred under the panel,
//! holding the keyboard while open, closed by Escape, Cancel or Send, and
//! when the notification goes away.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use umoja_common::i18n;

use super::Center;
use super::bubbles::icon_widget;
use super::list::Reply;

pub struct QuickReply {
    app: gtk::Application,
    center: Weak<Center>,
    /// (notification id, window) while one is open.
    open: RefCell<Option<(u32, gtk::Window)>>,
}

impl QuickReply {
    pub fn new(app: &gtk::Application, center: Weak<Center>) -> Rc<Self> {
        Rc::new(Self {
            app: app.clone(),
            center,
            open: RefCell::default(),
        })
    }

    pub fn show(self: &Rc<Self>, id: u32) {
        let Some(c) = self.center.upgrade() else {
            return;
        };
        let Some(note) = c.note(id) else { return };
        if let Some((open_id, win)) = self.open.borrow().as_ref() {
            if *open_id == id {
                win.present();
                return;
            }
        }
        self.close();

        let window = gtk::Window::builder()
            .application(&self.app)
            .css_classes(["umoja-quick-reply"])
            .decorated(false)
            .resizable(false)
            .default_width(420)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-quick-reply");
        umoja_common::blur::blur_behind(&window);
        window.set_anchor(Edge::Top, true);
        window.set_margin(Edge::Top, crate::PANEL_HEIGHT + 12);
        window.set_exclusive_zone(-1);
        // Typing goes straight in; Escape, Cancel and Send all close it, so
        // the keyboard is never held for long.
        window.set_keyboard_mode(KeyboardMode::Exclusive);

        let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card.add_css_class("quick-reply");
        card.add_css_class("message-list");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        if let Some(icon) = icon_widget(note.icon.as_ref(), 32) {
            icon.set_valign(gtk::Align::Start);
            header.append(&icon);
        }
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let title = gtk::Label::builder()
            .label(if note.app_name.is_empty() {
                note.summary.clone()
            } else {
                format!("{} · {}", note.summary, note.app_name)
            })
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["message-title"])
            .build();
        text.append(&title);
        if !note.body.is_empty() {
            let body = gtk::Label::builder()
                .label(&note.body)
                .use_markup(true)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .lines(4)
                .max_width_chars(48)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["message-body"])
                .build();
            text.append(&body);
        }
        header.append(&text);
        card.append(&header);

        let reply = Reply::new(&note, self.center.clone());
        reply.revealer.add_css_class("quick-reply-field");
        card.append(&reply.revealer);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label(&i18n::gtk("_Cancel").replace('_', ""));
        buttons.append(&cancel);
        card.append(&buttons);
        window.set_child(Some(&card));

        {
            let this = Rc::downgrade(self);
            cancel.connect_clicked(move |_| {
                if let Some(t) = this.upgrade() {
                    t.close();
                }
            });
        }
        {
            let this = Rc::downgrade(self);
            reply.connect_sent(move || {
                if let Some(t) = this.upgrade() {
                    t.close();
                }
            });
        }
        let keys = gtk::EventControllerKey::new();
        {
            let this = Rc::downgrade(self);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if let Some(t) = this.upgrade() {
                        t.close();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        window.add_controller(keys);

        window.present();
        reply.expand();
        self.open.replace(Some((id, window)));
    }

    pub fn close(&self) {
        if let Some((_, win)) = self.open.take() {
            win.close();
        }
    }

    /// The notification went away (answered elsewhere, closed by its app).
    pub fn sync(&self) {
        let gone = match (self.open.borrow().as_ref(), self.center.upgrade()) {
            (Some((id, _)), Some(c)) => c.note(*id).is_none(),
            (Some(_), None) => true,
            _ => false,
        };
        if gone {
            self.close();
        }
    }
}
