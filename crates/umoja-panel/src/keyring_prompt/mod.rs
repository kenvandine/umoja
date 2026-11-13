//! The keyring's system prompter, built in like gnome-shell's.
//!
//! When gnome-keyring needs a password from the user ("Unlock Keyring",
//! "Choose password for new keyring", "Store passwords unencrypted?"), it
//! does not draw a window itself: it asks whoever owns
//! `org.gnome.keyring.SystemPrompter` on the session bus. In GNOME that is
//! gnome-shell (Gcr.SystemPrompter + keyringPrompt.js). Ubuntu's gcr4 has
//! no stand-alone prompter, so under Umoja nobody answered and every such
//! prompt failed: a login keyring left locked (fingerprint login, autologin,
//! a changed password) could never be unlocked, and apps could not create
//! a keyring.
//!
//! The panel now owns that name and speaks gcr's prompter protocol
//! (`org.gnome.keyring.internal.Prompter`, gcr-system-prompter.c) one
//! caller at a time, as gnome-shell does (GCR_SYSTEM_PROMPTER_SINGLE):
//!
//! 1. the caller exports a callback object and calls `BeginPrompting`;
//! 2. when no other caller is being served, we call its `PromptReady("")`
//!    with our half of the secret exchange;
//! 3. it calls `PerformPrompt(type, properties, exchange)`; we show the
//!    dialog and answer with `PromptReady("yes"/"no", ..., exchange)`, the
//!    password encrypted in the exchange (repeat for a wrong password);
//! 4. it calls `StopPrompting`, and we close the dialog and call
//!    `PromptDone`.

mod dialog;
mod ffi;

use dialog::{Dialog, Kind, Reply};
use gio::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

const BUS_NAME: &str = "org.gnome.keyring.SystemPrompter";
const PATH: &str = "/org/gnome/keyring/Prompter";
const IFACE: &str = "org.gnome.keyring.internal.Prompter";
const CALLBACK_IFACE: &str = "org.gnome.keyring.internal.Prompter.Callback";
const FAILED: &str = "org.freedesktop.DBus.Error.Failed";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const XML: &str = r#"<node>
  <interface name="org.gnome.keyring.internal.Prompter">
    <method name="BeginPrompting">
      <arg name="callback" type="o" direction="in"/>
    </method>
    <method name="PerformPrompt">
      <arg name="callback" type="o" direction="in"/>
      <arg name="type" type="s" direction="in"/>
      <arg name="properties" type="a{sv}" direction="in"/>
      <arg name="exchange" type="s" direction="in"/>
    </method>
    <method name="StopPrompting">
      <arg name="callback" type="o" direction="in"/>
    </method>
  </interface>
</node>"#;

/// The GcrPrompt properties a caller sets (only changed ones are sent, so
/// they accumulate over a caller's prompts).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Props {
    pub title: String,
    pub message: String,
    pub description: String,
    pub warning: String,
    pub password_new: bool,
    pub choice_label: String,
    pub choice_chosen: bool,
    pub continue_label: String,
    pub cancel_label: String,
}

impl Props {
    /// Apply an `a{sv}` of changed properties. Names are GObject property
    /// names, so `choice_chosen` means `choice-chosen`.
    fn update(&mut self, changed: &glib::Variant) {
        for entry in changed.iter() {
            let Some(name) = entry.child_value(0).str().map(|n| n.replace('_', "-")) else {
                continue;
            };
            let value = entry.child_value(1);
            let value = value.as_variant().unwrap_or(value);
            let text = || value.str().unwrap_or_default().to_string();
            let flag = || value.get::<bool>().unwrap_or(false);
            match name.as_str() {
                "title" => self.title = text(),
                "message" => self.message = text(),
                "description" => self.description = text(),
                "warning" => self.warning = text(),
                "password-new" => self.password_new = flag(),
                "choice-label" => self.choice_label = text(),
                "choice-chosen" => self.choice_chosen = flag(),
                "continue-label" => self.continue_label = text(),
                "cancel-label" => self.cancel_label = text(),
                // caller-window: an X11 id we can't parent to; password-strength
                // is ours to report.
                _ => {}
            }
        }
    }
}

/// A prompting client: its unique bus name and callback object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Caller {
    name: String,
    path: String,
}

/// The caller being served.
struct Active {
    caller: Caller,
    exchange: ffi::Exchange,
    /// We have the caller's half of the exchange.
    received: bool,
    /// We told the caller to go ahead; a PerformPrompt is welcome.
    ready: bool,
    props: Props,
    dialog: Option<Rc<Dialog>>,
    cancellable: gio::Cancellable,
}

