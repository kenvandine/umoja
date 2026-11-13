//! The NetworkManager secret agent, built in like gnome-shell's
//! NetworkAgent.
//!
//! NetworkManager never asks for a Wi-Fi password itself. When a
//! connection needs one (a new secured network, a changed password, a
//! VPN), it calls `GetSecrets` on the secret agents registered by the
//! user's session, and in a GNOME session the only one is gnome-shell.
//! Without one, connecting to a secured network from Quick Settings or
//! from GNOME Settings just fails without ever prompting.
//!
//! As gnome-shell's agent does, this one:
//! - answers from the keyring for secrets stored "for this user only"
//!   (agent-owned), in the same keyring items as gnome-shell, see
//!   [`keyring`];
//! - otherwise shows a password card, or, when NetworkManager asked on its
//!   own (an automatic reconnect), a notification that opens it;
//! - runs the VPN plugin's auth dialog for VPN secrets;
//! - saves and deletes agent-owned secrets on `SaveSecrets` /
//!   `DeleteSecrets`.

mod content;
mod dialog;
mod keyring;
mod vpn;

use crate::bus;
use content::Conn;
use gio::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};

const NM: &str = "org.freedesktop.NetworkManager";
const AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";
const AGENT_MANAGER: &str = "org.freedesktop.NetworkManager.AgentManager";
const AGENT_IFACE: &str = "org.freedesktop.NetworkManager.SecretAgent";
const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const IDENTIFIER: &str = "org.umoja.Shell.NetworkAgent";
// NMSecretAgentCapabilities
const CAP_VPN_HINTS: u32 = 0x1;

// NMSecretAgentGetSecretsFlags
const ALLOW_INTERACTION: u32 = 0x1;
const REQUEST_NEW: u32 = 0x2;
const USER_REQUESTED: u32 = 0x4;
const WPS_PBC_ACTIVE: u32 = 0x8;

const ERR: &str = "org.freedesktop.NetworkManager.SecretAgent";

const XML: &str = r#"<node>
  <interface name="org.freedesktop.NetworkManager.SecretAgent">
    <method name="GetSecrets">
      <arg type="a{sa{sv}}" name="connection" direction="in"/>
      <arg type="o" name="connection_path" direction="in"/>
      <arg type="s" name="setting_name" direction="in"/>
      <arg type="as" name="hints" direction="in"/>
      <arg type="u" name="flags" direction="in"/>
      <arg type="a{sa{sv}}" name="secrets" direction="out"/>
    </method>
    <method name="CancelGetSecrets">
      <arg type="o" name="connection_path" direction="in"/>
      <arg type="s" name="setting_name" direction="in"/>
    </method>
    <method name="SaveSecrets">
      <arg type="a{sa{sv}}" name="connection" direction="in"/>
      <arg type="o" name="connection_path" direction="in"/>
    </method>
    <method name="DeleteSecrets">
      <arg type="a{sa{sv}}" name="connection" direction="in"/>
      <arg type="o" name="connection_path" direction="in"/>
    </method>
  </interface>
</node>"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Response {
    Confirmed,
    UserCanceled,
    Failed,
}

/// What is showing for a request.
#[derive(Default)]
enum Ui {
    #[default]
    None,
    /// Waiting its turn for the dialog.
    Queued,
    Dialog(Rc<dialog::Dialog>),
    Notification(u32),
    Vpn(Rc<vpn::Helper>),
}

/// One pending `GetSecrets`.
struct Request {
    id: String,
    conn: RefCell<Conn>,
    setting: String,
    hints: Vec<String>,
    flags: u32,
    invocation: RefCell<Option<gio::DBusMethodInvocation>>,
    /// The answer so far, for the requested setting.
    entries: RefCell<HashMap<String, String>>,
    /// VPN secrets, sent as the setting's `secrets` a{ss}.
    vpn_secrets: RefCell<HashMap<String, String>>,
    ui: RefCell<Ui>,
}

impl Request {
    fn reply_error(&self, name: &str, text: &str) {
        if let Some(inv) = self.invocation.borrow_mut().take() {
            inv.return_dbus_error(&format!("{ERR}.{name}"), text);
        }
    }

