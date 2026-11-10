//! The overlay window: search entry, app grid and assistant answers.

use crate::assistant::agent::{self, Question, Reply, StepState};
use crate::assistant::tools::Risk;
use crate::clipboard::{self, RowAction};
use crate::context::{self, Attachment, FocusedWindow, Kind, OcrError, Transform};
use crate::lemonade::{self, ChatEvent, LemonadeClient, RUNTIME};
use crate::myna::{Myna, MynaEvent};
use crate::screenshot::ScreenshotUi;
use adw::prelude::*;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Command;
use std::rc::{Rc, Weak};
use std::time::Duration;
use umoja_common::gsettings as gs;
use umoja_common::i18n;
use umoja_common::types::{AssistantAction, extract_actions, strip_actions};

/// Most apps shown for a search. With no search the grid lists every app
/// and scrolls, like GNOME's app grid.
const MAX_RESULTS: usize = 48;
/// Earlier messages sent with a follow-up (three exchanges).
const MAX_HISTORY: usize = 6;
/// How much of an earlier message (with its attached context) is kept.
const MAX_HISTORY_CHARS: usize = 6000;
/// How long the overlay hides before a capture so it isn't in the shot.
const STEP_ASIDE: Duration = Duration::from_millis(250);

pub struct Overlay {
    model_status: gtk::Label,
    // An Adwaita window so dialogs (the "Run this command?" prompt) show
    // inside the overlay; on a plain GTK window they became a separate
    // toplevel underneath the overlay layer.
    window: adw::ApplicationWindow,
    entry: gtk::SearchEntry,
    grid: gtk::FlowBox,
    ask_button: gtk::Button,
    answer: gtk::Label,
    /// One line per tool call ("✓ Turn on Dark Style").
    steps: gtk::Box,
    chips: gtk::FlowBox,
    assistant: gtk::Box,
    /// Matching files from umoja-files, under the assistant card.
    files: Rc<crate::files::FilesSection>,
    myna_pill: gtk::Button,
    apps: RefCell<Vec<gio::AppInfo>>,
    /// Held for the overlay's lifetime: the monitor is a weak singleton,
    /// and once the last reference drops it stops emitting `changed`.
    app_monitor: gio::AppInfoMonitor,
    myna: Myna,
    /// Bumped on every new question so stale streams are dropped.
    generation: Cell<u64>,
    /// The next question goes to the local model (crash diagnosis).
    local_once: Cell<bool>,
    // Context attached to the next question (window, area, selection...).
    context_button: gtk::MenuButton,
    pick_selection: gtk::Button,
    pick_window: (gtk::Button, gtk::Label),
    pick_area: gtk::Button,
    pick_clipboard: gtk::Button,
    attach_row: gtk::Box,
    pending: gtk::Box,
    pending_label: gtk::Label,
    attachments: RefCell<Vec<Attachment>>,
    /// The window the user was in when the overlay opened.
    last_window: RefCell<Option<FocusedWindow>>,
    shots: RefCell<Weak<ScreenshotUi>>,
    // Clipboard history page, in place of the app grid.
    history: Rc<clipboard::History>,
    clip_button: gtk::ToggleButton,
    stack: gtk::Stack,
    clip_list: gtk::ListBox,
    clip_empty: gtk::Label,
    copy_button: gtk::Button,
    answer_text: RefCell<String>,
    /// The conversation shown in the assistant card: earlier user and
    /// assistant messages, sent along with follow-ups. Cleared when the
    /// card goes away.
    conversation: RefCell<Vec<serde_json::Value>>,
    /// Bumped by show() and hide(): a hide waiting for its clean frame
    /// only goes ahead if nothing reopened the overlay meanwhile.
    hide_serial: Rc<Cell<u32>>,
    /// The last answer asked the user something: the search entry is a
    /// reply box until it's answered.
    awaiting_reply: Cell<bool>,
    /// This conversation started on the local model (crash diagnosis), so
    /// its follow-ups stay local too.
    local_chat: Cell<bool>,
    /// Where `AnswerFinished` / `ConfirmShown` go (main.rs emits them on
    /// D-Bus, for tools/demo).
    events: RefCell<Option<Rc<dyn Fn(&str, glib::Variant)>>>,
    /// The current answer's tool steps: id -> (state, label).
    step_log: RefCell<std::collections::BTreeMap<usize, (String, String)>>,
    /// The open "Allow the assistant to do this?" dialog, closed (declined)
    /// when the overlay hides.
    confirm_dialog: RefCell<Option<adw::AlertDialog>>,
}

/// Width of an app grid tile, icon plus label, as in GNOME (about 128px).
const TILE_WIDTH: i32 = 128;

impl Overlay {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-overlay"])
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-overlay");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        umoja_common::blur::blur_behind(&window);

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(i18n::umoja("Search apps, or ask anything…"))
            .width_request(560)
            .halign(gtk::Align::Center)
            .build();
        let myna_pill = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .tooltip_text(i18n::umoja("Dictate with Myna"))
            .css_classes(["circular", "myna-pill"])
            .build();
        let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        search_row.set_halign(gtk::Align::Center);
        // "Add context" menu and the clipboard history toggle.
        let pick_selection =
            menu_item(&["edit-select-all-symbolic"], &i18n::umoja("Selected Text")).0;
        let pick_window = menu_item(
            &["focus-windows-symbolic", "window-new-symbolic"],
            &i18n::umoja("Current Window"),
        );
        let pick_area = menu_item(
            &["screenshot-recorded-symbolic", "camera-photo-symbolic"],
            &i18n::umoja("Screen Area…"),
        )
        .0;
        let pick_clipboard =
            menu_item(&["edit-paste-symbolic"], &i18n::umoja("Last Copied Text")).0;
        let picks = gtk::Box::new(gtk::Orientation::Vertical, 2);
        picks.set_margin_top(6);
        picks.set_margin_bottom(6);
        picks.set_margin_start(6);
        picks.set_margin_end(6);
        picks.append(
            &gtk::Label::builder()
                .label(i18n::umoja("Ask About"))
                .xalign(0.0)
                .margin_start(8)
                .margin_bottom(4)
                .css_classes(["heading"])
                .build(),
        );
        for b in [&pick_selection, &pick_window.0, &pick_area, &pick_clipboard] {
            picks.append(b);
        }
        let context_button = gtk::MenuButton::builder()
            .icon_name("mail-attachment-symbolic")
            .tooltip_text(i18n::umoja("Ask about something on screen"))
            .css_classes(["circular", "context-button"])
            .popover(&gtk::Popover::builder().child(&picks).build())
            .build();
        let clip_button = gtk::ToggleButton::builder()
            .icon_name("edit-paste-symbolic")
            .tooltip_text(i18n::umoja("Clipboard History"))
            .css_classes(["circular", "clip-button"])
            .build();
        search_row.append(&entry);
        search_row.append(&context_button);
        search_row.append(&clip_button);
        search_row.append(&myna_pill);
        search_row.append(&crate::model_picker::button());

        // "Downloading the assistant's model… 42%" while it's fetched.
        let model_status = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .css_classes(["dim-label", "model-status"])
            .visible(false)
            .build();

        // Attached context, as removable pills under the search entry.
        let attach_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        attach_row.set_halign(gtk::Align::Center);
        attach_row.add_css_class("attach-row");
        let pending_label = gtk::Label::new(None);
        let pending = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pending.add_css_class("attach-pill");
        pending.append(&gtk::Spinner::builder().spinning(true).build());
        pending.append(&pending_label);
        pending.set_visible(false);
        let attach_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        attach_box.set_halign(gtk::Align::Center);
        attach_box.append(&attach_row);
        attach_box.append(&pending);

