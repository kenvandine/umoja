//! Event sounds through libcanberra, as gnome-settings-daemon plays the
//! volume-change click: by XDG sound-theme event id, so the theme's file
//! is looked up the standard way and played as an "event" stream.

use std::cell::Cell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr;

#[link(name = "canberra")]
unsafe extern "C" {
    fn ca_context_create(c: *mut *mut c_void) -> c_int;
    fn ca_context_change_props(c: *mut c_void, ...) -> c_int;
    fn ca_context_play(c: *mut c_void, id: u32, ...) -> c_int;
    fn ca_context_playing(c: *mut c_void, id: u32, playing: *mut c_int) -> c_int;
    fn ca_context_cancel(c: *mut c_void, id: u32) -> c_int;
}

thread_local! {
    /// One context for the panel's life, created on first use.
    static CONTEXT: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
}

fn context() -> Option<*mut c_void> {
    CONTEXT.with(|c| {
        if c.get().is_null() {
            let mut ctx = ptr::null_mut();
            // SAFETY: ca_context_create fills `ctx` on success.
            if unsafe { ca_context_create(&mut ctx) } != 0 || ctx.is_null() {
                tracing::warn!("could not create a libcanberra context; no event sounds");
                return None;
            }
            let (k, v) = (c"application.name", c"Umoja");
            let (k2, v2) = (c"application.id", c"org.umoja.Panel");
            // SAFETY: NULL-terminated key/value list of C strings.
            unsafe {
                ca_context_change_props(
                    ctx,
                    k.as_ptr(),
                    v.as_ptr(),
                    k2.as_ptr(),
                    v2.as_ptr(),
                    ptr::null::<c_char>(),
                )
            };
            c.set(ctx);
        }
        Some(c.get())
    })
}

/// Whether sound `id` (as passed to `play`) is still playing.
pub fn playing(id: u32) -> bool {
    let Some(ctx) = context() else { return false };
    let mut p: c_int = 0;
    // SAFETY: ctx is a live context; `p` is written on success.
    unsafe { ca_context_playing(ctx, id, &mut p) == 0 && p != 0 }
}

/// Play the sound theme's `event` (e.g. "audio-volume-change") from
/// `theme`, under `id` (replacing whatever `id` was playing).
pub fn play(id: u32, event: &str, description: &str, theme: &str) {
    let Some(ctx) = context() else { return };
    let (Ok(event), Ok(description), Ok(theme)) =
        (CString::new(event), CString::new(description), CString::new(theme))
    else {
        return;
    };
    // SAFETY: ctx is live; the property list is C strings ending in NULL.
    let r = unsafe {
        ca_context_cancel(ctx, id);
        ca_context_play(
            ctx,
            id,
            c"event.id".as_ptr(),
            event.as_ptr(),
            c"event.description".as_ptr(),
            description.as_ptr(),
            c"canberra.xdg-theme.name".as_ptr(),
            theme.as_ptr(),
            c"canberra.cache-control".as_ptr(),
            c"permanent".as_ptr(),
            ptr::null::<c_char>(),
        )
    };
    if r != 0 {
        tracing::debug!(code = r, "libcanberra could not play the event sound");
    }
}
