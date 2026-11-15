//! The "Dictation" page of the assistant settings popover, next to Voice:
//! sets Myna up when it's missing, picks the speech-to-text model it uses
//! (Parakeet by default), and shows the shortcut. See [`crate::dictation`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use umoja_common::i18n;

use crate::dictation::{self, MODELS};
use crate::lemonade::RUNTIME;

thread_local! {
    static PAGE: RefCell<std::rc::Weak<DictationPage>> = RefCell::new(std::rc::Weak::new());
}

/// Feature test: "myna=yes|no;model=…;busy=yes|no;status=…".
pub fn test_state() -> (bool, String) {
    let Some(p) = PAGE.with(|p| p.borrow().upgrade()) else {
        return (false, String::new());
    };
    (
        true,
        format!(
            "myna={};model={};busy={};status={}",
            if dictation::installed(dictation::MYNA_SNAP) { "yes" } else { "no" },
            p.active.borrow().clone().unwrap_or_default(),
            if p.busy.get() { "yes" } else { "no" },
            p.status.text()
        ),
    )
}

pub struct DictationPage {
    pub page: gtk::Box,
    banner: gtk::Box,
    set_up: gtk::Button,
    list: gtk::ListBox,
    status: gtk::Label,
    shortcut: gtk::Label,
    /// The model `myna:backend` is connected to.
    active: RefCell<Option<String>>,
    busy: Cell<bool>,
}

impl DictationPage {
    pub fn new() -> Rc<Self> {
        let banner_label = gtk::Label::builder()
            .label(i18n::umoja(
                "Talk instead of typing, in any app. Myna turns your speech into text on this computer; nothing is sent anywhere.",
            ))
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .build();
        let set_up = gtk::Button::builder()
            .label(i18n::umoja("Set Up Dictation"))
            .halign(gtk::Align::Start)
            .css_classes(["suggested-action"])
            .build();
        let banner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        banner.add_css_class("voice-banner");
        banner.append(&banner_label);
        banner.append(&set_up);
        banner.set_visible(false);

        let note = gtk::Label::builder()
            .label(i18n::umoja(
                "Press the shortcut in any text box and speak, then press it again to stop. Pick the model that knows your language best.",
            ))
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["model-list"])
            .build();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();
        let shortcut = gtk::Label::builder().css_classes(["dim-label"]).build();
        let shortcut_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        shortcut_row.set_margin_top(4);
        let l = gtk::Label::builder()
            .label(i18n::umoja("Shortcut"))
            .xalign(0.0)
            .hexpand(true)
            .build();
        shortcut_row.append(&l);
        shortcut_row.append(&shortcut);
        shortcut_row.set_tooltip_text(Some(&i18n::umoja(
            "Change it in Settings › Keyboard › Custom Shortcuts (Dictation)",
        )));

        let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.append(&banner);
        page.append(&note);
        page.append(&list);
        page.append(&status);
        page.append(&shortcut_row);

        let this = Rc::new(Self {
            page,
            banner,
            set_up,
            list,
            status,
            shortcut,
            active: RefCell::default(),
            busy: Cell::new(false),
        });
        PAGE.with(|p| p.replace(Rc::downgrade(&this)));
        let weak = Rc::downgrade(&this);
        this.set_up.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.choose(dictation::DEFAULT_MODEL);
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(p) = weak.upgrade() {
                p.choose(&row.widget_name());
            }
        });
        this.fill();
        this
    }

    /// Read Myna's state again. Call when the popover opens.
    pub fn load(self: &Rc<Self>) {
        self.shortcut.set_text(
            &dictation::shortcut().unwrap_or_else(|| i18n::umoja("None")),
        );
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (tx, rx) = async_channel::bounded(1);
            RUNTIME.spawn(async move {
                let _ = tx.send(dictation::active_backend().await).await;
            });
            let active = rx.recv().await.ok().flatten();
            let Some(p) = weak.upgrade() else { return };
            p.active.replace(active);
            p.fill();
        });
    }

    fn fill(self: &Rc<Self>) {
        let myna = dictation::installed(dictation::MYNA_SNAP);
        self.banner.set_visible(!myna || self.active.borrow().is_none());
        self.set_up.set_visible(dictation::can_install() && !self.busy.get());
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        let active = self.active.borrow().clone();
        for m in MODELS {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.set_margin_end(6);
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_opacity(if active.as_deref() == Some(m.snap) { 1.0 } else { 0.0 });
            row.append(&check);
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            let mut title = m.name.to_owned();
            if m.snap == dictation::DEFAULT_MODEL {
                title = format!("{title} · {}", i18n::umoja("Default"));
            }
            text.append(&gtk::Label::builder().label(&title).xalign(0.0).build());
            text.append(
                &gtk::Label::builder()
                    .label(i18n::umoja(m.blurb))
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(40)
                    .css_classes(["dim-label", "caption"])
                    .build(),
            );
            row.append(&text);
            let have = dictation::installed(m.snap);
            let state = gtk::Image::from_icon_name(if have {
                "emblem-ok-symbolic"
            } else {
                "folder-download-symbolic"
            });
            state.set_tooltip_text(Some(&if have {
                i18n::umoja("Downloaded")
            } else {
                i18n::umoja("Downloads when selected")
            }));
            state.add_css_class("dim-label");
            row.append(&state);
            let item = gtk::ListBoxRow::builder()
                .child(&row)
                .activatable(!self.busy.get())
                .name(m.snap)
                .build();
            self.list.append(&item);
        }
    }

    fn set_status(&self, text: Option<&str>) {
        self.status.set_visible(text.is_some());
        self.status.set_text(text.unwrap_or_default());
    }

    /// Set Myna up with `snap`, or switch to it.
    fn choose(self: &Rc<Self>, snap: &str) {
        if self.busy.get() || self.active.borrow().as_deref() == Some(snap) {
            return;
        }
        if !dictation::can_install() {
            self.set_status(Some(&format!(
                "{} sudo snap install --edge myna {snap}",
                i18n::umoja("Install it with:")
            )));
            return;
        }
        self.busy.set(true);
        self.fill();
        self.set_status(Some(&i18n::umoja("You may be asked for your password.")));
        let (tx, rx) = async_channel::unbounded::<Result<&'static str, Result<(), String>>>();
        let backend = snap.to_owned();
        RUNTIME.spawn(async move {
            let step_tx = tx.clone();
            let result = dictation::use_model(&backend, move |label| {
                let _ = step_tx.try_send(Ok(label));
            })
            .await;
            let _ = tx.send(Err(result)).await;
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                let Some(p) = weak.upgrade() else { return };
                match msg {
                    Ok(label) => p.set_status(Some(&i18n::umoja(label))),
                    Err(result) => {
                        p.busy.set(false);
                        match result {
                            Ok(()) => p.set_status(Some(&i18n::umoja(
                                "Dictation is ready. Press the shortcut in any text box and speak.",
                            ))),
                            Err(e) => p.set_status(Some(&format!(
                                "{} ({e})",
                                i18n::umoja("Dictation wasn't set up")
                            ))),
                        }
                        p.load();
                        return;
                    }
                }
            }
        });
    }
}