    /// Take down whatever is showing; true when that was the dialog.
    fn close_ui(&self) -> bool {
        match self.ui.replace(Ui::None) {
            Ui::Dialog(d) => {
                d.close();
                return true;
            }
            Ui::Vpn(h) => h.cancel(),
            Ui::Notification(id) => {
                if let Some(c) = crate::notification_center() {
                    c.close(id, crate::notifications::CloseReason::Closed);
                }
            }
            Ui::None | Ui::Queued => {}
        }
        false
    }
}

#[derive(Default)]
struct Agent {
    requests: RefCell<HashMap<String, Rc<Request>>>,
    /// Dialogs show one at a time, like the polkit agent's.
    queue: RefCell<VecDeque<String>>,
    dialog_open: Cell<bool>,
}

thread_local! {
    static AGENT: Rc<Agent> = Rc::new(Agent::default());
}

/// Export the agent and register it with NetworkManager, again whenever
/// NetworkManager restarts.
pub fn start() {
    let Some(conn) = bus::system_bus() else { return };
    let agent = AGENT.with(Rc::clone);
    if let Err(e) = export(&conn, Rc::downgrade(&agent)) {
        tracing::warn!(error = %e, "could not export the network secret agent");
        return;
    }
    let _watch = gio::bus_watch_name_on_connection(
        &conn,
        NM,
        gio::BusNameWatcherFlags::NONE,
        |conn, _, _| register(&conn),
        |_, _| {
            // Its requests died with it.
            AGENT.with(|a| a.drop_all());
        },
    );
}

