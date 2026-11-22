//! The Agents pill: the personal agent (Hermes first), running in its sandbox
//! (umoja-agentd, org.umoja.Agents), in the top bar.
//!
//! It shows only once Hermes is set up (or being set up). A sparkle-shield
//! icon, a dot while Hermes works, and a count in accent colour when
//! Hermes is waiting for an answer. Clicking it opens the Agents card:
//!
//! * consent cards first: a site OpenShell held back ("Hermes wants to
//!   reach example.org"), a command Hermes's rules flag, or a question,
//!   each with Deny / Allow (and Always allow for a site);
//! * the tasks, with what each is doing now and Stop, or the answer and
//!   Open for finished ones;
//! * Pause all, the agent's folder, and setup progress or problems.
//!
//! The same consent cards also arrive as notifications (umoja-agentd sends
//! them), so answering works from either place. This module only renders
//! the state umoja-agentd sends with StateChanged; everything it does is a
//! method call back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use serde::Deserialize;
use umoja_common::i18n;

const BUS: &str = "org.umoja.Agents";
const PATH: &str = "/org/umoja/Agents";
const IFACE: &str = "org.umoja.Agents";
/// Finished tasks kept on the card.
const DONE_SHOWN: usize = 3;

/// A folder shared with the agent.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SharedFolder {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub finished: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Approval {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub flagged: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub setup: String,
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub needs: String,
    #[serde(default)]
    pub preset: String,
    #[serde(default)]
    pub paused: bool,
    /// "pausing" or "resuming" while the sandbox stops or starts.
    #[serde(default)]
    pub switching: String,
    #[serde(default)]
    pub sandbox: String,
    #[serde(default)]
    pub memory_mb: u32,
    #[serde(default)]
    pub memory_used_mb: u32,
    #[serde(default)]
    pub workspace: String,
    /// The agent's name ("Hermes"); agents come from umoja-agentd's catalog.
    #[serde(default)]
    pub agent_name: String,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub approvals: Vec<Approval>,
    #[serde(default)]
    pub shared: Vec<SharedFolder>,
}

impl State {
    /// The agent's name, or "Agent" before umoja-agentd says.
    pub fn name(&self) -> String {
        if self.agent_name.is_empty() { i18n::umoja("Agent") } else { self.agent_name.clone() }
    }

    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    /// Whether the pill shows at all.
    pub fn visible(&self) -> bool {
        // "error" only follows a Set Up the user pressed, so the button stays
        // to show what went wrong and how to fix it (Allow, Try Again).
        matches!(self.setup.as_str(), "ready" | "working" | "error") || !self.approvals.is_empty()
    }

    pub fn running(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == "running" || t.status == "waiting")
            .count()
    }

    /// One line under "Hermes" on the card.
    pub fn summary(&self) -> String {
        if self.setup == "working" {
            return format!("{}…", if self.step.is_empty() { "Setting up" } else { &self.step });
        }
        if self.setup == "error" {
            return i18n::umoja("Needs attention");
        }
        match self.switching.as_str() {
            "pausing" => return i18n::umoja("Pausing…"),
            "resuming" => return i18n::umoja("Resuming…"),
            _ => {}
        }
        if self.paused {
            return i18n::umoja("Paused");
        }
        let waiting = self.approvals.len();
        let running = self.running();
        match (waiting, running) {
            (0, 0) => i18n::umoja("Ready"),
            (0, 1) => i18n::umoja("Working on 1 task"),
            (0, n) => format!("Working on {n} tasks"),
            (1, _) => i18n::umoja("Waiting for you"),
            (n, _) => format!("{n} things wait for you"),
        }
    }
}

