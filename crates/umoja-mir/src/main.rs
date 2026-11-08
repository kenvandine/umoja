//! umoja-mir: the Umoja Wayland compositor.
//!
//! Mir provides the display server, input, DRM/KMS and Xwayland. Umoja adds
//! Ubuntu-style window management on top through a thin MirAL shim. The
//! shell UI (panel, dock, overlay) runs in separate processes started by
//! systemd once the compositor is ready, so a crash there never takes the
//! session down.

mod display_config;
mod ffi;
mod input;
mod outputs;
mod protocols;
mod session;
mod shell;
mod snap;
mod switcher;
mod tiling;
mod workspaces;

/// Mir looks for `<argv0 basename>.display` in $XDG_CONFIG_HOME.
const DISPLAY_CONFIG_FILE: &str = "umoja-mir.display";

use std::ffi::{CString, c_char};

fn main() {
    umoja_common::init_logging("umoja-mir");
    input::start_watching();
    outputs::import_gnome_monitors(DISPLAY_CONFIG_FILE);
    display_config::start(DISPLAY_CONFIG_FILE);
    shell::start();

    let args: Vec<CString> = std::env::args_os()
        .map(|a| CString::new(a.into_encoded_bytes()).expect("argv has no NUL"))
        .collect();
    let argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();

    let extensions = protocols::as_cstrings();
    let ext_ptrs: Vec<*const c_char> = extensions.iter().map(|e| e.as_ptr()).collect();

    let config = ffi::UmojaConfig {
        extensions: ext_ptrs.as_ptr(),
        n_extensions: ext_ptrs.len(),
        enable_x11: std::env::var_os("UMOJA_NO_XWAYLAND").is_none(),
    };

    let code = unsafe { ffi::umoja_mir_run(argv.len() as i32, argv.as_ptr(), &config) };
    std::process::exit(code);
}
