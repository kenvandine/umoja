//! Mount prompts: the passphrase of an encrypted drive, the login of a
//! network share, and "this drive is busy" while apps still use it.
//!
//! In GNOME, gnome-shell asks these for every app: GTK's GtkMountOperation
//! (Files, file choosers, anything mounting through GIO) hands its
//! questions to the session's `org.gtk.MountOperationHandler`, and
//! gnome-shell owns that name (js/ui/shellMountOperation.js,
//! GnomeShellMountOpHandler). Without it, GTK 4 apps fall back to their
//! own plain dialogs, and some (the busy-drive question) to none. Our
//! handler is a port, drawn as the PolicyKit dialog's blurred dark card.
//!
//! [`local::Operation`] is gnome-shell's ShellMountOperation: the same
//! dialogs for mounts the shell starts itself (automounting a drive that
//! was plugged in, see [`crate::automount`]), plus the "don't unplug yet"
//! notifications while a drive is ejected.

mod dialog;
pub mod local;

pub use dialog::{App, Dialog, Kind, Password, Reply};

use gio::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const NAME: &str = "org.gtk.MountOperationHandler";
const PATH: &str = "/org/gtk/MountOperationHandler";

const XML: &str = r#"<node>
  <interface name="org.gtk.MountOperationHandler">
    <method name="AskPassword">
      <arg type="s" direction="in" name="object_id"/>
      <arg type="s" direction="in" name="message"/>
      <arg type="s" direction="in" name="icon_name"/>
      <arg type="s" direction="in" name="default_user"/>
      <arg type="s" direction="in" name="default_domain"/>
      <arg type="u" direction="in" name="flags"/>
      <arg type="u" direction="out" name="response"/>
      <arg type="a{sv}" direction="out" name="response_details"/>
    </method>
    <method name="AskQuestion">
      <arg type="s" direction="in" name="object_id"/>
      <arg type="s" direction="in" name="message"/>
      <arg type="s" direction="in" name="icon_name"/>
      <arg type="as" direction="in" name="choices"/>
      <arg type="u" direction="out" name="response"/>
      <arg type="a{sv}" direction="out" name="response_details"/>
    </method>
    <method name="ShowProcesses">
      <arg type="s" direction="in" name="object_id"/>
      <arg type="s" direction="in" name="message"/>
      <arg type="s" direction="in" name="icon_name"/>
      <arg type="ai" direction="in" name="application_pids"/>
      <arg type="as" direction="in" name="choices"/>
      <arg type="u" direction="out" name="response"/>
      <arg type="a{sv}" direction="out" name="response_details"/>
    </method>
    <method name="Close"/>
  </interface>
</node>"#;

// GMountOperationResult
const HANDLED: u32 = 0;
const ABORTED: u32 = 1;
const UNHANDLED: u32 = 2;

/// How long a card waits, after the user answered, for the app to tell us
/// how it went (Close, or the same question again) before going away.
const ANSWERED_TIMEOUT_SECS: u32 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Type {
    Password,
    Question,
    Processes,
}

/// The request being shown.
struct Current {
    /// object_id@sender, as gnome-shell keys requests.
    id: String,
    kind: Type,
    invocation: Option<gio::DBusMethodInvocation>,
    /// The user gave a password for it, so the same question again means
    /// it was wrong.
    answered: bool,
    sender_watch: Option<Unwatch>,
}

/// Stops watching the requesting app's bus name.
type Unwatch = Box<dyn FnOnce()>;

#[derive(Default)]
struct Handler {
    current: RefCell<Option<Current>>,
    dialog: RefCell<Option<Rc<Dialog>>>,
    /// Bumped per request, so a stale timeout leaves a newer card alone.
    generation: Cell<u32>,
}

thread_local! {
    static HANDLER: Rc<Handler> = Rc::new(Handler::default());
}

/// Own `org.gtk.MountOperationHandler` on the session bus, taking it over
/// from a previous panel.
pub fn start() {
    let id = gio::bus_own_name(
        gio::BusType::Session,
        NAME,
        gio::BusNameOwnerFlags::ALLOW_REPLACEMENT | gio::BusNameOwnerFlags::REPLACE,
        |conn, _| {
            if let Err(e) = export(&conn) {
                tracing::warn!(error = %e, "could not export the mount operation handler");
            }
        },
        |_, _| tracing::info!("owning {NAME}"),
        |_, _| tracing::warn!("lost {NAME}; apps will ask for mount passwords themselves"),
    );
    // Owned for the panel's lifetime (dropping the id does not release it).
    let _ = id;
}