struct Pill {
    button: gtk::MenuButton,
    dot: gtk::Box,
    count: gtk::Label,
    popover: gtk::Popover,
    title: gtk::Label,
    subtitle: gtk::Label,
    folder: gtk::Button,
    pause: gtk::Button,
    approvals: gtk::Box,
    tasks: gtk::Box,
    problem: gtk::Box,
    /// "Shared folders": each share (read-only or can change, Stop
    /// Sharing) and Share a Folder…
    shares: gtk::Box,
    shares_shown: RefCell<Option<Vec<SharedFolder>>>,
    /// Why the last button press didn't work ("That request isn't waiting
    /// any more."), for a few seconds.
    error: gtk::Label,
    state: RefCell<State>,
    /// Text typed into question replies, kept across redraws.
    drafts: RefCell<HashMap<String, String>>,
    /// Last rendered (approvals, tasks), so a memory-only update doesn't
    /// rebuild the card under the pointer.
    rendered: RefCell<(Vec<Approval>, Vec<Task>, String)>,
    conn: RefCell<Option<gio::DBusConnection>>,
}

pub fn button() -> gtk::MenuButton {
    // Our own hicolor icon only: with a fallback name GTK would pick the
    // icon theme's fallback first.
    let icon = gtk::Image::from_icon_name("umoja-agents-symbolic");
    let dot = gtk::Box::builder()
        .css_classes(["agents-dot"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    let count = gtk::Label::builder()
        .css_classes(["agents-count"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    content.append(&icon);
    content.append(&dot);
    content.append(&count);

    let title = gtk::Label::builder()
        .label(i18n::umoja("Agent"))
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .css_classes(["dim-label", "agents-subtitle"])
        .build();
    let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
    titles.set_hexpand(true);
    titles.append(&title);
    titles.append(&subtitle);
    let pause = gtk::Button::builder()
        .css_classes(["flat", "circular", "agents-pause"])
        .valign(gtk::Align::Center)
        .build();
    let folder = gtk::Button::builder()
        .icon_name("folder-symbolic")
        .tooltip_text(i18n::umoja("Open the agent's folder"))
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.add_css_class("agents-header");
    let badge = gtk::Image::from_gicon(&gio::ThemedIcon::from_names(&[
        "umoja-agents-symbolic",
        "security-high-symbolic",
    ]));
    badge.add_css_class("agents-badge");
    head.append(&badge);
    head.append(&titles);
    head.append(&folder);
    head.append(&pause);

    let problem = gtk::Box::new(gtk::Orientation::Vertical, 6);
    problem.add_css_class("agents-problem");
    problem.set_visible(false);
    let error = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .max_width_chars(40)
        .css_classes(["agents-error"])
        .visible(false)
        .build();
    let approvals = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let tasks = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.append(&head);
    card.append(&problem);
    card.append(&error);
    card.append(&approvals);
    card.append(&tasks);
    let shares = gtk::Box::new(gtk::Orientation::Vertical, 4);
    shares.add_css_class("agents-shares");
    shares.set_visible(false);
    card.append(&shares);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&card)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(640)
        .build();
    let popover = gtk::Popover::builder()
        .child(&scroller)
        .css_classes(["agents-popover"])
        .has_arrow(false)
        .build();
    crate::menu_gap::below_bar(&popover);
    umoja_common::blur::blur_behind(&popover);
    // Keyboard while open (Escape, typing an answer), like Quick Settings.
    popover.connect_show(|p| crate::notifications::want_keyboard(p, true));
    popover.connect_hide(|p| crate::notifications::want_keyboard(p, false));

    let button = gtk::MenuButton::builder()
        .child(&content)
        .popover(&popover)
        .css_classes(["flat", "agents-button"])
        .visible(false)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&i18n::umoja("Agents"))]);

    let this = Rc::new(Pill {
        button: button.clone(),
        dot,
        count,
        popover,
        title,
        subtitle,
        folder: folder.clone(),
        pause: pause.clone(),
        approvals,
        tasks,
        problem,
        shares,
        shares_shown: RefCell::default(),
        error,
        state: RefCell::default(),
        drafts: RefCell::default(),
        rendered: RefCell::default(),
        conn: RefCell::default(),
    });
    {
        let weak = Rc::downgrade(&this);
        pause.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let paused = this.state.borrow().paused;
                this.call("SetPaused", Some(&(!paused,).to_variant()));
            }
        });
    }
    {
        let weak = Rc::downgrade(&this);
        folder.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let path = this.state.borrow().workspace.clone();
                this.popover.popdown();
                if !path.is_empty() {
                    let file = gio::File::for_path(path);
                    gtk::FileLauncher::new(Some(&file)).launch(
                        None::<&gtk::Window>,
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                }
            }
        });
    }
    watch(&this);
    std::mem::forget(this);
    button
}

