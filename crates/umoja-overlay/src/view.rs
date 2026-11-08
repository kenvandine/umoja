//! The overlay window: search entry, app grid and assistant answers.

use crate::assistant::agent::{self, Question, StepState};
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

const MAX_RESULTS: usize = 48;
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
}

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
        let pick_selection = menu_item("edit-select-all-symbolic", &i18n::umoja("Selected Text")).0;
        let pick_window = menu_item("focus-windows-symbolic", &i18n::umoja("Current Window"));
        let pick_area = menu_item("screenshot-recorded-symbolic", &i18n::umoja("Screen Area…")).0;
        let pick_clipboard = menu_item("edit-paste-symbolic", &i18n::umoja("Last Copied Text")).0;
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

        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(8)
            .min_children_per_line(4)
            .homogeneous(true)
            .row_spacing(24)
            .column_spacing(24)
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

    pub fn hide(&self) {
        self.generation.set(self.generation.get() + 1);
        self.window.set_visible(false);
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
            MynaEvent::Partial(text) if self.is_visible() => self.entry.set_text(&text),
            MynaEvent::Final(text) if self.is_visible() => {
                self.entry.set_text(&text);
                self.entry.set_position(-1);
                if lemonade::looks_like_question(&text) {
                    self.ask(&text);
                }
            }
            _ => {}
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
        for app in found.iter().take(MAX_RESULTS) {
            self.grid.append(&self.app_tile(app));
        }
        let attached = !self.attachments.borrow().is_empty();
        self.ask_button.set_visible(
            attached
                || (!q.trim().is_empty() && (found.is_empty() || lemonade::looks_like_question(&q))),
        );
        if q.trim().is_empty() && !attached && self.stack.visible_child_name().as_deref() != Some("clipboard") {
            self.assistant.set_visible(false);
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
            .max_width_chars(14)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&image);
        content.append(&label);
        let button = gtk::Button::builder()
            .child(&content)
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
        if let Some(id) = app.id().map(|s| s.to_string()) {
            let menu = gtk::GestureClick::builder().button(3).build();
            let id_m = id.clone();
            menu.connect_pressed(move |g, _, _, _| {
                if let Some(w) = g.widget() {
                    show_tile_menu(&w, &id_m);
                }
            });
            button.add_controller(menu);
            let press = gtk::GestureLongPress::builder().touch_only(true).build();
            press.connect_pressed(move |g, _, _| {
                if let Some(w) = g.widget() {
                    show_tile_menu(&w, &id);
                }
            });
            button.add_controller(press);
        }
        button.upcast()
    }

    fn launch(&self, app: &gio::AppInfo) {
        let ctx = gtk::prelude::WidgetExt::display(&self.window).app_launch_context();
        if let Err(e) = app.launch(&[], Some(&ctx)) {
            tracing::warn!(error = %e, "launch failed");
        }
        self.hide();
    }

    /// Answer a question, letting the assistant act through its tools. Any
    /// attached context goes with it.
    fn ask(self: &Rc<Self>, query: &str) {
        let attachments = self.attachments.borrow().clone();
        let text = match query.trim() {
            "" if !attachments.is_empty() => context::DEFAULT_QUESTION.to_string(),
            q => q.to_string(),
        };
        self.ask_question(Question {
            text,
            context: context::build_context(&attachments),
        });
    }

    /// [`Self::ask`] with optional context (selected text, window contents).
    pub fn ask_question(self: &Rc<Self>, question: Question) {
        if question.text.trim().is_empty() {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        self.reset_card();
        self.answer.set_text(&i18n::umoja("Thinking…"));
        // Typed-query matches would disagree with the answer; the
        // search_files tool fills the list with its own results instead.
        self.files.clear();
        crate::assistant::take_hide_request();

        let ui: Rc<dyn agent::Ui> = Rc::new(OverlayUi {
            overlay: Rc::downgrade(self),
            generation,
            rows: RefCell::new(HashMap::new()),
        });
        // Fresh per request: picks up model and backend changes.
        let client = LemonadeClient::new();
        let weak = Rc::downgrade(self);
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
            if text.is_empty() && o.answer.text() == i18n::umoja("Thinking…") {
                o.answer.set_visible(false);
            }
            let shown = strip_actions(&text);
            o.copy_button.set_visible(!shown.trim().is_empty());
            *o.answer_text.borrow_mut() = shown;
            // A tool opened an app or moved a window: get out of the way.
            if crate::assistant::take_hide_request() {
                o.hide();
            }
        });
    }

    /// Translate, reformat, summarize or explain `text`. A plain chat with
    /// its own system prompt: no tools, no desktop actions.
    fn transform(self: &Rc<Self>, text: String, t: Transform) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
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
        for b in [self.chips.upcast_ref::<gtk::Widget>(), self.steps.upcast_ref()] {
            while let Some(c) = b.first_child() {
                c.unparent();
            }
        }
        self.steps.set_visible(false);
    }

    fn show_error(&self, text: &str) {
        self.generation.set(self.generation.get() + 1);
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
    fn read_screen(self: &Rc<Self>, rect: Option<(i32, i32, i32, i32)>, kind: Kind, pending: String) {
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
                    && let Some(o) = weak.upgrade() {
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
                    "-p",
                    "NoNewPrivileges=yes",
                    "--",
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
            StepState::Declined => gtk::Image::from_icon_name("action-unavailable-symbolic").upcast(),
        };
        icon.set_size_request(16, 16);
        row.prepend(&icon);
        text.set_text(label);
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
        Box::pin(async move { dialog.choose_future(&window).await == "allow" })
    }

    fn cancelled(&self) -> bool {
        self.live().is_none()
    }
}

