//! The message list in the clock menu, as in gnome-shell's calendar
//! drop-down: notifications newest first, a Do Not Disturb switch and a
//! Clear button underneath.
//!
//! On top of GNOME's list, the on-device assistant adds a digest card
//! ("5 new notifications" with a summary of them) and quick replies drafted
//! from a message.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use umoja_common::i18n;

use super::assistant::{self, Event};
use super::{Center, CloseReason, Note, bubbles::icon_widget};

/// Summarize automatically when the list opens with at least this many new
/// notifications; below it the card offers a button.
const AUTO_DIGEST: usize = 3;
/// The digest card appears from this many new notifications.
const MIN_DIGEST: usize = 2;

pub struct MessageList {
    inner: Rc<Inner>,
}

struct Inner {
    center: Weak<Center>,
    root: gtk::Box,
    rows: gtk::Box,
    /// Built rows by notification id, with what they were built from.
    built: RefCell<HashMap<u32, (Fingerprint, Row)>>,
    digest: Digest,
}

#[derive(PartialEq)]
struct Fingerprint {
    summary: String,
    body: String,
    value: Option<i32>,
    actions: usize,
    minute: i64,
}

impl Fingerprint {
    fn of(n: &Note) -> Self {
        Self {
            summary: n.summary.clone(),
            body: n.body.clone(),
            value: n.value,
            actions: n.actions.len(),
            // Rebuild once a minute so "5 min" stays right.
            minute: glib::DateTime::now_local().map_or(0, |d| d.to_unix() / 60),
        }
    }
}

#[derive(Clone)]
struct Row {
    widget: gtk::Box,
    reply: Option<Reply>,
}

