//! `GcrSecretExchange` from libgcr-4: the Diffie-Hellman + AES handshake
//! the keyring and its prompter use so the typed password never crosses
//! the session bus in the clear. Both ends must speak gcr's exact
//! "sx-aes-1" protocol, so we use gcr's own implementation rather than a
//! copy of it. It is a plain GObject with four functions.

use gtk4::glib::{self, gobject_ffi, prelude::*, translate::*};
use std::ffi::{CStr, CString, c_char, c_int};

#[link(name = "gcr-4")]
unsafe extern "C" {
    fn gcr_secret_exchange_new(protocol: *const c_char) -> *mut gobject_ffi::GObject;
    fn gcr_secret_exchange_begin(exchange: *mut gobject_ffi::GObject) -> *mut c_char;
    fn gcr_secret_exchange_receive(
        exchange: *mut gobject_ffi::GObject,
        exchange_str: *const c_char,
    ) -> c_int;
    fn gcr_secret_exchange_send(
        exchange: *mut gobject_ffi::GObject,
        secret: *const c_char,
        secret_len: isize,
    ) -> *mut c_char;
}

pub struct Exchange {
    obj: glib::Object,
}

/// Take a g_malloc'd string.
unsafe fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { glib::ffi::g_free(p.cast()) };
    Some(s)
}

impl Exchange {
    pub fn new() -> Self {
        let obj = unsafe { from_glib_full(gcr_secret_exchange_new(std::ptr::null())) };
        Self { obj }
    }

    fn ptr(&self) -> *mut gobject_ffi::GObject {
        self.obj.as_ptr()
    }

    /// Our half of the handshake, sent with the first PromptReady.
    pub fn begin(&self) -> String {
        unsafe { take(gcr_secret_exchange_begin(self.ptr())) }.unwrap_or_default()
    }

    /// The caller's half (and any secret it sent), from PerformPrompt.
    pub fn receive(&self, data: &str) -> bool {
        let Ok(data) = CString::new(data) else {
            return false;
        };
        unsafe { gcr_secret_exchange_receive(self.ptr(), data.as_ptr()) != 0 }
    }

    /// The exchange string carrying `secret`, encrypted for the caller (or
    /// carrying none, for confirm prompts and cancels).
    pub fn send(&self, secret: Option<&str>) -> String {
        let secret = secret.and_then(|s| CString::new(s).ok());
        let (ptr, len) = match &secret {
            Some(s) => (s.as_ptr(), s.as_bytes().len() as isize),
            None => (std::ptr::null(), 0),
        };
        unsafe { take(gcr_secret_exchange_send(self.ptr(), ptr, len)) }.unwrap_or_default()
    }
}
