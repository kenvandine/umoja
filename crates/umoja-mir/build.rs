//! Builds the MirAL C++ shim and links it into umoja-mir.
//!
//! Mir is a C++ library, so instead of binding its whole API we keep a thin
//! C++ layer (`src/shim/umoja_shim.cpp`) that owns the `MirRunner` and the
//! window-management policy, and calls back into Rust for every decision
//! (tiling geometry, keybinding lookup, session start-up).

fn main() {
    println!("cargo:rerun-if-changed=src/shim/umoja_shim.cpp");
    println!("cargo:rerun-if-changed=src/shim/natural_scroll.cpp");
    println!("cargo:rerun-if-changed=src/shim/idle.cpp");
    println!("cargo:rerun-if-changed=src/shim/capture.cpp");
    println!("cargo:rerun-if-changed=src/shim/input_config.cpp");
    println!("cargo:rerun-if-changed=src/shim/umoja_shim.h");
    println!("cargo:rerun-if-changed=src/shim/session_management.cpp");
    println!("cargo:rerun-if-changed=src/shim/session_management.h");
    println!("cargo:rerun-if-changed=protocols/xdg-session-management-v1.xml");

    // Probe without emitting link flags yet: the static shim must come
    // before libmiral on the linker line, or `--as-needed` drops miral.
    let miral = pkg_config::Config::new()
        .atleast_version("3.9")
        .cargo_metadata(false)
        .probe("miral")
        .expect("miral (libmiral-dev) >= 3.9 is required to build umoja-mir");

    // Mir's event classes, used to rewrite scroll axes (natural scrolling).
    let mircommon = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("mircommon")
        .expect("mircommon (libmircommon-dev) is required to build umoja-mir");

    // mir::Server and the idle handler (screen blanking, idle.cpp). Only
    // libmirserver-internal-dev ships these headers.
    let mirserver = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("mirserver-internal")
        .expect("mirserver-internal (libmirserver-internal-dev) is required to build umoja-mir");

    // Protocols umoja-mir implements itself (Mir has no support for them):
    // generated with wayland-scanner, like any libwayland server.
    let wayland = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("wayland-server")
        .expect("wayland-server (libwayland-dev) is required to build umoja-mir");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let scanner = std::env::var("WAYLAND_SCANNER").unwrap_or_else(|_| "wayland-scanner".into());
    let mut protocol_code = Vec::new();
    for name in ["xdg-session-management-v1"] {
        let xml = format!("protocols/{name}.xml");
        let header = out.join(format!("{name}-server-protocol.h"));
        let code = out.join(format!("{name}-protocol.c"));
        for (kind, dest) in [("server-header", &header), ("private-code", &code)] {
            let status = std::process::Command::new(&scanner)
                .arg(kind)
                .arg(&xml)
                .arg(dest)
                .status()
                .expect("wayland-scanner (libwayland-bin) is required to build umoja-mir");
            assert!(status.success(), "wayland-scanner {kind} {xml} failed");
        }
        protocol_code.push(code);
    }
    let mut protocols = cc::Build::new();
    protocols.files(&protocol_code).include(&out);
    for path in &wayland.include_paths {
        protocols.include(path);
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .file("src/shim/umoja_shim.cpp")
        .file("src/shim/natural_scroll.cpp")
        .file("src/shim/idle.cpp")
        .file("src/shim/capture.cpp")
        .file("src/shim/input_config.cpp")
        .file("src/shim/session_management.cpp")
        .include(&out)
        .warnings(true)
        .flag_if_supported("-Wno-deprecated-declarations");
    for path in miral
        .include_paths
        .iter()
        .chain(&mircommon.include_paths)
        .chain(&mirserver.include_paths)
        .chain(&wayland.include_paths)
    {
        build.include(path);
    }
    build.compile("umoja_shim");
    // After the shim, which uses it, for the static link order.
    protocols.compile("umoja_protocols");

    // Our Mir patch (packaging/mir) finds the touchpad gesture hook with
    // dlsym(RTLD_DEFAULT), so it must be in the dynamic symbol table.
    println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic-symbol=mir_touchpad_gesture_hook");
    // Same for the scene filter behind window thumbnails (shim/capture.cpp).
    println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic-symbol=mir_scene_filter_hook");

    for path in miral
        .link_paths
        .iter()
        .chain(&mircommon.link_paths)
        .chain(&mirserver.link_paths)
        .chain(&wayland.link_paths)
    {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for lib in miral
        .libs
        .iter()
        .chain(&mircommon.libs)
        .chain(&mirserver.libs)
        .chain(&wayland.libs)
    {
        println!("cargo:rustc-link-lib={lib}");
    }
}
