//! Clipboard history: what the user copied this session, newest first.
//!
//! Kept in memory only (never written to disk), capped at [`MAX_ITEMS`], and
//! turned off with `org.umoja.shell clipboard-history`. The overlay shows
//! it as a list with Translate, Reformat and Summarize actions.

use crate::clipwatch::{self, ClipEvent};
use crate::context::Transform;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};
use umoja_common::{gsettings as gs, i18n};

pub const MAX_ITEMS: usize = 50;
/// A selection this recent counts as "what the user is looking at".
pub const FRESH_SELECTION: Duration = Duration::from_secs(5 * 60);

pub struct History {
    items: RefCell<VecDeque<String>>,
    primary: RefCell<Option<(String, Instant)>>,
    enabled: Cell<bool>,
    watching: Cell<bool>,
    listeners: RefCell<Vec<Box<dyn Fn()>>>,
    _settings: RefCell<Option<gio::Settings>>,
}

thread_local! {
    static HISTORY: std::cell::OnceCell<Rc<History>> = const { std::cell::OnceCell::new() };
}

/// The session's clipboard history (started on first use, GTK thread only).
pub fn history() -> Rc<History> {
    HISTORY.with(|h| h.get_or_init(History::start).clone())
}

impl History {
    fn start() -> Rc<Self> {
        let h = Rc::new(Self {
            items: RefCell::default(),
            primary: RefCell::default(),
            enabled: Cell::new(gs::get_bool(gs::UMOJA_SHELL, "clipboard-history", true)),
            watching: Cell::new(false),
            listeners: RefCell::default(),
            _settings: RefCell::default(),
        });
        let weak = Rc::downgrade(&h);
        *h._settings.borrow_mut() = gs::watch(gs::UMOJA_SHELL, "clipboard-history", move |s, _| {
            if let Some(h) = weak.upgrade() {
                h.enabled.set(s.boolean("clipboard-history"));
                if !h.enabled.get() {
                    h.clear();
                }
            }
        });
        if let Some(rx) = clipwatch::start() {
            h.watching.set(true);
            let weak = Rc::downgrade(&h);
            glib::spawn_future_local(async move {
                while let Ok(ev) = rx.recv().await {
                    let Some(h) = weak.upgrade() else { break };
                    h.on_event(ev);
                }
            });
        }
        h
    }

    /// Whether we see every copy (the compositor lets us watch), rather than
    /// only what's on the clipboard when the overlay opens.
    pub fn is_watching(&self) -> bool {
        self.watching.get()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.get()
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn notify(&self) {
        for f in self.listeners.borrow().iter() {
            f();
        }
    }

    pub fn on_event(&self, ev: ClipEvent) {
        match ev {
            ClipEvent::Clipboard(text) => self.add(text),
            ClipEvent::Primary(text) if text.trim().is_empty() => {
                self.primary.borrow_mut().take();
            }
            ClipEvent::Primary(text) => {
                *self.primary.borrow_mut() = Some((text, Instant::now()));
            }
        }
    }

    pub fn add(&self, text: String) {
        if !self.enabled.get() {
            return;
        }
        if push(&mut self.items.borrow_mut(), text) {
            self.notify();
        }
    }

    pub fn remove(&self, index: usize) {
        if self.items.borrow_mut().remove(index).is_some() {
            self.notify();
        }
    }

    pub fn clear(&self) {
        self.items.borrow_mut().clear();
        self.primary.borrow_mut().take();
        self.notify();
    }

    pub fn items(&self) -> Vec<String> {
        self.items.borrow().iter().cloned().collect()
    }

    pub fn latest(&self) -> Option<String> {
        self.items.borrow().front().cloned()
    }

    /// The current selection, and whether it was made in the last few minutes.
    pub fn selection(&self) -> Option<(String, bool)> {
        self.primary
            .borrow()
            .as_ref()
            .map(|(t, at)| (t.clone(), at.elapsed() < FRESH_SELECTION))
    }

    /// Fallback when we can't watch: remember a selection read some other way.
    pub fn set_selection(&self, text: String) {
        self.on_event(ClipEvent::Primary(text));
    }
}

/// Add `text` to the front, moving it up if already there. Returns whether
/// anything changed.
pub fn push(items: &mut VecDeque<String>, text: String) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    if items.front() == Some(&text) {
        return false;
    }
    items.retain(|t| *t != text);
    items.push_front(text);
    items.truncate(MAX_ITEMS);
    true
}

/// What a row's buttons ask the overlay to do.
pub enum RowAction {
    Copy(String),
    Attach(String),
    Transform(String, Transform),
    Remove(usize),
}

/// One history entry with its actions.
pub fn row(index: usize, text: &str, on: Rc<dyn Fn(RowAction)>) -> gtk::Widget {
    let preview: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    let label = gtk::Label::builder()
        .label(crate::context::truncate_chars(&preview, 400))
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(3)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .css_classes(["clip-text"])
        .build();
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    actions.set_valign(gtk::Align::Center);
    for t in [
        Transform::Translate,
        Transform::Reformat,
        Transform::Summarize,
    ] {
        let b = gtk::Button::builder()
            .label(t.label())
            .css_classes(["pill", "clip-action"])
            .build();
        let on = on.clone();
        let text = text.to_string();
        b.connect_clicked(move |_| on(RowAction::Transform(text.clone(), t)));
        actions.append(&b);
    }
    let icon_button = |icon: &str, tip: &str, action: Box<dyn Fn() -> RowAction>| {
        let b = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tip)
            .css_classes(["flat", "circular"])
            .build();
        let on = on.clone();
        b.connect_clicked(move |_| on(action()));
        b
    };
    let t = text.to_string();
    actions.append(&icon_button(
        "mail-attachment-symbolic",
        &i18n::umoja("Ask about this"),
        Box::new(move || RowAction::Attach(t.clone())),
    ));
    let t = text.to_string();
    actions.append(&icon_button(
        "edit-copy-symbolic",
        &i18n::umoja("Copy"),
        Box::new(move || RowAction::Copy(t.clone())),
    ));
    actions.append(&icon_button(
        "window-close-symbolic",
        &i18n::umoja("Remove from history"),
        Box::new(move || RowAction::Remove(index)),
    ));

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.add_css_class("clip-row");
    content.append(&label);
    content.append(&actions);
    content.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_without_duplicates() {
        let mut items = VecDeque::new();
        assert!(push(&mut items, "a".into()));
        assert!(push(&mut items, "b".into()));
        assert!(!push(&mut items, "b".into()));
        assert!(push(&mut items, "a".into()));
        assert_eq!(items, ["a", "b"]);
        assert!(!push(&mut items, "   ".into()));
    }

    #[test]
    fn capped() {
        let mut items = VecDeque::new();
        for i in 0..MAX_ITEMS + 10 {
            push(&mut items, i.to_string());
        }
        assert_eq!(items.len(), MAX_ITEMS);
        assert_eq!(items.front().unwrap(), &(MAX_ITEMS + 9).to_string());
    }
}