thread_local! {
    /// Others following the agent's state (the Quick Settings tile).
    static LISTENERS: RefCell<Vec<Box<dyn Fn(&State)>>> = RefCell::default();
    static LAST: RefCell<Option<State>> = RefCell::default();
}

/// The Agents tile in Quick Settings (the mockup's kill switch): on while
/// the agent may work, off pauses it. Subtitle like the pill's ("Working on
/// 1 task", "Waiting for you"); accent-amber while it waits. Shown once the
/// agent is set up.
pub fn quick_tile(make: fn(&str, &str) -> gtk::ToggleButton) -> gtk::ToggleButton {
    let t = make("umoja-agents-symbolic", &i18n::umoja("Agents"));
    t.set_visible(false);
    t.set_tooltip_text(Some(&i18n::umoja("Turn off to pause the background agent")));
    let syncing = Rc::new(std::cell::Cell::new(false));
    let show = {
        let (t, syncing) = (t.clone(), syncing.clone());
        move |state: &State| {
            t.set_visible(state.setup == "ready");
            syncing.set(true);
            t.set_active(!state.paused);
            t.set_sensitive(state.switching.is_empty());
            syncing.set(false);
            let text = format!("{}: {}", state.name(), state.summary());
            crate::network::set_tile_subtitle(&t, &text);
            if state.approvals.is_empty() {
                t.remove_css_class("agents-waiting");
            } else {
                t.add_css_class("agents-waiting");
            }
        }
    };
    if let Some(state) = LAST.with(|l| l.borrow().clone()) {
        show(&state);
    }
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(show)));
    t.connect_toggled(move |t| {
        if syncing.get() {
            return;
        }
        let paused = !t.is_active();
        glib::spawn_future_local(async move {
            if let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await {
                let _ = conn
                    .call_future(Some(BUS), PATH, IFACE, "SetPaused", Some(&(paused,).to_variant()), None,
                        gio::DBusCallFlags::NONE, 30_000)
                    .await;
            }
        });
    });
    t
}

/// "~/Documents/Finances" for a folder under the home folder.
fn home_words(path: &str) -> String {
    let home = glib::home_dir();
    match std::path::Path::new(path).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_owned(),
    }
}

/// "using 231 MB of its 4 GB memory": what the sandbox uses, under its cap.
fn memory_text(used_mb: u32, cap_mb: u32) -> String {
    fn size(mb: u32) -> String {
        if mb < 1024 {
            return format!("{mb} MB");
        }
        let gb = f64::from(mb) / 1024.0;
        if (gb - gb.round()).abs() < 0.05 {
            format!("{} GB", gb.round())
        } else {
            format!("{gb:.1} GB")
        }
    }
    i18n::umoja("using {used} of its {cap} memory")
        .replace("{used}", &size(used_mb))
        .replace("{cap}", &size(cap_mb))
}

impl Pill {
    fn call(&self, method: &str, args: Option<&glib::Variant>) {
        let Some(conn) = self.conn.borrow().clone() else { return };
        let name = method.to_owned();
        let error = self.error.clone();
        error.set_visible(false);
        conn.call(
            Some(BUS),
            PATH,
            IFACE,
            method,
            args,
            None,
            gio::DBusCallFlags::NONE,
            // FixLxd waits on the password prompt (and may install LXD).
            if method == "FixLxd" { i32::MAX } else { 30_000 },
            gio::Cancellable::NONE,
            move |res| {
                if let Err(mut e) = res {
                    glib::g_warning!("umoja-panel", "Agents.{name}: {e}");
                    // umoja-agentd's errors are written for people.
                    gio::DBusError::strip_remote_error(&mut e);
                    error.set_text(e.message());
                    error.set_visible(true);
                    let error = error.clone();
                    glib::timeout_add_seconds_local_once(8, move || error.set_visible(false));
                }
            },
        );
    }

