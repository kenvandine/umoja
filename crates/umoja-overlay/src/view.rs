//! Ask Umoja: the floating card that searches, runs and answers.
//!
//! The Copilot key (or a tap of Alt) opens it over the focused window. As
//! the user types, rows come up for apps, launcher actions (the focused
//! app's first), desktop switches and Settings pages, sums, files and a web
//! search. Those run straight away, through umoja-mcp or the overlay's own
//! screens, with no model. "Ask Umoja" (Tab, or the row) hands the text to
//! the assistant; its tool steps, confirmations and answer show inside the
//! card. Screen readers get a label for every row and hear the selected
//! row, confirmations and answers.

use crate::appgrid;
use crate::ask::{
    self, calc,
    catalog::{self, Instant, Local, Run},
};
use crate::assistant::agent::{self, Question, Reply, StepState, Ui as _};
use crate::assistant::mcp;
use crate::assistant::tools::Risk;
use crate::clipboard::{self, RowAction};
use crate::context::{self, Attachment, FocusedWindow, Kind, OcrError, Transform};
use crate::files::{FilesSection, Hit};
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

/// Earlier messages sent with a follow-up (three exchanges).
const MAX_HISTORY: usize = 6;
/// How much of an earlier message (with its attached context) is kept.
const MAX_HISTORY_CHARS: usize = 6000;
/// How long the overlay hides before a capture so it isn't in the shot.
const STEP_ASIDE: Duration = Duration::from_millis(250);
/// Rows of matches for typed text (apps, actions, switches, settings).
const MAX_MATCHES: usize = 7;
/// The read-aloud button, idle and while speaking.
const SPEAK_ICON: &str = "audio-volume-high-symbolic";
const SPEAKING_ICON: &str = "media-playback-stop-symbolic";
/// File rows for typed text.
const MAX_FILES: u32 = 3;
/// Width of the card, as in the mockup.
const CARD_WIDTH: i32 = 640;
/// Pause in typing before umoja-files is asked, and before the top row is
/// read out.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// What a result row does.
#[derive(Clone)]
enum Item {
    App(gio::AppInfo),
    /// A launcher action from an app's .desktop file ("New Private Window").
    Action {
        app: gio::DesktopAppInfo,
        action: String,
        name: String,
    },
    /// A switch, desktop action or Settings page. `intent` is the on/off
    /// the text asked for ("wifi off").
    Instant {
        item: &'static Instant,
        intent: Option<bool>,
    },
    /// A calculator result; Enter copies it.
    Sum(String),
    File(Hit),
    /// Hand `question` to the assistant.
    Ask {
        question: String,
        label: String,
        sub: String,
    },
    Web(String),
}

impl Item {
    /// The id usage history keeps (never the typed text).
    fn id(&self) -> Option<String> {
        match self {
            Item::App(a) => Some(format!("app:{}", appgrid::app_key(a))),
            Item::Action { app, action, .. } => Some(format!(
                "action:{}:{action}",
                app.id().map(|s| s.to_string()).unwrap_or_default()
            )),
            Item::Instant { item, .. } => Some(format!("instant:{}", item.id)),
            _ => None,
        }
    }
}

/// A row of the result list: a section heading or something to pick.
enum Entry {
    Heading(String),
    Item(Item),
}

pub struct Overlay {
    model_status: gtk::Label,
    // An Adwaita window, as the rest of the overlay's.
    window: adw::ApplicationWindow,
    card: gtk::Box,
    entry: gtk::SearchEntry,
    /// "Firefox": the app Ask Umoja opened over. Its launcher actions come
    /// first and questions are asked with it in mind. Backspace in the
    /// empty entry (or a click) removes it.
    focus_chip: gtk::Button,
    focus_icon: gtk::Image,
    focus_label: gtk::Label,
    focus_app: RefCell<Option<gio::DesktopAppInfo>>,
    results: gtk::ListBox,
    /// What each row of `results` does; None for a heading.
    items: RefCell<Vec<Option<Item>>>,
    /// The text `items` were made for.
    items_text: RefCell<String>,
    selected: Cell<usize>,
    /// What the user picks most, for ranking and the empty search.
    usage: RefCell<ask::history::History>,
    /// On/off state of the switches, by desktop_status line name.
    switches: RefCell<HashMap<String, bool>>,
    /// Files matching `.0`, from umoja-files.
    file_hits: RefCell<(String, Vec<Hit>)>,
    file_generation: Cell<u64>,
    /// Every app's launcher actions: (app, action id, its name).
    actions: RefCell<Vec<(gio::DesktopAppInfo, String, String)>>,
    /// The label of the row read out last, so it isn't repeated.
    announced: RefCell<String>,
    announce_generation: Cell<u64>,
    foot_backend: gtk::Label,
    question_label: gtk::Label,
    answer: gtk::Label,
    /// One line per tool call ("✓ Turn on Dark Style").
    steps: gtk::Box,
    chips: gtk::FlowBox,
    assistant: gtk::Box,
    /// "Allow Umoja to do this?" inside the card.
    confirm_box: gtk::Box,
    confirm_tx: RefCell<Option<async_channel::Sender<bool>>>,
    /// The text the answer on show was asked with: typing something else
    /// puts the rows back.
    asked_text: RefCell<String>,
    /// Files from the assistant's search_files, under the answer.
    files: Rc<FilesSection>,
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
    // Clipboard history page, in place of the results.
    history: Rc<clipboard::History>,
    clip_button: gtk::ToggleButton,
    stack: gtk::Stack,
    clip_list: gtk::ListBox,
    clip_empty: gtk::Label,
    copy_button: gtk::Button,
    /// Reads the answer aloud (Piper); a stop button while it speaks.
    speak_button: gtk::Button,
    /// Bumped per read-aloud, so a finished one can't reset a newer one.
    speak_serial: Cell<u64>,
    answer_text: RefCell<String>,
    /// The conversation so far: earlier user and assistant messages, sent
    /// along with follow-ups. Cleared when the card opens or the entry is
    /// emptied.
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
}

fn placeholder() -> String {
    i18n::umoja("Search, run a command, or ask anything…")
}

impl Overlay {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-overlay", "ask-umoja"])
            .title(i18n::umoja("Ask Umoja"))
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-ask");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        // Only acts on GTK < 4.24 (and only where the card is); newer GTK
        // blurs from the card's CSS.
        umoja_common::blur::blur_behind(&window);

