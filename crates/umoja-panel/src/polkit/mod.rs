//! The PolicyKit authentication agent, built in like gnome-shell's.
//!
//! When an app asks polkitd for something that needs a password
//! (`pkexec`, Settings > Users, software updates, mounting a disk...),
//! polkitd calls `BeginAuthentication` on the agent registered for the
//! user's session and waits for it. Without one, polkitd has nobody to ask
//! and the request just fails, which is what Umoja did before this: on
//! Ubuntu the only agent is gnome-shell itself.
//!
//! The panel registers for its logind session on the system bus, shows one
//! dialog at a time (later requests queue), and leaves the password check
//! itself to libpolkit-agent-1's `PolkitAgentSession`, which hands it to
//! polkit's privileged helper; the helper tells polkitd the result. The
//! panel never sees whether the password was right except through that
//! session's `completed` signal.

mod dialog;
mod ffi;

use crate::{bus, login};
use gio::prelude::*;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

const POLKIT: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY: &str = "org.freedesktop.PolicyKit1.Authority";
const AGENT_IFACE: &str = "org.freedesktop.PolicyKit1.AuthenticationAgent";
const AGENT_PATH: &str = "/org/umoja/PolicyKit1/AuthenticationAgent";
const CANCELLED: &str = "org.freedesktop.PolicyKit1.Error.Cancelled";

const XML: &str = r#"<node>
  <interface name="org.freedesktop.PolicyKit1.AuthenticationAgent">
    <method name="BeginAuthentication">
      <arg type="s" name="action_id" direction="in"/>
      <arg type="s" name="message" direction="in"/>
      <arg type="s" name="icon_name" direction="in"/>
      <arg type="a{ss}" name="details" direction="in"/>
      <arg type="s" name="cookie" direction="in"/>
      <arg type="a(sa{sv})" name="identities" direction="in"/>
    </method>
    <method name="CancelAuthentication">
      <arg type="s" name="cookie" direction="in"/>
    </method>
  </interface>
</node>"#;

/// One `BeginAuthentication` call, answered when the dialog is done.
pub struct Request {
    pub action_id: String,
    pub message: String,
    pub cookie: String,
    /// Users who may authenticate, the current user first if allowed.
    pub uids: Vec<u32>,
    invocation: gio::DBusMethodInvocation,
}

impl Request {
    fn reply(self, authorized: bool) {
        if authorized {
            self.invocation.return_value(None);
        } else {
            self.invocation
                .return_dbus_error(CANCELLED, "Authentication dialog was dismissed by the user");
        }
    }
}

#[derive(Default)]
struct Agent {
    queue: RefCell<VecDeque<Request>>,
    current: RefCell<Option<(Request, Rc<dialog::Dialog>)>>,
}

impl Agent {
    fn begin(self: &Rc<Self>, req: Request) {
        tracing::info!(action = req.action_id, "authentication requested");
        self.queue.borrow_mut().push_back(req);
        self.show_next();
    }

    fn show_next(self: &Rc<Self>) {
        if self.current.borrow().is_some() {
            return;
        }
        let Some(req) = self.queue.borrow_mut().pop_front() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let dialog = dialog::Dialog::new(&req, move |authorized| {
            if let Some(agent) = weak.upgrade() {
                agent.finish(authorized);
            }
        });
        *self.current.borrow_mut() = Some((req, dialog));
    }

    /// The current dialog is done, either way.
    fn finish(self: &Rc<Self>, authorized: bool) {
        let Some((req, dialog)) = self.current.borrow_mut().take() else {
            return;
        };
        tracing::info!(
            action = req.action_id,
            authorized,
            "authentication finished"
        );
        dialog.close();
        req.reply(authorized);
        // Not from inside the dialog's own callbacks.
        let agent = self.clone();
        glib::idle_add_local_once(move || agent.show_next());
    }

    /// polkitd withdrew a request (the app went away, or it timed out).
    fn cancel(self: &Rc<Self>, cookie: &str) {
        let current = self
            .current
            .borrow()
            .as_ref()
            .map(|(r, d)| (r.cookie == cookie, d.clone()));
        if let Some((true, dialog)) = current {
            dialog.cancel();
            return;
        }
        let mut queue = self.queue.borrow_mut();
        if let Some(i) = queue.iter().position(|r| r.cookie == cookie)
            && let Some(req) = queue.remove(i)
        {
            req.reply(false);
        }
    }
}

