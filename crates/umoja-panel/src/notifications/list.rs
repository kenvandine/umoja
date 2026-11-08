//! The message list in the clock menu, as in gnome-shell's calendar
//! drop-down: notifications newest first, a Do Not Disturb switch and a
//! Clear button underneath.

use std::rc::{Rc, Weak};

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use umoja_common::i18n;

use super::{bubbles::icon_widget, Center, CloseReason, Note};

pub struct MessageList {
    root: gtk::Box,
}

impl MessageList {
    pub fn new(center: &Rc<Center>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("message-list");
        root.set_size_request(360, -1);

        let rows = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(480)
            .vexpand(true)
            .child(&rows)
            .build();

        let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 8);
        placeholder.add_css_class("message-list-empty");
        placeholder.set_valign(gtk::Align::Center);
        placeholder.set_vexpand(true);
        let ph_icon = gtk::Image::from_icon_name("no-notifications-symbolic");
        ph_icon.set_pixel_size(64);
        placeholder.append(&ph_icon);
        placeholder.append(&gtk::Label::new(Some(&i18n::shell("No Notifications"))));

        // Bottom bar: "Do Not Disturb [switch]" .......... [Clear]
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("message-list-bar");
        bar.append(&gtk::Label::new(Some(&i18n::shell("Do Not Disturb"))));
        let dnd = gtk::Switch::new();
        dnd.set_valign(gtk::Align::Center);
        bar.append(&dnd);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&spacer);
        let clear = gtk::Button::with_label(&i18n::shell("Clear"));
        clear.add_css_class("message-list-clear");
        bar.append(&clear);

        root.append(&scroller);
        root.append(&placeholder);
        root.append(&bar);

        {
            let weak = Rc::downgrade(center);
            clear.connect_clicked(move |_| {
                if let Some(c) = weak.upgrade() {
                    c.clear();
                }
            });
        }
        {
            let weak = Rc::downgrade(center);
            dnd.connect_state_set(move |_, on| {
                if let Some(c) = weak.upgrade() {
                    c.set_do_not_disturb(on);
                }
                glib::Propagation::Proceed
            });
        }

        let refresh = {
            let weak = Rc::downgrade(center);
            let rows = rows.clone();
            move || {
                let Some(c) = weak.upgrade() else { return };
                while let Some(child) = rows.first_child() {
                    rows.remove(&child);
                }
                let notes = c.notes();
                for n in &notes {
                    rows.append(&row(n, Rc::downgrade(&c)));
                }
                scroller.set_visible(!notes.is_empty());
                placeholder.set_visible(notes.is_empty());
                clear.set_sensitive(!notes.is_empty());
                if dnd.is_active() == c.banners_enabled() {
                    dnd.set_active(!c.banners_enabled());
                }
            }
        };
        refresh();
        center.connect_changed(refresh);
        Self { root }
    }

    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
}

fn row(note: &Note, center: Weak<Center>) -> gtk::Widget {
    let id = note.id;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("message");

    if let Some(icon) = icon_widget(note.icon.as_ref(), 32) {
        icon.set_valign(gtk::Align::Start);
        row.append(&icon);
    }
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let title = gtk::Label::builder()
        .label(&note.summary)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["message-title"])
        .build();
    header.append(&title);
    let time = gtk::Label::builder()
        .label(if note.app_name.is_empty() {
            relative_time(&note.time)
        } else {
            format!("{} · {}", note.app_name, relative_time(&note.time))
        })
        .css_classes(["message-time", "dim-label"])
        .build();
    header.append(&time);
    text.append(&header);

    if !note.body.is_empty() {
        let body = gtk::Label::builder()
            .label(&note.body)
            .use_markup(true)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["message-body"])
            .build();
        text.append(&body);
    }
    // Progress (the value hint), e.g. the assistant's model download.
    if let Some(v) = note.value {
        let gauge = gtk::ProgressBar::builder()
            .fraction(v as f64 / 100.0)
            .css_classes(["message-gauge"])
            .build();
        text.append(&gauge);
    }
    row.append(&text);

    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.add_css_class("circular");
    close.set_valign(gtk::Align::Start);
    {
        let center = center.clone();
        close.connect_clicked(move |_| {
            if let Some(c) = center.upgrade() {
                c.close(id, CloseReason::Dismissed);
            }
        });
    }
    row.append(&close);

    let click = gtk::GestureClick::new();
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        if let Some(c) = center.upgrade() {
            c.activate(id);
        }
    });
    row.add_controller(click);
    row.upcast()
}

/// "Just now", "5 min", "14:02", "Yesterday", "Mon"... like gnome-shell.
fn relative_time(t: &glib::DateTime) -> String {
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };
    let secs = now.difference(t).as_seconds();
    if secs < 60 {
        return i18n::shell("Just now");
    }
    if secs < 3600 {
        return format!("{} min", secs / 60);
    }
    let fmt = if now.ymd() == t.ymd() {
        "%H:%M"
    } else if secs < 7 * 24 * 3600 {
        "%a"
    } else {
        "%b %-d"
    };
    t.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}