impl MessageList {
    pub fn new(center: &Rc<Center>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("message-list");
        root.set_size_request(360, -1);

        let digest = Digest::new(Rc::downgrade(center));
        root.append(&digest.card);

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
        // GNOME's own "no-notifications-symbolic" ships inside gnome-shell,
        // not in an icon theme, so asking for it alone drew the missing-icon
        // image. Try theme bells in turn; the first one present wins.
        let ph_icon = gtk::Image::from_gicon(&gtk::gio::ThemedIcon::from_names(&[
            "no-notifications-symbolic",
            "notifications-disabled-symbolic",
            "preferences-system-notifications-symbolic",
            "notification-symbolic",
            "notifications-symbolic",
        ]));
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

        let inner = Rc::new(Inner {
            center: Rc::downgrade(center),
            root,
            rows,
            built: RefCell::default(),
            digest,
        });

        let refresh = {
            let weak = Rc::downgrade(&inner);
            move || {
                let Some(inner) = weak.upgrade() else { return };
                let Some(c) = inner.center.upgrade() else {
                    return;
                };
                let notes = c.notes();
                inner.sync_rows(&notes);
                inner.digest.sync(&c.missed());
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
        Self { inner }
    }

    pub fn widget(&self) -> gtk::Widget {
        self.inner.root.clone().upcast()
    }

    /// The clock menu opened: summarize a pile of new notifications.
    pub fn opened(&self) {
        let Some(c) = self.inner.center.upgrade() else {
            return;
        };
        let missed = c.missed();
        if missed.len() >= AUTO_DIGEST {
            self.inner.digest.run(&missed);
        }
    }

    /// The clock menu closed: everything in it has now been seen.
    pub fn closed(&self) {
        for (_, row) in self.inner.built.borrow().values() {
            if let Some(r) = &row.reply {
                r.collapse();
            }
        }
        if let Some(c) = self.inner.center.upgrade() {
            c.mark_seen();
        }
    }
}

impl Inner {
    /// Add, update and reorder rows without rebuilding the ones that didn't
    /// change, so an open reply field keeps its text and focus when another
    /// notification comes in.
    fn sync_rows(&self, notes: &[Note]) {
        let mut built = self.built.borrow_mut();
        built.retain(|id, (_, row)| {
            let keep = notes.iter().any(|n| n.id == *id);
            if !keep {
                self.rows.remove(&row.widget);
            }
            keep
        });
        let mut prev: Option<gtk::Widget> = None;
        for n in notes {
            let fp = Fingerprint::of(n);
            let replying = built
                .get(&n.id)
                .and_then(|(_, r)| r.reply.as_ref())
                .is_some_and(Reply::is_open);
            let fresh = built
                .get(&n.id)
                .is_none_or(|(old, _)| *old != fp && !replying);
            if fresh {
                if let Some((_, old)) = built.remove(&n.id) {
                    self.rows.remove(&old.widget);
                }
                let row = row(n, self.center.clone());
                self.rows.append(&row.widget);
                built.insert(n.id, (fp, row));
            }
            let w = built[&n.id].1.widget.clone().upcast::<gtk::Widget>();
            self.rows.reorder_child_after(&w, prev.as_ref());
            prev = Some(w);
        }
    }
}

fn row(note: &Note, center: Weak<Center>) -> Row {
    let id = note.id;
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("message");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    outer.append(&row);

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

    let reply = note.can_reply().then(|| Reply::new(note, center.clone()));
    if let Some(r) = &reply {
        let button = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
        button.add_css_class("flat");
        button.add_css_class("circular");
        button.set_valign(gtk::Align::Start);
        button.set_tooltip_text(Some(&i18n::shell("Reply")));
        let r2 = r.clone();
        button.connect_clicked(move |_| r2.toggle());
        row.append(&button);
        outer.append(&r.revealer);
    }

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
    Row {
        widget: outer,
        reply,
    }
}

/// Quick reply under a message: suggestions drafted by the assistant, a
/// text field and Send.
#[derive(Clone)]
pub(super) struct Reply {
    pub(super) revealer: gtk::Revealer,
    entry: gtk::Entry,
    suggestions: gtk::FlowBox,
    status: gtk::Label,
    spinner: gtk::Spinner,
    note: Rc<Note>,
    drafted: Rc<Cell<bool>>,
    /// Called after the reply went out (the quick-reply window closes).
    sent: Rc<RefCell<Option<Box<dyn Fn()>>>>,
}

impl Reply {
    pub(super) fn new(note: &Note, center: Weak<Center>) -> Self {
        let sent: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::default();
        let id = note.id;
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.add_css_class("message-reply");

        let state = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let spinner = gtk::Spinner::new();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "message-reply-status"])
            .build();
        state.append(&spinner);
        state.append(&status);
        column.append(&state);

        let suggestions = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(3)
            .column_spacing(6)
            .row_spacing(6)
            .homogeneous(false)
            .css_classes(["message-suggestions"])
            .build();
        column.append(&suggestions);

        let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text(
                note.reply_placeholder
                    .clone()
                    .unwrap_or_else(|| i18n::shell("Reply")),
            )
            .build();
        let send = gtk::Button::from_icon_name("mail-send-symbolic");
        send.add_css_class("suggested-action");
        send.add_css_class("circular");
        send.set_tooltip_text(Some(&i18n::shell("Send")));
        send.set_sensitive(false);
        line.append(&entry);
        line.append(&send);
        column.append(&line);
        if !note.inline_reply() {
            let hint = gtk::Label::builder()
                .label(format!(
                    "{} can't take replies from here. Send copies your reply and opens it.",
                    if note.app_name.is_empty() {
                        "This app"
                    } else {
                        &note.app_name
                    }
                ))
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label", "message-reply-hint"])
                .build();
            column.append(&hint);
        }
        revealer.set_child(Some(&column));

        {
            let send = send.clone();
            entry.connect_changed(move |e| send.set_sensitive(!e.text().trim().is_empty()));
        }
        let submit = {
            let entry = entry.clone();
            let sent = sent.clone();
            move || {
                let text = entry.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                if let Some(c) = center.upgrade() {
                    let app = c.note(id).map(|n| n.app_name).unwrap_or_default();
                    if !c.reply(id, &text) {
                        c.notify_internal(
                            &i18n::shell("Reply copied"),
                            &format!(
                                "Paste it into {} to send it.",
                                if app.is_empty() { "the app" } else { &app }
                            ),
                            "edit-paste-symbolic",
                        );
                    }
                }
                if let Some(f) = sent.borrow().as_ref() {
                    f();
                }
            }
        };
        {
            let submit = submit.clone();
            entry.connect_activate(move |_| submit());
        }
        send.connect_clicked(move |_| submit());

        Self {
            revealer,
            entry,
            suggestions,
            status,
            spinner,
            note: Rc::new(note.clone()),
            drafted: Rc::new(Cell::new(false)),
            sent,
        }
    }

    pub(super) fn connect_sent(&self, f: impl Fn() + 'static) {
        self.sent.replace(Some(Box::new(f)));
    }

    fn is_open(&self) -> bool {
        self.revealer.reveals_child()
    }

    fn toggle(&self) {
        if self.is_open() {
            self.collapse();
        } else {
            self.expand();
        }
    }

    fn collapse(&self) {
        self.revealer.set_reveal_child(false);
    }

    pub(super) fn expand(&self) {
        self.revealer.set_reveal_child(true);
        crate::notifications::want_keyboard(&self.entry, true);
        self.entry.grab_focus();
        if !self.drafted.replace(true) {
            self.draft();
        }
    }

    fn draft(&self) {
        self.spinner.set_visible(true);
        self.spinner.start();
        self.status.set_text(&i18n::shell("Drafting replies…"));
        self.status.set_visible(true);
        let this = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = assistant::draft_replies(&this.note).await;
            // A stopped spinner still draws a faint ring in Yaru.
            this.spinner.stop();
            this.spinner.set_visible(false);
            match result {
                Ok(replies) => {
                    this.status.set_visible(false);
                    for r in replies {
                        let chip = gtk::Button::with_label(&r);
                        chip.add_css_class("message-suggestion");
                        if let Some(l) = chip.child().and_downcast::<gtk::Label>() {
                            l.set_wrap(true);
                            l.set_max_width_chars(28);
                        }
                        let entry = this.entry.clone();
                        chip.connect_clicked(move |_| {
                            entry.set_text(&r);
                            entry.grab_focus();
                            entry.set_position(-1);
                        });
                        this.suggestions.append(&chip);
                    }
                }
                Err(e) => {
                    this.drafted.set(false); // try again next time
                    this.status.set_text(&e);
                }
            }
        });
    }
}

