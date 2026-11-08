//! Minimal safe wrapper around liblightdm-gobject-1.
//!
//! Only the handful of calls the greeter needs are bound by hand; the
//! greeter object is wrapped as a plain `glib::Object` so its signals can be
//! connected with `connect_local`.

use glib::prelude::*;
use glib::translate::*;
use std::ffi::{CStr, CString, c_char};
use std::ptr;

#[allow(non_camel_case_types)]
type gboolean = libc::c_int;
type GObjectPtr = *mut glib::gobject_ffi::GObject;
type GListPtr = *mut glib::ffi::GList;

#[link(name = "lightdm-gobject-1")]
unsafe extern "C" {
    fn lightdm_greeter_new() -> GObjectPtr;
    fn lightdm_greeter_connect_to_daemon_sync(
        g: GObjectPtr,
        err: *mut *mut glib::ffi::GError,
    ) -> gboolean;
    fn lightdm_greeter_get_default_session_hint(g: GObjectPtr) -> *const c_char;
    fn lightdm_greeter_get_select_user_hint(g: GObjectPtr) -> *const c_char;
    fn lightdm_greeter_authenticate(
        g: GObjectPtr,
        user: *const c_char,
        err: *mut *mut glib::ffi::GError,
    ) -> gboolean;
    fn lightdm_greeter_respond(
        g: GObjectPtr,
        response: *const c_char,
        err: *mut *mut glib::ffi::GError,
    ) -> gboolean;
    fn lightdm_greeter_cancel_authentication(
        g: GObjectPtr,
        err: *mut *mut glib::ffi::GError,
    ) -> gboolean;
    fn lightdm_greeter_get_is_authenticated(g: GObjectPtr) -> gboolean;
    fn lightdm_greeter_start_session_sync(
        g: GObjectPtr,
        session: *const c_char,
        err: *mut *mut glib::ffi::GError,
    ) -> gboolean;
    fn lightdm_get_sessions() -> GListPtr;
    fn lightdm_session_get_key(s: GObjectPtr) -> *const c_char;
    fn lightdm_session_get_name(s: GObjectPtr) -> *const c_char;
    fn lightdm_user_list_get_instance() -> GObjectPtr;
    fn lightdm_user_list_get_users(list: GObjectPtr) -> GListPtr;
    fn lightdm_user_get_name(u: GObjectPtr) -> *const c_char;
    fn lightdm_user_get_display_name(u: GObjectPtr) -> *const c_char;
    fn lightdm_user_get_image(u: GObjectPtr) -> *const c_char;
    fn lightdm_user_get_session(u: GObjectPtr) -> *const c_char;
    fn lightdm_user_get_logged_in(u: GObjectPtr) -> gboolean;
}

/// LightDMPromptType
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptType {
    Question,
    Secret,
}

#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub display_name: String,
    pub image: Option<String>,
    pub session: Option<String>,
    pub logged_in: bool,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub key: String,
    pub name: String,
}

/// `obj` is `None` in preview mode (`--preview`), used to develop and
/// screenshot the UI without a LightDM daemon.
pub struct Greeter {
    obj: Option<glib::Object>,
}

fn opt_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
    }
}

fn check(ok: gboolean, err: *mut glib::ffi::GError) -> Result<(), glib::Error> {
    if ok != 0 {
        Ok(())
    } else if err.is_null() {
        Err(glib::Error::new(
            glib::FileError::Failed,
            "lightdm call failed",
        ))
    } else {
        Err(unsafe { from_glib_full(err) })
    }
}

/// Iterate a GList of GObjects owned by liblightdm (not freed by us).
fn each(list: GListPtr, mut f: impl FnMut(GObjectPtr)) {
    let mut node = list;
    while !node.is_null() {
        unsafe {
            f((*node).data as GObjectPtr);
            node = (*node).next;
        }
    }
}

impl Greeter {
    pub fn connect() -> Result<Self, glib::Error> {
        let raw = unsafe { lightdm_greeter_new() };
        let obj: glib::Object = unsafe { from_glib_full(raw) };
        let mut err = ptr::null_mut();
        check(
            unsafe { lightdm_greeter_connect_to_daemon_sync(obj.as_ptr(), &mut err) },
            err,
        )?;
        Ok(Self { obj: Some(obj) })
    }

    pub fn preview() -> Self {
        Self { obj: None }
    }

    pub fn is_preview(&self) -> bool {
        self.obj.is_none()
    }

    fn ptr(&self) -> Option<GObjectPtr> {
        self.obj.as_ref().map(|o| o.as_ptr())
    }

    pub fn default_session_hint(&self) -> Option<String> {
        opt_str(unsafe { lightdm_greeter_get_default_session_hint(self.ptr()?) })
    }

    pub fn select_user_hint(&self) -> Option<String> {
        opt_str(unsafe { lightdm_greeter_get_select_user_hint(self.ptr()?) })
    }

