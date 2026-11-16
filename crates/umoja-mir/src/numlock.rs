//! Num Lock that survives logging out, like GNOME's.
//!
//! Mir starts every keyboard with Num Lock off. Our Mir patch
//! (packaging/mir `3023_seat-num-lock.patch`) asks
//! [`mir_num_lock_initial_hook`] once whether it should start on, keeps a
//! single Num Lock for the whole seat (every keyboard and its LED follow it,
//! as in mutter), and reports each toggle to [`mir_num_lock_changed_hook`].
//!
//! In a session we follow `org.gnome.desktop.peripherals.keyboard`: with
//! `remember-numlock-state` on (GNOME's default), Num Lock starts as
//! `numlock-state` says and every change is written back there. On the
//! login screen the lightdm user's dconf is redirected to a runtime dir
//! (greeter.rs), so the greeter keeps its own state in a small file under
//! its state dir instead, as GDM remembers its own.

use gio::prelude::*;
use std::ffi::c_int;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};
use umoja_common::gsettings as gs;

const KEYBOARD: &str = "org.gnome.desktop.peripherals.keyboard";

/// `$XDG_STATE_HOME/umoja-greeter/numlock` (the lightdm user's home is
/// /var/lib/lightdm).
fn greeter_state_file() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".local/state"))
        .join("umoja-greeter")
        .join("numlock")
}

fn initial() -> bool {
    if crate::greeter::enabled() {
        return std::fs::read_to_string(greeter_state_file())
            .map(|s| s.trim() == "on")
            .unwrap_or(false);
    }
    let Some(settings) = gs::settings(KEYBOARD) else {
        return false;
    };
    if !gs::has_key(&settings, "remember-numlock-state") || !gs::has_key(&settings, "numlock-state")
    {
        return false;
    }
    settings.boolean("remember-numlock-state") && settings.boolean("numlock-state")
}

fn save(on: bool) {
    if crate::greeter::enabled() {
        let file = greeter_state_file();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&file, if on { "on\n" } else { "off\n" }) {
            tracing::warn!(error = %e, path = %file.display(), "could not save greeter Num Lock");
        }
        return;
    }
    let Some(settings) = gs::settings(KEYBOARD) else {
        return;
    };
    if !gs::has_key(&settings, "numlock-state") || !settings.boolean("remember-numlock-state") {
        return;
    }
    if settings.boolean("numlock-state") != on {
        if let Err(e) = settings.set_boolean("numlock-state", on) {
            tracing::warn!(error = %e, "could not save Num Lock");
        }
        gio::Settings::sync();
    }
}

/// Changes arrive on Mir's input thread with its key mapper locked, so they
/// are saved on a thread of their own.
fn saver() -> &'static Sender<bool> {
    static SAVER: OnceLock<Sender<bool>> = OnceLock::new();
    SAVER.get_or_init(|| {
        let (tx, rx) = channel::<bool>();
        std::thread::Builder::new()
            .name("umoja-numlock".into())
            .spawn(move || {
                while let Ok(mut on) = rx.recv() {
                    // Only the latest of a burst of toggles matters.
                    while let Ok(next) = rx.try_recv() {
                        on = next;
                    }
                    save(on);
                }
            })
            .expect("spawn numlock thread");
        tx
    })
}

/// Whether Num Lock starts on (1) or off (0). Called once by Mir's key
/// mapper (patch 3023); exported by build.rs's linker flags.
#[unsafe(no_mangle)]
pub extern "C" fn mir_num_lock_initial_hook() -> c_int {
    let on = std::panic::catch_unwind(initial).unwrap_or(false);
    tracing::info!(on, "Num Lock at start");
    on as c_int
}

/// Num Lock was toggled on some keyboard (Mir patch 3023).
#[unsafe(no_mangle)]
pub extern "C" fn mir_num_lock_changed_hook(on: c_int) {
    let _ = std::panic::catch_unwind(|| {
        let _ = saver().send(on != 0);
    });
}