    fn update(self: &Rc<Self>, state: State) {
        LISTENERS.with(|l| {
            for f in l.borrow().iter() {
                f(&state);
            }
        });
        LAST.replace(Some(state.clone()));
        let visible = state.visible();
        self.button.set_visible(visible);
        if !visible {
            self.popover.popdown();
        }
        let waiting = state.approvals.len();
        let running = state.running();
        self.count.set_text(&waiting.to_string());
        self.count.set_visible(waiting > 0);
        self.dot.set_visible(waiting == 0 && (running > 0 || state.setup == "working"));
        if waiting > 0 {
            self.button.add_css_class("attention");
        } else {
            self.button.remove_css_class("attention");
        }
        if state.paused {
            self.button.add_css_class("paused");
        } else {
            self.button.remove_css_class("paused");
        }
        let name = state.name();
        self.title.set_text(&name);
        self.folder
            .set_tooltip_text(Some(&i18n::umoja("Open {name}'s folder").replace("{name}", &name)));
        let tip = format!("{name} · {}", state.summary());
        self.button.set_tooltip_text(Some(&tip));

        let mut sub = state.summary();
        if state.memory_mb > 0 && state.memory_used_mb > 0 && state.setup == "ready" {
            sub = format!("{sub} · {}", memory_text(state.memory_used_mb, state.memory_mb));
        }
        self.subtitle.set_text(&sub);
        self.pause.set_icon_name(if state.paused {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        });
        self.pause.set_tooltip_text(Some(&if state.paused {
            i18n::umoja("Resume {name}").replace("{name}", &name)
        } else {
            i18n::umoja("Pause {name}").replace("{name}", &name)
        }));
        self.pause.set_visible(state.setup == "ready");
        self.pause.set_sensitive(state.switching.is_empty());

        self.fill_problem(&state);
        let shown_tasks = tasks_shown(&state.tasks);
        let key = (state.approvals.clone(), shown_tasks.clone(), state.setup.clone());
        if *self.rendered.borrow() != key {
            self.fill_approvals(&state.approvals);
            self.fill_tasks(&shown_tasks);
            self.rendered.replace(key);
        }
        self.shares.set_visible(state.setup == "ready");
        if self.shares_shown.borrow().as_ref() != Some(&state.shared) {
            self.fill_shares(&state.shared, &name);
            self.shares_shown.replace(Some(state.shared.clone()));
        }
        // New consent cards don't pop the card open: the notification
        // umoja-agentd sends for each is the interruption, and opening a
        // popover would take the keyboard from whatever the user is doing.
        self.state.replace(state);
    }

