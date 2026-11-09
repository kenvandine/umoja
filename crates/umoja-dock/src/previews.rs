//! Window list popover shown when an app with several windows is clicked.
//!
//! TODO(phase 2): live thumbnails via zwlr_screencopy_manager_v1.

use crate::tracker::{Command, WindowInfo};
use gtk::prelude::*;
use gtk4 as gtk;

pub fn popover(windows: &[WindowInfo], commands: async_channel::Sender<Command>) -> gtk::Popover {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let popover = gtk::Popover::builder()
        .child(&list)
        .has_arrow(false)
        .build();
    for w in windows {
        let title = if w.title.is_empty() {
            w.app_id.clone()
        } else {
            w.title.clone()
        };
        let row = gtk::Button::builder()
            .child(
                &gtk::Label::builder()
                    .label(&title)
                    .xalign(0.0)
                    .max_width_chars(40)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            )
            .css_classes(["flat"])
            .build();
        let id = w.id;
        let commands = commands.clone();
        let popover = popover.downgrade();
        row.connect_clicked(move |_| {
            let _ = commands.try_send(Command::Activate(id));
            if let Some(p) = popover.upgrade() {
                p.popdown();
            }
        });
        list.append(&row);
    }
    popover
}