fn register(conn: &gio::DBusConnection) {
    conn.call(
        Some(NM),
        AGENT_MANAGER_PATH,
        AGENT_MANAGER,
        "RegisterWithCapabilities",
        Some(&(IDENTIFIER, CAP_VPN_HINTS).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
        |r| match r {
            Ok(_) => tracing::info!("registered as the NetworkManager secret agent"),
            Err(e) => tracing::warn!(error = %e, "could not register the network secret agent"),
        },
    );
}

fn export(conn: &gio::DBusConnection, agent: Weak<Agent>) -> Result<(), glib::Error> {
    let info = gio::DBusNodeInfo::for_xml(XML)?;
    let iface = info
        .lookup_interface(AGENT_IFACE)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no interface"))?;
    let _id = conn
        .register_object(AGENT_PATH, &iface)
        .method_call(move |conn, sender, _, _, method, params, invocation| {
            if !from_nm(&conn, sender) {
                invocation.return_dbus_error(
                    &format!("{ERR}.PermissionDenied"),
                    "only NetworkManager may call the secret agent",
                );
                return;
            }
            let Some(agent) = agent.upgrade() else {
                invocation.return_dbus_error(&format!("{ERR}.Failed"), "agent gone");
                return;
            };
            match method {
                "GetSecrets" => agent.get_secrets(&params, invocation),
                "CancelGetSecrets" => {
                    let path = params.child_value(0).str().unwrap_or_default().to_string();
                    let setting = params.child_value(1).str().unwrap_or_default().to_string();
                    agent.cancel(&format!("{path}/{setting}"));
                    invocation.return_value(None);
                }
                "SaveSecrets" => {
                    let conn = Conn::from_variant(&params.child_value(0));
                    glib::spawn_future_local(async move {
                        let _ = gio::spawn_blocking(move || save_secrets(&conn, &HashMap::new())).await;
                        invocation.return_value(None);
                    });
                }
                "DeleteSecrets" => {
                    let uuid = Conn::from_variant(&params.child_value(0)).uuid();
                    glib::spawn_future_local(async move {
                        if !uuid.is_empty() {
                            let _ = gio::spawn_blocking(move || keyring::delete(&uuid)).await;
                        }
                        invocation.return_value(None);
                    });
                }
                _ => invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
            }
        })
        .build()?;
    Ok(())
}

fn from_nm(conn: &gio::DBusConnection, sender: Option<&str>) -> bool {
    let Some(sender) = sender else { return false };
    conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetNameOwner",
        Some(&(NM,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|r| r.child_value(0).str().map(|o| o == sender))
    .unwrap_or(false)
}

impl Agent {
    fn get_secrets(self: &Rc<Self>, params: &glib::Variant, invocation: gio::DBusMethodInvocation) {
        let conn = Conn::from_variant(&params.child_value(0));
        let path = params.child_value(1).str().unwrap_or_default().to_string();
        let setting = params.child_value(2).str().unwrap_or_default().to_string();
        let hints: Vec<String> = params
            .child_value(3)
            .iter()
            .filter_map(|h| h.str().map(String::from))
            .collect();
        let flags: u32 = params.child_value(4).get().unwrap_or(0);
        let id = format!("{path}/{setting}");
        tracing::info!(
            connection = conn.id(),
            setting,
            flags,
            ?hints,
            "network secrets requested"
        );
        // A newer request for the same thing replaces the old one.
        self.cancel(&id);

        let req = Rc::new(Request {
            id: id.clone(),
            conn: RefCell::new(conn),
            setting,
            hints,
            flags,
            invocation: RefCell::new(Some(invocation)),
            entries: RefCell::default(),
            vpn_secrets: RefCell::default(),
            ui: RefCell::default(),
        });
        self.requests.borrow_mut().insert(id, req.clone());

        let ask_now = flags & REQUEST_NEW != 0
            || (flags & ALLOW_INTERACTION != 0 && req.conn.borrow().always_ask());
        if ask_now {
            self.ask(&req);
            return;
        }
        // First the keyring, for secrets saved "for this user only".
        let (uuid, setting) = (req.conn.borrow().uuid(), req.setting.clone());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let found = gio::spawn_blocking(move || keyring::lookup(&uuid, &setting))
                .await
                .unwrap_or_default();
            let Some(agent) = weak.upgrade() else { return };
            if !agent.is_current(&req) {
                return;
            }
            let any = !found.is_empty();
            if req.setting == "vpn" {
                // The VPN's auth dialog knows best what it needs; it gets
                // what the keyring had.
                req.vpn_secrets.borrow_mut().extend(found);
                agent.ask(&req);
            } else if !any && flags & ALLOW_INTERACTION != 0 {
                agent.ask(&req);
            } else {
                // NetworkManager fails the connection if nothing comes
                // back, rather than asking again with REQUEST_NEW; so with
                // no way to ask, this is the answer.
                req.entries.borrow_mut().extend(found);
                agent.respond(&req, Response::Confirmed, false);
            }
        });
    }

    fn is_current(&self, req: &Rc<Request>) -> bool {
        self.requests
            .borrow()
            .get(&req.id)
            .is_some_and(|r| Rc::ptr_eq(r, req))
    }

    /// Ask the user, now or through a notification.
    fn ask(self: &Rc<Self>, req: &Rc<Request>) {
        if req.flags & USER_REQUESTED == 0 {
            self.notify(req);
        } else {
            self.handle(req);
        }
    }

    fn notify(self: &Rc<Self>, req: &Rc<Request>) {
        let text = content::notification(&req.conn.borrow(), &req.hints);
        let (Some((title, body)), Some(center)) = (text, crate::notification_center()) else {
            self.respond(req, Response::Failed, false);
            return;
        };
        // The notification's handler is dropped when it closes: unless it
        // was clicked, that cancels the request, as in gnome-shell.
        struct Guard {
            agent: Weak<Agent>,
            req: Weak<Request>,
            answered: Cell<bool>,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                if self.answered.get() {
                    return;
                }
                if let (Some(a), Some(r)) = (self.agent.upgrade(), self.req.upgrade())
                    && a.is_current(&r)
                    && matches!(*r.ui.borrow(), Ui::Notification(_))
                {
                    r.ui.replace(Ui::None);
                    let a = a.clone();
                    glib::idle_add_local_once(move || a.respond(&r, Response::UserCanceled, false));
                }
            }
        }
        let guard = Guard {
            agent: Rc::downgrade(self),
            req: Rc::downgrade(req),
            answered: Cell::new(false),
        };
        let nid = center.notify_with_actions(
            &title,
            &body,
            "dialog-password-symbolic",
            &[("default", "")],
            move |_| {
                guard.answered.set(true);
                if let (Some(a), Some(r)) = (guard.agent.upgrade(), guard.req.upgrade())
                    && a.is_current(&r)
                {
                    r.ui.replace(Ui::None);
                    glib::idle_add_local_once(move || a.handle(&r));
                }
            },
        );
        req.ui.replace(Ui::Notification(nid));
    }

    fn handle(self: &Rc<Self>, req: &Rc<Request>) {
        if req.setting == "vpn" {
            let weak = Rc::downgrade(self);
            let r = Rc::downgrade(req);
            match vpn::Helper::start(&req.conn.borrow(), &req.hints, req.flags, &req.vpn_secrets.borrow(), move |ev| {
                let (Some(a), Some(r)) = (weak.upgrade(), r.upgrade()) else { return };
                if a.is_current(&r) {
                    a.on_vpn(&r, ev);
                }
            }) {
                Some(h) => {
                    req.ui.replace(Ui::Vpn(h));
                }
                None => self.respond(req, Response::Failed, false),
            }
            return;
        }
        let known = req.entries.borrow().clone();
        let built = content::build(&req.conn.borrow(), &req.setting, &req.hints, &known);
        let Some(mut c) = built else {
            self.respond(req, Response::Failed, false);
            return;
        };
        c.wps = req.flags & WPS_PBC_ACTIVE != 0;
        self.show_dialog(req, c);
    }

    fn show_dialog(self: &Rc<Self>, req: &Rc<Request>, content: content::Content) {
        if self.dialog_open.get() {
            req.ui.replace(Ui::Queued);
            PENDING.with(|p| p.borrow_mut().insert(req.id.clone(), content));
            self.queue.borrow_mut().push_back(req.id.clone());
            return;
        }
        self.dialog_open.set(true);
        let weak = Rc::downgrade(self);
        let r = Rc::downgrade(req);
        let vpn = req.setting == "vpn";
        let d = dialog::Dialog::new(content, move |answer| {
            let Some(a) = weak.upgrade() else { return };
            a.dialog_closed();
            let Some(r) = r.upgrade() else { return };
            if !a.is_current(&r) {
                return;
            }
            r.ui.replace(Ui::None);
            match answer {
                Some(values) => {
                    for (k, v) in values {
                        if vpn {
                            r.vpn_secrets.borrow_mut().insert(k, v);
                        } else {
                            r.entries.borrow_mut().insert(k, v);
                        }
                    }
                    a.respond(&r, Response::Confirmed, true);
                }
                None => a.respond(&r, Response::UserCanceled, false),
            }
        });
        req.ui.replace(Ui::Dialog(d));
    }

    fn dialog_closed(self: &Rc<Self>) {
        self.dialog_open.set(false);
        let agent = self.clone();
        glib::idle_add_local_once(move || agent.show_next());
    }

    fn show_next(self: &Rc<Self>) {
        while !self.dialog_open.get() {
            let Some(id) = self.queue.borrow_mut().pop_front() else { return };
            let content = PENDING.with(|p| p.borrow_mut().remove(&id));
            let req = self.requests.borrow().get(&id).cloned();
            if let (Some(req), Some(content)) = (req, content)
                && matches!(*req.ui.borrow(), Ui::Queued)
            {
                self.show_dialog(&req, content);
            }
        }
    }

    fn on_vpn(self: &Rc<Self>, req: &Rc<Request>, ev: vpn::Event) {
        match ev {
            vpn::Event::Secret(k, v) => {
                req.vpn_secrets.borrow_mut().insert(k, v);
            }
            vpn::Event::Ask(content) => self.show_dialog(req, content),
            vpn::Event::Done(true) => self.respond(req, Response::Confirmed, true),
            vpn::Event::Done(false) => self.respond(req, Response::UserCanceled, false),
            vpn::Event::Failed => self.respond(req, Response::Failed, false),
        }
    }

    /// Answer NetworkManager and forget the request.
    fn respond(self: &Rc<Self>, req: &Rc<Request>, response: Response, save: bool) {
        if !self.is_current(req) {
            return;
        }
        self.requests.borrow_mut().remove(&req.id);
        if req.close_ui() {
            self.dialog_closed();
        }
        tracing::info!(connection = req.conn.borrow().id(), ?response, "network secrets answered");
        match response {
            Response::UserCanceled => {
                req.reply_error("UserCanceled", "Network dialog was canceled by the user")
            }
            Response::Failed => req.reply_error(
                "Failed",
                "An internal error occurred while processing the request.",
            ),
            Response::Confirmed => {
                let mut setting: HashMap<String, glib::Variant> = req
                    .entries
                    .borrow()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_variant()))
                    .collect();
                let vpn = req.vpn_secrets.borrow().clone();
                if !vpn.is_empty() {
                    setting.insert("secrets".into(), vpn.to_variant());
                }
                let reply: HashMap<String, HashMap<String, glib::Variant>> =
                    HashMap::from([(req.setting.clone(), setting)]);
                if let Some(inv) = req.invocation.borrow_mut().take() {
                    inv.return_value(Some(&(reply,).to_variant()));
                }
                // Keep what the user typed, when it's ours to keep.
                if save && req.flags & (ALLOW_INTERACTION | REQUEST_NEW) != 0 {
                    let conn = req.conn.borrow().clone();
                    let mut answers = req.entries.borrow().clone();
                    answers.extend(vpn);
                    let setting = req.setting.clone();
                    glib::spawn_future_local(async move {
                        let _ = gio::spawn_blocking(move || {
                            save_secrets(&conn, &HashMap::from([(setting, answers)]))
                        })
                        .await;
                    });
                }
            }
        }
    }

    /// NetworkManager withdrew a request (or replaced it).
    fn cancel(self: &Rc<Self>, id: &str) {
        let Some(req) = self.requests.borrow_mut().remove(id) else { return };
        if req.close_ui() {
            self.dialog_closed();
        }
        self.queue.borrow_mut().retain(|q| q != id);
        PENDING.with(|p| p.borrow_mut().remove(id));
        req.reply_error("AgentCanceled", "Canceled by NetworkManager");
    }

    fn drop_all(self: &Rc<Self>) {
        let all: Vec<_> = self.requests.borrow_mut().drain().map(|(_, r)| r).collect();
        self.queue.borrow_mut().clear();
        self.dialog_open.set(false);
        PENDING.with(|p| p.borrow_mut().clear());
        for r in all {
            r.close_ui();
            r.invocation.borrow_mut().take();
        }
    }
}

