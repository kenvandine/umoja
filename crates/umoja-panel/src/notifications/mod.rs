//! Notification server (`org.freedesktop.Notifications`).
//!
//! Behaves like gnome-shell: banners for new notifications, a message list
//! in the clock menu, Do Not Disturb (`org.gnome.desktop.notifications
//! show-banners`), per-app settings from Settings › Notifications (the
//! relocatable `org.gnome.desktop.notifications.application` schema),
//! actions, default-action on click, critical notifications that stay until
//! dismissed, and the `transient`/`resident` hints.
//!
//! Banners are drawn like Ubuntu's notify-osd: dark translucent bubbles in
//! the top center under the clock (where GNOME puts banners), a large icon
//! beside a bold title, and a gauge for the `value` hint (with
//! `x-canonical-private-synchronous` replacing the previous bubble of the
//! same kind, as notify-osd's volume and brightness bubbles did).
//!
//! The on-device assistant adds two things (see `assistant`): a digest of
//! notifications that arrived since you last opened the message list, and
//! quick replies drafted from a message. Apps that support KDE's
//! `inline-reply` action get the reply back through `NotificationReplied`;
//! for other chat and mail apps the reply is copied and the app raised.
//! The same features are on D-Bus as `org.umoja.Notifications` (see `api`)
//! so the assistant overlay can answer "what did I miss?".

mod api;
pub mod assistant;
mod bubbles;
mod list;
mod quick_reply;
pub mod quiet;
mod server;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gio::prelude::*;
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use umoja_common::gsettings as gs;

pub use list::MessageList;

/// Listed notifications kept per app, as gnome-shell's
/// MAX_NOTIFICATIONS_PER_SOURCE.
const PER_APP: usize = 3;
/// Listed notifications kept in all.
const MAX_LISTED: usize = 50;

pub const NOTIFICATIONS: &str = "org.gnome.desktop.notifications";
pub const APP_NOTIFICATIONS: &str = "org.gnome.desktop.notifications.application";

/// Why a notification closed (`NotificationClosed` reason codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

#[derive(Clone, Debug)]
pub enum Icon {
    Name(String),
    File(String),
    Texture(gdk::Texture),
}

#[derive(Clone, Debug)]
pub struct Note {
    pub id: u32,
    pub app_name: String,
    /// Desktop file id without `.desktop`, from the `desktop-entry` hint.
    pub app_id: Option<String>,
    pub summary: String,
    /// Pango markup, already validated.
    pub body: String,
    pub icon: Option<Icon>,
    /// (key, label) pairs in the order the app sent them.
    pub actions: Vec<(String, String)>,
    pub urgency: Urgency,
    /// `value` hint, 0-100: drawn as a notify-osd gauge.
    pub value: Option<i32>,
    pub sync_tag: Option<String>,
    pub transient: bool,
    pub resident: bool,
    pub expire_timeout: i32,
    pub time: glib::DateTime,
    /// `category` hint, e.g. `im.received` or `email.arrived`.
    pub category: Option<String>,
    /// `x-kde-reply-placeholder-text` hint for the reply field.
    pub reply_placeholder: Option<String>,
}

/// Chat and mail apps whose notifications are worth replying to even
/// though they don't send the `inline-reply` action. Matched against the
/// lower-cased desktop id and app name.
const MESSAGING_APPS: &[&str] = &[
    "thunderbird",
    "evolution",
    "geary",
    "mail",
    "telegram",
    "signal",
    "discord",
    "slack",
    "element",
    "fractal",
    "polari",
    "whatsapp",
    "teams",
    "chat",
    "messag",
    "skype",
    "zulip",
    "mattermost",
    "rocketchat",
    "rocket.chat",
    "dino",
    "gajim",
    "pidgin",
    "hexchat",
    "irc",
    "matrix",
    "beeper",
    "ferdium",
    "franz",
    "wire",
    "threema",
    "viber",
    "zoom",
];

impl Note {
    pub fn default_action(&self) -> bool {
        self.actions.iter().any(|(k, _)| k == "default")
    }

    pub fn buttons(&self) -> impl Iterator<Item = &(String, String)> {
        self.actions
            .iter()
            .filter(|(k, _)| k != "default" && k != INLINE_REPLY)
    }

    /// The app takes a typed reply back (KDE's inline-reply extension).
    pub fn inline_reply(&self) -> bool {
        self.actions.iter().any(|(k, _)| k == INLINE_REPLY)
    }