        let ask_button = gtk::Button::builder()
            .label(i18n::umoja("Ask the assistant"))
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .visible(false)
            .build();
        let answer = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .max_width_chars(80)
            .build();
        let steps = gtk::Box::new(gtk::Orientation::Vertical, 4);
        steps.add_css_class("assistant-steps");
        steps.set_visible(false);
        let chips = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(4)
            .build();
        let copy_button = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(i18n::umoja("Copy"))
            .css_classes(["flat", "circular"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .visible(false)
            .build();
        let answer_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        answer.set_hexpand(true);
        answer_row.append(&answer);
        answer_row.append(&copy_button);
        let assistant = gtk::Box::new(gtk::Orientation::Vertical, 12);
        assistant.add_css_class("assistant-card");
        assistant.set_halign(gtk::Align::Center);
        assistant.set_width_request(640);
        assistant.append(&steps);
        assistant.append(&answer_row);
        assistant.append(&chips);
        assistant.set_visible(false);

        // Like GNOME's app grid: a compact block of fixed-width tiles in the
        // middle of the screen. Centering the FlowBox gives it its natural
        // width (at most 8 tiles), so on a wide monitor the leftover space
        // goes to the sides instead of between the icons.
        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(8)
            .min_children_per_line(4)
            .homogeneous(true)
            .row_spacing(12)
            .column_spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&grid)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let clip_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["clip-list"])
            .build();
        let clip_empty = gtk::Label::builder()
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .vexpand(true)
            .valign(gtk::Align::Start)
            .margin_top(48)
            .build();
        let clip_clear = gtk::Button::builder()
            .label(i18n::umoja("Clear History"))
            .css_classes(["pill", "flat"])
            .halign(gtk::Align::End)
            .build();
        let clip_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        clip_header.append(
            &gtk::Label::builder()
                .label(i18n::umoja("Clipboard History"))
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["title-4"])
                .build(),
        );
        clip_header.append(&clip_clear);
        let clip_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        clip_page.set_halign(gtk::Align::Center);
        clip_page.set_width_request(760);
        clip_page.append(&clip_header);
        clip_page.append(&clip_empty);
        clip_page.append(
            &gtk::ScrolledWindow::builder()
                .child(&clip_list)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        stack.add_named(&scroller, Some("apps"));
        stack.add_named(&clip_page, Some("clipboard"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 24);
        root.set_margin_top(64);
        root.set_margin_start(96);
        root.set_margin_end(96);
        root.set_margin_bottom(48);
        root.append(&search_row);
        root.append(&attach_box);
        root.append(&model_status);
        root.append(&ask_button);
        root.append(&assistant);
        root.append(&stack);
        window.set_content(Some(&root));

        let overlay = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| Self {
            files: crate::files::FilesSection::new({
                let weak = weak.clone();
                move || {
                    if let Some(o) = weak.upgrade() {
                        o.hide();
                    }
                }
            }),
            window,
            entry,
            grid,
            ask_button,
            answer,
            steps,
            chips,
            assistant,
            myna_pill,
            model_status,
            apps: RefCell::new(load_apps()),
            app_monitor: gio::AppInfoMonitor::get(),
            myna: Myna::start(),
            generation: Cell::new(0),
            local_once: Cell::new(false),
            events: RefCell::new(None),
            step_log: RefCell::new(Default::default()),
            confirm_dialog: RefCell::new(None),
            context_button,
            pick_selection,
            pick_window,
            pick_area,
            pick_clipboard,
            attach_row,
            pending,
            pending_label,
            attachments: RefCell::default(),
            last_window: RefCell::default(),
            shots: RefCell::new(Weak::new()),
            history: clipboard::history(),
            clip_button,
            stack,
            clip_list,
            clip_empty,
            copy_button,
            answer_text: RefCell::default(),
            conversation: RefCell::default(),
            awaiting_reply: Cell::new(false),
            local_chat: Cell::new(false),
            hide_serial: Rc::new(Cell::new(0)),
        });
        let weak = Rc::downgrade(&overlay);
        clip_clear.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.history.clear();
            }
        });
        root.insert_child_after(&overlay.files.widget, Some(&overlay.assistant));
        overlay.connect_signals();
        overlay.connect_context();
        // The assistant's read_focused_window tool steps the overlay aside too.
        let weak = Rc::downgrade(&overlay);
        context::set_window_reader(Rc::new(move |rect| {
            let weak = weak.clone();
            Box::pin(async move {
                let o = weak.upgrade().ok_or(OcrError::CaptureFailed)?;
                let image = o.capture_aside(Some(rect)).await?;
                context::blocking(move || context::ocr_file(&image.0)).await
            })
        }));
        crate::assistant::capture::set_capturer(Rc::new(OverlayCapturer(Rc::downgrade(&overlay))));
        overlay.refresh_results();
        overlay
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.entry.connect_search_changed(move |_| {
            if let Some(o) = weak.upgrade() {
                o.refresh_results();
            }
        });

        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |e| {
            let Some(o) = weak.upgrade() else { return };
            let q = e.text().to_string();
            // Answering the assistant's question.
            if o.awaiting_reply.get() && !q.trim().is_empty() {
                o.ask(&q);
                return;
            }
            if crate::files::looks_like_file_search(&q) && o.files.activate_first() {
                return;
            }
            if !o.attachments.borrow().is_empty()
                || lemonade::looks_like_question(&q)
                || o.grid.first_child().is_none()
            {
                o.ask(&q);
            } else if let Some(app) = o.matches(&q).into_iter().next() {
                o.launch(&app);
            }
        });

        let weak = Rc::downgrade(self);
        self.entry.connect_stop_search(move |_| {
            if let Some(o) = weak.upgrade() {
                o.hide();
            }
        });

        let weak = Rc::downgrade(self);
        self.ask_button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                let q = o.entry.text().to_string();
                o.ask(&q);
            }
        });

        let weak = Rc::downgrade(self);
        self.myna_pill.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.myna.toggle();
            }
        });

        // Escape anywhere closes the overlay.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(o) = weak.upgrade() {
                    o.hide();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        // Rescan apps when packages are installed or removed.
        let weak = Rc::downgrade(self);
        self.app_monitor.connect_changed(move |_| {
            if let Some(o) = weak.upgrade() {
                *o.apps.borrow_mut() = load_apps();
                o.refresh_results();
            }
        });

        let weak = Rc::downgrade(self);
        let events = self.myna.events.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = events.recv().await {
                let Some(o) = weak.upgrade() else { break };
                o.on_myna(ev);
            }
        });
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn show(self: &Rc<Self>) {
        self.hide_serial.set(self.hide_serial.get().wrapping_add(1));
        self.end_conversation();
        self.entry.set_text("");
        self.assistant.set_visible(false);
        self.files.clear();
        self.clear_attachments();
        self.clip_button.set_active(false);
        self.remember_window();
        self.window.present();
        self.entry.grab_focus();
    }

    /// Bring the overlay back after a capture, keeping what was typed.
    fn reveal(&self) {
        self.window.present();
        self.entry.grab_focus();
        self.entry.set_position(-1);
    }

    pub fn set_screenshot_ui(&self, shots: &Rc<ScreenshotUi>) {
        *self.shots.borrow_mut() = Rc::downgrade(shots);
    }

    pub fn set_model_status(&self, status: Option<String>) {
        self.model_status.set_visible(status.is_some());
        self.model_status.set_text(status.as_deref().unwrap_or(""));
    }

    /// Open the app search with `text` already typed (from the overview).
    pub fn show_with_text(self: &Rc<Self>, text: &str) {
        self.show();
        self.entry.set_text(text);
        self.entry.set_position(-1);
    }

    /// Open the overlay with `question` typed and asked on the local model,
    /// whatever backend is selected: the panel's crash notification uses
    /// it for "Ask Why", and crash reports stay on this computer.
    pub fn ask_now(self: &Rc<Self>, question: &str) {
        self.show_with_text(question);
        self.local_once.set(true);
        self.ask(question);
    }

    /// Like `ask_now`, for org.umoja.Overlay.AskWith: on the selected
    /// backend unless `local`, and with `typed` the question is typed into
    /// the entry a few letters at a time first, as a person would.
    pub fn ask_with(self: &Rc<Self>, question: &str, local: bool, typed: bool) {
        if !typed {
            self.show_with_text(question);
            self.local_once.set(local);
            self.ask(question);
            return;
        }
        self.show();
        self.entry.set_text("");
        let generation = self.generation.get();
        let chars: Vec<char> = question.chars().collect();
        let weak = Rc::downgrade(self);
        let question = question.to_string();
        glib::spawn_future_local(async move {
            for n in 1..=chars.len() {
                glib::timeout_future(std::time::Duration::from_millis(38)).await;
                let Some(o) = weak.upgrade() else { return };
                if o.generation.get() != generation || !o.is_visible() {
                    return;
                }
                let typed: String = chars[..n].iter().collect();
                o.entry.set_text(&typed);
                o.entry.set_position(-1);
            }
            glib::timeout_future(std::time::Duration::from_millis(350)).await;
            let Some(o) = weak.upgrade() else { return };
            if o.generation.get() != generation {
                return;
            }
            o.local_once.set(local);
            o.ask(&question);
        });
    }

    pub fn set_event_sink(&self, sink: impl Fn(&str, glib::Variant) + 'static) {
        self.events.replace(Some(Rc::new(sink)));
    }

    fn emit(&self, signal: &str, params: glib::Variant) {
        let sink = self.events.borrow().clone();
        if let Some(sink) = sink {
            sink(signal, params);
        }
    }

    pub fn hide(&self) {
        self.generation.set(self.generation.get() + 1);
        // A question still waiting for Allow is declined, so the dialog
        // can't come back (and be allowed) on a later, unrelated question.
        if let Some(d) = self.confirm_dialog.take() {
            d.force_close();
        }
        let serial = self.hide_serial.get().wrapping_add(1);
        self.hide_serial.set(serial);
        if !self.window.is_visible() {
            return;
        }
        // The compositor keeps an unmapped surface's last frame and shows it
        // for a moment when the overlay opens again, so the previous answer
        // flashed up before the app grid (plainly visible in screencasts).
        // Paint one clean frame first, then unmap.
        self.entry.set_text("");
        self.assistant.set_visible(false);
        let window = self.window.clone();
        let pending = self.hide_serial.clone();
        let done = Rc::new(Cell::new(false));
        let finish = {
            let done = done.clone();
            move || {
                if !done.replace(true) && pending.get() == serial {
                    window.set_visible(false);
                }
            }
        };
        let finish = Rc::new(finish);
        if let Some(clock) = self.window.frame_clock() {
            let f = finish.clone();
            let id = Rc::new(RefCell::new(None::<glib::SignalHandlerId>));
            let id2 = id.clone();
            let c2 = clock.clone();
            *id.borrow_mut() = Some(clock.connect_after_paint(move |_| {
                if let Some(h) = id2.borrow_mut().take() {
                    c2.disconnect(h);
                }
                f();
            }));
            self.window.queue_draw();
        }
        // In case no frame comes (the surface isn't being drawn).
        glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || finish());
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    pub fn toggle_dictation(&self) {
        self.myna.toggle();
    }

    fn on_myna(self: &Rc<Self>, ev: MynaEvent) {
        match ev {
            MynaEvent::Available(a) => self.myna_pill.set_sensitive(a),
            MynaEvent::Listening(on) => {
                if on {
                    self.myna_pill.add_css_class("listening");
                } else {
                    self.myna_pill.remove_css_class("listening");
                }
            }
        }
    }

    fn matches(&self, query: &str) -> Vec<gio::AppInfo> {
        let q = query.trim().to_lowercase();
        let apps = self.apps.borrow();
        if q.is_empty() {
            return apps.clone();
        }
        let mut scored: Vec<(u8, gio::AppInfo)> = apps
            .iter()
            .filter_map(|a| {
                let name = a.display_name().to_lowercase();
                let score = if name.starts_with(&q) {
                    0
                } else if name.contains(&q) {
                    1
                } else if haystack(a).contains(&q) {
                    2
                } else {
                    return None;
                };
                Some((score, a.clone()))
            })
            .collect();
        scored.sort_by_key(|(s, a)| (*s, a.display_name().to_lowercase()));
        scored.into_iter().map(|(_, a)| a).collect()
    }

    fn refresh_results(self: &Rc<Self>) {
        while let Some(c) = self.grid.first_child() {
            self.grid.remove(&c);
        }
        let q = self.entry.text().to_string();
        let found = self.matches(&q);
        let limit = if q.trim().is_empty() {
            usize::MAX
        } else {
            MAX_RESULTS
        };
        for app in found.iter().take(limit) {
            self.grid.append(&self.app_tile(app));
        }
        let attached = !self.attachments.borrow().is_empty();
        let replying = self.awaiting_reply.get();
        self.ask_button.set_label(&if replying {
            i18n::umoja("Reply")
        } else {
            i18n::umoja("Ask the assistant")
        });
        self.ask_button.set_visible(
            attached
                || (!q.trim().is_empty()
                    && (replying || found.is_empty() || lemonade::looks_like_question(&q))),
        );
        // Emptying the search closes the answer, unless it's waiting for
        // the user's reply.
        if q.trim().is_empty()
            && !attached
            && !replying
            && self.stack.visible_child_name().as_deref() != Some("clipboard")
        {
            self.assistant.set_visible(false);
            self.conversation.borrow_mut().clear();
        }
        self.files.update(&q);
        self.refresh_clipboard();
    }

    fn app_tile(self: &Rc<Self>, app: &gio::AppInfo) -> gtk::Widget {
        let image = match app.icon() {
            Some(icon) => gtk::Image::from_gicon(&icon),
            None => gtk::Image::from_icon_name("application-x-executable"),
        };
        image.set_pixel_size(64);
        let label = gtk::Label::builder()
            .label(app.display_name())
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(12)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&image);
        content.append(&label);
        let button = gtk::Button::builder()
            .child(&content)
            .width_request(TILE_WIDTH)
            .css_classes(["flat", "app-tile"])
            .build();
        let weak = Rc::downgrade(self);
        let app = app.clone();
        let app_c = app.clone();
        button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.launch(&app_c);
            }
        });

        // Right-click (or long-press) menu, as in GNOME's app grid.
        if let Ok(desktop) = app.clone().downcast::<gio::DesktopAppInfo>() {
            let menu = gtk::GestureClick::builder().button(3).build();
            let (weak, d) = (Rc::downgrade(self), desktop.clone());
            menu.connect_pressed(move |g, _, _, _| {
                if let (Some(o), Some(w)) = (weak.upgrade(), g.widget()) {
                    o.show_tile_menu(&w, &d);
                }
            });
            button.add_controller(menu);
            let press = gtk::GestureLongPress::builder().touch_only(true).build();
            let weak = Rc::downgrade(self);
            press.connect_pressed(move |g, _, _| {
                if let (Some(o), Some(w)) = (weak.upgrade(), g.widget()) {
                    o.show_tile_menu(&w, &desktop);
                }
            });
            button.add_controller(press);
        }
        button.upcast()
    }

    fn launch(&self, app: &gio::AppInfo) {
        let ctx = umoja_common::app_scope::scoped(
            gtk::prelude::WidgetExt::display(&self.window).app_launch_context(),
        );
        if let Err(e) = app.launch(&[], Some(&ctx)) {
            tracing::warn!(error = %e, "launch failed");
        }
        self.hide();
    }

    /// GNOME's app-grid menu: the app's desktop actions, Pin to Dock /
    /// Unpin (org.gnome.shell favorite-apps, which the dock follows) and
    /// App Details.
    fn show_tile_menu(self: &Rc<Self>, parent: &gtk::Widget, app: &gio::DesktopAppInfo) {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let popover = gtk::Popover::builder().child(&col).build();
        let weak_pop = popover.downgrade();
        let item = |label: &str, action: Box<dyn Fn()>| {
            let b = gtk::Button::builder()
                .child(&gtk::Label::builder().label(label).xalign(0.0).build())
                .css_classes(["flat"])
                .build();
            let pop = weak_pop.clone();
            b.connect_clicked(move |_| {
                if let Some(p) = pop.upgrade() {
                    p.popdown();
                }
                action();
            });
            col.append(&b);
        };

        let actions = app.list_actions();
        for action in &actions {
            let label = app.action_name(action).to_string();
            let (weak, a, action) = (Rc::downgrade(self), app.clone(), action.to_string());
            item(
                &label,
                Box::new(move || {
                    if let Some(o) = weak.upgrade() {
                        let ctx = umoja_common::app_scope::scoped(
                            gtk::prelude::WidgetExt::display(&o.window).app_launch_context(),
                        );
                        a.launch_action(&action, Some(&ctx));
                        o.hide();
                    }
                }),
            );
        }
        if !actions.is_empty() {
            col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }

        let id = app.id().map(|s| s.to_string()).unwrap_or_default();
        let pinned = umoja_common::apps::is_favorite(&id);
        let label = if pinned {
            i18n::shell("Unpin")
        } else {
            i18n::dock("Pin to Dock")
        };
        let pin_id = id.clone();
        item(
            &label,
            Box::new(move || umoja_common::apps::set_favorite(&pin_id, !pinned)),
        );
        if let Some(details) = umoja_common::apps::details_for(app) {
            let weak = Rc::downgrade(self);
            item(
                &i18n::shell("App Details"),
                Box::new(move || {
                    umoja_common::apps::show_details(&details);
                    if let Some(o) = weak.upgrade() {
                        o.hide();
                    }
                }),
            );
        }

        popover.set_parent(parent);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// Answer a question, letting the assistant act through its tools. Any
    /// attached context goes with it.
    fn ask(self: &Rc<Self>, query: &str) {
        // A reply to the assistant's question: any attached context already
        // went with the first message.
        let attachments = if self.awaiting_reply.get() {
            Vec::new()
        } else {
            self.attachments.borrow().clone()
        };
        let text = match query.trim() {
            "" if !attachments.is_empty() => context::DEFAULT_QUESTION.to_string(),
            q => q.to_string(),
        };
        self.ask_question(Question {
            text,
            context: context::build_context(&attachments),
            history: Vec::new(),
        });
    }

    /// Forget the conversation: the next question starts afresh.
    fn end_conversation(&self) {
        self.conversation.borrow_mut().clear();
        self.local_chat.set(false);
        self.set_awaiting_reply(false);
    }

    fn set_awaiting_reply(&self, on: bool) {
        self.awaiting_reply.set(on);
        self.entry.set_placeholder_text(Some(&if on {
            i18n::umoja("Reply to the assistant…")
        } else {
            i18n::umoja("Search apps, or ask anything…")
        }));
    }

    /// "Yes" / "No" under an answer that asks a yes-or-no question.
    fn reply_chip(self: &Rc<Self>, label: &str, suggested: bool) -> gtk::Widget {
        let button = gtk::Button::builder()
            .label(label)
            .css_classes(if suggested {
                ["pill", "reply-chip", "suggested"]
            } else {
                ["pill", "reply-chip", "flat"]
            })
            .build();
        let weak = Rc::downgrade(self);
        let text = label.to_string();
        button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.ask(&text);
            }
        });
        button.upcast()
    }

    /// [`Self::ask`] with optional context (selected text, window contents).
    pub fn ask_question(self: &Rc<Self>, mut question: Question) {
        if question.text.trim().is_empty() {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        // While the card is up, a new message continues the conversation:
        // "yes" answers "Do you want me to open it?".
        if question.history.is_empty() && self.assistant.is_visible() {
            question.history = self.conversation.borrow().clone();
        }
        if question.history.is_empty() {
            self.conversation.borrow_mut().clear();
            self.local_chat.set(false);
        }
        self.set_awaiting_reply(false);
        let asked = context::truncate_chars(&agent::user_content(&question), MAX_HISTORY_CHARS);

        self.reset_card();
        self.answer.set_text(&i18n::umoja("Thinking…"));
        // Typed-query matches would disagree with the answer; the
        // search_files tool fills the list with its own results instead.
        self.files.clear();
        self.step_log.borrow_mut().clear();
        crate::assistant::take_hide_request();

        let ui: Rc<dyn agent::Ui> = Rc::new(OverlayUi {
            overlay: Rc::downgrade(self),
            generation,
            rows: RefCell::new(HashMap::new()),
        });
        // Fresh per request: picks up model and backend changes.
        let local = self.local_once.replace(false) || self.local_chat.get();
        self.local_chat.set(local);
        let client = if local {
            LemonadeClient::for_endpoint(umoja_common::assistant_backend::Endpoint::local(), None)
        } else {
            LemonadeClient::new()
        };
        let weak = Rc::downgrade(self);
        let coding =
            umoja_common::agents::looks_like_coding(&question.text).then(|| question.text.clone());
        glib::spawn_future_local(async move {
            let text = agent::run(client, question, ui).await;
            let Some(o) = weak.upgrade() else { return };
            if o.generation.get() != generation {
                return;
            }
            // Older models may still answer with action JSON: offer chips.
            for action in extract_actions(&text) {
                o.chips.append(&o.chip(action));
            }
            // A coding job: offer to carry on in the user's coding agent.
            if let Some(task) = coding
                && let Some(agent) = umoja_common::agents::chosen()
            {
                o.chips.append(&o.agent_chip(agent, task));
            }
            if text.is_empty() && o.answer.text() == i18n::umoja("Thinking…") {
                o.answer.set_visible(false);
            }
            let shown = strip_actions(&text);
            o.copy_button.set_visible(!shown.trim().is_empty());
            *o.answer_text.borrow_mut() = shown.clone();
            let steps: Vec<String> = o
                .step_log
                .borrow()
                .values()
                .map(|(state, label)| format!("{state}:{label}"))
                .collect();
            o.emit("AnswerFinished", (shown.trim(), steps).to_variant());
            // A tool opened an app or moved a window: get out of the way.
            if crate::assistant::take_hide_request() {
                o.hide();
                return;
            }
            if shown.trim().is_empty() {
                return;
            }
            {
                let mut conv = o.conversation.borrow_mut();
                conv.push(serde_json::json!({"role": "user", "content": asked}));
                conv.push(serde_json::json!({"role": "assistant", "content": shown.trim()}));
                let extra = conv.len().saturating_sub(MAX_HISTORY);
                conv.drain(..extra);
            }
            // The assistant asked something: the search entry becomes the
            // reply box, with Yes / No for a yes-or-no question.
            if let Some(kind) = agent::expects_reply(&shown) {
                o.set_awaiting_reply(true);
                o.entry.set_text("");
                o.entry.grab_focus();
                if kind == Reply::YesNo {
                    o.chips.append(&o.reply_chip(&i18n::umoja("Yes"), true));
                    o.chips.append(&o.reply_chip(&i18n::umoja("No"), false));
                }
            }
        });
    }

    /// Translate, reformat, summarize or explain `text`. A plain chat with
    /// its own system prompt: no tools, no desktop actions.
    fn transform(self: &Rc<Self>, text: String, t: Transform) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.end_conversation();
        self.reset_card();
        self.answer.set_text(&i18n::umoja("Thinking…"));

        let messages = vec![
            serde_json::json!({"role": "system", "content": t.system_prompt(&context::user_language())}),
            serde_json::json!({"role": "user", "content": context::truncate_chars(&text, context::MAX_CONTEXT_CHARS)}),
        ];
        let (tx, rx) = async_channel::unbounded();
        // Fresh per request: picks up model and backend changes.
        let client = LemonadeClient::new();
        RUNTIME.spawn(async move { client.stream_chat(messages, Vec::new(), tx).await });

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut text = String::new();
            while let Ok(ev) = rx.recv().await {
                let Some(o) = weak.upgrade() else { return };
                if o.generation.get() != generation {
                    return; // superseded; dropping rx cancels the stream
                }
                match ev {
                    ChatEvent::Text(d) => {
                        text.push_str(&d);
                        o.answer.set_text(&context::strip_fences(&text));
                    }
                    ChatEvent::ToolCalls(_) => {}
                    ChatEvent::Done => {
                        let shown = context::strip_fences(&text);
                        o.answer.set_text(&shown);
                        o.copy_button.set_visible(!shown.is_empty());
                        *o.answer_text.borrow_mut() = shown;
                        return;
                    }
                    ChatEvent::Error(e) => {
                        o.answer.set_text(&e);
                        return;
                    }
                }
            }
        });
    }

    fn reset_card(&self) {
        self.assistant.set_visible(true);
        self.answer.set_visible(true);
        self.copy_button.set_visible(false);
        self.answer_text.borrow_mut().clear();
        for b in [
            self.chips.upcast_ref::<gtk::Widget>(),
            self.steps.upcast_ref(),
        ] {
            while let Some(c) = b.first_child() {
                c.unparent();
            }
        }
        self.steps.set_visible(false);
    }

    fn show_error(&self, text: &str) {
        self.generation.set(self.generation.get() + 1);
        self.end_conversation();
        self.reset_card();
        self.answer.set_text(text);
    }

    // ---- Context: window, screen area, selection, clipboard ----------------

    fn connect_context(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.pick_selection.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.context_button.popdown();
                o.attach_selection();
            }
        });
        let weak = Rc::downgrade(self);
        self.pick_window.0.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.context_button.popdown();
                o.attach_window();
            }
        });
        let weak = Rc::downgrade(self);
        self.pick_area.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.context_button.popdown();
                o.attach_area();
            }
        });
        let weak = Rc::downgrade(self);
        self.pick_clipboard.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.context_button.popdown();
                if let Some(text) = o.history.latest() {
                    o.add_attachment(Attachment::new(Kind::Clipboard, text));
                }
            }
        });
        if let Some(popover) = self.context_button.popover() {
            let weak = Rc::downgrade(self);
            popover.connect_show(move |_| {
                if let Some(o) = weak.upgrade() {
                    o.refresh_context_menu();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.clip_button.connect_toggled(move |b| {
            if let Some(o) = weak.upgrade() {
                o.stack
                    .set_visible_child_name(if b.is_active() { "clipboard" } else { "apps" });
                o.refresh_clipboard();
            }
        });
        let weak = Rc::downgrade(self);
        self.history.connect_changed(move || {
            if let Some(o) = weak.upgrade() {
                o.refresh_clipboard();
            }
        });
        // Wayland only hands the clipboard to the focused client, so the
        // fallback read waits until the overlay actually has focus.
        let weak = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |w| {
            if w.is_active()
                && let Some(o) = weak.upgrade()
            {
                o.read_clipboard_fallback();
            }
        });
        let weak = Rc::downgrade(self);
        self.copy_button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                let text = o.answer_text.borrow().clone();
                o.copy_text(&text);
            }
        });
    }

    /// Note which window the user was in, for "Current Window".
    fn remember_window(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let win = context::focused_window().await;
            crate::assistant::capture::set_previous_window(win.as_ref().map(window_name));
            if let Some(o) = weak.upgrade() {
                *o.last_window.borrow_mut() = win;
            }
        });
    }

    /// Without a clipboard watcher, at least pick up what's on the clipboard
    /// and selected whenever the overlay (which has focus then) opens.
    fn read_clipboard_fallback(self: &Rc<Self>) {
        if self.history.is_watching() {
            return;
        }
        let display = gtk::prelude::WidgetExt::display(&self.window);
        let weak = Rc::downgrade(self);
        let clip = display.clipboard();
        let primary = display.primary_clipboard();
        glib::spawn_future_local(async move {
            match clip.read_text_future().await {
                Ok(Some(t)) => {
                    tracing::debug!(chars = t.len(), "read the clipboard on open");
                    if let Some(o) = weak.upgrade() {
                        o.history.add(t.to_string());
                    }
                }
                Ok(None) => tracing::debug!("clipboard has no text"),
                Err(e) => tracing::info!(error = %e, "couldn't read the clipboard"),
            }
            match primary.read_text_future().await {
                Ok(Some(t)) => {
                    if let Some(o) = weak.upgrade() {
                        o.history.set_selection(t.to_string());
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "couldn't read the primary selection"),
            }
        });
    }

    fn refresh_context_menu(&self) {
        self.pick_selection
            .set_sensitive(self.history.selection().is_some());
        let win = self.last_window.borrow();
        self.pick_window.1.set_text(&match win.as_ref() {
            Some(w) if !w.title.is_empty() => format!(
                "{} — {}",
                i18n::umoja("Current Window"),
                context::truncate_chars(&w.title, 32)
            ),
            _ => i18n::umoja("Current Window"),
        });
        self.pick_clipboard
            .set_sensitive(self.history.latest().is_some());
    }

    fn clear_attachments(self: &Rc<Self>) {
        self.attachments.borrow_mut().clear();
        self.rebuild_attachments();
    }

    fn add_attachment(self: &Rc<Self>, a: Attachment) {
        {
            let mut list = self.attachments.borrow_mut();
            // Re-attaching the same kind replaces it.
            list.retain(|x| std::mem::discriminant(&x.kind) != std::mem::discriminant(&a.kind));
            list.push(a);
        }
        self.rebuild_attachments();
        self.reveal();
    }

    fn rebuild_attachments(self: &Rc<Self>) {
        while let Some(c) = self.attach_row.first_child() {
            self.attach_row.remove(&c);
        }
        let list = self.attachments.borrow().clone();
        for (i, a) in list.iter().enumerate() {
            let pill = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            pill.add_css_class("attach-pill");
            pill.set_tooltip_text(Some(&context::truncate_chars(&a.text, 600)));
            pill.append(&gtk::Image::from_icon_name(a.icon()));
            pill.append(
                &gtk::Label::builder()
                    .label(a.label())
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(32)
                    .build(),
            );
            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(i18n::umoja("Remove"))
                .css_classes(["flat", "circular", "attach-remove"])
                .build();
            let weak = Rc::downgrade(self);
            remove.connect_clicked(move |_| {
                if let Some(o) = weak.upgrade() {
                    if i < o.attachments.borrow().len() {
                        o.attachments.borrow_mut().remove(i);
                    }
                    o.rebuild_attachments();
                }
            });
            pill.append(&remove);
            self.attach_row.append(&pill);
        }
        self.entry.set_placeholder_text(Some(&if list.is_empty() {
            i18n::umoja("Search apps, or ask anything…")
        } else {
            i18n::umoja("Ask about this, or press Enter to explain it…")
        }));
        self.refresh_results();
    }

    fn set_pending(&self, text: Option<&str>) {
        self.pending.set_visible(text.is_some());
        self.pending_label.set_text(text.unwrap_or(""));
    }

    fn attach_selection(self: &Rc<Self>) {
        match self.history.selection() {
            Some((text, _)) => self.add_attachment(Attachment::new(Kind::Selection, text)),
            None => self.show_error(&i18n::umoja(
                "Nothing is selected. Select some text in a window first.",
            )),
        }
    }

    /// Read the window the user was in. The overlay steps aside for the
    /// capture, then comes back.
    pub fn attach_window(self: &Rc<Self>) {
        let Some(win) = self.last_window.borrow().clone() else {
            self.read_screen(None, Kind::Area, i18n::umoja("Reading the screen…"));
            return;
        };
        let kind = Kind::Window {
            app_id: win.app_id.clone(),
            title: win.title.clone(),
        };
        self.read_screen(Some(win.rect), kind, i18n::umoja("Reading the window…"));
    }

    fn attach_area(self: &Rc<Self>) {
        let Some(shots) = self.shots.borrow().upgrade() else {
            return;
        };
        if !context::ocr_available() {
            self.ocr_failed(OcrError::NotInstalled);
            return;
        }
        self.window.set_visible(false);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(STEP_ASIDE, move || {
            shots.open_for_ask(Box::new(move |pixbuf| {
                let Some(o) = weak.upgrade() else { return };
                o.reveal();
                let Some(pixbuf) = pixbuf else { return };
                let path = context::temp_png();
                if pixbuf.savev(&path, "png", &[]).is_err() {
                    o.ocr_failed(OcrError::CaptureFailed);
                    return;
                }
                let image = context::TempImage(path);
                o.ocr_and_attach(image, Kind::Area, i18n::umoja("Reading the screen…"));
            }));
        });
    }

    /// Capture `rect` with the overlay out of the way, then OCR it.
    fn read_screen(
        self: &Rc<Self>,
        rect: Option<(i32, i32, i32, i32)>,
        kind: Kind,
        pending: String,
    ) {
        let o = self.clone();
        glib::spawn_future_local(async move {
            match o.capture_aside(rect).await {
                Ok(image) => o.ocr_and_attach(image, kind, pending),
                Err(e) => {
                    tracing::warn!(error = ?e, ?rect, "screen capture failed");
                    o.ocr_failed(e)
                }
            }
        });
    }

    /// Hide the overlay, grab `rect` (or the whole screen) and show it again.
    async fn capture_aside(
        self: &Rc<Self>,
        rect: Option<(i32, i32, i32, i32)>,
    ) -> Result<context::TempImage, OcrError> {
        if !context::ocr_available() {
            self.reveal();
            return Err(OcrError::NotInstalled);
        }
        let was_visible = self.is_visible();
        self.window.set_visible(false);
        // Give the compositor a frame or two to drop the overlay.
        glib::timeout_future(if was_visible {
            STEP_ASIDE
        } else {
            Duration::from_millis(16)
        })
        .await;
        let shot = context::blocking(move || context::capture(rect)).await;
        self.reveal();
        shot
    }

    /// The assistant's take_screenshot: step aside (unless the overlay
    /// itself was asked for), capture, come back.
    async fn assistant_screenshot(
        self: &Rc<Self>,
        target: crate::assistant::capture::Target,
        delay: u64,
    ) -> Result<String, String> {
        use crate::assistant::capture::Target;
        let shots = self
            .shots
            .borrow()
            .upgrade()
            .ok_or("screenshots aren't available")?;
        let was_visible = self.is_visible();
        let wait = Duration::from_secs(delay);
        let (region, what, raise) = match &target {
            Target::Overlay => (None, "the assistant".to_string(), None),
            Target::Screen => (None, "the screen".to_string(), None),
            Target::Area => (None, String::new(), None),
            Target::Window(Some(name)) => match context::find_window(name).await {
                Some((id, w)) => (Some(w.rect), window_name(&w), Some(id)),
                None => return Err(format!("no open window matches “{name}”")),
            },
            Target::Window(None) => match self.last_window.borrow().clone() {
                Some(w) => (Some(w.rect), window_name(&w), None),
                None => (None, "the screen (no window was open)".to_string(), None),
            },
        };
        if target == Target::Overlay {
            glib::timeout_future(wait).await;
            let path = context::blocking(|| crate::screenshot::grab_to_file(None)).await?;
            shots.announce(&path);
            return Ok(saved(&what, &path));
        }
        self.window.set_visible(false);
        if let Some(id) = raise {
            crate::screenshot::raise_window(id);
        }
        // Let the compositor drop the overlay (and raise the window).
        glib::timeout_future(
            wait + if was_visible || raise.is_some() {
                STEP_ASIDE
            } else {
                Duration::from_millis(16)
            },
        )
        .await;
        let result = if target == Target::Area {
            let (tx, rx) = async_channel::bounded(1);
            shots.pick_area(Box::new(move |p| {
                let _ = tx.try_send(p);
            }));
            match rx.recv().await.ok().flatten() {
                Some(p) => shots
                    .save_pixbuf(&p)
                    .map(|path| saved("the area the user picked", &path))
                    .ok_or_else(|| "the screenshot couldn't be saved".to_string()),
                None => Err("the user cancelled the screenshot".into()),
            }
        } else {
            match context::blocking(move || crate::screenshot::grab_to_file(region)).await {
                Ok(path) => {
                    shots.announce(&path);
                    Ok(saved(&what, &path))
                }
                Err(e) => Err(e),
            }
        };
        if was_visible {
            self.reveal();
        }
        result
    }

    async fn assistant_record(self: &Rc<Self>, start: bool) -> Result<String, String> {
        let shots = self
            .shots
            .borrow()
            .upgrade()
            .ok_or("screen recording isn't available")?;
        let rec = shots.recorder().clone();
        if !start {
            if !rec.is_recording() {
                return Err("no screen recording is running".into());
            }
            rec.stop();
            return Ok("Stopped the screen recording; the video is in Videos/Screencasts".into());
        }
        if rec.is_recording() {
            return Err("the screen is already being recorded".into());
        }
        // Out of the picture for good: the recording starts with the next frame.
        self.window.set_visible(false);
        glib::timeout_future(STEP_ASIDE).await;
        rec.start(shots.screen_region());
        if rec.is_recording() {
            Ok("Recording the screen to Videos/Screencasts. Stop it with the red button in the top bar, or ask me to stop.".into())
        } else {
            Err("the screen recording couldn't start (is wf-recorder installed?)".into())
        }
    }

    fn ocr_and_attach(self: &Rc<Self>, image: context::TempImage, kind: Kind, pending: String) {
        self.set_pending(Some(&pending));
        let o = self.clone();
        glib::spawn_future_local(async move {
            let result = context::blocking(move || context::ocr_file(&image.0)).await;
            match &result {
                Ok(t) => tracing::info!(chars = t.len(), "read text from the screen"),
                Err(e) => tracing::info!(error = ?e, "no text read from the screen"),
            }
            o.set_pending(None);
            if !o.is_visible() {
                return; // dismissed meanwhile
            }
            match result {
                Ok(text) => o.add_attachment(Attachment::new(kind, text)),
                Err(e) => o.ocr_failed(e),
            }
        });
    }

    fn ocr_failed(self: &Rc<Self>, e: OcrError) {
        self.set_pending(None);
        self.show_error(&e.message());
        if e == OcrError::NotInstalled {
            self.chips.append(&self.chip(AssistantAction::RunCommand {
                cmd: "sudo apt install tesseract-ocr".into(),
                risk: Risk::Medium,
            }));
        }
    }

    /// Super+Shift+A: ask about what the user is looking at. A fresh text
    /// selection wins; otherwise the current window is read.
    pub fn ask_about_screen(self: &Rc<Self>) {
        let opening = !self.is_visible();
        if let Some((text, true)) = self.history.selection() {
            tracing::info!(chars = text.len(), "ask about the selection");
            if opening {
                self.show();
            }
            self.add_attachment(Attachment::new(Kind::Selection, text));
            return;
        }
        if !opening {
            self.attach_window();
            return;
        }
        // Not open yet: read the window first, so the overlay isn't in the shot.
        self.entry.set_text("");
        self.assistant.set_visible(false);
        self.clear_attachments();
        self.clip_button.set_active(false);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let win = context::focused_window().await;
            tracing::info!(window = ?win, "ask about screen");
            let Some(o) = weak.upgrade() else { return };
            let found = win.is_some();
            *o.last_window.borrow_mut() = win;
            if found {
                o.attach_window();
            } else {
                // No window list (compositor not on the bus): read the screen.
                o.read_screen(None, Kind::Area, i18n::umoja("Reading the screen…"));
            }
        });
    }

    /// Super+Shift+V: open straight to the clipboard history.
    pub fn show_clipboard(self: &Rc<Self>) {
        if !self.is_visible() {
            self.show();
        }
        self.clip_button.set_active(true);
    }

    fn refresh_clipboard(self: &Rc<Self>) {
        if self.stack.visible_child_name().as_deref() != Some("clipboard") {
            return;
        }
        while let Some(c) = self.clip_list.first_child() {
            self.clip_list.remove(&c);
        }
        let filter = self.entry.text().trim().to_lowercase();
        let weak = Rc::downgrade(self);
        let on: Rc<dyn Fn(RowAction)> = Rc::new(move |a| {
            if let Some(o) = weak.upgrade() {
                o.on_clip_row(a);
            }
        });
        let mut shown = 0;
        for (i, text) in self.history.items().iter().enumerate() {
            if !filter.is_empty() && !text.to_lowercase().contains(&filter) {
                continue;
            }
            self.clip_list.append(&clipboard::row(i, text, on.clone()));
            shown += 1;
        }
        self.clip_empty.set_visible(shown == 0);
        self.clip_empty.set_text(&if !self.history.is_enabled() {
            i18n::umoja("Clipboard history is turned off.")
        } else if !filter.is_empty() && !self.history.items().is_empty() {
            i18n::umoja("Nothing you copied matches.")
        } else {
            i18n::umoja("Text you copy shows up here, so you can paste it again, translate it, tidy it up or summarize it. It's kept in memory only.")
        });
    }

    fn on_clip_row(self: &Rc<Self>, action: RowAction) {
        match action {
            RowAction::Copy(text) => {
                self.copy_text(&text);
                self.hide();
            }
            RowAction::Attach(text) => {
                self.clip_button.set_active(false);
                self.add_attachment(Attachment::new(Kind::Clipboard, text));
            }
            RowAction::Transform(text, t) => self.transform(text, t),
            RowAction::Remove(i) => self.history.remove(i),
        }
    }

    fn copy_text(&self, text: &str) {
        gtk::prelude::WidgetExt::display(&self.window)
            .clipboard()
            .set_text(text);
        // Without the watcher we wouldn't otherwise see our own copy.
        self.history.add(text.to_string());
    }

    fn chip(self: &Rc<Self>, action: AssistantAction) -> gtk::Widget {
        let (icon, label) = match &action {
            AssistantAction::OpenSettings { panel, subpage } => (
                "emblem-system-symbolic",
                format!(
                    "{} › {}{}",
                    i18n::control_center("Settings"),
                    panel,
                    subpage
                        .as_deref()
                        .map(|s| format!(" › {s}"))
                        .unwrap_or_default()
                ),
            ),
            AssistantAction::RunCommand { cmd, .. } => ("utilities-terminal-symbolic", cmd.clone()),
            AssistantAction::LaunchApp { app_id } => {
                ("application-x-executable-symbolic", app_id.clone())
            }
            AssistantAction::SetSetting { key, value, .. } => {
                ("preferences-system-symbolic", format!("{key} = {value}"))
            }
        };
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.append(&gtk::Image::from_icon_name(icon));
        content.append(
            &gtk::Label::builder()
                .label(&label)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(48)
                .build(),
        );
        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["pill", "action-chip"])
            .build();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.run_action(&action);
            }
        });
        button.upcast()
    }

    /// "Continue in Claude Code": hand the question to the coding agent.
    fn agent_chip(
        self: &Rc<Self>,
        agent: umoja_common::agents::Agent,
        task: String,
    ) -> gtk::Widget {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.append(&gtk::Image::from_icon_name("utilities-terminal-symbolic"));
        content.append(&gtk::Label::new(Some(
            &i18n::umoja("Continue in %s").replace("%s", agent.name),
        )));
        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["pill", "action-chip"])
            .build();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.launch_agent(&task);
            }
        });
        button.upcast()
    }

    /// Open the chosen coding agent in a terminal, on `task` if given
    /// (the ai-agent-hotkey opens it with no task).
    pub fn launch_agent(self: &Rc<Self>, task: &str) {
        use umoja_common::agents;
        let Some(agent) = agents::chosen() else {
            self.show();
            self.assistant.set_visible(true);
            self.answer.set_visible(true);
            self.answer.set_text(&i18n::umoja(
                "No coding agent is installed. Install one, such as Claude Code, and Umoja connects it at your next login.",
            ));
            return;
        };
        match agents::launch(agent, task, None) {
            Ok(()) => self.hide(),
            Err(e) => {
                tracing::warn!(error = %e, agent = agent.id, "could not start the coding agent");
                self.answer.set_text(&e);
            }
        }
    }

    fn run_action(self: &Rc<Self>, action: &AssistantAction) {
        if action.needs_confirmation() {
            let AssistantAction::RunCommand { cmd, .. } = action else {
                return;
            };
            let dialog = adw::AlertDialog::new(Some(&i18n::umoja("Run this command?")), Some(cmd));
            dialog.add_response("cancel", &i18n::gtk("_Cancel"));
            dialog.add_response("run", &i18n::umoja("Run"));
            dialog.set_response_appearance("run", adw::ResponseAppearance::Destructive);
            let weak = Rc::downgrade(self);
            let action = action.clone();
            dialog.connect_response(None, move |_, response| {
                if response == "run"
                    && let Some(o) = weak.upgrade()
                {
                    o.execute(&action);
                }
            });
            dialog.present(Some(&self.window));
        } else {
            self.execute(action);
        }
    }

    fn execute(&self, action: &AssistantAction) {
        let result = match action {
            AssistantAction::OpenSettings { panel, subpage } => {
                let mut c = Command::new("gnome-control-center");
                c.arg(panel);
                if let Some(s) = subpage {
                    c.arg(s);
                }
                c.spawn().map(|_| ())
            }
            // Commands that install software or need root run in a terminal,
            // where sudo can ask for the password and the output is visible.
            AssistantAction::RunCommand { cmd, .. } if needs_terminal(cmd) => {
                let r = spawn_in_terminal(cmd);
                if r.is_ok() {
                    self.hide();
                }
                r
            }
            // Anything else is isolated in a transient scope that can't gain
            // privileges.
            AssistantAction::RunCommand { cmd, .. } => Command::new("systemd-run")
                .args([
                    "--user",
                    "--scope",
                    "--collect",
                    "--quiet",
                    "--",
                    // NoNewPrivileges= isn't valid for scopes.
                    "setpriv",
                    "--no-new-privs",
                    "sh",
                    "-c",
                    cmd,
                ])
                .current_dir(glib::home_dir())
                .spawn()
                .map(|_| ()),
            AssistantAction::LaunchApp { app_id } => {
                let id = if app_id.ends_with(".desktop") {
                    app_id.clone()
                } else {
                    format!("{app_id}.desktop")
                };
                if let Some(app) = gio::DesktopAppInfo::new(&id) {
                    self.launch(app.upcast_ref());
                }
                Ok(())
            }
            AssistantAction::SetSetting { schema, key, value } => {
                if let (Some(s), Some(v)) = (
                    gs::settings(schema).filter(|s| gs::has_key(s, key)),
                    json_to_variant(value),
                ) {
                    let _ = s.set_value(key, &v);
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            self.answer.set_text(&e.to_string());
        }
    }
}

/// The overlay as the assistant agent sees it.
struct OverlayUi {
    overlay: Weak<Overlay>,
    generation: u64,
    rows: RefCell<HashMap<usize, (gtk::Box, gtk::Label)>>,
}

impl OverlayUi {
    fn live(&self) -> Option<Rc<Overlay>> {
        self.overlay
            .upgrade()
            .filter(|o| o.generation.get() == self.generation)
    }
}

impl agent::Ui for OverlayUi {
    fn text(&self, text: &str) {
        let Some(o) = self.live() else { return };
        let text = strip_actions(text);
        // Keep "Thinking…" up until the first words arrive.
        if !text.is_empty() {
            o.answer.set_visible(true);
            o.answer.set_text(&text);
        } else if !self.rows.borrow().is_empty() {
            o.answer.set_visible(false);
        }
    }

    fn step(&self, id: usize, label: &str, state: StepState) {
        let Some(o) = self.live() else { return };
        o.steps.set_visible(true);
        if o.answer.text() == i18n::umoja("Thinking…") {
            o.answer.set_visible(false);
        }
        let mut rows = self.rows.borrow_mut();
        let (row, text) = rows.entry(id).or_insert_with(|| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("assistant-step");
            let text = gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .hexpand(true)
                .build();
            row.append(&text);
            o.steps.append(&row);
            (row, text)
        });
        // Swap the leading status icon.
        if let Some(first) = row.first_child()
            && first != *text.upcast_ref::<gtk::Widget>()
        {
            row.remove(&first);
        }
        let icon: gtk::Widget = match state {
            StepState::Running => adw::Spinner::new().upcast(),
            StepState::Done => gtk::Image::from_icon_name("object-select-symbolic").upcast(),
            StepState::Failed => gtk::Image::from_icon_name("dialog-warning-symbolic").upcast(),
            StepState::Declined => {
                gtk::Image::from_icon_name("action-unavailable-symbolic").upcast()
            }
        };
        icon.set_size_request(16, 16);
        row.prepend(&icon);
        text.set_text(label);
        let state_name = match state {
            StepState::Running => "running",
            StepState::Done => "done",
            StepState::Failed => "failed",
            StepState::Declined => "declined",
        };
        o.step_log
            .borrow_mut()
            .insert(id, (state_name.to_string(), label.to_string()));
        for c in ["running", "done", "failed", "declined"] {
            row.remove_css_class(c);
        }
        row.add_css_class(match state {
            StepState::Running => "running",
            StepState::Done => "done",
            StepState::Failed => "failed",
            StepState::Declined => "declined",
        });
    }

    fn confirm(&self, label: &str, risk: Risk) -> Pin<Box<dyn Future<Output = bool>>> {
        let Some(o) = self.live() else {
            return Box::pin(async { false });
        };
        o.emit("ConfirmShown", (label,).to_variant());
        let dialog = adw::AlertDialog::new(
            Some(&i18n::umoja("Allow the assistant to do this?")),
            Some(label),
        );
        dialog.add_response("cancel", &i18n::gtk("_Cancel"));
        dialog.add_response("allow", &i18n::umoja("Allow"));
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance(
            "allow",
            if risk == Risk::High {
                adw::ResponseAppearance::Destructive
            } else {
                adw::ResponseAppearance::Suggested
            },
        );
        let window = o.window.clone();
        o.confirm_dialog.replace(Some(dialog.clone()));
        let weak = self.overlay.clone();
        let generation = self.generation;
        Box::pin(async move {
            let allowed = dialog.choose_future(&window).await == "allow";
            let Some(o) = weak.upgrade() else { return false };
            o.confirm_dialog.take();
            // Allowed only if it is still the same question.
            allowed && o.generation.get() == generation
        })
    }

    fn cancelled(&self) -> bool {
        self.live().is_none()
    }
}