    fn fill_shares(self: &Rc<Self>, shared: &[SharedFolder], name: &str) {
        while let Some(c) = self.shares.first_child() {
            self.shares.remove(&c);
        }
        let heading = gtk::Label::builder()
            .label(i18n::umoja("Shared folders"))
            .xalign(0.0)
            .css_classes(["dim-label", "caption-heading"])
            .build();
        self.shares.append(&heading);
        for f in shared {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.add_css_class("agents-share");
            let label = gtk::Label::builder()
                .label(home_words(&f.path))
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(28)
                .tooltip_text(&f.path)
                .build();
            let change = gtk::CheckButton::builder()
                .label(i18n::umoja("Can change"))
                .active(f.writable)
                .tooltip_text(i18n::umoja("Let {name} change files here, not only read them").replace("{name}", name))
                .build();
            let stop = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(i18n::umoja("Stop sharing"))
                .css_classes(["flat", "circular"])
                .valign(gtk::Align::Center)
                .build();
            let path = f.path.clone();
            let weak = Rc::downgrade(self);
            change.connect_toggled(move |c| {
                if let Some(this) = weak.upgrade() {
                    this.reshare(path.clone(), c.is_active());
                }
            });
            let path = f.path.clone();
            let weak = Rc::downgrade(self);
            stop.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.call("Unshare", Some(&(path.as_str(),).to_variant()));
                }
            });
            row.append(&label);
            row.append(&change);
            row.append(&stop);
            self.shares.append(&row);
        }
        if shared.is_empty() {
            let none = gtk::Label::builder()
                .label(i18n::umoja("{name} only sees its own folder.").replace("{name}", name))
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label", "caption"])
                .build();
            self.shares.append(&none);
        }
        let add = gtk::Button::builder()
            .label(i18n::umoja("Share a Folder…"))
            .halign(gtk::Align::Start)
            .css_classes(["flat", "agents-share-add"])
            .build();
        let weak = Rc::downgrade(self);
        add.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                this.pick_folder(b);
            }
        });
        self.shares.append(&add);
    }

    /// Read-only or can change: shared again with the new access.
    fn reshare(self: &Rc<Self>, path: String, writable: bool) {
        let Some(conn) = self.conn.borrow().clone() else { return };
        let error = self.error.clone();
        glib::spawn_future_local(async move {
            let res = async {
                conn.call_future(Some(BUS), PATH, IFACE, "Unshare", Some(&(path.as_str(),).to_variant()), None,
                    gio::DBusCallFlags::NONE, 120_000).await?;
                conn.call_future(Some(BUS), PATH, IFACE, "ShareFolder", Some(&(path.as_str(), writable).to_variant()),
                    None, gio::DBusCallFlags::NONE, 180_000).await
            }
            .await;
            if let Err(mut e) = res {
                gio::DBusError::strip_remote_error(&mut e);
                error.set_text(e.message());
                error.set_visible(true);
            }
        });
    }

    /// A folder picker; the folder is shared read-only (tick Can change
    /// to let the agent change it).
    fn pick_folder(self: &Rc<Self>, from: &gtk::Button) {
        let window = from.root().and_downcast::<gtk::Window>();
        self.popover.popdown();
        let dialog = gtk::FileDialog::builder()
            .title(i18n::umoja("Share a folder with {name}").replace("{name}", &self.state.borrow().name()))
            .accept_label(i18n::umoja("Share"))
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        dialog.select_folder(window.as_ref(), gio::Cancellable::NONE, move |res| {
            let (Some(this), Ok(file)) = (weak.upgrade(), res) else { return };
            let Some(path) = file.path() else { return };
            let path = path.to_string_lossy().into_owned();
            let Some(conn) = this.conn.borrow().clone() else { return };
            let error = this.error.clone();
            glib::spawn_future_local(async move {
                if let Err(mut e) = conn
                    .call_future(Some(BUS), PATH, IFACE, "ShareFolder", Some(&(path.as_str(), false).to_variant()),
                        None, gio::DBusCallFlags::NONE, 180_000)
                    .await
                {
                    gio::DBusError::strip_remote_error(&mut e);
                    error.set_text(e.message());
                    error.set_visible(true);
                }
            });
        });
    }

    fn fill_problem(self: &Rc<Self>, state: &State) {
        while let Some(c) = self.problem.first_child() {
            self.problem.remove(&c);
        }
        let show = state.setup == "error" || !state.needs.is_empty();
        self.problem.set_visible(show);
        if !show {
            return;
        }
        let text = gtk::Label::builder()
            .label(if state.error.is_empty() {
                i18n::umoja("{name} couldn't start.").replace("{name}", &state.name())
            } else {
                state.error.clone()
            })
            .wrap(true)
            .xalign(0.0)
            .max_width_chars(40)
            .build();
        self.problem.append(&text);
        let action = match state.needs.as_str() {
            "lxd-install" | "lxd-group" => Some((i18n::umoja("Allow"), "FixLxd", None)),
            "relogin" => None,
            _ if state.setup == "error" => Some((
                i18n::umoja("Try Again"),
                "Setup",
                Some((if state.preset.is_empty() { "balanced".to_owned() } else { state.preset.clone() },).to_variant()),
            )),
            _ => None,
        };
        if let Some((label, method, args)) = action {
            let b = gtk::Button::builder()
                .label(label)
                .halign(gtk::Align::Start)
                .css_classes(["agents-allow"])
                .build();
            let weak = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.call(method, args.as_ref());
                }
            });
            self.problem.append(&b);
        }
    }

    fn fill_approvals(self: &Rc<Self>, approvals: &[Approval]) {
        while let Some(c) = self.approvals.first_child() {
            self.approvals.remove(&c);
        }
        for a in approvals {
            self.approvals.append(&self.approval_card(a));
        }
    }

    fn approval_card(self: &Rc<Self>, a: &Approval) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("agents-approval");
        card.add_css_class(&a.kind);
        let icon_name = match a.kind.as_str() {
            "network" => "network-workgroup-symbolic",
            "command" => "utilities-terminal-symbolic",
            _ => "dialog-question-symbolic",
        };
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.add_css_class("agents-approval-icon");
        icon.set_valign(gtk::Align::Start);
        let title = gtk::Label::builder()
            .label(&a.title)
            .wrap(true)
            .xalign(0.0)
            .max_width_chars(36)
            .css_classes(["agents-approval-title"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        text.set_hexpand(true);
        text.append(&title);
        if !a.command.is_empty() {
            let cmd = gtk::Label::builder()
                .label(&a.command)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .max_width_chars(36)
                .selectable(true)
                .css_classes(["monospace", "agents-command"])
                .build();
            text.append(&cmd);
        }
        if !a.detail.is_empty() {
            let detail = gtk::Label::builder()
                .label(&a.detail)
                .wrap(true)
                .xalign(0.0)
                .max_width_chars(36)
                .css_classes(["dim-label", "agents-detail"])
                .build();
            text.append(&detail);
        }
        row.append(&text);
        card.append(&row);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        if a.kind == "question" {
            let entry = gtk::Entry::builder()
                .placeholder_text(i18n::umoja("Answer {name}").replace("{name}", &self.state.borrow().name()))
                .hexpand(true)
                .text(self.drafts.borrow().get(&a.id).cloned().unwrap_or_default())
                .build();
            let send = gtk::Button::builder()
                .label(i18n::umoja("Send"))
                .css_classes(["agents-allow"])
                .build();
            let id = a.id.clone();
            let weak = Rc::downgrade(self);
            entry.connect_changed(move |e| {
                if let Some(this) = weak.upgrade() {
                    this.drafts.borrow_mut().insert(id.clone(), e.text().to_string());
                }
            });
            let reply = {
                let id = a.id.clone();
                let weak = Rc::downgrade(self);
                let entry = entry.downgrade();
                move || {
                    let (Some(this), Some(entry)) = (weak.upgrade(), entry.upgrade()) else { return };
                    let text = entry.text().to_string();
                    if text.trim().is_empty() {
                        return;
                    }
                    this.drafts.borrow_mut().remove(&id);
                    this.call("Reply", Some(&(id.as_str(), text.as_str()).to_variant()));
                }
            };
            let r = reply.clone();
            entry.connect_activate(move |_| r());
            send.connect_clicked(move |_| reply());
            buttons.set_halign(gtk::Align::Fill);
            buttons.append(&entry);
            buttons.append(&send);
        } else {
            let deny = gtk::Button::builder()
                .label(i18n::umoja("Deny"))
                .css_classes(["agents-deny"])
                .build();
            buttons.append(&deny);
            let choices: &[(&str, &str)] = if a.kind == "network" && !a.flagged {
                &[("always", "Always Allow"), ("allow", "Allow")]
            } else {
                &[("allow", "Allow")]
            };
            self.answer_on(&deny, &a.id, "deny");
            for (choice, label) in choices {
                let b = gtk::Button::builder()
                    .label(i18n::umoja(label))
                    .css_classes([if *choice == "allow" { "agents-allow" } else { "agents-always" }])
                    .build();
                self.answer_on(&b, &a.id, choice);
                buttons.append(&b);
            }
        }
        card.append(&buttons);
        card
    }

    fn answer_on(self: &Rc<Self>, b: &gtk::Button, id: &str, choice: &str) {
        let (id, choice) = (id.to_owned(), choice.to_owned());
        let weak = Rc::downgrade(self);
        b.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                // No double answers while the state catches up.
                if let Some(parent) = b.parent() {
                    parent.set_sensitive(false);
                }
                this.call("Answer", Some(&(id.as_str(), choice.as_str()).to_variant()));
            }
        });
    }

    fn fill_tasks(self: &Rc<Self>, tasks: &[Task]) {
        while let Some(c) = self.tasks.first_child() {
            self.tasks.remove(&c);
        }
        for t in tasks {
            self.tasks.append(&self.task_row(t));
        }
    }

    fn task_row(self: &Rc<Self>, t: &Task) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("agents-task");
        row.add_css_class(&t.status);
        let active = t.status == "running" || t.status == "waiting";
        let lead: gtk::Widget = if active {
            let s = gtk::Spinner::builder().spinning(true).valign(gtk::Align::Center).build();
            s.upcast()
        } else {
            let name = match t.status.as_str() {
                "done" => "check-symbolic",
                "cancelled" => "media-playback-stop-symbolic",
                _ => "dialog-warning-symbolic",
            };
            let i = gtk::Image::from_icon_name(name);
            i.set_valign(gtk::Align::Center);
            i.upcast()
        };
        lead.add_css_class("agents-task-icon");
        row.append(&lead);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        let title = gtk::Label::builder()
            .label(&t.title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .hexpand(true)
            .css_classes(["agents-task-title"])
            .build();
        text.append(&title);
        let sub = if active {
            t.step.clone()
        } else if !t.result.is_empty() {
            t.result.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_owned()
        } else {
            match t.status.as_str() {
                "done" => i18n::umoja("Done"),
                "cancelled" => i18n::umoja("Stopped"),
                _ => i18n::umoja("Failed"),
            }
        };
        let subtitle = gtk::Label::builder()
            .label(&sub)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .css_classes(["dim-label", "agents-task-step"])
            .build();
        text.append(&subtitle);
        row.append(&text);
        let (icon, tip, method) = if active {
            ("media-playback-stop-symbolic", i18n::umoja("Stop"), "Stop")
        } else {
            ("document-open-symbolic", i18n::umoja("Open"), "Open")
        };
        if active || t.status == "done" {
            let b = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .css_classes(["flat", "circular"])
                .valign(gtk::Align::Center)
                .build();
            let id = t.id.clone();
            let weak = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    if method == "Open" {
                        this.popover.popdown();
                    }
                    this.call(method, Some(&(id.as_str(),).to_variant()));
                }
            });
            row.append(&b);
        }
        // The row itself opens the task in Ask Umoja: the whole answer, and
        // a reply box to follow up.
        row.set_tooltip_text(Some(&i18n::umoja("Show the whole answer and reply")));
        row.set_cursor_from_name(Some("pointer"));
        let click = gtk::GestureClick::new();
        let id = t.id.clone();
        let weak = Rc::downgrade(self);
        // (The row's own button claims its clicks, so they don't get here.)
        click.connect_released(move |_, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            this.popover.popdown();
            let Some(conn) = this.conn.borrow().clone() else { return };
            conn.call(
                Some("org.umoja.Overlay"),
                "/org/umoja/Overlay",
                "org.umoja.Overlay",
                "ShowAgentTask",
                Some(&(id.as_str(),).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5_000,
                gio::Cancellable::NONE,
                |_| {},
            );
        });
        row.add_controller(click);
        row
    }
}