fn export(conn: &gio::DBusConnection) -> Result<(), glib::Error> {
    let info = gio::DBusNodeInfo::for_xml(XML)?;
    let iface = info
        .lookup_interface(NAME)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no interface"))?;
    let _id =
        conn.register_object(PATH, &iface)
            .method_call(|conn, sender, _, _, method, params, invocation| {
                let h = HANDLER.with(Rc::clone);
                let sender = sender.unwrap_or_default().to_string();
                let s = |i: usize| params.child_value(i).str().unwrap_or_default().to_string();
                match method {
                    "AskPassword" => {
                        let flags = params.child_value(5).get::<u32>().unwrap_or(0);
                        h.ask_password(
                            &conn,
                            invocation,
                            &format!("{}@{sender}", s(0)),
                            &sender,
                            &s(1),
                            &s(2),
                            &s(3),
                            &s(4),
                            gio::AskPasswordFlags::from_bits_truncate(flags),
                        );
                    }
                    "AskQuestion" => {
                        let choices = params
                            .child_value(3)
                            .get::<Vec<String>>()
                            .unwrap_or_default();
                        h.ask_question(
                            &conn,
                            invocation,
                            &format!("{}@{sender}", s(0)),
                            &sender,
                            &s(1),
                            &s(2),
                            choices,
                        );
                    }
                    "ShowProcesses" => {
                        let pids = params.child_value(3).get::<Vec<i32>>().unwrap_or_default();
                        let choices = params
                            .child_value(4)
                            .get::<Vec<String>>()
                            .unwrap_or_default();
                        h.show_processes(
                            &conn,
                            invocation,
                            &format!("{}@{sender}", s(0)),
                            &sender,
                            &s(1),
                            &s(2),
                            &pids,
                            choices,
                        );
                    }
                    "Close" => {
                        h.clear(UNHANDLED, None);
                        h.close_dialog();
                        invocation.return_value(None);
                    }
                    _ => invocation
                        .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
                }
            })
            .build()?;
    Ok(())
}

/// The `(ua{sv})` reply. Built by hand: a Variant inside a Rust tuple
/// would be serialised as `v`.
fn reply_value(response: u32, details: Option<glib::Variant>) -> glib::Variant {
    let details = details.unwrap_or_else(|| glib::VariantDict::new(None).end());
    glib::Variant::tuple_from_iter([response.to_variant(), details])
}

fn password_details(p: &Password) -> glib::Variant {
    let d = glib::VariantDict::new(None);
    let save = if p.remember {
        gio::PasswordSave::Permanently
    } else {
        gio::PasswordSave::Never
    };
    d.insert_value("password", &p.password.to_variant());
    d.insert_value("password_save", &save.into_glib_u32().to_variant());
    d.insert_value("hidden_volume", &p.hidden_volume.to_variant());
    d.insert_value("system_volume", &p.system_volume.to_variant());
    d.insert_value("pim", &p.pim.to_variant());
    d.end()
}

fn choice_details(choice: i32) -> glib::Variant {
    let d = glib::VariantDict::new(None);
    d.insert_value("choice", &choice.to_variant());
    d.end()
}

trait PasswordSaveExt {
    fn into_glib_u32(self) -> u32;
}

impl PasswordSaveExt for gio::PasswordSave {
    fn into_glib_u32(self) -> u32 {
        match self {
            gio::PasswordSave::Never => 0,
            gio::PasswordSave::ForSession => 1,
            _ => 2,
        }
    }
}

impl Handler {
    /// Make this the current request, answering the one it replaces with
    /// UNHANDLED like gnome-shell. Returns (same card can be reused,
    /// a password was already given for it).
    fn set_current(
        self: &Rc<Self>,
        conn: &gio::DBusConnection,
        invocation: gio::DBusMethodInvocation,
        id: &str,
        sender: &str,
        kind: Type,
    ) -> (bool, bool) {
        let old = self.current.borrow_mut().take();
        let mut answered = false;
        let mut same = false;
        let mut watch = None;
        if let Some(mut old) = old {
            if let Some(inv) = old.invocation.take() {
                inv.return_value(Some(&reply_value(UNHANDLED, None)));
            }
            same = old.id == id && old.kind == kind;
            answered = same && old.answered;
            if same {
                watch = old.sender_watch.take();
            } else if let Some(w) = old.sender_watch.take() {
                w();
            }
        }
        // The app quit (or crashed) with the card up: take it down.
        let watch = watch.or_else(|| {
            let weak = Rc::downgrade(self);
            let id = id.to_string();
            let w = gio::bus_watch_name_on_connection(
                conn,
                sender,
                gio::BusNameWatcherFlags::NONE,
                |_, _, _| {},
                move |_, _| {
                    let Some(h) = weak.upgrade() else { return };
                    if h.current.borrow().as_ref().is_some_and(|c| c.id == id) {
                        h.clear(UNHANDLED, None);
                        h.close_dialog();
                    }
                },
            );
            Some(Box::new(move || gio::bus_unwatch_name(w)) as Unwatch)
        });
        *self.current.borrow_mut() = Some(Current {
            id: id.to_string(),
            kind,
            invocation: Some(invocation),
            answered: false,
            sender_watch: watch,
        });
        self.generation.set(self.generation.get().wrapping_add(1));
        let open = self
            .dialog
            .borrow()
            .as_ref()
            .is_some_and(|d| !d.is_closed());
        (same && open, answered)
    }

