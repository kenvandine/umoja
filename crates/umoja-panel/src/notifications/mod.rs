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
//! the top-right corner, a large icon beside a bold title, and a gauge for
//! the `value` hint (with `x-canonical-private-synchronous` replacing the
//! previous bubble of the same kind, as notify-osd's volume and brightness
//! bubbles did).
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
mod server;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gio::prelude::*;
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use umoja_common::gsettings as gs;

pub use list::MessageList;

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
    "thunderbird", "evolution", "geary", "mail", "telegram", "signal", "discord", "slack",
    "element", "fractal", "polari", "whatsapp", "teams", "chat", "messag", "skype", "zulip",
    "mattermost", "rocketchat", "rocket.chat", "dino", "gajim", "pidgin", "hexchat", "irc",
    "matrix", "beeper", "ferdium", "franz", "wire", "threema", "viber", "zoom",
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
        server::own_name(center.clone());
        api::own_name(center.clone());
        center
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
        let Ok(time) = glib::DateTime::now_local() else { return };
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

    pub fn note(&self, id: u32) -> Option<Note> {
        self.find(id)
    }

    /// Ask the UI to open the reply field for `id` (the banner's Reply
    /// button, or `org.umoja.Notifications.OpenReply`).
    pub fn request_reply(&self, id: u32) {
        if self.find(id).is_none_or(|n| !n.can_reply()) {
            return;
        }
        self.bubbles.remove(id);
        for l in self.reply_listeners.borrow().iter() {
            l(id);
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
        let Some(note) = self.find(id) else { return false };
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
            self.emit("ActionInvoked", &(id, "default").to_variant());
        } else {
            self.activate(id);
        }
        if !resident {
            self.close(id, CloseReason::Dismissed);
        }
        false
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

        let critical = note.urgency == Urgency::Critical;
        // Progress updates (`value` hint) to a notification whose banner has
        // already gone only refresh the message list, rather than popping a
        // bubble up again for every percent.
        let quiet_update = existing.is_some() && note.value.is_some() && !self.bubbles.contains(note.id);
        let show_banner =
            !quiet_update && (critical || (self.banners_enabled() && policy.show_banners));
        if show_banner {
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

    /// Remove a notification everywhere and tell its sender.
    pub fn close(&self, id: u32, reason: CloseReason) {
        let in_list = {
            let mut notes = self.notes.borrow_mut();
            let before = notes.len();
            notes.retain(|n| n.id != id);
            before != notes.len()
        };
        self.unseen.borrow_mut().remove(&id);
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
    pub fn activate(&self, id: u32) {
        let Some(note) = self.find(id) else { return };
        if note.default_action() {
            self.invoke(id, "default");
            return;
        }
        // No default action: GNOME raises/launches the sending app.
        if let Some(app) = note
            .app_id
            .as_deref()
            .and_then(|a| gio::DesktopAppInfo::new(&format!("{a}.desktop")))
        {
            let ctx = gdk::Display::default().map(|d| d.app_launch_context());
            if let Err(e) = app.launch(&[], ctx.as_ref()) {
                tracing::warn!(error = %e, "could not launch notification app");
            }
        }
        if !note.resident {
            self.close(id, CloseReason::Dismissed);
        }
    }

    /// The user picked an action button.
    pub fn invoke(&self, id: u32, action: &str) {
        let Some(note) = self.find(id) else { return };
        self.emit("ActionInvoked", &(id, action).to_variant());
        if !note.resident {
            self.close(id, CloseReason::Dismissed);
        } else {
            self.bubbles.remove(id);
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
/// Layer surfaces default to no keyboard at all.
pub fn want_keyboard(widget: &impl IsA<gtk::Widget>, on: bool) {
    use gtk4_layer_shell::{KeyboardMode, LayerShell};
    let Some(win) = widget.as_ref().root().and_downcast::<gtk::Window>() else { return };
    // The clock menu is a popover; its root is the panel window.
    if win.is_layer_window() {
        win.set_keyboard_mode(if on { KeyboardMode::OnDemand } else { KeyboardMode::None });
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
    let Some(s) = gs::settings(NOTIFICATIONS) else { return };
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

#[cfg(test)]
mod tests {
    use super::tests_support::note;
    use super::*;

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
        assert_eq!(banner_timeout(&note("Hi", &long)).unwrap().as_millis(), 10_000);
        let mut c = note("Battery critical", "");
        c.urgency = Urgency::Critical;
        assert!(banner_timeout(&c).is_none());
    }
}
