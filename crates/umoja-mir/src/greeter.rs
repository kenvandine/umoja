//! Greeter mode: umoja-mir as the login screen's compositor.
//!
//! LightDM has no Wayland compositor of its own for greeters. With
//! `X-LightDM-Session-Type=wayland` it just runs the greeter's `Exec` on the
//! seat's VT, so `umoja-greeter-session` starts umoja-mir with `UMOJA_GREETER`
//! set to the greeter's command line. The compositor then runs that one
//! client and nothing else, and exits when it does, which is how LightDM
//! sees the greeter finish.
//!
//! Compared with a session, greeter mode:
//!
//! * starts no `umoja-session.target` and exports nothing to systemd or
//!   D-Bus (the lightdm user has no desktop to start),
//! * serves none of the session's D-Bus names (org.umoja.Compositor,
//!   DisplayConfig, the screensaver/inhibit interfaces),
//! * turns off Xwayland and every keybinding (no Alt+F4, minimise,
//!   workspaces or overview on the login screen),
//! * blanks the screen when idle but never locks,
//! * offers only the Wayland protocols a GTK client needs, so nothing on
//!   the login screen can capture the screen or read the clipboard,
//! * works out per-panel HiDPI scales afresh at every start (the lightdm
//!   user has no monitors.xml, and a dock may come and go between logins)
//!   in a runtime directory instead of the lightdm user's home.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// The greeter's command line, if umoja-mir runs in greeter mode.
fn command() -> Option<&'static [String]> {
    static CMD: OnceLock<Option<Vec<String>>> = OnceLock::new();
    CMD.get_or_init(|| {
        let v = std::env::var("UMOJA_GREETER").ok()?;
        let words: Vec<String> = v.split_whitespace().map(str::to_owned).collect();
        (!words.is_empty()).then_some(words)
    })
    .as_deref()
}

pub fn enabled() -> bool {
    command().is_some()
}

/// The XDG_CONFIG_HOME the greeter itself should see (Mir's is redirected).
static CLIENT_CONFIG_HOME: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();

/// Protocols for the greeter: xdg-shell and the core come from Mir. Input
/// method and virtual keyboard are kept for on-screen keyboards.
pub const PROTOCOLS: &[&str] = &[
    "zxdg_output_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zwp_input_method_manager_v2",
    "zwp_primary_selection_device_manager_v1",
    "wp_fractional_scale_manager_v1",
    "wp_viewporter",
];

/// Write this boot's display layout where Mir will look for it. Must run
/// before Mir starts and before any other thread reads the environment.
pub fn prepare(display_file_name: &str) {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("umoja-greeter");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(display_file_name);
    match crate::outputs::default_hidpi_layout() {
        Some(yaml) => {
            if let Err(e) = std::fs::write(&file, yaml) {
                tracing::warn!(error = %e, "could not write greeter display scales");
            } else {
                tracing::info!(path = %file.display(), "seeded greeter display scales");
            }
        }
        None => {
            let _ = std::fs::remove_file(&file);
        }
    }
    let _ = CLIENT_CONFIG_HOME.set(std::env::var_os("XDG_CONFIG_HOME"));
    // SAFETY: called from main before Mir or any of our threads start.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
}

/// Blank the screen after the lightdm user's idle-delay (GNOME's 5 minutes
/// by default); the login screen has nothing to lock.
pub fn start_idle() {
    let delay = umoja_common::gsettings::settings("org.gnome.desktop.session")
        .filter(|s| umoja_common::gsettings::has_key(s, "idle-delay"))
        .map(|s| gio::prelude::SettingsExt::uint(&s, "idle-delay"))
        .unwrap_or(300)
        .min(i32::MAX as u32) as i32;
    tracing::info!(off = delay, "greeter screen blank timeout");
    unsafe { crate::idle::umoja_idle_set_timeouts(delay, delay) };
}

/// Run the greeter on `wayland_display`; stop the compositor when it exits.
pub fn launch(wayland_display: &str) {
    let Some(argv) = command() else { return };
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("XDG_SESSION_TYPE", "wayland")
        .env_remove("DISPLAY")
        .env_remove("UMOJA_GREETER");
    match CLIENT_CONFIG_HOME.get().cloned().flatten() {
        Some(v) => cmd.env("XDG_CONFIG_HOME", v),
        None => cmd.env_remove("XDG_CONFIG_HOME"),
    };
    // The greeter must not outlive the compositor (LightDM stops the
    // greeter by signalling us). PDEATHSIG fires when the spawning thread
    // dies, so spawn and wait on the same long-lived thread.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let program = argv[0].clone();
    std::thread::Builder::new()
        .name("umoja-greeter-client".into())
        .spawn(move || {
            // Let the screens move apart first (display_config::start_greeter),
            // so the greeter's windows open on their final monitors.
            for _ in 0..20 {
                if !crate::display_config::outputs_overlap() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let code = match cmd.spawn() {
                Ok(mut child) => {
                    tracing::info!(program, pid = child.id(), "greeter started");
                    match child.wait() {
                        Ok(s) => {
                            tracing::info!(program, status = %s, "greeter exited");
                            s.code().unwrap_or(1)
                        }
                        Err(e) => {
                            tracing::error!(program, error = %e, "could not wait for greeter");
                            1
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(program, error = %e, "could not start greeter");
                    1
                }
            };
            unsafe { crate::ffi::umoja_mir_stop(code) };
        })
        .expect("spawn greeter thread");
}
