//! umoja-mir: the Umoja Wayland compositor.
//!
//! Mir provides the display server, input, DRM/KMS and Xwayland. Umoja adds
//! Ubuntu-style window management on top through a thin MirAL shim. The
//! shell UI (panel, dock, overlay) runs in separate processes started by
//! systemd once the compositor is ready, so a crash there never takes the
//! session down.

mod attention;
mod display_config;
mod dock_icons;
mod edge_pressure;
mod eis;
mod ffi;
mod gestures;
mod gpu;
mod greeter;
mod hot_corner;
mod idle;
mod input;
mod input_capture;
mod introspect;
mod keymap;
mod media_keys;
mod numlock;
mod outputs;
mod peripherals;
mod protocols;
mod remote_desktop;
mod screencast;
mod screensaver;
mod session;
mod shell;
mod snap;
mod switcher;
mod tiling;
mod workspaces;
mod xwayland_grab;

/// Mir looks for `<argv0 basename>.display` in $XDG_CONFIG_HOME.
const DISPLAY_CONFIG_FILE: &str = "umoja-mir.display";

use std::ffi::{CString, c_char};

fn main() {
    // The package gives umoja-mir CAP_SYS_NICE (debian/umoja-desktop.postinst)
    // so its GPU contexts get high priority. A file capability makes the
    // kernel mark the process undumpable: no core dumps for coredumpctl or
    // Apport, no gdb -p, and /proc/<pid> owned by root. Undo that, so a crash
    // can still be debugged; the process is then as open to its own user as
    // before the capability.
    // SAFETY: prctl with plain integer arguments; it only changes a flag.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
    }
    umoja_common::init_logging("umoja-mir");
    let greeter = greeter::enabled();
    if greeter {
        // The login screen (see greeter.rs): no session services, D-Bus
        // names or Xwayland, and display scales worked out afresh.
        greeter::prepare(DISPLAY_CONFIG_FILE);
    }
    // Keyboard layout, repeat and pointer settings apply on the login
    // screen too (the lightdm user's, i.e. the system defaults).
    input::start_watching();
    peripherals::start_watching();
    if greeter {
        greeter::start_idle();
        display_config::start_greeter(DISPLAY_CONFIG_FILE);
    } else {
        outputs::import_gnome_monitors(DISPLAY_CONFIG_FILE);
        display_config::start(DISPLAY_CONFIG_FILE);
        shell::start();
        // Screen sharing, remote desktop and input capture for the portals.
        remote_desktop::start();
        idle::start();
    }

    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // Hybrid laptops: keep the compositor on the integrated GPU (gpu.rs).
    gpu::add_quirks(&mut args);
    let args: Vec<CString> = args
        .into_iter()
        .map(|a| CString::new(a.into_encoded_bytes()).expect("argv has no NUL"))
        .collect();
    let argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();

    let extensions = if greeter {
        protocols::to_cstrings(greeter::PROTOCOLS)
    } else {
        protocols::to_cstrings(protocols::ENABLED)
    };
    let ext_ptrs: Vec<*const c_char> = extensions.iter().map(|e| e.as_ptr()).collect();

    let config = ffi::UmojaConfig {
        extensions: ext_ptrs.as_ptr(),
        n_extensions: ext_ptrs.len(),
        enable_x11: !greeter && std::env::var_os("UMOJA_NO_XWAYLAND").is_none(),
    };

    let code = unsafe { ffi::umoja_mir_run(argv.len() as i32, argv.as_ptr(), &config) };
    std::process::exit(code);
}
