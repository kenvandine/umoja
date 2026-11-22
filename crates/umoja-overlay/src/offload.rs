//! "Cloud for harder requests [switch]" on the This Computer page of the
//! settings popover: opt-in cloud offload (`org.umoja.shell cloud-offload`).
//! Shown once a cloud model is set up on the Cloud page. Everyday requests
//! stay on the local model; Umoja's Lemonade sends long or complex ones
//! (and the background agent's long tasks) to the cloud model. The policy
//! lives in umoja-lemonade-routing.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use umoja_common::assistant_backend::{self as backend, routing};
use umoja_common::{gsettings as gs, i18n};

pub struct OffloadRow {
    pub root: gtk::Box,
    switch: gtk::Switch,
    detail: gtk::Label,
    /// Set while refresh() moves the switch, so it doesn't save.
    filling: Cell<bool>,
}

impl OffloadRow {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 2);
        root.set_margin_top(6);
        root.set_visible(false);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let label = gtk::Label::builder()
            .label(i18n::umoja("Use the cloud model for harder requests"))
            .xalign(0.0)
            .hexpand(true)
            .build();
        let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        label.set_mnemonic_widget(Some(&switch));
        top.append(&label);
        top.append(&switch);
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .build();
        root.append(&top);
        root.append(&detail);
        let row = Rc::new(Self { root, switch, detail, filling: Cell::new(false) });
        let weak = Rc::downgrade(&row);
        row.switch.connect_active_notify(move |s| {
            let Some(r) = weak.upgrade() else { return };
            if r.filling.get() {
                return;
            }
            if let Some(set) = gs::settings(gs::UMOJA_SHELL).filter(|g| gs::has_key(g, routing::OFFLOAD_KEY)) {
                let _ = set.set_boolean(routing::OFFLOAD_KEY, s.is_active());
            }
            routing::apply();
            r.show_detail(s.is_active());
        });
        row
    }

    /// Reads the settings again (each time the popover opens).
    pub fn refresh(&self) {
        let (url, model) = backend::cloud_settings();
        let set_up = backend::valid_base_url(&url) && !model.trim().is_empty();
        let schema = gs::settings(gs::UMOJA_SHELL).is_some_and(|g| gs::has_key(&g, routing::OFFLOAD_KEY));
        self.root.set_visible(set_up && schema);
        if !self.root.is_visible() {
            return;
        }
        let on = gs::get_bool(gs::UMOJA_SHELL, routing::OFFLOAD_KEY, false);
        self.filling.set(true);
        self.switch.set_active(on);
        self.filling.set(false);
        self.show_detail(on);
    }

    fn show_detail(&self, on: bool) {
        self.detail.set_text(&if on {
            i18n::umoja(
                "Everyday requests stay on this computer. Research, planning, writing, code and long agent tasks go to the cloud model. Anything you share from your screen stays here.",
            )
        } else {
            i18n::umoja("Everything runs on this computer.")
        });
    }
}
