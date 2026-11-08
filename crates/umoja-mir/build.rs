//! Builds the MirAL C++ shim and links it into umoja-mir.
//!
//! Mir is a C++ library, so instead of binding its whole API we keep a thin
//! C++ layer (`src/shim/umoja_shim.cpp`) that owns the `MirRunner` and the
//! window-management policy, and calls back into Rust for every decision
//! (tiling geometry, keybinding lookup, session start-up).

fn main() {
    println!("cargo:rerun-if-changed=src/shim/umoja_shim.cpp");
    println!("cargo:rerun-if-changed=src/shim/natural_scroll.cpp");
    println!("cargo:rerun-if-changed=src/shim/umoja_shim.h");

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

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .file("src/shim/umoja_shim.cpp")
        .file("src/shim/natural_scroll.cpp")
        .warnings(true)
        .flag_if_supported("-Wno-deprecated-declarations");
    for path in miral.include_paths.iter().chain(&mircommon.include_paths) {
        build.include(path);
    }
    build.compile("umoja_shim");

    for path in miral.link_paths.iter().chain(&mircommon.link_paths) {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for lib in miral.libs.iter().chain(&mircommon.libs) {
        println!("cargo:rustc-link-lib={lib}");
    }
}