/// Ends a caller's name watch (gio doesn't export the watch's id type).
type BusWatch = Box<dyn FnOnce()>;

struct Prompter {
    conn: gio::DBusConnection,
    /// Every caller that began prompting, with its name watch.
    callers: RefCell<HashMap<Caller, BusWatch>>,
    waiting: RefCell<VecDeque<Caller>>,
    active: RefCell<Option<Active>>,
    /// Bumped when a caller stops, so its late dialog replies are ignored.
    generation: Cell<u64>,
}

impl Prompter {
    fn new(conn: &gio::DBusConnection) -> Rc<Self> {
        Rc::new(Self {
            conn: conn.clone(),
            callers: RefCell::default(),
            waiting: RefCell::default(),
            active: RefCell::default(),
            generation: Cell::new(0),
        })
    }

    fn begin(self: &Rc<Self>, caller: Caller) {
        tracing::info!(caller = caller.name, "keyring prompt requested");
        let weak = Rc::downgrade(self);
        let watch = gio::bus_watch_name_on_connection(
            &self.conn,
            &caller.name,
            gio::BusNameWatcherFlags::NONE,
            |_, _, _| {},
            move |_, name| {
                if let Some(p) = weak.upgrade() {
                    p.vanished(name);
                }
            },
        );
        self.callers.borrow_mut().insert(
            caller.clone(),
            Box::new(move || gio::bus_unwatch_name(watch)),
        );
        self.waiting.borrow_mut().push_back(caller);
        self.next();
    }

    /// Serve the next waiting caller, if nobody is being served.
    fn next(self: &Rc<Self>) {
        if self.active.borrow().is_some() {
            return;
        }
        let Some(caller) = self.waiting.borrow_mut().pop_front() else {
            return;
        };
        *self.active.borrow_mut() = Some(Active {
            caller,
            exchange: ffi::Exchange::new(),
            received: false,
            ready: false,
            props: Props::default(),
            dialog: None,
            cancellable: gio::Cancellable::new(),
        });
        self.send_ready("", None);
    }

    /// `PromptReady(reply, properties, exchange)` to the active caller.
    fn send_ready(self: &Rc<Self>, reply: &str, secret: Option<&str>) {
        let (caller, args, cancellable) = {
            let mut active = self.active.borrow_mut();
            let Some(a) = active.as_mut() else { return };
            let exchange = if a.received {
                a.exchange.send(secret)
            } else {
                a.exchange.begin()
            };
            // What the user changed: the remember choice and how strong a
            // new password is, as gnome-shell's prompt notifies them.
            // gcr's prompter double-wraps these values (a `v` holding a
            // `v`, prompt_build_properties) and its client unwraps twice.
            let wrap = |v: glib::Variant| glib::Variant::from_variant(&v);
            let mut changed: HashMap<String, glib::Variant> = HashMap::new();
            if let Some(d) = &a.dialog {
                if let Some(chosen) = d.choice_chosen() {
                    a.props.choice_chosen = chosen;
                    changed.insert("choice-chosen".into(), wrap(chosen.to_variant()));
                }
                if let Some(s) = d.password_strength() {
                    changed.insert("password-strength".into(), wrap(s.to_variant()));
                }
            }
            a.ready = true;
            let args = (reply, changed, exchange).to_variant();
            (a.caller.clone(), args, a.cancellable.clone())
        };
        let weak = Rc::downgrade(self);
        let (name, path) = (caller.name.clone(), caller.path.clone());
        self.conn.call(
            Some(&name),
            &path,
            CALLBACK_IFACE,
            "PromptReady",
            Some(&args),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            -1,
            Some(&cancellable),
            move |r| {
                let Err(e) = r else { return };
                if e.matches(gio::IOErrorEnum::Cancelled) {
                    return;
                }
                tracing::warn!(caller = caller.name, error = %e, "keyring prompt callback failed");
                if let Some(p) = weak.upgrade() {
                    p.stop(&caller, false);
                    p.next();
                }
            },
        );
    }