        // ---- Top row: focused app, entry, microphone ------------------------
        let focus_icon = gtk::Image::builder().pixel_size(20).build();
        focus_icon.set_accessible_role(gtk::AccessibleRole::Presentation);
        let focus_label = gtk::Label::new(None);
        let chip_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip_box.append(&focus_icon);
        chip_box.append(&focus_label);
        let focus_chip = gtk::Button::builder()
            .child(&chip_box)
            .css_classes(["focus-chip"])
            .tooltip_text(i18n::umoja(
                "Searching with this app first. Backspace searches everything.",
            ))
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let entry = gtk::SearchEntry::builder()
            .placeholder_text(placeholder())
            .hexpand(true)
            .css_classes(["ask-entry"])
            .search_delay(60)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label(&i18n::umoja("Ask Umoja"))]);
        let myna_pill = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .tooltip_text(i18n::umoja("Dictate with Myna"))
            .css_classes(["circular", "myna-pill"])
            .valign(gtk::Align::Center)
            .build();
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        top.add_css_class("ask-top");
        top.append(&focus_chip);
        top.append(&entry);
        top.append(&myna_pill);

        // "Add context" menu and the clipboard history toggle, in the foot.
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

        // "Downloading the assistant's model… 42%" while it's fetched.
        let model_status = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "model-status"])
            .visible(false)
            .build();

        // Attached context, as removable pills under the entry.
        let attach_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        attach_row.add_css_class("attach-row");
        let pending_label = gtk::Label::new(None);
        let pending = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pending.add_css_class("attach-pill");
        pending.append(&gtk::Spinner::builder().spinning(true).build());
        pending.append(&pending_label);
        pending.set_visible(false);
        let attach_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        attach_box.add_css_class("ask-attach");
        attach_box.append(&attach_row);
        attach_box.append(&pending);

        // ---- Results ---------------------------------------------------------
        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(true)
            .css_classes(["ask-list"])
            .build();
        results.update_property(&[gtk::accessible::Property::Label(&i18n::umoja("Results"))]);
        entry.update_relation(&[gtk::accessible::Relation::Controls(&[results.upcast_ref()])]);
        let results_scroll = gtk::ScrolledWindow::builder()
            .child(&results)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(400)
            .build();

        let clip_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["clip-list"])
            .build();
        let clip_empty = gtk::Label::builder()
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_top(24)
            .margin_bottom(24)
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
                .css_classes(["heading"])
                .build(),
        );
        clip_header.append(&clip_clear);
        let clip_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        clip_page.add_css_class("ask-clip");
        clip_page.append(&clip_header);
        clip_page.append(&clip_empty);
        clip_page.append(
            &gtk::ScrolledWindow::builder()
                .child(&clip_list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(420)
                .build(),
        );
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vhomogeneous(false)
            .interpolate_size(true)
            .build();
        stack.add_named(&results_scroll, Some("results"));
        stack.add_named(&clip_page, Some("clipboard"));

        // ---- The answer ------------------------------------------------------
        let question_label = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "ask-question"])
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
        let speak_button = gtk::Button::builder()
            .icon_name(SPEAK_ICON)
            .tooltip_text(i18n::umoja("Read Aloud"))
            .css_classes(["flat", "circular"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .visible(false)
            .build();
        let answer_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        answer.set_hexpand(true);
        answer_row.append(&answer);
        answer_row.append(&speak_button);
        answer_row.append(&copy_button);
        let confirm_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        confirm_box.add_css_class("ask-confirm");
        confirm_box.set_visible(false);
        let assistant = gtk::Box::new(gtk::Orientation::Vertical, 10);
        assistant.add_css_class("ask-answer");
        assistant.append(&question_label);
        assistant.append(&steps);
        assistant.append(&answer_row);
        assistant.append(&confirm_box);
        assistant.append(&chips);
        assistant.set_visible(false);

        // ---- Foot: keys, what answers, context and clipboard -----------------
        let hints = gtk::Label::builder()
            .label(i18n::umoja(
                "↑↓ move · Enter run · Tab ask Umoja · Esc close",
            ))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["ask-hints"])
            .build();
        let foot_backend = gtk::Label::builder()
            .css_classes(["ask-hints"])
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(40)
            .build();
        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        foot.add_css_class("ask-foot");
        foot.append(&hints);
        foot.append(&foot_backend);
        foot.append(&context_button);
        foot.append(&clip_button);
        foot.append(&crate::model_picker::button());

        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["ask-card"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .width_request(CARD_WIDTH)
            .accessible_role(gtk::AccessibleRole::Dialog)
            .build();
        card.update_property(&[gtk::accessible::Property::Label(&i18n::umoja("Ask Umoja"))]);
        card.append(&top);
        card.append(&attach_box);
        card.append(&model_status);
        card.append(&stack);
        card.append(&assistant);

        // The card floats over the focused window, about a sixth of the way
        // down; clicking anywhere else closes it.
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_margin_top(96);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.set_margin_bottom(48);
        root.append(&card);
        window.set_content(Some(&root));

        let overlay = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| Self {
            files: FilesSection::new({
                let weak = weak.clone();
                move || {
                    if let Some(o) = weak.upgrade() {
                        o.hide();
                    }
                }
            }),
            window,
            card,
            entry,
            focus_chip,
            focus_icon,
            focus_label,
            focus_app: RefCell::default(),
            results,
            items: RefCell::default(),
            items_text: RefCell::default(),
            selected: Cell::new(0),
            usage: RefCell::new(ask::history::History::load()),
            switches: RefCell::default(),
            file_hits: RefCell::default(),
            file_generation: Cell::new(0),
            actions: RefCell::new(load_actions()),
            announced: RefCell::default(),
            announce_generation: Cell::new(0),
            foot_backend,
            question_label,
            answer,
            steps,
            chips,
            assistant,
            confirm_box,
            confirm_tx: RefCell::default(),
            asked_text: RefCell::default(),
            myna_pill,
            model_status,
            apps: RefCell::new(appgrid::load_apps()),
            app_monitor: gio::AppInfoMonitor::get(),
            myna: Myna::start(),
            generation: Cell::new(0),
            local_once: Cell::new(false),
            events: RefCell::new(None),
            step_log: RefCell::new(Default::default()),
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
            speak_button,
            speak_serial: Cell::new(0),
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
        overlay
            .card
            .insert_child_after(&overlay.files.widget, Some(&overlay.assistant));
        overlay.card.append(&foot);
        overlay.files.widget.set_halign(gtk::Align::Fill);
        overlay.files.widget.set_width_request(-1);
        overlay.files.widget.add_css_class("ask-files");
        // Clicks outside the card close it.
        let outside = gtk::GestureClick::new();
        let weak = Rc::downgrade(&overlay);
        let root_w = root.clone();
        outside.connect_pressed(move |_, _, x, y| {
            let Some(o) = weak.upgrade() else { return };
            let inside = o
                .card
                .compute_bounds(&root_w)
                .is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
            if !inside {
                o.hide();
            }
        });
        root.add_controller(outside);
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
                o.selected.set(0);
                o.refresh_results();
            }
        });

        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |_| {
            if let Some(o) = weak.upgrade() {
                o.activate_selected();
            }
        });

        let weak = Rc::downgrade(self);
        self.entry.connect_stop_search(move |_| {
            if let Some(o) = weak.upgrade() {
                o.hide();
            }
        });

        let weak = Rc::downgrade(self);
        self.results.connect_row_activated(move |_, row| {
            if let Some(o) = weak.upgrade() {
                let i = row.index().max(0) as usize;
                let item = o.items.borrow().get(i).cloned().flatten();
                if let Some(item) = item {
                    o.activate(item);
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.focus_chip.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.drop_focus_app();
                o.entry.grab_focus();
            }
        });

        let weak = Rc::downgrade(self);
        self.myna_pill.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.myna.toggle();
            }
        });

        // Keys: Escape anywhere closes; in the entry, arrows move through
        // the rows, Tab asks, Backspace in the empty entry drops the app.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            use gtk::gdk::Key;
            let Some(o) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == Key::Escape {
                o.hide();
                return glib::Propagation::Stop;
            }
            if !o.entry_has_focus() {
                return glib::Propagation::Proceed;
            }
            let plain = !mods.intersects(
                gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SHIFT_MASK,
            );
            match key {
                Key::Down | Key::KP_Down => o.move_selection(1),
                Key::Up | Key::KP_Up => o.move_selection(-1),
                Key::Page_Down | Key::KP_Page_Down => o.move_selection(5),
                Key::Page_Up | Key::KP_Page_Up => o.move_selection(-5),
                Key::Tab | Key::KP_Tab if plain => {
                    let q = o.entry.text().to_string();
                    if !q.trim().is_empty() || !o.attachments.borrow().is_empty() {
                        o.ask(&q);
                    }
                }
                Key::BackSpace
                    if plain && o.entry.text().is_empty() && o.focus_chip.is_visible() =>
                {
                    o.drop_focus_app()
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);

        // Rescan apps when packages are installed or removed.
        let weak = Rc::downgrade(self);
        self.app_monitor.connect_changed(move |_| {
            if let Some(o) = weak.upgrade() {
                *o.apps.borrow_mut() = appgrid::load_apps();
                *o.actions.borrow_mut() = load_actions();
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

    fn entry_has_focus(&self) -> bool {
        gtk::prelude::GtkWindowExt::focus(&self.window).is_some_and(|f| {
            f == *self.entry.upcast_ref::<gtk::Widget>() || f.is_ancestor(&self.entry)
        })
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn show(self: &Rc<Self>) {
        self.hide_serial.set(self.hide_serial.get().wrapping_add(1));
        self.end_conversation();
        self.asked_text.borrow_mut().clear();
        self.entry.set_text("");
        self.assistant.set_visible(false);
        self.files.clear();
        self.clear_attachments();
        self.clip_button.set_active(false);
        self.focus_chip.set_visible(false);
        self.focus_app.replace(None);
        self.announced.borrow_mut().clear();
        self.selected.set(0);
        self.update_foot();
        self.remember_window();
        self.refresh_switches();
        // Text may already be empty (no "changed"): rebuild the rows.
        self.refresh_results();
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

    /// Open Ask Umoja with `text` already typed.
    pub fn show_with_text(self: &Rc<Self>, text: &str) {
        self.show();
        self.entry.set_text(text);
        self.entry.set_position(-1);
    }

    /// Open Ask Umoja and type `text` into it, a few letters at a time if
    /// `typed`, without asking: org.umoja.Overlay.AskType, so tools/demo
    /// can show the rows that come up.
    pub fn type_text(self: &Rc<Self>, text: &str, typed: bool) {
        if !typed {
            self.show_with_text(text);
            return;
        }
        if !self.is_visible() {
            self.show();
        }
        self.entry.set_text("");
        let generation = self.generation.get();
        let serial = self.hide_serial.get();
        let chars: Vec<char> = text.chars().collect();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            for n in 1..=chars.len() {
                glib::timeout_future(Duration::from_millis(60)).await;
                let Some(o) = weak.upgrade() else { return };
                if o.generation.get() != generation || o.hide_serial.get() != serial {
                    return;
                }
                let typed: String = chars[..n].iter().collect();
                o.entry.set_text(&typed);
                o.entry.set_position(-1);
            }
        });
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
        self.stop_speaking();
        // A question still waiting for Allow is declined, so it can't come
        // back (and be allowed) on a later, unrelated question.
        self.cancel_confirm();
        let serial = self.hide_serial.get().wrapping_add(1);
        self.hide_serial.set(serial);
        if !self.window.is_visible() {
            return;
        }
        self.entry.set_text("");
        self.assistant.set_visible(false);
        appgrid::hide_after_clean_frame(self.window.upcast_ref(), self.hide_serial.clone(), serial);
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

    /// Read `text` out with the screen reader, if one is running.
    fn announce(&self, text: &str) {
        let text = context::truncate_chars(text.trim(), 600);
        if !text.is_empty() {
            self.card
                .announce(&text, gtk::AccessibleAnnouncementPriority::Medium);
        }
    }

    fn show_answer_actions(&self, on: bool) {
        self.copy_button.set_visible(on);
        self.speak_button.set_visible(on);
    }

    /// Read `text` aloud in Umoja's voice (Piper, see [`crate::speech`]).
    /// The read-aloud button turns into a stop button meanwhile.
    pub fn speak(self: &Rc<Self>, text: &str) {
        // Odd while speaking (or fetching the voice), even when idle.
        let serial = (self.speak_serial.get() | 1) + 2;
        self.speak_serial.set(serial);
        self.set_speak_button(SPEAKING_ICON, &i18n::umoja("Stop Reading"));
        let weak = Rc::downgrade(self);
        crate::speech::speak(text, move |ev| {
            let Some(o) = weak.upgrade() else { return };
            if o.speak_serial.get() != serial {
                return;
            }
            match ev {
                crate::speech::Event::Downloading => {
                    o.set_speak_button(SPEAKING_ICON, &i18n::umoja("Downloading the voice…"))
                }
                crate::speech::Event::Speaking => {
                    o.set_speak_button(SPEAKING_ICON, &i18n::umoja("Stop Reading"))
                }
                crate::speech::Event::Done => o.speak_finished(None),
                crate::speech::Event::Failed(e) => o.speak_finished(Some(e)),
            }
        });
    }

    pub fn stop_speaking(&self) {
        crate::speech::stop();
        self.speak_finished(None);
    }

    fn speak_finished(&self, error: Option<String>) {
        let s = self.speak_serial.get();
        self.speak_serial.set(s + (s % 2));
        let tip = match error {
            Some(e) => format!("{}\n{e}", i18n::umoja("Read Aloud")),
            None => i18n::umoja("Read Aloud"),
        };
        self.set_speak_button(SPEAK_ICON, &tip);
    }

    fn set_speak_button(&self, icon: &str, tooltip: &str) {
        self.speak_button.set_icon_name(icon);
        self.speak_button.set_tooltip_text(Some(tooltip));
    }

    /// With "speak-answers" on, read a finished answer aloud, unless a
    /// screen reader is running (it reads the answer already).
    fn speak_if_wanted(self: &Rc<Self>, text: &str) {
        let wanted =
            umoja_common::gsettings::get_bool(crate::speech::SCHEMA, "speak-answers", false);
        let screen_reader = umoja_common::gsettings::get_bool(
            "org.gnome.desktop.a11y.applications",
            "screen-reader-enabled",
            false,
        );
        // Already reading (the assistant's read_aloud tool): leave it.
        if wanted && !screen_reader && self.speak_serial.get() % 2 == 0 {
            self.speak(text);
        }
    }

    /// "Qwen3-4B · on this computer": what answers questions.
    fn update_foot(&self) {
        let b = umoja_common::assistant_backend::current();
        // "~anthropic/claude-sonnet-latest" -> "claude-sonnet-latest": the
        // provider prefix (OpenRouter's) only pushes the name out of view.
        let model = b.model.trim_end_matches("-GGUF");
        let model = model.rsplit('/').next().unwrap_or(model).trim_start_matches('~');
        self.foot_backend.set_text(&if b.cloud {
            format!("{model} · {}", b.host())
        } else {
            format!("{model} · {}", i18n::umoja("on this computer"))
        });
    }

    // ---- The focused app ---------------------------------------------------

    /// Note which window the user was in, for "Current Window" and the chip.
    fn remember_window(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let serial = self.hide_serial.get();
        glib::spawn_future_local(async move {
            let win = context::focused_window().await;
            crate::assistant::capture::set_previous_window(win.as_ref().map(window_name));
            let Some(o) = weak.upgrade() else { return };
            *o.last_window.borrow_mut() = win.clone();
            if o.hide_serial.get() != serial {
                return;
            }
            let app = win.as_ref().and_then(|w| {
                let id = w.app_id.trim_end_matches(".desktop");
                (!id.is_empty())
                    .then(|| gio::DesktopAppInfo::new(&format!("{id}.desktop")))
                    .flatten()
            });
            if let Some(app) = app {
                o.focus_icon.set_from_gicon(
                    &app.icon().unwrap_or_else(|| {
                        gio::ThemedIcon::new("application-x-executable").upcast()
                    }),
                );
                o.focus_label.set_text(&app.display_name());
                o.focus_chip
                    .update_property(&[gtk::accessible::Property::Label(
                        &i18n::umoja("Searching in %s first. Press to search everything.")
                            .replace("%s", &app.display_name()),
                    )]);
                o.focus_app.replace(Some(app));
                o.focus_chip.set_visible(true);
                o.refresh_results();
            }
        });
    }

    fn drop_focus_app(self: &Rc<Self>) {
        self.focus_chip.set_visible(false);
        self.focus_app.replace(None);
        self.refresh_results();
    }

    /// The focused app while the chip is up.
    fn focused_app(&self) -> Option<gio::DesktopAppInfo> {
        self.focus_chip
            .is_visible()
            .then(|| self.focus_app.borrow().clone())
            .flatten()
    }

    /// A line for the assistant about the window Ask Umoja opened over.
    fn focus_context(&self) -> Option<String> {
        let app = self.focused_app()?;
        let title = self
            .last_window
            .borrow()
            .as_ref()
            .map(|w| w.title.clone())
            .unwrap_or_default();
        Some(format!(
            "The user opened the assistant from {} (window title “{}”). Keep it in mind \
             when the question is about what they are doing there; otherwise ignore it.\n\n\
             Question:",
            app.display_name(),
            context::truncate_chars(&title, 120)
        ))
    }

    // ---- Switch state --------------------------------------------------------

    /// Read which switches are on (desktop_status), for their rows.
    fn refresh_switches(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(client) = mcp::shared().await else {
                return;
            };
            let never: mcp::Confirm = Rc::new(|_, _| Box::pin(async { false }));
            let r = client
                .call("desktop_status", serde_json::json!({}), never)
                .await;
            if r.is_error {
                return;
            }
            let Some(o) = weak.upgrade() else { return };
            *o.switches.borrow_mut() = ask::parse_status(&r.text);
            if o.is_visible() {
                o.rebuild_rows(true);
            }
        });
    }

    // ---- Results -------------------------------------------------------------

    fn refresh_results(self: &Rc<Self>) {
        let q = self.entry.text().to_string();
        let replying = self.awaiting_reply.get();
        // Typing something else after an answer brings the rows back (the
        // conversation is kept for a follow-up); emptying the entry ends it.
        if self.assistant.is_visible() && !replying && q.trim() != self.asked_text.borrow().trim() {
            self.assistant.set_visible(false);
            self.cancel_confirm();
            if q.trim().is_empty() {
                self.conversation.borrow_mut().clear();
                self.local_chat.set(false);
            }
        }
        self.search_files(&q);
        self.rebuild_rows(false);
        self.refresh_clipboard();
    }

    /// Fill the list for the current text. `keep` keeps the selected row
    /// (new file hits or switch states arrived).
    fn rebuild_rows(self: &Rc<Self>, keep: bool) {
        let q = self.entry.text().to_string();
        let answering = self.assistant.is_visible() && !self.awaiting_reply.get();
        let entries = if answering {
            Vec::new()
        } else {
            self.compute(&q)
        };
        while let Some(c) = self.results.first_child() {
            self.results.remove(&c);
        }
        let mut items = Vec::with_capacity(entries.len());
        for e in entries {
            match e {
                Entry::Heading(h) => {
                    let label = gtk::Label::builder()
                        .label(h)
                        .xalign(0.0)
                        .css_classes(["ask-section"])
                        .build();
                    let row = gtk::ListBoxRow::builder()
                        .child(&label)
                        .activatable(false)
                        .selectable(false)
                        .can_focus(false)
                        .build();
                    self.results.append(&row);
                    items.push(None);
                }
                Entry::Item(item) => {
                    self.results.append(&self.row_widget(&item));
                    items.push(Some(item));
                }
            }
        }
        let has_rows = items.iter().any(Option::is_some);
        *self.items.borrow_mut() = items;
        *self.items_text.borrow_mut() = q.clone();
        let clip = self.stack.visible_child_name().as_deref() == Some("clipboard");
        self.stack.set_visible(clip || has_rows);
        let start = if keep { self.selected.get() } else { 0 };
        self.select(
            self.nearest_item(start, 1)
                .or_else(|| self.nearest_item(start, -1)),
        );
        if !keep {
            self.announce_top_soon();
        }
    }

    /// The first item at or after (`dir` 1) or before (-1) `from`.
    fn nearest_item(&self, from: usize, dir: i32) -> Option<usize> {
        let items = self.items.borrow();
        let mut i = from.min(items.len().saturating_sub(1)) as i64;
        while i >= 0 && (i as usize) < items.len() {
            if items[i as usize].is_some() {
                return Some(i as usize);
            }
            i += dir as i64;
        }
        None
    }

    fn select(&self, i: Option<usize>) {
        let Some(i) = i else {
            self.results.unselect_all();
            return;
        };
        self.selected.set(i);
        if let Some(row) = self.results.row_at_index(i as i32) {
            self.results.select_row(Some(&row));
            // Keep it in view (the focus stays in the entry).
            if let (Some(adj), Some(b)) = (
                self.results
                    .parent()
                    .and_then(|p| p.downcast::<gtk::Viewport>().ok())
                    .and_then(|v| v.vadjustment()),
                row.compute_bounds(&self.results),
            ) {
                adj.clamp_page(b.y() as f64, (b.y() + b.height()) as f64);
            }
        }
    }

    fn move_selection(&self, by: i32) {
        let n = self.items.borrow().len();
        if n == 0 {
            return;
        }
        let cur = self.selected.get() as i32;
        let target = (cur + by).clamp(0, n as i32 - 1) as usize;
        let dir = if by >= 0 { 1 } else { -1 };
        let found = self
            .nearest_item(target, dir)
            .or_else(|| self.nearest_item(target, -dir));
        if let Some(i) = found {
            self.select(Some(i));
            if let Some(label) = self.row_label(i) {
                self.announced.replace(label.clone());
                self.announce(&label);
            }
        }
    }

    /// The selected row's accessible label.
    fn row_label(&self, i: usize) -> Option<String> {
        let item = self.items.borrow().get(i).cloned().flatten()?;
        let (title, sub, kind) = self.describe(&item);
        Some(join_label(&title, &kind, &sub))
    }

    /// Read out the top row once typing pauses, unless it is the same row.
    fn announce_top_soon(self: &Rc<Self>) {
        let g = self.announce_generation.get() + 1;
        self.announce_generation.set(g);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(DEBOUNCE * 2, move || {
            let Some(o) = weak.upgrade() else { return };
            if o.announce_generation.get() != g || !o.is_visible() || !o.entry_has_focus() {
                return;
            }
            let Some(label) = o.row_label(o.selected.get()) else {
                return;
            };
            if *o.announced.borrow() != label {
                o.announced.replace(label.clone());
                o.announce(&label);
            }
        });
    }

    fn activate_selected(self: &Rc<Self>) {
        let q = self.entry.text().to_string();
        // After an answer, or while it waits for a reply, Enter asks.
        if self.assistant.is_visible() || self.awaiting_reply.get() {
            if !q.trim().is_empty() {
                self.ask(&q);
            }
            return;
        }
        // Enter straight after typing, before the rows caught up.
        if *self.items_text.borrow() != q {
            self.selected.set(0);
            self.refresh_results();
        }
        let item = self
            .items
            .borrow()
            .get(self.selected.get())
            .cloned()
            .flatten();
        if let Some(item) = item {
            self.activate(item);
        } else if !q.trim().is_empty() {
            self.ask(&q);
        }
    }

    /// The rows for `text`, best first.
    fn compute(&self, text: &str) -> Vec<Entry> {
        let text = text.trim();
        let mut out = Vec::new();
        let ask_row = |label: String, sub: String| {
            Entry::Item(Item::Ask {
                question: text.to_string(),
                label,
                sub,
            })
        };
        if self.awaiting_reply.get() {
            if !text.is_empty() {
                out.push(ask_row(
                    i18n::umoja("Reply: “%s”").replace("%s", text),
                    i18n::umoja("Enter"),
                ));
            }
            return out;
        }
        if !self.attachments.borrow().is_empty() {
            out.push(ask_row(
                if text.is_empty() {
                    i18n::umoja("Explain what you attached")
                } else {
                    i18n::umoja("Ask Umoja: “%s”").replace("%s", text)
                },
                i18n::umoja("With what you attached"),
            ));
            return out;
        }
        let focus = self.focused_app();
        let usage = self.usage.borrow();

        if text.is_empty() {
            let mut shown: Vec<String> = Vec::new();
            if let Some(app) = &focus {
                let acts: Vec<Item> = self
                    .actions
                    .borrow()
                    .iter()
                    .filter(|(a, _, _)| a.id() == app.id())
                    .take(4)
                    .map(|(a, action, name)| Item::Action {
                        app: a.clone(),
                        action: action.clone(),
                        name: name.clone(),
                    })
                    .collect();
                if !acts.is_empty() {
                    out.push(Entry::Heading(
                        i18n::umoja("In %s").replace("%s", &app.display_name()),
                    ));
                    for a in acts {
                        shown.extend(a.id());
                        out.push(Entry::Item(a));
                    }
                }
            }
            let frequent: Vec<Item> = usage
                .top(10)
                .into_iter()
                .filter(|id| !shown.contains(id))
                .filter_map(|id| self.resolve(&id))
                .take(5)
                .collect();
            if !frequent.is_empty() {
                out.push(Entry::Heading(i18n::umoja("Frequently used")));
                for item in frequent {
                    shown.extend(item.id());
                    out.push(Entry::Item(item));
                }
            }
            out.push(Entry::Heading(i18n::umoja("Suggested")));
            for id in ["ask-window", "clipboard", "screenshot"] {
                if id == "ask-window" && self.last_window.borrow().is_none() {
                    continue;
                }
                if let Some(item) = catalog::CATALOG.iter().find(|i| i.id == id)
                    && !shown.contains(&format!("instant:{id}"))
                {
                    out.push(Entry::Item(Item::Instant { item, intent: None }));
                }
            }
            return out;
        }

        if let Some(v) = calc::evaluate(text) {
            out.push(Entry::Item(Item::Sum(calc::format(v))));
        }
        let words = ask::search_words(text);
        let intent = ask::switch_intent(text);
        let mut scored: Vec<(u32, Item)> = Vec::new();
        for app in self.apps.borrow().iter() {
            let kw = appgrid::keywords(app);
            let kw: Vec<&str> = kw.iter().map(String::as_str).collect();
            let s = ask::score(&app.display_name(), &kw, &words);
            if s > 0 {
                let item = Item::App(app.clone());
                let bonus = item.id().map(|id| usage.bonus(&id)).unwrap_or(0);
                scored.push((s + bonus + 2, item));
            }
        }
        for (app, action, name) in self.actions.borrow().iter() {
            let app_name = app.display_name().to_lowercase();
            let focused = focus.as_ref().is_some_and(|f| f.id() == app.id());
            // Another app's actions only when its name is typed too:
            // "new window" alone would list every browser's.
            if !focused && ask::score(&app_name, &[], &words) == 0 {
                continue;
            }
            let s = ask::score(name, &[app_name.as_str()], &words);
            if s > 0 {
                let item = Item::Action {
                    app: app.clone(),
                    action: action.clone(),
                    name: name.clone(),
                };
                let bonus = item.id().map(|id| usage.bonus(&id)).unwrap_or(0);
                scored.push((s + bonus + if focused { 10 } else { 0 }, item));
            }
        }
        for item in catalog::CATALOG {
            let title = i18n::umoja(item.title);
            let mut kw: Vec<&str> = item.keywords.to_vec();
            kw.push(item.title);
            let s = ask::score(&title, &kw, &words);
            if s > 0 {
                let intent = matches!(item.run, Run::Toggle { .. })
                    .then_some(intent)
                    .flatten();
                // "wifi off" is about the switch, not its Settings page.
                let bonus = usage.bonus(&format!("instant:{}", item.id))
                    + if intent.is_some() { 5 } else { 0 };
                scored.push((s + bonus, Item::Instant { item, intent }));
            }
        }
        // Stable: equal scores keep apps, then actions, then the catalog.
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        let best = scored.first().map(|(s, _)| *s);
        let hits: Vec<Item> = scored
            .into_iter()
            .take(MAX_MATCHES)
            .map(|(_, i)| i)
            .collect();
        let files: Vec<Item> = {
            let fh = self.file_hits.borrow();
            if fh.0 == text {
                fh.1.iter().cloned().map(Item::File).collect()
            } else {
                Vec::new()
            }
        };
        let ask_item = ask_row(
            i18n::umoja("Ask Umoja: “%s”").replace("%s", text),
            match &focus {
                Some(app) => i18n::umoja("With %s in mind").replace("%s", &app.display_name()),
                None => i18n::umoja("Tab"),
            },
        );
        let questionish = lemonade::looks_like_question(text) || hits.is_empty();
        let file_first = crate::files::looks_like_file_search(text);
        if ask::ask_first(questionish && !file_first, best, text) {
            out.push(ask_item);
            out.extend(files.into_iter().map(Entry::Item));
            out.extend(hits.into_iter().map(Entry::Item));
        } else {
            if file_first {
                out.extend(files.into_iter().map(Entry::Item));
                out.extend(hits.into_iter().map(Entry::Item));
            } else {
                out.extend(hits.into_iter().map(Entry::Item));
                out.extend(files.into_iter().map(Entry::Item));
            }
            out.push(ask_item);
        }
        out.push(Entry::Item(Item::Web(text.to_string())));
        out
    }

    /// The row a usage-history id stands for, if it still exists.
    fn resolve(&self, id: &str) -> Option<Item> {
        if let Some(key) = id.strip_prefix("app:") {
            return self
                .apps
                .borrow()
                .iter()
                .find(|a| appgrid::app_key(a) == key)
                .map(|a| Item::App(a.clone()));
        }
        if let Some(rest) = id.strip_prefix("action:") {
            let (app_id, action) = rest.rsplit_once(':')?;
            return self
                .actions
                .borrow()
                .iter()
                .find(|(a, act, _)| act == action && a.id().as_deref() == Some(app_id))
                .map(|(a, act, name)| Item::Action {
                    app: a.clone(),
                    action: act.clone(),
                    name: name.clone(),
                });
        }
        let iid = id.strip_prefix("instant:")?;
        catalog::CATALOG
            .iter()
            .find(|i| i.id == iid)
            .map(|item| Item::Instant { item, intent: None })
    }

    /// Ask umoja-files about `q` once typing pauses; the rows are rebuilt
    /// when the hits arrive.
    fn search_files(self: &Rc<Self>, q: &str) {
        let q = q.trim().to_string();
        let g = self.file_generation.get() + 1;
        self.file_generation.set(g);
        if q.chars().count() < 3 || self.awaiting_reply.get() {
            self.file_hits.replace((String::new(), Vec::new()));
            return;
        }
        if self.file_hits.borrow().0 == q {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(DEBOUNCE, move || {
            let Some(o) = weak.upgrade() else { return };
            if o.file_generation.get() != g {
                return;
            }
            glib::spawn_future_local(async move {
                let hits = crate::files::search(&q, MAX_FILES)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::debug!(error = %e, "file search unavailable");
                        Vec::new()
                    });
                if o.file_generation.get() != g {
                    return;
                }
                o.file_hits.replace((q.clone(), hits));
                if !o.file_hits.borrow().1.is_empty() && o.entry.text().trim() == q {
                    o.rebuild_rows(true);
                }
            });
        });
    }

    /// Title, subtitle and kind shown for `item`.
    fn describe(&self, item: &Item) -> (String, String, String) {
        match item {
            Item::App(a) => (
                a.display_name().to_string(),
                a.description().map(|d| d.to_string()).unwrap_or_default(),
                i18n::umoja("App"),
            ),
            Item::Action { app, name, .. } => (
                name.clone(),
                i18n::umoja("%s · from its launcher").replace("%s", &app.display_name()),
                app.display_name().to_string(),
            ),
            Item::Instant { item, intent } => {
                let title = i18n::umoja(item.title);
                match item.run {
                    Run::Toggle { state, .. } => {
                        let now = self.switches.borrow().get(state).copied();
                        let title = match intent {
                            Some(true) => i18n::umoja("Turn %s On").replace("%s", &title),
                            Some(false) => i18n::umoja("Turn %s Off").replace("%s", &title),
                            None => title,
                        };
                        let sub = match now {
                            Some(true) => i18n::umoja("On · Enter switches it"),
                            Some(false) => i18n::umoja("Off · Enter switches it"),
                            None => i18n::umoja("Enter switches it"),
                        };
                        (title, sub, i18n::umoja("Switch"))
                    }
                    _ => {
                        let kind = match item.group {
                            catalog::Group::Control => i18n::umoja("Action"),
                            catalog::Group::Setting => i18n::umoja("Settings"),
                        };
                        (title, String::new(), kind)
                    }
                }
            }
            Item::Sum(v) => (
                format!("= {v}"),
                i18n::umoja("Enter copies the result"),
                i18n::umoja("Answer"),
            ),
            Item::File(h) => (h.name.clone(), h.detail.clone(), i18n::umoja("File")),
            Item::Ask { label, sub, .. } => (label.clone(), sub.clone(), i18n::umoja("Ask")),
            Item::Web(q) => (
                i18n::umoja("Search the web for “%s”").replace("%s", q),
                String::new(),
                i18n::umoja("Web"),
            ),
        }
    }

    fn row_widget(&self, item: &Item) -> gtk::ListBoxRow {
        let (title, sub, kind) = self.describe(item);
        let icon: gtk::Image = match item {
            Item::App(a) => a
                .icon()
                .map(|i| gtk::Image::from_gicon(&i))
                .unwrap_or_else(|| gtk::Image::from_icon_name("application-x-executable")),
            Item::Action { app, .. } => app
                .icon()
                .map(|i| gtk::Image::from_gicon(&i))
                .unwrap_or_else(|| gtk::Image::from_icon_name("application-x-executable")),
            Item::Instant { item, .. } => gtk::Image::from_gicon(&gio::ThemedIcon::from_names(
                &crate::ask::catalog::icon_names(item.icon),
            )),
            Item::Sum(_) => gtk::Image::from_icon_name("accessories-calculator-symbolic"),
            Item::File(h) => gtk::Image::from_gicon(&crate::files::file_icon(&h.path)),
            Item::Ask { .. } => gtk::Image::from_gicon(&gio::ThemedIcon::from_names(&[
                "starred-symbolic",
                "dialog-question-symbolic",
            ])),
            Item::Web(_) => gtk::Image::from_icon_name("web-browser-symbolic"),
        };
        icon.set_pixel_size(match item {
            Item::App(_) | Item::Action { .. } | Item::File(_) => 28,
            _ => 18,
        });
        icon.set_accessible_role(gtk::AccessibleRole::Presentation);
        let icon_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_box.add_css_class("ask-icon");
        if matches!(item, Item::Ask { .. }) {
            icon_box.add_css_class("ask-icon-accent");
        }
        icon_box.set_valign(gtk::Align::Center);
        // Set outright, or the icon's hexpand spreads to the box and the
        // box takes half the row, pushing the icon and text to the middle.
        icon_box.set_hexpand(false);
        icon.set_hexpand(true);
        icon.set_halign(gtk::Align::Center);
        icon_box.append(&icon);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 1);
        texts.set_hexpand(true);
        texts.set_valign(gtk::Align::Center);
        texts.append(
            &gtk::Label::builder()
                .label(&title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["ask-title"])
                .build(),
        );
        if !sub.is_empty() {
            texts.append(
                &gtk::Label::builder()
                    .label(&sub)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["ask-sub"])
                    .build(),
            );
        }
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.add_css_class("ask-row");
        content.append(&icon_box);
        content.append(&texts);
        let mut state = None;
        if let Item::Instant { item, .. } = item
            && let Run::Toggle { state: key, .. } = item.run
        {
            state = self.switches.borrow().get(key).copied();
        }
        match state {
            Some(on) => {
                // Shows the state; picking the row is what switches it.
                let sw = gtk::Switch::builder()
                    .active(on)
                    .can_focus(false)
                    .can_target(false)
                    .valign(gtk::Align::Center)
                    .build();
                sw.set_accessible_role(gtk::AccessibleRole::Presentation);
                content.append(&sw);
            }
            None => content.append(
                &gtk::Label::builder()
                    .label(&kind)
                    .css_classes(["ask-kind"])
                    .valign(gtk::Align::Center)
                    .build(),
            ),
        }
        // The focus stays in the entry; the arrows move the selection.
        let row = gtk::ListBoxRow::builder()
            .child(&content)
            .can_focus(false)
            .build();
        let state_word = state.map(|on| {
            if on {
                i18n::umoja("on")
            } else {
                i18n::umoja("off")
            }
        });
        let label = match state_word {
            Some(s) => format!("{title}, {s}"),
            None => format!("{title}, {kind}"),
        };
        row.update_property(&[gtk::accessible::Property::Label(&label)]);
        if !sub.is_empty() {
            row.update_property(&[gtk::accessible::Property::Description(&sub)]);
        }
        row
    }

    /// Do what `item` says.
    fn activate(self: &Rc<Self>, item: Item) {
        if let Some(id) = item.id() {
            self.usage.borrow_mut().record(&id);
        }
        match item {
            Item::App(a) => {
                appgrid::launch_app(self.window.upcast_ref(), &a);
                self.hide();
            }
            Item::Action { app, action, .. } => {
                appgrid::launch_action(self.window.upcast_ref(), &app, &action);
                self.hide();
            }
            Item::Instant { item, intent } => self.run_instant(item, intent),
            Item::Sum(v) => {
                self.copy_text(&v);
                self.announce(&i18n::umoja("Copied %s").replace("%s", &v));
                self.hide();
            }
            Item::File(h) => {
                let ctx = WidgetExt::display(&self.window).app_launch_context();
                match crate::files::open(&h.path, Some(ctx.upcast_ref())) {
                    Ok(()) => self.hide(),
                    Err(e) => self.show_error(&e.to_string()),
                }
            }
            Item::Ask { question, .. } => self.ask(&question),
            Item::Web(q) => {
                let ctx = WidgetExt::display(&self.window).app_launch_context();
                match gio::AppInfo::launch_default_for_uri(&ask::web_search_url(&q), Some(&ctx)) {
                    Ok(()) => self.hide(),
                    Err(e) => self.show_error(&e.to_string()),
                }
            }
        }
    }

    fn run_instant(self: &Rc<Self>, item: &'static Instant, intent: Option<bool>) {
        match item.run {
            Run::Local(l) => {
                let shots = self.shots.borrow().upgrade();
                match l {
                    Local::ScreenshotUi => {
                        self.hide();
                        if let Some(s) = shots {
                            s.open();
                        }
                    }
                    Local::ScreenRecordingUi => {
                        self.hide();
                        if let Some(s) = shots {
                            s.toggle_screen_recording();
                        }
                    }
                    Local::ClipboardHistory => self.clip_button.set_active(true),
                    Local::AskAboutWindow => self.attach_window(),
                }
            }
            Run::Toggle { tool, state } => {
                let on = intent.unwrap_or_else(|| {
                    !self.switches.borrow().get(state).copied().unwrap_or(false)
                });
                self.call_tool(
                    tool,
                    serde_json::json!({ "enabled": on }),
                    Some((state, on)),
                );
            }
            Run::Call { tool, args } => {
                let args = serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
                self.call_tool(tool, args, None);
            }
        }
    }

    /// One umoja-mcp tool call, with the server's confirm rules. A switch
    /// (`switch`: its state name and the new value) just flips its row; any
    /// other call shows as a step, and the card closes when the tool put
    /// something in front of the user.
    fn call_tool(
        self: &Rc<Self>,
        tool: &'static str,
        args: serde_json::Value,
        switch: Option<(&'static str, bool)>,
    ) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let ui = Rc::new(OverlayUi {
            overlay: Rc::downgrade(self),
            generation,
            rows: RefCell::new(HashMap::new()),
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(client) = mcp::shared().await else {
                if let Some(o) = weak.upgrade() {
                    o.show_error(&i18n::umoja("The desktop tools aren't running."));
                }
                return;
            };
            let (label, _) = client.describe(tool, &args).await;
            if switch.is_none() {
                if let Some(o) = weak.upgrade() {
                    o.reset_card();
                    o.answer.set_visible(false);
                    o.stack.set_visible(false);
                    *o.asked_text.borrow_mut() = o.entry.text().to_string();
                }
                ui.step(0, &label, StepState::Running);
            }
            let confirm: mcp::Confirm = {
                let ui = ui.clone();
                Rc::new(move |label, risk| ui.confirm(&label, risk))
            };
            let r = client.call(tool, args, confirm).await;
            let Some(o) = weak.upgrade() else { return };
            if o.generation.get() != generation {
                return;
            }
            if let Some((state, on)) = switch {
                if !r.is_error && !r.declined {
                    o.switches.borrow_mut().insert(state.to_string(), on);
                    o.rebuild_rows(true);
                    o.announce(&r.text);
                } else {
                    o.show_error(&r.text);
                }
                return;
            }
            let state = if r.declined {
                StepState::Declined
            } else if r.is_error {
                StepState::Failed
            } else {
                StepState::Done
            };
            ui.step(0, &label, state);
            if r.hide && !r.is_error {
                o.hide();
                return;
            }
            if r.is_error {
                o.answer.set_visible(true);
                o.answer.set_text(&r.text);
            }
            o.announce(&if r.is_error { r.text.clone() } else { label });
        });
    }

    /// Ask "Allow Umoja to do this?" (or `title`) inside the card, with
    /// Cancel focused. Resolves to the answer; false if the card closes.
    fn confirm_inline(
        self: &Rc<Self>,
        label: &str,
        risk: Risk,
        title: &str,
        allow: &str,
    ) -> Pin<Box<dyn Future<Output = bool>>> {
        self.cancel_confirm();
        while let Some(c) = self.confirm_box.first_child() {
            self.confirm_box.remove(&c);
        }
        let (tx, rx) = async_channel::bounded::<bool>(1);
        self.confirm_box.append(
            &gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["heading"])
                .build(),
        );
        self.confirm_box.append(
            &gtk::Label::builder()
                .label(label)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .css_classes(["ask-confirm-what"])
                .build(),
        );
        let cancel = gtk::Button::builder()
            .label(i18n::gtk("_Cancel"))
            .use_underline(true)
            .css_classes(["pill", "flat"])
            .build();
        let allow_b = gtk::Button::builder()
            .label(allow)
            .css_classes(if risk == Risk::High {
                ["pill", "destructive-action"]
            } else {
                ["pill", "suggested-action"]
            })
            .build();
        for (b, answer) in [(&cancel, false), (&allow_b, true)] {
            let tx = tx.clone();
            b.connect_clicked(move |_| {
                let _ = tx.try_send(answer);
            });
        }
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&allow_b);
        self.confirm_box.append(&buttons);
        self.confirm_tx.replace(Some(tx));
        self.assistant.set_visible(true);
        self.confirm_box.set_visible(true);
        self.stack.set_visible(false);
        *self.asked_text.borrow_mut() = self.entry.text().to_string();
        cancel.grab_focus();
        self.announce(&format!("{title} {label}"));
        let weak = Rc::downgrade(self);
        Box::pin(async move {
            let answer = rx.recv().await.unwrap_or(false);
            if let Some(o) = weak.upgrade() {
                o.confirm_tx.take();
                o.confirm_box.set_visible(false);
                o.entry.grab_focus();
            }
            answer
        })
    }

    /// Answer the confirmation that is showing, as if Allow or Cancel was
    /// clicked (tools/demo). False when none is showing.
    pub fn answer_confirm(&self, allow: bool) -> bool {
        match self.confirm_tx.take() {
            Some(tx) => tx.try_send(allow).is_ok(),
            None => false,
        }
    }

    /// Decline a confirmation that is waiting.
    fn cancel_confirm(&self) {
        if let Some(tx) = self.confirm_tx.take() {
            let _ = tx.try_send(false);
        }
        self.confirm_box.set_visible(false);
    }

    /// Answer a question, letting the assistant act through its tools. Any
    /// attached context goes with it, or else a line about the focused app.
    fn ask(self: &Rc<Self>, query: &str) {
        // A reply to the assistant's question: any attached context already
        // went with the first message.
        let replying = self.awaiting_reply.get();
        let attachments = if replying {
            Vec::new()
        } else {
            self.attachments.borrow().clone()
        };
        let text = match query.trim() {
            "" if !attachments.is_empty() => context::DEFAULT_QUESTION.to_string(),
            q => q.to_string(),
        };
        let context = context::build_context(&attachments).or_else(|| {
            (!replying && self.conversation.borrow().is_empty())
                .then(|| self.focus_context())
                .flatten()
        });
        self.ask_question(Question {
            text,
            context,
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
            i18n::umoja("Reply to Umoja…")
        } else {
            placeholder()
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

        // A new message in the same card continues the conversation: "yes"
        // answers "Do you want me to open it?".
        if question.history.is_empty() {
            question.history = self.conversation.borrow().clone();
        }
        if question.history.is_empty() {
            self.local_chat.set(false);
        }
        self.set_awaiting_reply(false);
        let asked = context::truncate_chars(&agent::user_content(&question), MAX_HISTORY_CHARS);

        self.reset_card();
        *self.asked_text.borrow_mut() = self.entry.text().to_string();
        self.stack.set_visible(false);
        self.question_label
            .set_text(&context::truncate_chars(question.text.trim(), 200));
        self.question_label.set_visible(true);
        self.answer.set_text(&i18n::umoja("Thinking…"));
        // The search_files tool fills the list with its own results.
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
            o.show_answer_actions(!shown.trim().is_empty());
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
            o.announce(&shown);
            o.speak_if_wanted(&shown);
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
        *self.asked_text.borrow_mut() = self.entry.text().to_string();
        self.stack.set_visible(false);
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
                        o.show_answer_actions(!shown.is_empty());
                        o.announce(&shown);
                        o.speak_if_wanted(&shown);
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
        self.cancel_confirm();
        self.assistant.set_visible(true);
        self.answer.set_visible(true);
        self.question_label.set_visible(false);
        self.show_answer_actions(false);
        self.stop_speaking();
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
        *self.asked_text.borrow_mut() = self.entry.text().to_string();
        self.stack.set_visible(false);
        self.answer.set_text(text);
        self.announce(text);
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
                o.stack.set_visible_child_name(if b.is_active() {
                    "clipboard"
                } else {
                    "results"
                });
                o.rebuild_rows(true);
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
        let weak = Rc::downgrade(self);
        self.speak_button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                if o.speak_serial.get() % 2 == 1 {
                    o.stop_speaking();
                } else {
                    let text = o.answer_text.borrow().clone();
                    o.speak(&text);
                }
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
            pill.append(&gtk::Image::from_gicon(&gio::ThemedIcon::from_names(
                &crate::ask::catalog::icon_names(a.icon()),
            )));
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
            placeholder()
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
            let asked = self.confirm_inline(
                cmd,
                Risk::High,
                &i18n::umoja("Run this command?"),
                &i18n::umoja("Run"),
            );
            let weak = Rc::downgrade(self);
            let action = action.clone();
            glib::spawn_future_local(async move {
                if asked.await
                    && let Some(o) = weak.upgrade()
                {
                    o.execute(&action);
                }
            });
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
                    appgrid::launch_app(self.window.upcast_ref(), app.upcast_ref());
                    self.hide();
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
        let asked = o.confirm_inline(
            label,
            risk,
            &i18n::umoja("Allow Umoja to do this?"),
            &i18n::umoja("Allow"),
        );
        let weak = self.overlay.clone();
        let generation = self.generation;
        Box::pin(async move {
            let allowed = asked.await;
            // Allowed only if it is still the same question.
            allowed
                && weak
                    .upgrade()
                    .is_some_and(|o| o.generation.get() == generation)
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

/// Every app's launcher actions (Desktop Actions in its .desktop file).
fn load_actions() -> Vec<(gio::DesktopAppInfo, String, String)> {
    let mut out = Vec::new();
    for app in gio::AppInfo::all().into_iter().filter(|a| a.should_show()) {
        let Ok(d) = app.downcast::<gio::DesktopAppInfo>() else {
            continue;
        };
        for action in d.list_actions() {
            let name = d.action_name(&action).to_string();
            if !name.is_empty() {
                out.push((d.clone(), action.to_string(), name));
            }
        }
    }
    out
}

/// "Wi-Fi, Switch, On · Enter switches it": what a screen reader says for
/// a row.
fn join_label(title: &str, kind: &str, sub: &str) -> String {
    [title, kind, sub]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) use umoja_common::terminal::{needs_terminal, spawn_in_terminal};