    /// Answer the current request (if it is still waiting).
    fn clear(&self, response: u32, details: Option<glib::Variant>) {
        let mut cur = self.current.borrow_mut();
        let Some(c) = cur.as_mut() else { return };
        if let Some(inv) = c.invocation.take() {
            inv.return_value(Some(&reply_value(response, details)));
        }
        if response != HANDLED {
            if let Some(w) = c.sender_watch.take() {
                w();
            }
            *cur = None;
        }
    }

    fn close_dialog(&self) {
        if let Some(d) = self.dialog.borrow_mut().take() {
            d.close();
        }
    }

    /// A reply handler for the request just made current.
    fn responder(self: &Rc<Self>) -> impl Fn(Reply) + 'static {
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        move |reply| {
            let Some(h) = weak.upgrade() else { return };
            if h.generation.get() != generation {
                return;
            }
            match reply {
                Reply::Abort => {
                    h.clear(ABORTED, None);
                    h.close_dialog();
                }
                Reply::Choice(i) => h.clear(HANDLED, Some(choice_details(i))),
                Reply::Password(p) => {
                    if let Some(c) = h.current.borrow_mut().as_mut() {
                        c.answered = true;
                    }
                    h.clear(HANDLED, Some(password_details(&p)));
                    // Wait for Close or the question again; don't leave
                    // the card spinning if neither ever comes.
                    let weak = Rc::downgrade(&h);
                    glib::timeout_add_seconds_local_once(ANSWERED_TIMEOUT_SECS, move || {
                        if let Some(h) = weak.upgrade()
                            && h.generation.get() == generation
                        {
                            h.close_dialog();
                        }
                    });
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_password(
        self: &Rc<Self>,
        conn: &gio::DBusConnection,
        invocation: gio::DBusMethodInvocation,
        id: &str,
        sender: &str,
        message: &str,
        icon: &str,
        user: &str,
        domain: &str,
        flags: gio::AskPasswordFlags,
    ) {
        tracing::info!(id, "mount password requested");
        let (reuse, answered) = self.set_current(conn, invocation, id, sender, Type::Password);
        let respond = self.responder();
        let kind = Kind::Password {
            flags,
            user: user.into(),
            domain: domain.into(),
        };
        if reuse {
            let d = self.dialog.borrow().clone();
            if let Some(d) = d {
                if answered && d.is_password() {
                    d.reask(respond);
                } else {
                    d.update(message, icon, kind, respond);
                }
                return;
            }
        }
        self.close_dialog();
        let d = Dialog::new(message, icon, kind, respond);
        if answered {
            // The card timed out before the app asked again.
            let respond = self.responder();
            d.reask(respond);
        }
        *self.dialog.borrow_mut() = Some(d);
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_question(
        self: &Rc<Self>,
        conn: &gio::DBusConnection,
        invocation: gio::DBusMethodInvocation,
        id: &str,
        sender: &str,
        message: &str,
        icon: &str,
        choices: Vec<String>,
    ) {
        tracing::info!(id, "mount question");
        let (reuse, _) = self.set_current(conn, invocation, id, sender, Type::Question);
        self.show(reuse, message, icon, Kind::Question { choices });
    }

    #[allow(clippy::too_many_arguments)]
    fn show_processes(
        self: &Rc<Self>,
        conn: &gio::DBusConnection,
        invocation: gio::DBusMethodInvocation,
        id: &str,
        sender: &str,
        message: &str,
        icon: &str,
        pids: &[i32],
        choices: Vec<String>,
    ) {
        tracing::info!(id, ?pids, "volume busy");
        let (reuse, _) = self.set_current(conn, invocation, id, sender, Type::Processes);
        let apps = apps_for_pids(pids);
        self.show(reuse, message, icon, Kind::Processes { apps, choices });
    }

    fn show(self: &Rc<Self>, reuse: bool, message: &str, icon: &str, kind: Kind) {
        let respond = self.responder();
        if reuse {
            let d = self.dialog.borrow().clone();
            if let Some(d) = d {
                d.update(message, icon, kind, respond);
                return;
            }
        }
        self.close_dialog();
        *self.dialog.borrow_mut() = Some(Dialog::new(message, icon, kind, respond));
    }
}

/// The apps behind `pids`, as gnome-shell's ShellProcessesDialog lists
/// them (it asks its window tracker; we read each process's cgroup, which
/// names the app's systemd scope, and fall back to its launch environment
/// and then its command name).
pub fn apps_for_pids(pids: &[i32]) -> Vec<App> {
    let mut seen = std::collections::HashSet::new();
    let mut apps = Vec::new();
    for &pid in pids {
        if pid <= 0 {
            continue;
        }
        let (key, app) = match app_info_for_pid(pid) {
            Some(info) => (
                info.id()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| info.name().to_string()),
                App {
                    name: info.name().to_string(),
                    icon: info.icon(),
                },
            ),
            None => {
                let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
                    continue;
                };
                let comm = comm.trim().to_string();
                (
                    comm.clone(),
                    App {
                        name: comm,
                        icon: None,
                    },
                )
            }
        };
        if seen.insert(key) {
            apps.push(app);
        }
    }
    apps
}

fn app_info_for_pid(pid: i32) -> Option<gio::DesktopAppInfo> {
    if let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        && let Some(unit) = cgroup
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .and_then(|p| p.rsplit('/').next())
    {
        for id in desktop_ids_for_unit(unit) {
            if let Some(info) = gio::DesktopAppInfo::new(&format!("{id}.desktop")) {
                return Some(info);
            }
        }
    }
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ
        .split(|&b| b == 0)
        .find_map(|kv| kv.strip_prefix(b"GIO_LAUNCHED_DESKTOP_FILE="))
        .and_then(|p| gio::DesktopAppInfo::from_filename(String::from_utf8_lossy(p).as_ref()))
}

/// Desktop ids a systemd unit may belong to, most likely first:
/// `app[-<launcher>]-<app id>[-<random>].scope` / `app-…@….service` (the
/// XDG convention; dashes in the id are escaped as `\x2d`), and snapd's
/// `snap.<snap>.<app>-<uuid>.scope`.
fn desktop_ids_for_unit(unit: &str) -> Vec<String> {
    let base = unit
        .strip_suffix(".scope")
        .or_else(|| unit.strip_suffix(".service"))
        .unwrap_or(unit);
    let base = base.split('@').next().unwrap_or(base);
    if let Some(rest) = base.strip_prefix("snap.") {
        let mut parts = rest.splitn(2, '.');
        let (Some(snap), Some(app)) = (parts.next(), parts.next()) else {
            return vec![];
        };
        // The app part ends in -<uuid> for scopes.
        let app = app.split('-').next().unwrap_or(app);
        return vec![format!("{snap}_{app}"), snap.to_string()];
    }
    let Some(rest) = base.strip_prefix("app-") else {
        return vec![];
    };
    rest.split('-')
        .map(unescape)
        .filter(|p| {
            !p.is_empty()
                && !p
                    .chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_hexdigit())
        })
        .collect()
}

/// Undo systemd's `\xNN` escaping.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        let hex = rest
            .get(i + 2..i + 4)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_to_desktop_ids() {
        assert_eq!(
            desktop_ids_for_unit("app-gnome-org.gnome.Nautilus-4242.scope"),
            vec!["gnome", "org.gnome.Nautilus"]
        );
        assert_eq!(
            desktop_ids_for_unit("app-umoja-org.gnome.Ptyxis-1234-7.scope"),
            vec!["umoja", "org.gnome.Ptyxis"]
        );
        assert_eq!(
            desktop_ids_for_unit("app-gnome-gnome\\x2dterminal-99.scope"),
            vec!["gnome", "gnome-terminal"]
        );
        assert_eq!(
            desktop_ids_for_unit("app-org.gnome.Nautilus@0a1b2c.service"),
            vec!["org.gnome.Nautilus"]
        );
        assert_eq!(
            desktop_ids_for_unit("snap.firefox.firefox-5f2e6c1a-1b2c-4d5e-8f90-123456789abc.scope"),
            vec!["firefox_firefox", "firefox"]
        );
        assert!(desktop_ids_for_unit("session-2.scope").is_empty());
    }

    #[test]
    fn reply_is_u_asv() {
        assert_eq!(
            reply_value(HANDLED, Some(choice_details(1)))
                .type_()
                .as_str(),
            "(ua{sv})"
        );
        let p = Password {
            password: "x".into(),
            ..Default::default()
        };
        let v = reply_value(HANDLED, Some(password_details(&p)));
        assert_eq!(v.type_().as_str(), "(ua{sv})");
        let d = glib::VariantDict::new(Some(&v.child_value(1)));
        assert_eq!(
            d.lookup::<String>("password").ok().flatten().as_deref(),
            Some("x")
        );
        assert_eq!(d.lookup::<u32>("password_save").ok().flatten(), Some(0));
    }
}