/// The overlay's side of the assistant's capture tools.
struct OverlayCapturer(Weak<Overlay>);

impl crate::assistant::capture::Capturer for OverlayCapturer {
    fn screenshot(
        &self,
        target: crate::assistant::capture::Target,
        delay: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>>>> {
        let o = self.0.upgrade();
        Box::pin(async move {
            o.ok_or("the overlay is gone")?
                .assistant_screenshot(target, delay)
                .await
        })
    }

    fn record(&self, start: bool) -> Pin<Box<dyn Future<Output = Result<String, String>>>> {
        let o = self.0.upgrade();
        Box::pin(async move {
            o.ok_or("the overlay is gone")?
                .assistant_record(start)
                .await
        })
    }

    fn copy(&self, text: &str) {
        if let Some(o) = self.0.upgrade() {
            o.copy_text(text);
        }
    }
}

/// "Firefox", "Terminal": a window's app name for the user, else its title.
fn window_name(w: &FocusedWindow) -> String {
    let id = w.app_id.trim_end_matches(".desktop");
    gio::DesktopAppInfo::new(&format!("{id}.desktop"))
        .map(|a| a.display_name().to_string())
        .filter(|n| !n.is_empty())
        .or_else(|| (!w.title.is_empty()).then(|| w.title.clone()))
        .unwrap_or_else(|| id.to_string())
}

fn saved(what: &str, path: &std::path::Path) -> String {
    let home = glib::home_dir();
    let shown = path
        .strip_prefix(&home)
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| path.display().to_string());
    format!("Saved a screenshot of {what} to {shown} and copied it to the clipboard")
}