    /// Whether to offer quick replies: a message from a person, not a
    /// progress bar, a volume bubble or a system warning.
    pub fn can_reply(&self) -> bool {
        if self.inline_reply() {
            return true;
        }
        if self.value.is_some() || self.sync_tag.is_some() || self.body.trim().is_empty() {
            return false;
        }
        if let Some(c) = &self.category {
            if c.starts_with("im") || c == "email.arrived" || c == "email" {
                return true;
            }
            // Any other category (device, network, transfer...) is not a
            // conversation.
            return false;
        }
        let id = self.app_id.as_deref().unwrap_or_default().to_lowercase();
        let name = self.app_name.to_lowercase();
        MESSAGING_APPS
            .iter()
            .any(|m| id.contains(m) || name.contains(m))
    }
}

/// Action key apps send to ask for a reply field (KDE convention, also
/// used by Chromium, Firefox and Electron apps on Plasma).
pub const INLINE_REPLY: &str = "inline-reply";

type Listener = Box<dyn Fn()>;
type ReplyListener = Box<dyn Fn(u32)>;
/// What the shell does when one of its own notifications' actions is
/// picked (there is no D-Bus sender to tell).
type ActionHandler = Rc<dyn Fn(&str)>;
/// Open the clock menu's message list (org.umoja.Panel, which this
/// process serves).
fn open_message_list() {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(umoja_common::dbus::PANEL_BUS_NAME),
            umoja_common::dbus::PANEL_OBJECT_PATH,
            umoja_common::dbus::PANEL_INTERFACE,
            "OpenMenu",
            Some(&("calendar",).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |_| {},
        );
    });
}

/// A row of org.umoja.Compositor `GetWindowsMru`: (id, app_id, title,
/// x, y, width, height, flags).
pub(crate) type WindowRow = (u64, String, String, i32, i32, i32, i32, u32);

/// Shared state between the D-Bus server, the banners and the message list.
pub struct Center {
    next_id: Cell<u32>,
    /// Newest first: what the message list shows.
    notes: RefCell<Vec<Note>>,
    conn: RefCell<Option<gio::DBusConnection>>,
    bubbles: bubbles::Bubbles,
    listeners: RefCell<Vec<Listener>>,
    expiry: RefCell<HashMap<u32, glib::SourceId>>,
    settings: Option<gio::Settings>,
    /// Ids that arrived since the message list was last opened: what the
    /// digest summarizes.
    unseen: RefCell<HashSet<u32>>,
    reply_listeners: RefCell<Vec<ReplyListener>>,
    handlers: RefCell<HashMap<u32, ActionHandler>>,
    /// The clock menu is open. Banners sit on a layer above it, right over
    /// the message list, so they'd cover (and take the clicks of) the rows
    /// beneath. As gnome-shell's bannerBlocked, none show meanwhile.
    banners_blocked: Cell<bool>,
    /// Automatic Do Not Disturb: why banners are held back right now
    /// (`quiet::RECORDING`, `quiet::FULLSCREEN`), and every reason seen
    /// since it began.
    quiet: Cell<u8>,
    quiet_seen: Cell<u8>,
    /// Notifications whose banner was held back meanwhile.
    held: RefCell<Vec<u32>>,
    /// Banners waiting on a fresh fullscreen check (see `notify`).
    pending_banner: RefCell<HashMap<u32, Note>>,
}

impl Center {
    pub fn start(app: &impl IsA<gtk::Application>) -> Rc<Self> {
        let center = Rc::new_cyclic(|weak: &std::rc::Weak<Center>| Self {
            next_id: Cell::new(1),
            notes: RefCell::default(),
            conn: RefCell::default(),
            bubbles: bubbles::Bubbles::new(app.upcast_ref(), weak.clone()),
            listeners: RefCell::default(),
            expiry: RefCell::default(),
            settings: gs::settings(NOTIFICATIONS),
            unseen: RefCell::default(),
            reply_listeners: RefCell::default(),
            handlers: RefCell::default(),
            banners_blocked: Cell::new(false),
            quiet: Cell::new(0),
            quiet_seen: Cell::new(0),
            held: RefCell::default(),
            pending_banner: RefCell::default(),
        });
        if let Some(s) = &center.settings {
            let weak = Rc::downgrade(&center);
            s.connect_changed(Some("show-banners"), move |_, _| {
                if let Some(c) = weak.upgrade() {
                    if !c.banners_enabled() {
                        c.bubbles.hide_all_but_critical();
                    }
                    c.changed();
                }
            });
        }
        {
            let qr = quick_reply::QuickReply::new(app.upcast_ref(), Rc::downgrade(&center));
            let q = qr.clone();
            // Id 0 (never a notification's) closes it: close_reply().
            center.connect_reply_requested(move |id| if id == 0 { q.close() } else { q.show(id) });
            center.connect_changed(move || qr.sync());
        }
        server::own_name(center.clone());
        api::own_name(center.clone());
        quiet::start(&center);
        center
    }