thread_local! {
    /// Dialog content waiting for the dialog before it to close.
    static PENDING: RefCell<HashMap<String, content::Content>> = RefCell::default();
}

/// Store the connection's agent-owned secrets in the keyring, as
/// gnome-shell's save_secrets: only those whose flags are exactly
/// AGENT_OWNED (not system-owned, not "ask every time"). `answers` are
/// fresh values by setting, over what the connection carries. Blocking.
fn save_secrets(conn: &Conn, answers: &HashMap<String, HashMap<String, String>>) {
    let (uuid, id) = (conn.uuid(), conn.id());
    if uuid.is_empty() {
        return;
    }
    for (setting, props) in &conn.0 {
        let fresh = answers.get(setting);
        if setting == "vpn" {
            let service = conn.str("vpn", "service-type").unwrap_or_default();
            let mut secrets: HashMap<String, String> = conn.dict("vpn", "secrets").into_iter().collect();
            if let Some(f) = fresh {
                secrets.extend(f.clone());
            }
            for (k, v) in secrets {
                if v.is_empty() || conn.secret_flags("vpn", &k) != content::AGENT_OWNED {
                    continue;
                }
                let label = format!("VPN {k} secret for {id}/{service}/vpn");
                keyring::store(&uuid, "vpn", &k, &label, &v);
            }
            continue;
        }
        for key in content::secret_keys(setting) {
            let value = fresh.and_then(|f| f.get(*key).cloned()).or_else(|| props.str(key));
            let Some(value) = value.filter(|v| !v.is_empty()) else { continue };
            if conn.secret_flags(setting, key) != content::AGENT_OWNED {
                continue;
            }
            let label = format!("Network secret for {id}/{setting}/{key}");
            keyring::store(&uuid, setting, key, &label, &value);
        }
    }
    // Answers for a setting the connection didn't carry yet.
    for (setting, fresh) in answers {
        if conn.0.contains_key(setting) || setting == "vpn" {
            continue;
        }
        for (key, value) in fresh {
            if !value.is_empty() && conn.secret_flags(setting, key) == content::AGENT_OWNED {
                let label = format!("Network secret for {id}/{setting}/{key}");
                keyring::store(&uuid, setting, key, &label, value);
            }
        }
    }
}
