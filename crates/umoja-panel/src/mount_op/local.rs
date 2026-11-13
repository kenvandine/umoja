//! gnome-shell's ShellMountOperation: a `GMountOperation` for mounts and
//! ejects the shell starts itself, answered with our mount dialogs, and
//! ShellUnmountNotifier's "don't unplug yet" notifications.

use super::{Dialog, Kind, Reply, apps_for_pids};
use gio::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Default)]
struct State {
    dialog: RefCell<Option<Rc<Dialog>>>,
    /// The last password the user gave was sent; asking again means it
    /// was wrong.
    answered: Cell<bool>,
    /// The "Writing data to …" notification.
    unmount_note: Cell<u32>,
}

#[derive(Clone)]
pub struct Operation {
    op: gio::MountOperation,
    state: Rc<State>,
}

impl Operation {
    pub fn new() -> Self {
        let op = gio::MountOperation::new();
        let state = Rc::new(State::default());

        op.connect_ask_password({
            let state = Rc::downgrade(&state);
            move |op, message, user, domain, flags| {
                let Some(state) = state.upgrade() else {
                    op.reply(gio::MountOperationResult::Unhandled);
                    return;
                };
                let respond = password_responder(op, &state);
                let existing = state.dialog.borrow().clone().filter(|d| !d.is_closed());
                match existing {
                    Some(d) if d.is_password() && state.answered.get() => d.reask(respond),
                    Some(d) => d.update(
                        message,
                        "",
                        Kind::Password {
                            flags,
                            user: user.into(),
                            domain: domain.into(),
                        },
                        respond,
                    ),
                    None => {
                        let d = Dialog::new(
                            message,
                            "",
                            Kind::Password {
                                flags,
                                user: user.into(),
                                domain: domain.into(),
                            },
                            respond,
                        );
                        *state.dialog.borrow_mut() = Some(d);
                    }
                }
            }
        });

        // gio-rs doesn't bind these two (GStrv / GArray arguments).
        op.connect_local("ask-question", false, {
            let state = Rc::downgrade(&state);
            move |values| {
                let op = values[0].get::<gio::MountOperation>().ok()?;
                let message = values[1].get::<String>().unwrap_or_default();
                let choices = values[2].get::<Vec<String>>().unwrap_or_default();
                let state = state.upgrade()?;
                show(&op, &state, &message, Kind::Question { choices });
                None
            }
        });
        op.connect_local("show-processes", false, {
            let state = Rc::downgrade(&state);
            move |values| {
                let op = values[0].get::<gio::MountOperation>().ok()?;
                let message = values[1].get::<String>().unwrap_or_default();
                let pids = pid_array(&values[2]);
                let choices = values[3].get::<Vec<String>>().unwrap_or_default();
                let state = state.upgrade()?;
                let apps = apps_for_pids(&pids);
                show(&op, &state, &message, Kind::Processes { apps, choices });
                None
            }
        });

        op.connect_aborted({
            let state = Rc::downgrade(&state);
            move |_| {
                if let Some(s) = state.upgrade()
                    && let Some(d) = s.dialog.borrow_mut().take()
                {
                    d.close();
                }
            }
        });

        op.connect_show_unmount_progress({
            let state = Rc::downgrade(&state);
            move |_, message, _time_left, bytes_left| {
                let Some(state) = state.upgrade() else { return };
                let Some(center) = crate::notification_center() else {
                    return;
                };
                let old = state.unmount_note.replace(0);
                if old != 0 {
                    center.close(old, crate::notifications::CloseReason::Closed);
                }
                // gvfs sends "header\nbody"; bytes_left 0 means it's done.
                let mut lines = message.splitn(2, '\n');
                let header = lines.next().unwrap_or("");
                let body = lines.next().unwrap_or("");
                if bytes_left > 0 {
                    let id = center.notify_with_actions(
                        header,
                        body,
                        "media-removable-symbolic",
                        &[],
                        |_| {},
                    );
                    state.unmount_note.set(id);
                } else if !header.is_empty() {
                    center.notify_internal(header, body, "media-removable-symbolic");
                }
            }
        });

        Self { op, state }
    }

    pub fn mount_op(&self) -> &gio::MountOperation {
        &self.op
    }

    /// The operation is over: take the card down.
    pub fn close(&self) {
        self.state.answered.set(false);
        if let Some(d) = self.state.dialog.borrow_mut().take() {
            d.close();
        }
    }

    /// Whether a passphrase was given (so a failure means it was wrong).
    pub fn answered_password(&self) -> bool {
        self.state.answered.get()
    }
}

fn password_responder(op: &gio::MountOperation, state: &Rc<State>) -> impl Fn(Reply) + 'static {
    let op = op.downgrade();
    let state = Rc::downgrade(state);
    move |reply| {
        let (Some(op), Some(state)) = (op.upgrade(), state.upgrade()) else {
            return;
        };
        match reply {
            Reply::Password(p) => {
                state.answered.set(true);
                op.set_password(Some(&p.password));
                op.set_password_save(if p.remember {
                    gio::PasswordSave::Permanently
                } else {
                    gio::PasswordSave::Never
                });
                if !p.username.is_empty() {
                    op.set_username(Some(&p.username));
                }
                if !p.domain.is_empty() {
                    op.set_domain(Some(&p.domain));
                }
                op.set_anonymous(p.anonymous);
                op.set_is_tcrypt_hidden_volume(p.hidden_volume);
                op.set_is_tcrypt_system_volume(p.system_volume);
                op.set_pim(p.pim);
                op.reply(gio::MountOperationResult::Handled);
            }
            Reply::Choice(i) => {
                op.set_choice(i);
                op.reply(gio::MountOperationResult::Handled);
            }
            Reply::Abort => {
                state.answered.set(false);
                state.dialog.borrow_mut().take();
                op.reply(gio::MountOperationResult::Aborted);
            }
        }
    }
}

fn show(op: &gio::MountOperation, state: &Rc<State>, message: &str, kind: Kind) {
    let respond = password_responder(op, state);
    let existing = state.dialog.borrow().clone().filter(|d| !d.is_closed());
    match existing {
        Some(d) => d.update(message, "", kind, respond),
        None => {
            *state.dialog.borrow_mut() = Some(Dialog::new(message, "", kind, respond));
        }
    }
}

/// The `GArray` of `GPid` that "show-processes" passes.
fn pid_array(value: &glib::Value) -> Vec<i32> {
    use glib::translate::ToGlibPtr;
    // SAFETY: the signal's second argument is a boxed GArray of GPid
    // (gint); it lives for the duration of the emission.
    unsafe {
        let ptr = glib::gobject_ffi::g_value_get_boxed(value.to_glib_none().0)
            as *const glib::ffi::GArray;
        if ptr.is_null() || (*ptr).data.is_null() {
            return vec![];
        }
        std::slice::from_raw_parts((*ptr).data as *const i32, (*ptr).len as usize).to_vec()
    }
}