/// "5 new notifications" and the assistant's summary of them.
struct Digest {
    card: gtk::Revealer,
    title: gtk::Label,
    button: gtk::Button,
    spinner: gtk::Spinner,
    text: gtk::Label,
    /// The notifications the summary on show is about.
    ids: RefCell<Vec<u32>>,
    /// Bumped on each run so a stale stream stops writing.
    generation: Rc<Cell<u32>>,
    /// New notifications arrived under the summary on show.
    stale: Cell<bool>,
}

impl Digest {
    fn new(center: Weak<Center>) -> Self {
        let card = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.add_css_class("message-digest");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = gtk::Image::from_icon_name("view-list-bullet-symbolic");
        header.append(&icon);
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["message-title"])
            .build();
        header.append(&title);
        let spinner = gtk::Spinner::new();
        // Only take room while spinning (a stopped spinner still draws).
        spinner
            .bind_property("spinning", &spinner, "visible")
            .sync_create()
            .build();
        header.append(&spinner);
        let button = gtk::Button::with_label(&i18n::shell("Summarize"));
        button.add_css_class("message-digest-button");
        header.append(&button);
        column.append(&header);

        let text = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .max_width_chars(44)
            .visible(false)
            .css_classes(["message-digest-text"])
            .build();
        column.append(&text);
        card.set_child(Some(&column));

        let d = Self {
            card,
            title,
            button: button.clone(),
            spinner,
            text,
            ids: RefCell::default(),
            generation: Rc::new(Cell::new(0)),
            stale: Cell::new(false),
        };
        let (text, spinner, generation) = (d.text.clone(), d.spinner.clone(), d.generation.clone());
        button.connect_clicked(move |b| {
            let Some(c) = center.upgrade() else { return };
            let missed = c.missed();
            start(&missed, &text, &spinner, b, &generation);
        });
        d
    }

    /// Follow the set of new notifications: show the card for two or more,
    /// and drop a summary that no longer matches them.
    fn sync(&self, missed: &[Note]) {
        let ids: Vec<u32> = missed.iter().map(|n| n.id).collect();
        self.title.set_text(&new_count(missed.len()));
        self.card.set_reveal_child(missed.len() >= MIN_DIGEST);
        if *self.ids.borrow() != ids {
            self.ids.replace(ids);
            if self.text.is_visible() {
                // New ones came in under a summary: offer to redo it.
                self.stale.set(true);
                self.button.set_label(&i18n::shell("Update"));
                self.button.set_visible(true);
            }
        }
        if missed.len() < MIN_DIGEST {
            self.reset();
        }
    }

    fn reset(&self) {
        self.generation.set(self.generation.get() + 1);
        self.stale.set(false);
        self.spinner.stop();
        self.text.set_text("");
        self.text.set_visible(false);
        self.button.set_label(&i18n::shell("Summarize"));
        self.button.set_visible(true);
    }

    fn run(&self, missed: &[Note]) {
        if self.text.is_visible() && !self.stale.get() {
            return; // already summarized these
        }
        self.stale.set(false);
        start(
            missed,
            &self.text,
            &self.spinner,
            &self.button,
            &self.generation,
        );
    }
}

fn start(
    missed: &[Note],
    text: &gtk::Label,
    spinner: &gtk::Spinner,
    button: &gtk::Button,
    generation: &Rc<Cell<u32>>,
) {
    if missed.is_empty() {
        return;
    }
    let my_gen = generation.get() + 1;
    generation.set(my_gen);
    button.set_visible(false);
    spinner.start();
    text.remove_css_class("dim-label");
    text.set_text(&i18n::shell("Summarizing…"));
    text.set_visible(true);
    let rx = assistant::digest(missed);
    let (text, spinner, button, generation) = (
        text.clone(),
        spinner.clone(),
        button.clone(),
        generation.clone(),
    );
    glib::MainContext::default().spawn_local(async move {
        let mut acc = String::new();
        while let Ok(ev) = rx.recv().await {
            if generation.get() != my_gen {
                return;
            }
            match ev {
                Event::Delta(d) => {
                    acc.push_str(&d);
                    let shown = assistant::render_digest(&acc);
                    if !shown.is_empty() {
                        text.set_text(&shown);
                    }
                }
                Event::Done => break,
                Event::Error(e) => {
                    text.set_text(&e);
                    text.add_css_class("dim-label");
                    button.set_label(&i18n::shell("Try Again"));
                    button.set_visible(true);
                    spinner.stop();
                    return;
                }
            }
        }
        spinner.stop();
        if assistant::render_digest(&acc).is_empty() {
            text.set_text(&i18n::shell("Nothing to summarize."));
        }
    });
}

fn new_count(n: usize) -> String {
    if n == 1 {
        "1 new notification".to_string()
    } else {
        format!("{n} new notifications")
    }
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