    /// Hold banners back for `reason` (or stop). When nothing holds them
    /// any more, one banner says how many arrived meanwhile.
    pub fn set_quiet(self: &Rc<Self>, reason: u8, on: bool) {
        let before = self.quiet.get();
        let now = if on { before | reason } else { before & !reason };
        if now == before {
            return;
        }
        tracing::info!(quiet = now, "automatic Do Not Disturb");
        self.quiet.set(now);
        self.quiet_seen.set(self.quiet_seen.get() | now);
        if now != 0 {
            if before == 0 {
                self.bubbles.hide_all_but_critical();
            }
            return;
        }
        let reasons = self.quiet_seen.replace(0);
        let held: Vec<u32> = self.held.borrow_mut().drain(..).collect();
        let count = {
            let notes = self.notes.borrow();
            held.iter().filter(|id| notes.iter().any(|n| n.id == **id)).count()
        };
        if count == 0 || !self.banners_enabled() {
            return;
        }
        let (title, body) = quiet::summary(count, reasons);
        let weak = Rc::downgrade(self);
        let id = std::rc::Rc::new(Cell::new(0u32));
        let own = id.clone();
        let nid = self.notify_with_actions(
            &title,
            &body,
            "preferences-system-notifications-symbolic",
            &[("default", "Show")],
            move |_| {
                if let Some(c) = weak.upgrade() {
                    // The summary has done its job once the list is open.
                    c.close(own.get(), CloseReason::Dismissed);
                }
                open_message_list();
            },
        );
        id.set(nid);
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn changed(&self) {
        for l in self.listeners.borrow().iter() {
            l();
        }
    }

    pub fn notes(&self) -> Vec<Note> {
        self.notes.borrow().clone()
    }

    /// Notifications that arrived since the message list was last opened,
    /// newest first, leaving out progress bars and volume-style bubbles.
    pub fn missed(&self) -> Vec<Note> {
        let unseen = self.unseen.borrow();
        self.notes
            .borrow()
            .iter()
            .filter(|n| unseen.contains(&n.id) && n.value.is_none() && n.sync_tag.is_none())
            .cloned()
            .collect()
    }

    /// The user has looked at the message list.
    pub fn mark_seen(&self) {
        let had = !self.unseen.borrow().is_empty();
        self.unseen.borrow_mut().clear();
        if had {
            self.changed();
        }
    }

    /// A short-lived banner from the shell itself ("Reply copied").
    pub fn notify_internal(self: &Rc<Self>, summary: &str, body: &str, icon: &str) {
        let Ok(time) = glib::DateTime::now_local() else {
            return;
        };
        let note = Note {
            id: 0,
            app_name: String::new(),
            app_id: None,
            summary: summary.into(),
            body: glib::markup_escape_text(body).to_string(),
            icon: Some(Icon::Name(icon.into())),
            actions: vec![],
            urgency: Urgency::Low,
            value: None,
            sync_tag: Some("umoja-shell".into()),
            transient: true,
            resident: false,
            expire_timeout: -1,
            time,
            category: None,
            reply_placeholder: None,
        };
        self.notify(note, 0);
    }

    /// A notification from the shell itself with action buttons, kept in
    /// the message list like an app's. `actions` are (key, label) pairs; a
    /// `"default"` key is what clicking the notification does.
    pub fn notify_with_actions(
        self: &Rc<Self>,
        summary: &str,
        body: &str,
        icon: &str,
        actions: &[(&str, &str)],
        on_action: impl Fn(&str) + 'static,
    ) -> u32 {
        let Ok(time) = glib::DateTime::now_local() else {
            return 0;
        };
        let note = Note {
            id: 0,
            app_name: "Umoja".into(),
            app_id: None,
            summary: summary.into(),
            body: glib::markup_escape_text(body).to_string(),
            icon: Some(Icon::Name(icon.into())),
            actions: actions
                .iter()
                .map(|(k, l)| (k.to_string(), l.to_string()))
                .collect(),
            urgency: Urgency::Normal,
            value: None,
            sync_tag: None,
            transient: false,
            resident: false,
            expire_timeout: -1,
            time,
            category: None,
            reply_placeholder: None,
        };
        let id = self.notify(note, 0);
        self.handlers.borrow_mut().insert(id, Rc::new(on_action));
        id
    }

    pub fn note(&self, id: u32) -> Option<Note> {
        self.find(id)
    }

    /// Open the quick-reply window for `id` (the banner's Reply button, or
    /// `org.umoja.Notifications.OpenReply`).
    pub fn request_reply(&self, id: u32) {
        if self.find(id).is_none_or(|n| !n.can_reply()) {
            return;
        }
        self.bubbles.remove(id);
        for l in self.reply_listeners.borrow().iter() {
            l(id);
        }
    }

    /// Close the quick-reply window if it is open, sending nothing.
    pub fn close_reply(&self) {
        for l in self.reply_listeners.borrow().iter() {
            l(0);
        }
    }

    pub fn connect_reply_requested(&self, f: impl Fn(u32) + 'static) {
        self.reply_listeners.borrow_mut().push(Box::new(f));
    }

    /// Send `text` as a reply. Apps with `inline-reply` get it through
    /// `NotificationReplied`; for anything else it goes on the clipboard and
    /// the app is raised so it can be pasted. Returns true when the app got
    /// the reply itself.
    pub fn reply(&self, id: u32, text: &str) -> bool {
        let Some(note) = self.find(id) else {
            return false;
        };
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        if note.inline_reply() {
            self.emit("NotificationReplied", &(id, text).to_variant());
            self.close(id, CloseReason::Dismissed);
            return true;
        }
        if let Some(display) = gdk::Display::default() {
            display.clipboard().set_text(text);
        }
        // Raise the app (its default action usually opens the conversation).
        let resident = note.resident;
        if note.default_action() {
            self.emit_action(&note, "default");
            self.raise_app(&note, true, || {});
        } else {
            self.activate(id);
        }
        if !resident {
            self.close(id, CloseReason::Dismissed);
        }
        false
    }

    /// The clock menu opened (true) or closed. Everything a banner shows is
    /// in the menu's list too, so the banners just go.
    pub fn block_banners(&self, blocked: bool) {
        self.banners_blocked.set(blocked);
        if blocked {
            self.bubbles.hide_all();
        }
    }

    /// Do Not Disturb is "show-banners = false", as in GNOME.
    pub fn banners_enabled(&self) -> bool {
        self.settings
            .as_ref()
            .is_none_or(|s| !gs::has_key(s, "show-banners") || s.boolean("show-banners"))
    }

    pub fn set_do_not_disturb(&self, dnd: bool) {
        if let Some(s) = &self.settings {
            let _ = s.set_boolean("show-banners", !dnd);
        }
    }

    /// Handle `Notify`. Returns the id.
    pub(crate) fn notify(self: &Rc<Self>, mut note: Note, replaces_id: u32) -> u32 {
        // A synchronous bubble (volume, brightness...) replaces the last
        // one with the same tag.
        let sync_match = note.sync_tag.as_ref().and_then(|tag| {
            self.notes
                .borrow()
                .iter()
                .find(|n| n.sync_tag.as_ref() == Some(tag))
                .map(|n| n.id)
        });
        let existing = if replaces_id != 0
            && self.notes.borrow().iter().any(|n| n.id == replaces_id)
            || self.bubbles.contains(replaces_id)
        {
            Some(replaces_id)
        } else {
            sync_match
        };
        note.id = existing.unwrap_or_else(|| {
            let id = self.next_id.get();
            self.next_id.set(id.wrapping_add(1).max(1));
            id
        });

        let policy = AppPolicy::for_app(note.app_id.as_deref());
        if !policy.enabled {
            tracing::debug!(app = ?note.app_id, "notifications disabled for app");
            return note.id;
        }

        // Activity recall (opt-in): new messages, not progress or OSD bubbles.
        if existing.is_none() && note.sync_tag.is_none() && note.value.is_none() {
            let body = gtk::pango::parse_markup(&note.body, '\0')
                .map(|(_, t, _)| t.to_string())
                .unwrap_or_else(|_| note.body.clone());
            umoja_common::recall::add(umoja_common::recall::Entry {
                at: note.time.to_unix(),
                kind: umoja_common::recall::Kind::Notification,
                app: note.app_name.clone(),
                text: format!("{}: {}", note.summary, body),
                secs: None,
            });
        }

        {
            let mut notes = self.notes.borrow_mut();
            notes.retain(|n| n.id != note.id);
            // Transient notifications never reach the message list.
            if !note.transient {
                notes.insert(0, note.clone());
                // An update to a progress bar isn't news; a new message is.
                if existing.is_none() || note.value.is_none() {
                    self.unseen.borrow_mut().insert(note.id);
                }
            }
        }

        self.trim_history(note.id);

        let critical = note.urgency == Urgency::Critical;
        // Progress updates (`value` hint) to a notification whose banner has
        // already gone only refresh the message list, rather than popping a
        // bubble up again for every percent.
        let quiet_update =
            existing.is_some() && note.value.is_some() && !self.bubbles.contains(note.id);
        let show_banner = !quiet_update
            && !self.banners_blocked.get()
            && (critical || (self.banners_enabled() && policy.show_banners));
        // Automatic Do Not Disturb holds back all but critical banners.
        let held = show_banner && !critical && self.quiet.get() != 0;
        if held {
            self.held.borrow_mut().push(note.id);
        } else if show_banner && !critical && quiet::enabled() {
            // The fullscreen watcher hears of a window going fullscreen a
            // little late (WindowsChanged is coalesced), so a notification
            // in that moment would still pop up over the game or video.
            // Ask the compositor now; it only delays the banner by a
            // round trip.
            let first = self.pending_banner.borrow_mut().insert(note.id, note.clone()).is_none();
            if first {
                let weak = Rc::downgrade(self);
                let id = note.id;
                quiet::fullscreen_now(move |fullscreen| {
                    let Some(c) = weak.upgrade() else { return };
                    // Closed meanwhile: nothing to show. Replaced: show the latest.
                    let Some(note) = c.pending_banner.borrow_mut().remove(&id) else { return };
                    if fullscreen {
                        c.set_quiet(quiet::FULLSCREEN, true);
                    }
                    if c.quiet.get() != 0 {
                        c.held.borrow_mut().push(note.id);
                    } else if !c.banners_blocked.get() {
                        c.bubbles.show(&note);
                    }
                });
            }
        } else if show_banner {
            self.bubbles.show(&note);
        }
        self.schedule_expiry(&note);
        self.changed();
        note.id
    }

    /// An app asked for its own expiry time; GNOME otherwise keeps the
    /// notification in the list until the user deals with it.
    fn schedule_expiry(self: &Rc<Self>, note: &Note) {
        if let Some(old) = self.expiry.borrow_mut().remove(&note.id) {
            old.remove();
        }
        if note.expire_timeout <= 0 || note.urgency == Urgency::Critical {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = note.id;
        let src = glib::timeout_add_local_once(
            std::time::Duration::from_millis(note.expire_timeout as u64),
            move || {
                if let Some(c) = weak.upgrade() {
                    c.expiry.borrow_mut().remove(&id);
                    c.close(id, CloseReason::Expired);
                }
            },
        );
        self.expiry.borrow_mut().insert(id, src);
    }

    /// Keep the message list to [`PER_APP`] notifications per app (as
    /// gnome-shell's MessageTray keeps per source) and [`MAX_LISTED`] in all,
    /// dropping the oldest. Every listed notification keeps its image and
    /// its row widgets, so the list used to grow for as long as the session
    /// ran. `keep` (the one just added) always stays.
    fn trim_history(&self, keep: u32) {
        let drop: Vec<u32> = {
            let notes = self.notes.borrow();
            let mut per_app: HashMap<&str, usize> = HashMap::new();
            notes
                .iter()
                .enumerate()
                .filter_map(|(i, n)| {
                    let source = n.app_id.as_deref().unwrap_or(n.app_name.as_str());
                    let seen = per_app.entry(source).or_default();
                    *seen += 1;
                    (n.id != keep && !n.resident && (*seen > PER_APP || i >= MAX_LISTED)).then_some(n.id)
                })
                .collect()
        };
        for id in drop {
            self.close(id, CloseReason::Expired);
        }
    }

    /// Remove a notification everywhere and tell its sender.
    pub fn close(&self, id: u32, reason: CloseReason) {
        let in_list = {
            let mut notes = self.notes.borrow_mut();
            let before = notes.len();
            notes.retain(|n| n.id != id);
            before != notes.len()
        };
        self.unseen.borrow_mut().remove(&id);
        self.pending_banner.borrow_mut().remove(&id);
        self.handlers.borrow_mut().remove(&id);
        let in_bubble = self.bubbles.remove(id);
        if let Some(src) = self.expiry.borrow_mut().remove(&id) {
            src.remove();
        }
        if in_list || in_bubble {
            self.emit("NotificationClosed", &(id, reason as u32).to_variant());
            self.changed();
        }
    }

    pub fn clear(&self) {
        let ids: Vec<u32> = self.notes.borrow().iter().map(|n| n.id).collect();
        for id in ids {
            self.close(id, CloseReason::Dismissed);
        }
    }

    /// The user clicked a notification (banner or list row).
    ///
    /// The app gets an activation token and its default action, so it can
    /// open the right conversation and focus its own window. Apps that
    /// ignore the token (or have none to use, like Telegram with its window
    /// on another workspace) would stay behind, so once the app has had a
    /// moment, its newest window is raised unless it already has focus.
    /// Without a default action, the app's window is raised or the app
    /// launched, as gnome-shell's `app.activate()` does.
    pub fn activate(&self, id: u32) {
        let Some(note) = self.find(id) else { return };
        if note.default_action() {
            // The shell's own notifications act in-process, nothing to raise.
            let from_app = !self.handlers.borrow().contains_key(&id);
            self.invoke(id, "default");
            if from_app {
                self.raise_app(&note, true, || {});
            }
            return;
        }
        let launch_note = note.clone();
        self.raise_app(&note, false, move || launch_app(&launch_note));
        if !note.resident {
            self.close(id, CloseReason::Dismissed);
        }
    }

    /// The user picked an action button.
    pub fn invoke(&self, id: u32, action: &str) {
        let Some(note) = self.find(id) else { return };
        let handler = self.handlers.borrow().get(&id).cloned();
        match handler {
            Some(h) => h(action),
            None => self.emit_action(&note, action),
        }
        if !note.resident {
            self.close(id, CloseReason::Dismissed);
        } else {
            self.bubbles.remove(id);
        }
    }

    /// Spec 1.2 order, as gnome-shell: an xdg-activation token first, so a
    /// Wayland app is allowed to focus its window, then the action.
    fn emit_action(&self, note: &Note, action: &str) {
        if let Some(token) = activation_token(note) {
            self.emit("ActivationToken", &(note.id, token.as_str()).to_variant());
        }
        self.emit("ActionInvoked", &(note.id, action).to_variant());
    }

    /// Raise the newest window of the app that sent `note` through
    /// umoja-mir. With `after_app`, first give the app a moment to act on
    /// its default action and leave things alone if one of its windows has
    /// focus by then (it may have opened a new one). `none` runs when the
    /// app has no window at all.
    fn raise_app(&self, note: &Note, after_app: bool, none: impl FnOnce() + 'static) {
        let Some(conn) = self.conn.borrow().clone() else {
            none();
            return;
        };
        if note.app_id.is_none() && note.app_name.trim().is_empty() {
            none();
            return;
        }
        let app_id = note.app_id.clone();
        let app_name = note.app_name.clone();
        let raise = move || {
            conn.clone().call(
                Some(umoja_common::dbus::COMPOSITOR_BUS_NAME),
                umoja_common::dbus::COMPOSITOR_OBJECT_PATH,
                umoja_common::dbus::COMPOSITOR_INTERFACE,
                "GetWindowsMru",
                None,
                Some(glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
                move |res| {
                    let rows: Vec<WindowRow> = match res.ok().and_then(|v| v.get::<(Vec<_>,)>()) {
                        Some((rows,)) => rows,
                        None => {
                            none();
                            return;
                        }
                    };
                    let is_app = |w: &str| {
                        let desktop = umoja_common::apps::app_for_window(w)
                            .and_then(|a| a.id())
                            .map(|d| d.trim_end_matches(".desktop").to_string());
                        window_is_app(w, desktop.as_deref(), app_id.as_deref(), &app_name)
                    };
                    if after_app && rows.first().is_some_and(|r| is_app(&r.1)) {
                        return;
                    }
                    match rows.iter().find(|r| is_app(&r.1)) {
                        Some(r) => {
                            conn.call(
                                Some(umoja_common::dbus::COMPOSITOR_BUS_NAME),
                                umoja_common::dbus::COMPOSITOR_OBJECT_PATH,
                                umoja_common::dbus::COMPOSITOR_INTERFACE,
                                "ActivateWindow",
                                Some(&(r.0,).to_variant()),
                                None,
                                gio::DBusCallFlags::NONE,
                                1000,
                                gio::Cancellable::NONE,
                                |_| {},
                            );
                        }
                        None => none(),
                    }
                },
            );
        };
        if after_app {
            glib::timeout_add_local_once(std::time::Duration::from_millis(300), raise);
        } else {
            raise();
        }
    }

    fn find(&self, id: u32) -> Option<Note> {
        self.notes
            .borrow()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .or_else(|| self.bubbles.note(id))
    }

    fn emit(&self, signal: &str, args: &glib::Variant) {
        if let Some(conn) = self.conn.borrow().as_ref() {
            let _ = conn.emit_signal(
                None,
                server::OBJECT_PATH,
                server::INTERFACE,
                signal,
                Some(args),
            );
        }
    }
}

/// Let the panel take keyboard focus (for the reply field) or give it back.
/// Layer surfaces default to no keyboard at all. Only used while the clock
/// menu (an autohide popover, so a click elsewhere closes it) or the
/// quick-reply window (closed by Escape) is open; OnDemand never got
/// keyboard focus without a click under Mir.
pub fn want_keyboard(widget: &impl IsA<gtk::Widget>, on: bool) {
    use gtk4_layer_shell::{KeyboardMode, LayerShell};
    let Some(win) = widget.as_ref().root().and_downcast::<gtk::Window>() else {
        return;
    };
    // The clock menu is a popover; its root is the panel window.
    if win.is_layer_window() {
        // Ctrl+Alt+Tab navigation keeps the keyboard when a menu closes.
        win.set_keyboard_mode(if on || crate::menu_api::keyboard_nav() {
            KeyboardMode::Exclusive
        } else {
            KeyboardMode::None
        });
    }
}

/// Settings › Notifications › <app>.
struct AppPolicy {
    enabled: bool,
    show_banners: bool,
}

impl AppPolicy {
    fn for_app(app_id: Option<&str>) -> Self {
        let allow = Self {
            enabled: true,
            show_banners: true,
        };
        let Some(app_id) = app_id else { return allow };
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return allow;
        };
        if source.lookup(APP_NOTIFICATIONS, true).is_none() {
            return allow;
        }
        let key = settings_key(app_id);
        let s = gio::Settings::with_path(
            APP_NOTIFICATIONS,
            &format!("/org/gnome/desktop/notifications/application/{key}/"),
        );
        register_app(app_id, &key, &s);
        Self {
            enabled: s.boolean("enable"),
            show_banners: s.boolean("show-banners"),
        }
    }
}

/// gnome-shell's id for the per-app settings path: dots to dashes, lower
/// case (`org.gnome.Nautilus` -> `org-gnome-nautilus`).
pub fn settings_key(app_id: &str) -> String {
    app_id.replace('.', "-").to_lowercase()
}

/// List the app under Settings › Notifications, as gnome-shell does the
/// first time an app notifies.
fn register_app(app_id: &str, key: &str, app_settings: &gio::Settings) {
    let Some(s) = gs::settings(NOTIFICATIONS) else {
        return;
    };
    let mut children: Vec<String> = s
        .strv("application-children")
        .iter()
        .map(|g| g.to_string())
        .collect();
    if children.iter().any(|c| c == key) {
        return;
    }
    children.push(key.to_string());
    let refs: Vec<&str> = children.iter().map(String::as_str).collect();
    let _ = s.set_strv("application-children", refs.as_slice());
    let _ = app_settings.set_string("application-id", &format!("{app_id}.desktop"));
}

/// How long a banner stays up: ~4 s like GNOME, longer for long text the
/// way notify-osd paced it, never for critical notifications.
pub fn banner_timeout(note: &Note) -> Option<std::time::Duration> {
    if note.urgency == Urgency::Critical {
        return None;
    }
    if note.value.is_some() {
        return Some(std::time::Duration::from_millis(2000));
    }
    let chars = note.summary.chars().count() + note.body.chars().count();
    let ms = (4000 + chars as u64 * 30).min(10_000);
    Some(std::time::Duration::from_millis(ms))
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub fn note(summary: &str, body: &str) -> Note {
        Note {
            id: 1,
            app_name: "test".into(),
            app_id: None,
            summary: summary.into(),
            body: body.into(),
            icon: None,
            actions: vec![],
            urgency: Urgency::Normal,
            value: None,
            sync_tag: None,
            transient: false,
            resident: false,
            expire_timeout: -1,
            time: glib::DateTime::now_local().unwrap(),
            category: None,
            reply_placeholder: None,
        }
    }
}

fn note_app(note: &Note) -> Option<gio::DesktopAppInfo> {
    gio::DesktopAppInfo::new(&format!("{}.desktop", note.app_id.as_deref()?))
}

/// An xdg-activation token for the app to focus its window with.
fn activation_token(note: &Note) -> Option<String> {
    let ctx = gdk::Display::default()?.app_launch_context();
    ctx.startup_notify_id(note_app(note).as_ref(), &[])
        .map(|t| t.to_string())
}

/// Launch the app that sent `note` (it had no window to raise).
fn launch_app(note: &Note) {
    let Some(app) = note_app(note) else { return };
    let ctx: gio::AppLaunchContext = match gdk::Display::default() {
        Some(d) => umoja_common::app_scope::scoped(d.app_launch_context()).upcast(),
        None => umoja_common::app_scope::launch_context(),
    };
    if let Err(e) = app.launch(&[], Some(&ctx)) {
        tracing::warn!(error = %e, "could not launch notification app");
    }
}

/// Whether a window (its Wayland app-id, and the desktop id that resolves
/// to, as the dock matches them) belongs to the app that sent a
/// notification (its `desktop-entry` hint, else its app name).
fn window_is_app(
    window_app_id: &str,
    window_desktop_id: Option<&str>,
    note_app_id: Option<&str>,
    app_name: &str,
) -> bool {
    let window_app_id = window_app_id.trim();
    if window_app_id.is_empty() {
        return false;
    }
    if let Some(id) = note_app_id.filter(|i| !i.is_empty()) {
        return window_app_id.eq_ignore_ascii_case(id)
            || window_desktop_id.is_some_and(|d| d.eq_ignore_ascii_case(id));
    }
    let name = app_name.trim();
    !name.is_empty() && window_app_id.eq_ignore_ascii_case(name)
}

#[cfg(test)]
mod tests {
    use super::tests_support::note;
    use super::*;

    #[test]
    fn matches_the_sending_apps_window() {
        let tg = Some("org.telegram.desktop");
        assert!(window_is_app(
            "org.telegram.desktop",
            None,
            tg,
            "Telegram Desktop"
        ));
        assert!(window_is_app(
            "TelegramDesktop",
            Some("org.telegram.desktop"),
            tg,
            "Telegram Desktop"
        ));
        assert!(!window_is_app(
            "firefox",
            Some("firefox"),
            tg,
            "Telegram Desktop"
        ));
        // Snap: the compositor names snap windows by their snapd desktop id.
        let snap = Some("telegram-desktop_telegram-desktop");
        assert!(window_is_app(
            "telegram-desktop_telegram-desktop",
            None,
            snap,
            "Telegram"
        ));
        // No desktop-entry hint: fall back to the app name.
        assert!(window_is_app("Slack", None, None, "slack"));
        assert!(!window_is_app("", None, None, ""));
        assert!(!window_is_app("firefox", None, None, "notify-send"));
    }

    #[test]
    fn who_gets_quick_replies() {
        let mut chat = note("Alice", "Lunch?");
        chat.app_id = Some("org.telegram.desktop".into());
        assert!(chat.can_reply());

        let mut im = note("Bob", "hi");
        im.category = Some("im.received".into());
        assert!(im.can_reply());

        let mut device = note("Disk full", "Free up space");
        device.category = Some("device.error".into());
        device.app_id = Some("org.gnome.Chat".into());
        assert!(!device.can_reply());

        let mut volume = note("Volume", "");
        volume.value = Some(40);
        assert!(!volume.can_reply());

        assert!(!note("Updates", "3 updates are ready").can_reply());

        let mut inline = note("Carol", "");
        inline.actions = vec![(INLINE_REPLY.into(), "Reply".into())];
        assert!(inline.can_reply());
        assert_eq!(inline.buttons().count(), 0);
    }

    #[test]
    fn settings_keys_match_gnome_shell() {
        assert_eq!(settings_key("org.gnome.Nautilus"), "org-gnome-nautilus");
        assert_eq!(settings_key("firefox_firefox"), "firefox_firefox");
    }

    #[test]
    fn banner_timeouts() {
        assert_eq!(banner_timeout(&note("Hi", "")).unwrap().as_millis(), 4060);
        let long = "x".repeat(1000);
        assert_eq!(
            banner_timeout(&note("Hi", &long)).unwrap().as_millis(),
            10_000
        );
        let mut c = note("Battery critical", "");
        c.urgency = Urgency::Critical;
        assert!(banner_timeout(&c).is_none());
    }
}