    fn perform(
        self: &Rc<Self>,
        caller: &Caller,
        kind: &str,
        changed: &glib::Variant,
        exchange: &str,
    ) -> Result<(), (&'static str, &'static str)> {
        let kind = match kind {
            "password" => Kind::Password,
            "confirm" => Kind::Confirm,
            _ => return Err((INVALID_ARGS, "Invalid type argument")),
        };
        let (dialog, props) = {
            let mut active = self.active.borrow_mut();
            let a = match active.as_mut() {
                Some(a) if a.caller == *caller => a,
                _ => return Err((FAILED, "Not begun prompting for this prompt callback")),
            };
            if !a.ready {
                return Err((
                    FAILED,
                    "Already performing a prompt for this prompt callback",
                ));
            }
            a.props.update(changed);
            if !a.exchange.receive(exchange) {
                return Err((INVALID_ARGS, "Invalid secret exchange received"));
            }
            a.received = true;
            a.ready = false;
            if a.dialog.is_none() {
                let weak = Rc::downgrade(self);
                let generation = self.generation.get();
                a.dialog = Some(Dialog::new(move |reply| {
                    let Some(p) = weak.upgrade() else { return };
                    if p.generation.get() != generation {
                        return;
                    }
                    // Not from inside the dialog's own signal handlers.
                    glib::idle_add_local_full(glib::Priority::DEFAULT, {
                        let mut reply = Some(reply);
                        move || {
                            if let Some(r) = reply.take() {
                                p.answered(r);
                            }
                            glib::ControlFlow::Break
                        }
                    });
                }));
            }
            (a.dialog.clone(), a.props.clone())
        };
        if let Some(d) = dialog {
            d.prompt(kind, &props);
        }
        Ok(())
    }

    fn answered(self: &Rc<Self>, reply: Reply) {
        match reply {
            Reply::Continue(secret) => self.send_ready("yes", secret.as_deref()),
            Reply::Cancel => self.send_ready("no", None),
        }
    }

    /// Forget a caller; close its dialog and, if asked, tell it we're done.
    fn stop(self: &Rc<Self>, caller: &Caller, send_done: bool) {
        let Some(watch) = self.callers.borrow_mut().remove(caller) else {
            return;
        };
        watch();
        self.waiting.borrow_mut().retain(|c| c != caller);
        let active = {
            let mut active = self.active.borrow_mut();
            if active.as_ref().is_some_and(|a| a.caller == *caller) {
                active.take()
            } else {
                None
            }
        };
        if let Some(a) = active {
            self.generation.set(self.generation.get() + 1);
            a.cancellable.cancel();
            if let Some(d) = a.dialog {
                d.close();
            }
        }
        tracing::info!(caller = caller.name, "keyring prompt finished");
        if send_done {
            self.conn.call(
                Some(&caller.name),
                &caller.path,
                CALLBACK_IFACE,
                "PromptDone",
                None,
                None,
                gio::DBusCallFlags::NO_AUTO_START,
                -1,
                gio::Cancellable::NONE,
                |_| {},
            );
        }
    }

    fn vanished(self: &Rc<Self>, name: &str) {
        let gone: Vec<Caller> = self
            .callers
            .borrow()
            .keys()
            .filter(|c| c.name == name)
            .cloned()
            .collect();
        for caller in gone {
            self.stop(&caller, false);
        }
        self.next();
    }
}

/// Own the keyring's prompter name for the life of the panel.
pub fn start() {
    let on_bus = |conn: gio::DBusConnection, _: &str| {
        if let Err(e) = export(&conn, Prompter::new(&conn)) {
            tracing::warn!(error = %e, "could not export the keyring prompter");
        }
    };
    // Owned for the life of the panel (dropping the id doesn't release it).
    let _id = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        on_bus,
        |_, _| tracing::info!("keyring system prompter ready"),
        |_, name| tracing::warn!(name, "lost the keyring prompter name"),
    );
}