/// Register as the session's authentication agent, and again whenever
/// polkitd restarts.
pub fn start() {
    let Some(conn) = bus::system_bus() else {
        return;
    };
    let agent = Rc::new(Agent::default());
    if let Err(e) = export(&conn, agent) {
        tracing::warn!(error = %e, "could not export the polkit agent");
        return;
    }
    // Watched for the life of the panel.
    let _watch = gio::bus_watch_name_on_connection(
        &conn,
        POLKIT,
        gio::BusNameWatcherFlags::NONE,
        |conn, _, _| register(&conn),
        |_, _| tracing::info!("polkitd went away"),
    );
}

fn export(conn: &gio::DBusConnection, agent: Rc<Agent>) -> Result<(), glib::Error> {
    let info = gio::DBusNodeInfo::for_xml(XML)?;
    let iface = info
        .lookup_interface(AGENT_IFACE)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no interface"))?;
    // The registration lives as long as the connection.
    let _id =
        conn.register_object(AGENT_PATH, &iface)
            .method_call(move |conn, sender, _, _, method, params, invocation| {
                // Only polkitd gets to open password dialogs.
                if !bus::sent_by_owner(&conn, sender, POLKIT) {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.AccessDenied",
                        "only polkitd may call the authentication agent",
                    );
                    return;
                }
                match method {
                    "BeginAuthentication" => match parse_begin(&params, invocation.clone()) {
                        Some(req) => agent.begin(req),
                        None => invocation.return_dbus_error(
                            "org.freedesktop.PolicyKit1.Error.Failed",
                            "no identity this agent can authenticate",
                        ),
                    },
                    "CancelAuthentication" => {
                        if let Some(cookie) = params.child_value(0).str() {
                            agent.cancel(cookie);
                        }
                        invocation.return_value(None);
                    }
                    _ => invocation
                        .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
                }
            })
            .build()?;
    Ok(())
}

/// `(sssa{ss}sa(sa{sv}))` into a request, keeping the unix-user
/// identities (polkitd has already expanded groups into their members).
fn parse_begin(params: &glib::Variant, invocation: gio::DBusMethodInvocation) -> Option<Request> {
    let s = |i: usize| params.try_child_value(i)?.str().map(String::from);
    let (action_id, message, cookie) = (s(0)?, s(1)?, s(4)?);
    let me = unsafe { libc::getuid() };
    let mut uids: Vec<u32> = Vec::new();
    for identity in params.try_child_value(5)?.iter() {
        if identity.try_child_value(0)?.str() != Some("unix-user") {
            continue;
        }
        let details = bus::Props::from_dict(&identity.try_child_value(1)?);
        if let Some(uid) = details.u32("uid")
            && !uids.contains(&uid)
        {
            uids.push(uid);
        }
    }
    if let Some(i) = uids.iter().position(|&u| u == me) {
        uids.swap(0, i);
    }
    if uids.is_empty() {
        tracing::warn!(action = action_id, "no unix-user identity to authenticate");
        return None;
    }
    Some(Request {
        action_id,
        message,
        cookie,
        uids,
        invocation,
    })
}

fn register(conn: &gio::DBusConnection) {
    let Some(session) = login::session_id(conn) else {
        tracing::warn!("no login session; not registering the polkit agent");
        return;
    };
    let mut details: HashMap<String, glib::Variant> = HashMap::new();
    details.insert("session-id".into(), session.to_variant());
    let subject = ("unix-session", details).to_variant();
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".into());
    let args =
        glib::Variant::tuple_from_iter([subject, locale.to_variant(), AGENT_PATH.to_variant()]);
    conn.call(
        Some(POLKIT),
        AUTHORITY_PATH,
        AUTHORITY,
        "RegisterAuthenticationAgent",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
        move |r| match r {
            Ok(_) => tracing::info!(session, "registered as the polkit authentication agent"),
            Err(e) => tracing::warn!(session, error = %e, "could not register the polkit agent"),
        },
    );
}
