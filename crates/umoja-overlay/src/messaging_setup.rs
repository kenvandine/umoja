//! "Talk to Hermes from Telegram": a guided setup inside the Ask Umoja card.
//!
//! 1. Make a bot with BotFather and paste its token (checked with getMe).
//! 2. Say hi to the bot: the first message's sender is who the agent will
//!    answer, so nobody types a user id.
//! 3. "Is this you?" then umoja-agentd's SetMessaging connects it, and the
//!    bot says hello back.
//!
//! The Bot API calls are made here, before the agent's own bridge reads the
//! bot; umoja-agentd only stores the token and allowed id.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use serde_json::Value;
use umoja_common::i18n;

use crate::lemonade::RUNTIME;

const API: &str = "https://api.telegram.org";

thread_local! {
    /// Opens the setup in the Ask Umoja card (set by the view).
    static OPENER: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

pub fn set_opener(f: impl Fn() + 'static) {
    OPENER.with(|o| *o.borrow_mut() = Some(Box::new(f)));
}

/// Show the setup (from the settings row, a row in Ask Umoja, or D-Bus).
pub fn open() {
    OPENER.with(|o| {
        if let Some(f) = o.borrow().as_ref() {
            f();
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Token,
    Hello,
    Confirm,
    Connecting,
    Done,
    Connected,
    Blocked,
}

/// Who said hi to the bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sender {
    pub id: i64,
    pub chat: i64,
    pub name: String,
    pub username: String,
}

impl Sender {
    fn shown(&self) -> String {
        if self.username.is_empty() {
            self.name.clone()
        } else if self.name.is_empty() {
            format!("@{}", self.username)
        } else {
            format!("{} (@{})", self.name, self.username)
        }
    }
}

pub struct MessagingSetup {
    pub root: gtk::Box,
    title: gtk::Label,
    body: gtk::Label,
    link: gtk::Button,
    entry: gtk::PasswordEntry,
    spinner: gtk::Spinner,
    status: gtk::Label,
    back: gtk::Button,
    next: gtk::Button,
    step: Cell<Step>,
    /// Bumped on every step change and on close: stale replies and the
    /// hello poll stop.
    generation: Cell<u64>,
    agent: RefCell<String>,
    token: RefCell<String>,
    bot: RefCell<String>,
    sender: RefCell<Option<Sender>>,
    /// getUpdates offset: past the updates already looked at.
    offset: Cell<i64>,
    /// Senders the user said aren't them.
    not_me: RefCell<Vec<i64>>,
    /// Closes the card (Done).
    on_close: RefCell<Option<Box<dyn Fn()>>>,
}

impl MessagingSetup {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.add_css_class("ask-answer");
        root.add_css_class("ask-messaging");
        root.set_visible(false);
        let title = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading"]).build();
        let body = gtk::Label::builder().xalign(0.0).wrap(true).max_width_chars(70).build();
        let link = gtk::Button::builder().css_classes(["pill", "reply-chip", "flat"]).halign(gtk::Align::Start).build();
        let entry = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .placeholder_text(i18n::umoja("Paste the token here"))
            .hexpand(true)
            .build();
        let spinner = gtk::Spinner::new();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        let back = gtk::Button::builder().css_classes(["pill"]).build();
        let next = gtk::Button::builder().css_classes(["pill", "suggested-action"]).build();
        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        foot.append(&spinner);
        foot.append(&status);
        foot.append(&back);
        foot.append(&next);
        root.append(&title);
        root.append(&body);
        root.append(&link);
        root.append(&entry);
        root.append(&foot);
        let this = Rc::new(Self {
            root,
            title,
            body,
            link,
            entry,
            spinner,
            status,
            back,
            next,
            step: Cell::new(Step::Token),
            generation: Cell::new(0),
            agent: RefCell::new("Hermes".into()),
            token: RefCell::default(),
            bot: RefCell::default(),
            sender: RefCell::default(),
            offset: Cell::new(0),
            not_me: RefCell::default(),
            on_close: RefCell::default(),
        });
        let weak = Rc::downgrade(&this);
        this.next.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.on_next();
            }
        });
        let weak = Rc::downgrade(&this);
        this.back.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.on_back();
            }
        });
        let weak = Rc::downgrade(&this);
        this.entry.connect_activate(move |_| {
            if let Some(t) = weak.upgrade() {
                t.on_next();
            }
        });
        let weak = Rc::downgrade(&this);
        this.link.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                let url = match t.step.get() {
                    Step::Token => "https://t.me/BotFather".to_owned(),
                    _ => format!("https://t.me/{}", t.bot.borrow()),
                };
                let window = t.root.root().and_downcast::<gtk::Window>();
                gtk::UriLauncher::new(&url).launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
            }
        });
        this
    }

    pub fn set_on_close(&self, f: impl Fn() + 'static) {
        *self.on_close.borrow_mut() = Some(Box::new(f));
    }

    pub fn is_open(&self) -> bool {
        self.root.is_visible()
    }

    /// Starts over: connected already, blocked (not set up, paused), or
    /// the token step.
    pub fn start(self: &Rc<Self>) {
        self.root.set_visible(true);
        self.token.borrow_mut().clear();
        self.entry.set_text("");
        self.sender.replace(None);
        self.not_me.borrow_mut().clear();
        self.offset.set(0);
        self.go(Step::Blocked);
        self.status.set_text(&i18n::umoja("Loading…"));
        self.title.set_text("");
        self.body.set_text("");
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let state = agents_state().await;
            let Some(t) = weak.upgrade() else { return };
            if t.generation.get() != generation {
                return;
            }
            let Some(state) = state else {
                t.blocked(&i18n::umoja("No background agent is installed on this computer."));
                return;
            };
            let name = state["agent_name"].as_str().filter(|n| !n.is_empty()).unwrap_or("Hermes").to_owned();
            *t.agent.borrow_mut() = name.clone();
            if state["setup"] != "ready" {
                t.blocked(
                    &i18n::umoja("Set up {name} first: Ask Umoja's settings, Background agent, Set Up.")
                        .replace("{name}", &name),
                );
            } else if state["paused"] == true {
                t.blocked(&i18n::umoja("{name} is paused. Resume it first, from the Agents button on the top bar.")
                    .replace("{name}", &name));
            } else if state["messaging"]["platform"] == "telegram" {
                t.go(Step::Connected);
            } else {
                t.go(Step::Token);
            }
        });
    }

    /// Hides the setup; a hello poll still running stops.
    pub fn stop(&self) {
        self.generation.set(self.generation.get() + 1);
        self.root.set_visible(false);
        self.token.borrow_mut().clear();
        self.entry.set_text("");
    }

    fn blocked(self: &Rc<Self>, text: &str) {
        self.go(Step::Blocked);
        self.title.set_text(&i18n::umoja("Talk to {name} from Telegram").replace("{name}", &self.agent.borrow()));
        self.body.set_text(text);
    }

    fn go(self: &Rc<Self>, step: Step) {
        self.generation.set(self.generation.get() + 1);
        self.step.set(step);
        let name = self.agent.borrow().clone();
        let bot = self.bot.borrow().clone();
        self.status.remove_css_class("error");
        self.status.set_text("");
        self.spinner.set_spinning(false);
        self.spinner.set_visible(false);
        self.entry.set_visible(step == Step::Token);
        self.link.set_visible(matches!(step, Step::Token | Step::Hello));
        self.next.set_sensitive(true);
        self.next.remove_css_class("destructive-action");
        self.next.add_css_class("suggested-action");
        let (title, body, back, next): (String, String, Option<String>, Option<String>) = match step {
            Step::Token => {
                self.link.set_label(&i18n::umoja("Open BotFather in Telegram"));
                (
                    i18n::umoja("Talk to {name} from Telegram"),
                    i18n::umoja(
                        "Send {name} jobs from your phone, and it answers you there while it works on this computer.\n\n\
                         First, give it a Telegram bot: open BotFather, send /newbot, and pick a name. \
                         BotFather replies with a token (a long line with a colon in it). Paste it here.",
                    ),
                    Some(i18n::umoja("Cancel")),
                    Some(i18n::umoja("Next")),
                )
            }
            Step::Hello => {
                self.link.set_label(&i18n::umoja("Open @{bot}").replace("{bot}", &bot));
                self.spinner.set_visible(true);
                self.spinner.set_spinning(true);
                self.status.set_text(&i18n::umoja("Waiting for your message…"));
                self.poll_hello();
                (
                    i18n::umoja("Now say hi to @{bot}"),
                    i18n::umoja(
                        "Send @{bot} any message from your own Telegram account. That's how {name} knows it's you: \
                         it only ever answers you.",
                    ),
                    Some(i18n::umoja("Back")),
                    None,
                )
            }
            Step::Confirm => {
                let who = self.sender.borrow().as_ref().map(Sender::shown).unwrap_or_default();
                (
                    i18n::umoja("Is this you?"),
                    i18n::umoja("@{bot} got a message from {who}.").replace("{who}", &who),
                    Some(i18n::umoja("Not Me")),
                    Some(i18n::umoja("Yes, Connect")),
                )
            }
            Step::Connecting => {
                self.spinner.set_visible(true);
                self.spinner.set_spinning(true);
                self.status.set_text(&i18n::umoja("Connecting {name}…"));
                (i18n::umoja("Connecting"), String::new(), None, None)
            }
            Step::Done => (
                i18n::umoja("{name} is on Telegram"),
                i18n::umoja(
                    "Message @{bot} any time: {name} works on it here, in its sandbox, and answers you there. \
                     It said hello to you just now.",
                ),
                None,
                Some(i18n::umoja("Done")),
            ),
            Step::Connected => {
                self.next.remove_css_class("suggested-action");
                self.next.add_css_class("destructive-action");
                (
                    i18n::umoja("{name} is on Telegram"),
                    i18n::umoja("Only your Telegram account can talk to it. To use another bot or account, disconnect and set it up again."),
                    Some(i18n::umoja("Close")),
                    Some(i18n::umoja("Disconnect")),
                )
            }
            Step::Blocked => (String::new(), String::new(), Some(i18n::umoja("Close")), None),
        };
        let fill = |s: String| s.replace("{name}", &name).replace("{bot}", &bot);
        self.title.set_text(&fill(title));
        self.body.set_text(&fill(body));
        self.body.set_visible(step != Step::Connecting);
        self.back.set_visible(back.is_some());
        if let Some(b) = back {
            self.back.set_label(&b);
        }
        self.next.set_visible(next.is_some());
        if let Some(n) = next {
            self.next.set_label(&n);
        }
        if step == Step::Token {
            self.entry.grab_focus();
        } else if self.next.is_visible() {
            self.next.grab_focus();
        }
    }

    fn error(&self, text: &str) {
        self.spinner.set_spinning(false);
        self.spinner.set_visible(false);
        self.status.add_css_class("error");
        self.status.set_text(text);
        self.next.set_sensitive(true);
    }

    fn close(&self) {
        if let Some(f) = self.on_close.borrow().as_ref() {
            f();
        }
    }

    fn on_back(self: &Rc<Self>) {
        match self.step.get() {
            Step::Hello => self.go(Step::Token),
            Step::Confirm => {
                if let Some(s) = self.sender.take() {
                    self.not_me.borrow_mut().push(s.id);
                }
                self.go(Step::Hello);
            }
            _ => self.close(),
        }
    }

    fn on_next(self: &Rc<Self>) {
        match self.step.get() {
            Step::Token => self.check_token(),
            Step::Confirm => self.connect(),
            Step::Done => self.close(),
            Step::Connected => self.disconnect(),
            _ => {}
        }
    }

    fn check_token(self: &Rc<Self>) {
        let Some(token) = extract_token(&self.entry.text()) else {
            self.error(&i18n::umoja(
                "That doesn't look like a bot token. Copy the whole line BotFather sent, like 123456789:AAF…",
            ));
            return;
        };
        self.next.set_sensitive(false);
        self.spinner.set_visible(true);
        self.spinner.set_spinning(true);
        self.status.remove_css_class("error");
        self.status.set_text(&i18n::umoja("Checking…"));
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let t2 = token.clone();
            let res = RUNTIME.spawn(async move { get_me(&t2).await }).await.unwrap_or(Err(Problem::Offline));
            let Some(t) = weak.upgrade() else { return };
            if t.generation.get() != generation {
                return;
            }
            match res {
                Ok(bot) => {
                    *t.token.borrow_mut() = token;
                    *t.bot.borrow_mut() = bot;
                    t.go(Step::Hello);
                }
                Err(p) => t.error(&p.words(&t.agent.borrow())),
            }
        });
    }

    /// Long-polls getUpdates until someone (not turned down) says hi.
    fn poll_hello(self: &Rc<Self>) {
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                let Some(t) = weak.upgrade() else { return };
                if t.generation.get() != generation {
                    return;
                }
                let (token, offset) = (t.token.borrow().clone(), t.offset.get());
                drop(t);
                let res = RUNTIME
                    .spawn(async move { get_updates(&token, offset, 20).await })
                    .await
                    .unwrap_or(Err(Problem::Offline));
                let Some(t) = weak.upgrade() else { return };
                if t.generation.get() != generation {
                    return;
                }
                match res {
                    Ok(updates) => {
                        let not_me = t.not_me.borrow().clone();
                        let (next, sender) = first_sender(&updates, offset, &not_me);
                        t.offset.set(next);
                        if let Some(s) = sender {
                            t.sender.replace(Some(s));
                            t.go(Step::Confirm);
                            return;
                        }
                    }
                    Err(p) => {
                        t.error(&p.words(&t.agent.borrow()));
                        drop(t);
                        // Try again in a bit: Telegram or the network may be back.
                        glib::timeout_future(Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }

    fn connect(self: &Rc<Self>) {
        let Some(sender) = self.sender.borrow().clone() else { return };
        let token = self.token.borrow().clone();
        let offset = self.offset.get();
        self.go(Step::Connecting);
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // Mark the hello as read, so the agent doesn't take it as a job.
            let t2 = token.clone();
            let _ = RUNTIME.spawn(async move { get_updates(&t2, offset, 0).await }).await;
            let res = agents_call(
                "SetMessaging",
                ("telegram", token.as_str(), sender.id.to_string().as_str()).to_variant(),
                180_000,
            )
            .await;
            let Some(t) = weak.upgrade() else { return };
            if t.generation.get() != generation {
                return;
            }
            match res {
                Ok(()) => {
                    let name = t.agent.borrow().clone();
                    let first = sender.name.split_whitespace().next().unwrap_or_default().to_owned();
                    let hello = if first.is_empty() {
                        i18n::umoja("Hi! I'm {name}, on your computer. Send me a job any time.")
                    } else {
                        i18n::umoja("Hi {first}! I'm {name}, on your computer. Send me a job any time.")
                            .replace("{first}", &first)
                    }
                    .replace("{name}", &name);
                    let chat = sender.chat;
                    let _ = RUNTIME.spawn(async move { send_message(&token, chat, &hello).await }).await;
                    t.token.borrow_mut().clear();
                    t.go(Step::Done);
                }
                Err(e) => {
                    t.go(Step::Confirm);
                    t.error(&e);
                }
            }
        });
    }

    fn disconnect(self: &Rc<Self>) {
        self.next.set_sensitive(false);
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = agents_call("SetMessaging", ("", "", "").to_variant(), 180_000).await;
            let Some(t) = weak.upgrade() else { return };
            if t.generation.get() != generation {
                return;
            }
            match res {
                Ok(()) => t.go(Step::Token),
                Err(e) => t.error(&e),
            }
        });
    }
}

