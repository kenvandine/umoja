//! The two bits of libpolkit-agent-1 we need: a `PolkitAgentSession`, which
//! runs polkit's setuid helper (or talks to its socket) to check a password
//! with PAM and report the result to polkitd, and `PolkitUnixUser` to name
//! the identity it authenticates. There are no Rust bindings, but both are
//! plain GObjects, so glib-rs can hold them and connect to their signals
//! by name.

use gtk4::glib::{self, gobject_ffi, prelude::*, translate::*};
use std::ffi::{CString, c_char, c_int};

#[link(name = "polkit-gobject-1")]
unsafe extern "C" {
    fn polkit_unix_user_new(uid: c_int) -> *mut gobject_ffi::GObject;
}

#[link(name = "polkit-agent-1")]
unsafe extern "C" {
    fn polkit_agent_session_new(
        identity: *mut gobject_ffi::GObject,
        cookie: *const c_char,
    ) -> *mut gobject_ffi::GObject;
    fn polkit_agent_session_initiate(session: *mut gobject_ffi::GObject);
    fn polkit_agent_session_response(session: *mut gobject_ffi::GObject, response: *const c_char);
    fn polkit_agent_session_cancel(session: *mut gobject_ffi::GObject);
}

/// What the session asks of, or tells, the dialog.
#[derive(Debug, Clone)]
pub enum Event {
    /// PAM wants input: the prompt ("Password: ") and whether to echo it.
    Request(String, bool),
    Error(String),
    Info(String),
    /// Finished; true if the user gained the authorization.
    Completed(bool),
}

pub struct Session {
    obj: glib::Object,
}

impl Session {
    /// A session authenticating `uid` for polkit's request `cookie`.
    pub fn new(uid: u32, cookie: &str, on_event: impl Fn(Event) + 'static) -> Option<Self> {
        let cookie = CString::new(cookie).ok()?;
        let obj: glib::Object = unsafe {
            let identity: glib::Object = from_glib_full(polkit_unix_user_new(uid as c_int));
            let raw = polkit_agent_session_new(identity.as_ptr(), cookie.as_ptr());
            if raw.is_null() {
                return None;
            }
            from_glib_full(raw)
        };
        let on_event = std::rc::Rc::new(on_event);
        let str_at = |v: &[glib::Value], i: usize| {
            v.get(i)
                .and_then(|v| v.get::<Option<String>>().ok().flatten())
                .unwrap_or_default()
        };
        let f = on_event.clone();
        obj.connect_local("request", false, move |v| {
            let echo = v.get(2).and_then(|v| v.get::<bool>().ok()).unwrap_or(false);
            f(Event::Request(str_at(v, 1), echo));
            None
        });
        let f = on_event.clone();
        obj.connect_local("show-error", false, move |v| {
            f(Event::Error(str_at(v, 1)));
            None
        });
        let f = on_event.clone();
        obj.connect_local("show-info", false, move |v| {
            f(Event::Info(str_at(v, 1)));
            None
        });
        let f = on_event;
        obj.connect_local("completed", false, move |v| {
            let ok = v.get(1).and_then(|v| v.get::<bool>().ok()).unwrap_or(false);
            f(Event::Completed(ok));
            None
        });
        Some(Self { obj })
    }

    pub fn initiate(&self) {
        unsafe { polkit_agent_session_initiate(self.obj.as_ptr()) }
    }

    pub fn respond(&self, text: &str) {
        // A password with a NUL in it can't be passed on; send it empty so
        // PAM rejects it rather than hanging the prompt.
        let text = CString::new(text).unwrap_or_default();
        unsafe { polkit_agent_session_response(self.obj.as_ptr(), text.as_ptr()) }
    }

    pub fn cancel(&self) {
        unsafe { polkit_agent_session_cancel(self.obj.as_ptr()) }
    }
}