/// Running tasks first (newest first), then the last few finished.
pub fn tasks_shown(tasks: &[Task]) -> Vec<Task> {
    let mut active: Vec<Task> = tasks
        .iter()
        .filter(|t| t.status == "running" || t.status == "waiting")
        .cloned()
        .collect();
    active.reverse();
    let mut done: Vec<Task> = tasks
        .iter()
        .filter(|t| t.status != "running" && t.status != "waiting")
        .cloned()
        .collect();
    done.sort_by(|a, b| b.finished.cmp(&a.finished));
    done.truncate(DONE_SHOWN);
    active.extend(done);
    active
}

fn watch(this: &Rc<Pill>) {
    let weak = Rc::downgrade(this);
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let Some(this) = weak.upgrade() else { return };
        this.conn.replace(Some(conn.clone()));
        drop(this);
        let w = weak.clone();
        let id = conn.signal_subscribe(
            Some(BUS),
            Some(IFACE),
            Some("StateChanged"),
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let (Some(this), Some((json,))) = (w.upgrade(), params.get::<(String,)>())
                    && let Some(state) = State::parse(&json)
                {
                    this.update(state);
                }
            },
        );
        std::mem::forget(id);
        let (appeared, vanished) = (weak.clone(), weak.clone());
        // Not auto-starting the service: a Umoja without umoja-agents (or
        // never set up) just has no pill. umoja-agentd starts with the
        // session when it's installed.
        let watch = gio::bus_watch_name_on_connection(
            &conn,
            BUS,
            gio::BusNameWatcherFlags::NONE,
            move |conn, _, _| {
                let weak = appeared.clone();
                conn.call(
                    Some(BUS),
                    PATH,
                    IFACE,
                    "GetState",
                    None,
                    Some(glib::VariantTy::new("(s)").expect("type")),
                    gio::DBusCallFlags::NONE,
                    5000,
                    gio::Cancellable::NONE,
                    move |res| {
                        let Ok(v) = res else { return };
                        if let (Some(this), Some((json,))) = (weak.upgrade(), v.get::<(String,)>())
                            && let Some(state) = State::parse(&json)
                        {
                            this.update(state);
                        }
                    },
                );
            },
            move |_, _| {
                if let Some(this) = vanished.upgrade() {
                    this.update(State::default());
                }
            },
        );
        std::mem::forget(watch);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str, finished: u64) -> Task {
        Task { id: id.into(), title: id.into(), status: status.into(), finished, ..Default::default() }
    }

    #[test]
    fn parses_agentd_state() {
        let json = r#"{"setup":"ready","step":"","error":"","needs":"","preset":"balanced","model":"local",
            "paused":false,"sandbox":"running","memory_mb":2048,"memory_used_mb":300,"workspace":"/home/u/Agents/Hermes",
            "tasks":[{"id":"t1","run":"r","title":"Plan Lisbon","prompt":"p","status":"running","step":"Running curl",
            "steps":2,"started":1,"finished":0,"result":"","files":[],"sites":[],"tools":[]}],
            "approvals":[{"id":"net-1","kind":"network","task":"t1","title":"Hermes wants to reach example.org",
            "detail":"d","host":"example.org","port":443,"binary":"/usr/bin/curl","command":"","flagged":false,"created":5}],
            "shared":[],"messaging":null}"#;
        let s = State::parse(json).expect("parses");
        assert!(s.visible());
        assert_eq!(s.running(), 1);
        assert_eq!(s.approvals[0].host, "example.org");
        assert_eq!(s.summary(), "Waiting for you");
    }

    #[test]
    fn memory_reads_plainly() {
        assert_eq!(memory_text(231, 4096), "using 231 MB of its 4 GB memory");
        assert_eq!(memory_text(1536, 6144), "using 1.5 GB of its 6 GB memory");
    }

    #[test]
    fn hidden_until_set_up() {
        assert!(!State::default().visible());
        let s = State { setup: "none".into(), ..Default::default() };
        assert!(!s.visible());
        let s = State { setup: "working".into(), step: "Starting the guardrails".into(), ..Default::default() };
        assert!(s.visible());
        assert_eq!(s.summary(), "Starting the guardrails…");
        // A failed setup keeps the button, for its Allow / Try Again.
        let s = State { setup: "error".into(), needs: "lxd-group".into(), ..Default::default() };
        assert!(s.visible());
    }

    #[test]
    fn running_first_then_recent_done() {
        let tasks = vec![
            task("a", "done", 10),
            task("b", "running", 0),
            task("c", "failed", 30),
            task("d", "done", 20),
            task("e", "cancelled", 5),
            task("f", "waiting", 0),
        ];
        let ids: Vec<String> = tasks_shown(&tasks).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["f", "b", "c", "d", "a"]);
    }
}