/// What went wrong talking to Telegram.
#[derive(Debug, PartialEq, Eq)]
enum Problem {
    BadToken,
    /// Something else reads this bot's messages (another app, or a webhook).
    InUse,
    Offline,
}

impl Problem {
    fn words(&self, name: &str) -> String {
        match self {
            Problem::BadToken => i18n::umoja(
                "Telegram didn't accept that token. Copy it again from BotFather (or send /token there for a new one).",
            ),
            Problem::InUse => i18n::umoja(
                "Another app is reading this bot's messages. Use a new bot for {name}, or stop the other app.",
            )
            .replace("{name}", name),
            Problem::Offline => i18n::umoja("Couldn't reach Telegram. Check that you're online; trying again…"),
        }
    }
}

/// The bot token in pasted text: "123456789:AAF…" (BotFather's message
/// has words around it).
pub fn extract_token(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '<' | '>' | ',' | ';'))
        .map(|w| w.trim_end_matches(['.', ')']).trim_start_matches('('))
        .find(|w| {
            let Some((id, secret)) = w.split_once(':') else { return false };
            !id.is_empty()
                && id.len() <= 20
                && id.bytes().all(|b| b.is_ascii_digit())
                && secret.len() >= 30
                && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(str::to_owned)
}

/// The next offset, and the first private-chat sender who isn't a bot and
/// wasn't turned down.
fn first_sender(updates: &[Value], offset: i64, not_me: &[i64]) -> (i64, Option<Sender>) {
    let mut next = offset;
    for u in updates {
        if let Some(id) = u["update_id"].as_i64() {
            next = next.max(id + 1);
        }
        let m = &u["message"];
        let from = &m["from"];
        let Some(id) = from["id"].as_i64() else { continue };
        if from["is_bot"] == true || not_me.contains(&id) || m["chat"]["type"] != "private" {
            continue;
        }
        let name = [from["first_name"].as_str(), from["last_name"].as_str()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let sender = Sender {
            id,
            chat: m["chat"]["id"].as_i64().unwrap_or(id),
            name,
            username: from["username"].as_str().unwrap_or_default().to_owned(),
        };
        return (next, Some(sender));
    }
    (next, None)
}

fn client(timeout: u64) -> Result<reqwest::Client, Problem> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()
        .map_err(|_| Problem::Offline)
}

async fn bot_api(token: &str, method: &str, body: Value, timeout: u64) -> Result<Value, Problem> {
    let resp = client(timeout)?
        .post(format!("{API}/bot{token}/{method}"))
        .json(&body)
        .send()
        .await
        .map_err(|_| Problem::Offline)?;
    let code = resp.status().as_u16();
    let v: Value = resp.json().await.map_err(|_| Problem::Offline)?;
    match code {
        200 if v["ok"] == true => Ok(v["result"].clone()),
        401 | 404 => Err(Problem::BadToken),
        409 => Err(Problem::InUse),
        _ => Err(Problem::Offline),
    }
}

async fn get_me(token: &str) -> Result<String, Problem> {
    let me = bot_api(token, "getMe", serde_json::json!({}), 15).await?;
    me["username"].as_str().map(str::to_owned).ok_or(Problem::BadToken)
}

async fn get_updates(token: &str, offset: i64, wait: u64) -> Result<Vec<Value>, Problem> {
    let body = serde_json::json!({ "offset": offset, "timeout": wait, "allowed_updates": ["message"] });
    let v = bot_api(token, "getUpdates", body, wait + 15).await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

async fn send_message(token: &str, chat: i64, text: &str) -> Result<(), Problem> {
    bot_api(token, "sendMessage", serde_json::json!({ "chat_id": chat, "text": text }), 15).await.map(|_| ())
}

async fn agents_state() -> Option<Value> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    let v = conn
        .call_future(
            Some("org.umoja.Agents"),
            "/org/umoja/Agents",
            "org.umoja.Agents",
            "GetState",
            None,
            Some(glib::VariantTy::new("(s)").expect("type")),
            gio::DBusCallFlags::NONE,
            5_000,
        )
        .await
        .ok()?;
    let (json,) = v.get::<(String,)>()?;
    serde_json::from_str(&json).ok()
}

/// An org.umoja.Agents call; Err is umoja-agentd's message, for people.
async fn agents_call(method: &str, args: glib::Variant, timeout: i32) -> Result<(), String> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.map_err(|e| e.to_string())?;
    conn.call_future(
        Some("org.umoja.Agents"),
        "/org/umoja/Agents",
        "org.umoja.Agents",
        method,
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        timeout,
    )
    .await
    .map(|_| ())
    .map_err(|mut e| {
        gio::DBusError::strip_remote_error(&mut e);
        e.message().to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "123456789:AAFqWxYz0123456789abcdefghijklmnopQR";

    #[test]
    fn tokens_in_pasted_text() {
        assert_eq!(extract_token(T).as_deref(), Some(T));
        let msg = format!("Done! Use this token to access the HTTP API:\n{T}\nKeep your token secure.");
        assert_eq!(extract_token(&msg).as_deref(), Some(T));
        assert_eq!(extract_token(&format!("`{T}`.")).as_deref(), Some(T));
        assert_eq!(extract_token("hello"), None);
        assert_eq!(extract_token("12:short"), None);
        assert_eq!(extract_token("https://t.me/BotFather"), None);
    }

    #[test]
    fn hello_sender() {
        let updates: Vec<Value> = serde_json::from_str(
            r#"[
            {"update_id": 10, "message": {"from": {"id": 5, "is_bot": true, "first_name": "B"}, "chat": {"id": 5, "type": "private"}}},
            {"update_id": 11, "message": {"from": {"id": 7, "first_name": "Ann"}, "chat": {"id": -3, "type": "group"}}},
            {"update_id": 12, "message": {"from": {"id": 42, "first_name": "Ken", "last_name": "V", "username": "kv"}, "chat": {"id": 42, "type": "private"}}},
            {"update_id": 13, "message": {"from": {"id": 43, "first_name": "Other"}, "chat": {"id": 43, "type": "private"}}}
        ]"#,
        )
        .unwrap();
        let (next, s) = first_sender(&updates, 0, &[]);
        let s = s.unwrap();
        assert_eq!((s.id, s.chat, s.shown().as_str()), (42, 42, "Ken V (@kv)"));
        assert_eq!(next, 13);
        let (_, s) = first_sender(&updates, 0, &[42]);
        assert_eq!(s.unwrap().id, 43);
        assert_eq!(first_sender(&[], 9, &[]), (9, None));
    }
}