    pub fn authenticate(&self, user: &str) -> Result<(), glib::Error> {
        let Some(g) = self.ptr() else { return Ok(()) };
        let user = CString::new(user).unwrap_or_default();
        let mut err = ptr::null_mut();
        check(
            unsafe { lightdm_greeter_authenticate(g, user.as_ptr(), &mut err) },
            err,
        )
    }

    pub fn respond(&self, response: &str) -> Result<(), glib::Error> {
        let Some(g) = self.ptr() else { return Ok(()) };
        let response = CString::new(response).unwrap_or_default();
        let mut err = ptr::null_mut();
        check(
            unsafe { lightdm_greeter_respond(g, response.as_ptr(), &mut err) },
            err,
        )
    }

    pub fn cancel(&self) {
        let Some(g) = self.ptr() else { return };
        let mut err = ptr::null_mut();
        let _ = check(
            unsafe { lightdm_greeter_cancel_authentication(g, &mut err) },
            err,
        );
    }

    pub fn is_authenticated(&self) -> bool {
        self.ptr()
            .is_some_and(|g| unsafe { lightdm_greeter_get_is_authenticated(g) != 0 })
    }

    pub fn start_session(&self, key: &str) -> Result<(), glib::Error> {
        let Some(g) = self.ptr() else { return Ok(()) };
        let key = CString::new(key).unwrap_or_default();
        let mut err = ptr::null_mut();
        check(
            unsafe { lightdm_greeter_start_session_sync(g, key.as_ptr(), &mut err) },
            err,
        )
    }

    /// `show-prompt(text, type)`: PAM wants input (usually the password).
    pub fn on_prompt(&self, f: impl Fn(String, PromptType) + 'static) {
        let Some(obj) = &self.obj else { return };
        obj.connect_local("show-prompt", false, move |args| {
            let text = args
                .get(1)
                .and_then(|v| v.get::<String>().ok())
                .unwrap_or_default();
            let kind = args
                .get(2)
                .map(|v| unsafe { glib::gobject_ffi::g_value_get_enum(v.to_glib_none().0) })
                .unwrap_or(1);
            f(
                text,
                if kind == 0 {
                    PromptType::Question
                } else {
                    PromptType::Secret
                },
            );
            None
        });
    }

    /// `show-message(text, type)`: informational or error text from PAM.
    pub fn on_message(&self, f: impl Fn(String) + 'static) {
        let Some(obj) = &self.obj else { return };
        obj.connect_local("show-message", false, move |args| {
            f(args
                .get(1)
                .and_then(|v| v.get::<String>().ok())
                .unwrap_or_default());
            None
        });
    }

    pub fn on_authentication_complete(&self, f: impl Fn() + 'static) {
        let Some(obj) = &self.obj else { return };
        obj.connect_local("authentication-complete", false, move |_| {
            f();
            None
        });
    }
}

pub fn sessions() -> Vec<Session> {
    let mut out = Vec::new();
    each(unsafe { lightdm_get_sessions() }, |s| {
        if let (Some(key), Some(name)) = (
            opt_str(unsafe { lightdm_session_get_key(s) }),
            opt_str(unsafe { lightdm_session_get_name(s) }),
        ) {
            out.push(Session { key, name });
        }
    });
    out
}

/// Users from AccountsService, via LightDM's user list.
pub fn users() -> Vec<User> {
    let mut out = Vec::new();
    let list = unsafe { lightdm_user_list_get_instance() };
    if list.is_null() {
        return out;
    }
    each(unsafe { lightdm_user_list_get_users(list) }, |u| {
        let Some(name) = opt_str(unsafe { lightdm_user_get_name(u) }) else {
            return;
        };
        out.push(User {
            display_name: opt_str(unsafe { lightdm_user_get_display_name(u) })
                .unwrap_or_else(|| name.clone()),
            image: opt_str(unsafe { lightdm_user_get_image(u) })
                .filter(|p| std::path::Path::new(p).exists()),
            session: opt_str(unsafe { lightdm_user_get_session(u) }),
            logged_in: unsafe { lightdm_user_get_logged_in(u) != 0 },
            name,
        });
    });
    out
}

/// Sample data for `--preview`.
pub fn preview_users() -> Vec<User> {
    [("ken", "Ken"), ("amara", "Amara Okafor"), ("li", "Li Wei")]
        .iter()
        .map(|(n, d)| User {
            name: n.to_string(),
            display_name: d.to_string(),
            image: None,
            session: None,
            logged_in: *n == "amara",
        })
        .collect()
}

pub fn preview_sessions() -> Vec<Session> {
    vec![
        Session {
            key: "umoja".into(),
            name: "Umoja".into(),
        },
        Session {
            key: "ubuntu".into(),
            name: "Ubuntu".into(),
        },
    ]
}
