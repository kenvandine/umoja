//! Ask Umoja's prompt: a borderless, word-wrapping text box that grows
//! with what's typed, up to [`MAX_LINES`], then scrolls. A single-line
//! entry scrolled sideways and hid the start of a long question.
//!
//! It stands in for the `gtk::SearchEntry` it replaced, with the same
//! calls: search icon, placeholder, clear button, a short "changed" delay,
//! Enter activates and Escape stops. Shift+Enter starts a new line.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// As the old entry's search delay.
const CHANGED_DELAY: Duration = Duration::from_millis(60);
/// It grows to this many lines, then scrolls.
const MAX_LINES: i32 = 5;
/// The old entry's height: one line sits in the middle of it.
const LINE_SLOT: i32 = 40;

type Handler = Box<dyn Fn(&PromptInput)>;

struct Inner {
    root: gtk::Box,
    view: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    placeholder: gtk::Label,
    clear: gtk::Button,
    changed: RefCell<Vec<Handler>>,
    activate: RefCell<Vec<Handler>>,
    stop: RefCell<Vec<Handler>>,
    pending: RefCell<Option<glib::SourceId>>,
    /// Inside set_text: its delete and insert count as one change.
    quiet: Cell<bool>,
}

#[derive(Clone)]
pub struct PromptInput(Rc<Inner>);

/// `widget` centred in a [`LINE_SLOT`]-high box at the top, so icons and
/// buttons stay level with the first line as the text grows.
pub fn line_slot(widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    slot.set_valign(gtk::Align::Start);
    slot.set_size_request(-1, LINE_SLOT);
    widget.set_valign(gtk::Align::Center);
    widget.set_vexpand(true);
    slot.append(widget);
    // A hidden widget takes no room (nor spacing) in its row.
    widget
        .bind_property("visible", &slot, "visible")
        .sync_create()
        .build();
    slot
}

impl PromptInput {
    pub fn new(placeholder: &str) -> Self {
        let view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .hexpand(true)
            .left_margin(4)
            .right_margin(4)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .hexpand(true)
            .build();
        let placeholder = gtk::Label::builder()
            .label(placeholder)
            .xalign(0.0)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Start)
            .margin_start(4)
            .margin_end(4)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .can_target(false)
            .css_classes(["dim-label", "ask-placeholder"])
            .build();
        let stack = gtk::Overlay::builder()
            .child(&scroller)
            .hexpand(true)
            .build();
        stack.add_overlay(&placeholder);

        let icon = gtk::Image::builder()
            .icon_name("system-search-symbolic")
            .css_classes(["dim-label", "ask-search-icon"])
            .build();
        icon.set_accessible_role(gtk::AccessibleRole::Presentation);
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text(i18n_clear())
            .css_classes(["flat", "circular", "ask-clear"])
            .focus_on_click(false)
            .visible(false)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        root.add_css_class("ask-entry");
        root.set_hexpand(true);
        root.append(&line_slot(&icon));
        root.append(&stack);
        root.append(&line_slot(&clear));

        view.update_property(&[gtk::accessible::Property::Placeholder(
            placeholder.label().as_str(),
        )]);