fn export(conn: &gio::DBusConnection, prompter: Rc<Prompter>) -> Result<(), glib::Error> {
    let info = gio::DBusNodeInfo::for_xml(XML)?;
    let iface = info
        .lookup_interface(IFACE)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no interface"))?;
    let _id =
        conn.register_object(PATH, &iface)
            .method_call(move |_, sender, _, _, method, params, invocation| {
                let Some(name) = sender.map(String::from) else {
                    invocation.return_dbus_error(FAILED, "no sender");
                    return;
                };
                let Some(path) = params
                    .try_child_value(0)
                    .and_then(|p| p.str().map(String::from))
                else {
                    invocation.return_dbus_error(INVALID_ARGS, "no callback path");
                    return;
                };
                let caller = Caller { name, path };
                match method {
                    "BeginPrompting" => {
                        if prompter.callers.borrow().contains_key(&caller) {
                            invocation.return_dbus_error(
                                FAILED,
                                "Already begun prompting for this prompt callback",
                            );
                            return;
                        }
                        // Reply first: the caller isn't ready for
                        // PromptReady until its BeginPrompting returns.
                        invocation.return_value(None);
                        prompter.begin(caller);
                    }
                    "PerformPrompt" => {
                        let kind = params.child_value(1);
                        let exchange = params.child_value(3);
                        match prompter.perform(
                            &caller,
                            kind.str().unwrap_or_default(),
                            &params.child_value(2),
                            exchange.str().unwrap_or_default(),
                        ) {
                            Ok(()) => invocation.return_value(None),
                            Err((name, e)) => invocation.return_dbus_error(name, e),
                        }
                    }
                    "StopPrompting" => {
                        invocation.return_value(None);
                        prompter.stop(&caller, true);
                        prompter.next();
                    }
                    _ => invocation
                        .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
                }
            })
            .build()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_accumulate_and_accept_underscores() {
        let mut p = Props::default();
        let first: HashMap<String, glib::Variant> = [
            (
                "message".to_string(),
                "Authentication required".to_variant(),
            ),
            ("choice_label".to_string(), "Remember".to_variant()),
            ("password-new".to_string(), true.to_variant()),
        ]
        .into_iter()
        .collect();
        p.update(&first.to_variant());
        let second: HashMap<String, glib::Variant> = [(
            "warning".to_string(),
            "The unlock password was incorrect".to_variant(),
        )]
        .into_iter()
        .collect();
        p.update(&second.to_variant());
        assert_eq!(p.message, "Authentication required");
        assert_eq!(p.choice_label, "Remember");
        assert!(p.password_new);
        assert_eq!(p.warning, "The unlock password was incorrect");
    }

    /// gcr's own client (python3-gi, gir1.2-gcr-4) against our prompter:
    /// a password with the remember choice, a mismatched then matching new
    /// password, a confirm, then close. Needs a session bus and a display:
    /// `dbus-run-session -- xvfb-run cargo test -p umoja-panel live_keyring -- --ignored`
    #[test]
    #[ignore]
    fn live_keyring_prompt_with_gcr_client() {
        gtk4::init().expect("a display");
        let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let prompter = Prompter::new(&conn);
        export(&conn, prompter.clone()).unwrap();
        let owned = Rc::new(Cell::new(false));
        let _id = gio::bus_own_name_on_connection(
            &conn,
            BUS_NAME,
            gio::BusNameOwnerFlags::NONE,
            {
                let owned = owned.clone();
                move |_, _| owned.set(true)
            },
            |_, _| panic!("name lost"),
        );
        let ctx = glib::MainContext::default();
        while !owned.get() {
            ctx.iteration(true);
        }

        let script = r#"
import gi
gi.require_version('Gcr', '4')
from gi.repository import Gcr
p = Gcr.SystemPrompt.open_for_prompter("org.gnome.keyring.SystemPrompter", 10, None)
p.set_message("Authentication required")
p.set_choice_label("Automatically unlock this keyring whenever I'm logged in")
p.set_continue_label("Unlock")
print("1", p.password(None), p.get_choice_chosen(), flush=True)
p.set_choice_label("")
p.set_password_new(True)
print("2", p.password(None), p.get_password_strength() > 0, flush=True)
p.set_password_new(False)
print("3", p.confirm(None) == Gcr.PromptReply.CONTINUE, flush=True)
p.close(None)
print("done", flush=True)
"#;
        let child = gio::Subprocess::newv(
            &[
                std::ffi::OsStr::new("/usr/bin/python3.12"),
                "-c".as_ref(),
                script.as_ref(),
            ],
            gio::SubprocessFlags::STDOUT_PIPE,
        )
        .unwrap();
        let out = Rc::new(RefCell::new(None));
        child.communicate_utf8_async(None, gio::Cancellable::NONE, {
            let out = out.clone();
            move |r| *out.borrow_mut() = Some(r.map(|(o, _)| o.unwrap_or_default().to_string()))
        });

        // Answer each prompt as it appears: the password; a new password
        // whose "type again" differs (must be refused), then matching; the
        // confirm.
        let answers = [
            ("hunter2", ""),
            ("s3cret!", "typo"),
            ("s3cret!", "s3cret!"),
            ("", ""),
        ];
        let mut next = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while out.borrow().is_none() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
            let dialog = prompter
                .active
                .borrow()
                .as_ref()
                .and_then(|a| a.dialog.clone());
            if let Some(d) = dialog
                && next < answers.len()
                && d.type_and_continue(answers[next].0, answers[next].1)
            {
                if next == 1 {
                    assert_eq!(d.warning_text(), i18n_mismatch());
                }
                next += 1;
            }
        }
        let stdout = out.borrow_mut().take().unwrap().unwrap();
        assert_eq!(stdout, "1 hunter2 True\n2 s3cret! True\n3 True\ndone\n");
        assert!(prompter.active.borrow().is_none());
        assert!(prompter.callers.borrow().is_empty());
    }

    fn i18n_mismatch() -> String {
        umoja_common::i18n::shell("Passwords do not match.")
    }
}