/// A flat popover menu entry with an icon.
fn menu_item(icon: &str, label: &str) -> (gtk::Button, gtk::Label) {
    let text = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&text);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["flat", "context-item"])
        .build();
    (button, text)
}

/// "Pin to Dash" / "Unpin" for an app-grid tile; pinning adds the app to
/// `favorite-apps`, which the dock follows.
fn show_tile_menu(parent: &gtk::Widget, desktop_id: &str) {
    let pinned = umoja_common::apps::is_favorite(desktop_id);
    let label = if pinned {
        i18n::shell("Unpin")
    } else {
        i18n::shell("Pin to Dash")
    };
    let item = gtk::Button::builder()
        .label(label)
        .css_classes(["flat"])
        .build();
    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    col.append(&item);
    let popover = gtk::Popover::builder().child(&col).build();
    popover.set_parent(parent);
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    let id = desktop_id.to_string();
    let pop = popover.downgrade();
    item.connect_clicked(move |_| {
        umoja_common::apps::set_favorite(&id, !pinned);
        if let Some(p) = pop.upgrade() {
            p.popdown();
        }
    });
    popover.popup();
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

/// Whether a suggested command needs a terminal: it asks for privileges
/// (sudo, pkexec) or uses a package manager, which prompts and prints
/// progress the user should see.
pub(crate) fn needs_terminal(cmd: &str) -> bool {
    const TOOLS: &[&str] = &[
        "sudo", "pkexec", "apt", "apt-get", "aptitude", "dpkg", "snap", "flatpak",
    ];
    cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .any(|w| TOOLS.contains(&w.rsplit('/').next().unwrap_or(w)))
}

/// Run `cmd` in the user's terminal, kept open afterwards so the result
/// can be read.
pub(crate) fn spawn_in_terminal(cmd: &str) -> std::io::Result<()> {
    let script = format!("{cmd}\nstatus=$?\necho\nprintf '%s' \"$1\"\nread -r _\nexit $status");
    let prompt = i18n::umoja("Press Enter to close this window.");
    let shell = ["sh", "-c", script.as_str(), "umoja-run", prompt.as_str()];
    // xdg-terminal-exec takes the command directly; the others after a flag.
    let candidates: [(&str, &[&str]); 4] = [
        ("xdg-terminal-exec", &[]),
        ("ptyxis", &["--"]),
        ("gnome-terminal", &["--"]),
        ("x-terminal-emulator", &["-e"]),
    ];
    let mut last = std::io::Error::from(std::io::ErrorKind::NotFound);
    for (bin, flag) in candidates {
        if glib::find_program_in_path(bin).is_none() {
            continue;
        }
        match Command::new(bin)
            .args(flag)
            .args(shell)
            .current_dir(glib::home_dir())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod terminal_tests {
    use super::needs_terminal;

    #[test]
    fn install_commands_use_a_terminal() {
        assert!(needs_terminal("sudo apt install vlc"));
        assert!(needs_terminal("pkexec apt-get install -y gimp"));
        assert!(needs_terminal("snap install spotify"));
        assert!(needs_terminal("flatpak install flathub org.gimp.GIMP"));
        assert!(needs_terminal("cd /tmp && /usr/bin/sudo make install"));
        assert!(!needs_terminal("xdg-open ~/Downloads"));
        assert!(!needs_terminal("gsettings set org.gnome.desktop.interface color-scheme prefer-dark"));
        assert!(!needs_terminal("echo snapshot"));
    }
}