        let input = Self(Rc::new(Inner {
            root,
            view,
            scroller,
            placeholder,
            clear,
            changed: RefCell::default(),
            activate: RefCell::default(),
            stop: RefCell::default(),
            pending: RefCell::default(),
            quiet: Cell::new(false),
        }));
        input.wire();
        input
    }

    fn weak(&self) -> Weak<Inner> {
        Rc::downgrade(&self.0)
    }

    fn wire(&self) {
        let i = &self.0;

        // One line in the middle of the old entry's height; MAX_LINES of
        // them before it scrolls. Measured once the font from CSS applies.
        let weak = self.weak();
        i.view.connect_map(move |view| {
            let Some(i) = weak.upgrade() else { return };
            let line = view.create_pango_layout(Some("Ag")).pixel_size().1.max(1);
            let pad = ((LINE_SLOT - line) / 2).max(4);
            view.set_top_margin(pad);
            view.set_bottom_margin(pad);
            i.placeholder.set_margin_top(pad);
            i.scroller
                .set_max_content_height(line * MAX_LINES + 2 * pad);
        });

        let weak = self.weak();
        i.view.buffer().connect_changed(move |_| {
            if let Some(i) = weak.upgrade()
                && !i.quiet.get()
            {
                PromptInput(i).on_changed();
            }
        });

        let weak = self.weak();
        i.clear.connect_clicked(move |_| {
            if let Some(i) = weak.upgrade() {
                let input = PromptInput(i);
                input.set_text("");
                input.grab_focus();
            }
        });

        // Enter asks or runs, Shift+Enter is a new line, Escape stops.
        // An input method composing text gets the key first.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.weak();
        keys.connect_key_pressed(move |keys, key, _, mods| {
            use gtk::gdk::Key;
            let Some(i) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                Key::Return | Key::KP_Enter | Key::ISO_Enter
                    if !mods.contains(gtk::gdk::ModifierType::SHIFT_MASK) =>
                {
                    // Only Enter is asked of the input method here: the
                    // text view filters every other key itself.
                    let composing = keys
                        .current_event()
                        .is_some_and(|e| i.view.im_context_filter_keypress(&e));
                    if !composing {
                        PromptInput(i).emit(|i| &i.activate);
                    }
                    glib::Propagation::Stop
                }
                Key::Escape => {
                    PromptInput(i).emit(|i| &i.stop);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        i.view.add_controller(keys);
    }

    fn on_changed(&self) {
        let i = &self.0;
        let empty = i.view.buffer().char_count() == 0;
        i.placeholder.set_visible(empty);
        i.clear.set_visible(!empty);
        if let Some(id) = i.pending.take() {
            id.remove();
        }
        // Like a search entry: emptied reports at once, typing after a
        // pause.
        if empty {
            self.emit(|i| &i.changed);
            return;
        }
        let weak = self.weak();
        let id = glib::timeout_add_local_once(CHANGED_DELAY, move || {
            if let Some(i) = weak.upgrade() {
                i.pending.take();
                PromptInput(i).emit(|i| &i.changed);
            }
        });
        i.pending.replace(Some(id));
    }

    fn emit(&self, which: impl Fn(&Inner) -> &RefCell<Vec<Handler>>) {
        for f in which(&self.0).borrow().iter() {
            f(self);
        }
    }

    /// The whole thing, to put in a row.
    pub fn widget(&self) -> &gtk::Box {
        &self.0.root
    }

    /// The text box itself (the focus, and what assistive tech names).
    pub fn view(&self) -> &gtk::TextView {
        &self.0.view
    }

    pub fn text(&self) -> glib::GString {
        let buffer = self.0.view.buffer();
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), false)
    }

    pub fn set_text(&self, text: &str) {
        if self.text() != text {
            self.0.quiet.set(true);
            self.0.view.buffer().set_text(text);
            self.0.quiet.set(false);
            self.on_changed();
        }
    }

    /// Put the cursor at `pos` characters in; -1 is the end.
    pub fn set_position(&self, pos: i32) {
        let buffer = self.0.view.buffer();
        let iter = if pos < 0 {
            buffer.end_iter()
        } else {
            buffer.iter_at_offset(pos)
        };
        buffer.place_cursor(&iter);
        self.0.view.scroll_mark_onscreen(&buffer.get_insert());
    }

    pub fn grab_focus(&self) -> bool {
        self.0.view.grab_focus()
    }

    pub fn set_placeholder_text<S: AsRef<str>>(&self, text: Option<S>) {
        let text = text.as_ref().map_or("", |t| t.as_ref());
        self.0.placeholder.set_label(text);
        self.0
            .view
            .update_property(&[gtk::accessible::Property::Placeholder(text)]);
    }

    /// The text changed (after a short pause while typing).
    pub fn connect_search_changed(&self, f: impl Fn(&PromptInput) + 'static) {
        self.0.changed.borrow_mut().push(Box::new(f));
    }

    /// Enter.
    pub fn connect_activate(&self, f: impl Fn(&PromptInput) + 'static) {
        self.0.activate.borrow_mut().push(Box::new(f));
    }

    /// Escape.
    pub fn connect_stop_search(&self, f: impl Fn(&PromptInput) + 'static) {
        self.0.stop.borrow_mut().push(Box::new(f));
    }
}

fn i18n_clear() -> String {
    umoja_common::i18n::umoja("Clear")
}