/// A flat popover menu entry with an icon.
/// `icons` are tried in order; the first one the icon theme has is drawn.
fn menu_item(icons: &[&str], label: &str) -> (gtk::Button, gtk::Label) {
    let text = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&gtk::Image::from_gicon(&gtk::gio::ThemedIcon::from_names(
        icons,
    )));
    content.append(&text);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["flat", "context-item"])
        .build();
    (button, text)
}

fn haystack(app: &gio::AppInfo) -> String {
    let mut s = app
        .description()
        .map(|d| d.to_lowercase())
        .unwrap_or_default();
    if let Some(d) = app.downcast_ref::<gio::DesktopAppInfo>() {
        for k in d.keywords() {
            s.push(' ');
            s.push_str(&k.to_lowercase());
        }
    }
    s
}

fn load_apps() -> Vec<gio::AppInfo> {
    let mut apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .collect();
    apps.sort_by_key(|a| a.display_name().to_lowercase());
    apps
}

fn json_to_variant(v: &serde_json::Value) -> Option<glib::Variant> {
    use serde_json::Value;
    Some(match v {
        Value::Bool(b) => b.to_variant(),
        Value::Number(n) if n.is_i64() => (n.as_i64()? as i32).to_variant(),
        Value::Number(n) => n.as_f64()?.to_variant(),
        Value::String(s) => s.to_variant(),
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().map(String::from))
            .collect::<Option<Vec<_>>>()?
            .to_variant(),
        _ => return None,
    })
}

pub(crate) use umoja_common::terminal::{needs_terminal, spawn_in_terminal};
