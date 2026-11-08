//! The overlay window: search entry, app grid and assistant answers.

use crate::assistant::agent::{self, Question, StepState};
use crate::assistant::tools::Risk;
use crate::lemonade::{self, LemonadeClient};
use crate::myna::{Myna, MynaEvent};
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
use umoja_common::gsettings as gs;
use umoja_common::i18n;
use umoja_common::types::{AssistantAction, extract_actions, strip_actions};

const MAX_RESULTS: usize = 48;

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
    lemonade: LemonadeClient,
    myna: Myna,
    /// Bumped on every new question so stale streams are dropped.
    generation: Cell<u64>,
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
        search_row.append(&entry);
        search_row.append(&myna_pill);
        search_row.append(&crate::model_picker::button());

        // "Downloading the assistant's model… 42%" while it's fetched.
        let model_status = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .css_classes(["dim-label", "model-status"])
            .visible(false)
            .build();

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
        let assistant = gtk::Box::new(gtk::Orientation::Vertical, 12);
        assistant.add_css_class("assistant-card");
        assistant.set_halign(gtk::Align::Center);
        assistant.set_width_request(640);
        assistant.append(&steps);
        assistant.append(&answer);
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

        let root = gtk::Box::new(gtk::Orientation::Vertical, 24);
        root.set_margin_top(64);
        root.set_margin_start(96);
        root.set_margin_end(96);
        root.set_margin_bottom(48);
        root.append(&search_row);
        root.append(&model_status);
        root.append(&ask_button);
        root.append(&assistant);
        root.append(&scroller);
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
            lemonade: LemonadeClient::new(),
            myna: Myna::start(),
            generation: Cell::new(0),
        });
        root.insert_child_after(&overlay.files.widget, Some(&overlay.assistant));
        overlay.connect_signals();
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
            if lemonade::looks_like_question(&q) || o.grid.first_child().is_none() {
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

    pub fn show(&self) {
        self.entry.set_text("");
        self.assistant.set_visible(false);
        self.files.clear();
        self.window.present();
        self.entry.grab_focus();
    }

    pub fn set_model_status(&self, status: Option<String>) {
        self.model_status.set_visible(status.is_some());
        self.model_status.set_text(status.as_deref().unwrap_or(""));
    }

    /// Open the app search with `text` already typed (from the overview).
    pub fn show_with_text(&self, text: &str) {
        self.show();
        self.entry.set_text(text);
        self.entry.set_position(-1);
    }

    pub fn hide(&self) {
        self.generation.set(self.generation.get() + 1);
        self.window.set_visible(false);
    }

    pub fn toggle(&self) {
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
        self.ask_button.set_visible(
            !q.trim().is_empty() && (found.is_empty() || lemonade::looks_like_question(&q)),
        );
        if q.trim().is_empty() {
            self.assistant.set_visible(false);
        }
        self.files.update(&q);
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
        button.connect_clicked(move |_| {
            if let Some(o) = weak.upgrade() {
                o.launch(&app);
            }
        });
        button.upcast()
    }

    fn launch(&self, app: &gio::AppInfo) {
        let ctx = gtk::prelude::WidgetExt::display(&self.window).app_launch_context();
        if let Err(e) = app.launch(&[], Some(&ctx)) {
            tracing::warn!(error = %e, "launch failed");
        }
        self.hide();
    }

    /// Answer a question, letting the assistant act through its tools.
    fn ask(self: &Rc<Self>, query: &str) {
        self.ask_question(Question::from(query.trim()));
    }

    /// [`Self::ask`] with optional context (selected text, window contents).
    pub fn ask_question(self: &Rc<Self>, question: Question) {
        if question.text.trim().is_empty() {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        self.assistant.set_visible(true);
        self.answer.set_visible(true);
        self.answer.set_text(&i18n::umoja("Thinking…"));
        for b in [self.chips.upcast_ref::<gtk::Widget>(), self.steps.upcast_ref()] {
            while let Some(c) = b.first_child() {
                c.unparent();
            }
        }
        self.steps.set_visible(false);
        crate::assistant::take_hide_request();

        let ui: Rc<dyn agent::Ui> = Rc::new(OverlayUi {
            overlay: Rc::downgrade(self),
            generation,
            rows: RefCell::new(HashMap::new()),
        });
        let client = self.lemonade.clone();
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
            // A tool opened an app or moved a window: get out of the way.
            if crate::assistant::take_hide_request() {
                o.hide();
            }
        });
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
