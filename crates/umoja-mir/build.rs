//! Builds the MirAL C++ shim and links it into umoja-mir.
//!
//! Mir is a C++ library, so instead of binding its whole API we keep a thin
//! C++ layer (`src/shim/umoja_shim.cpp`) that owns the `MirRunner` and the
//! window-management policy, and calls back into Rust for every decision
//! (tiling geometry, keybinding lookup, session start-up).

fn main() {
    println!("cargo:rerun-if-changed=src/shim/umoja_shim.cpp");
    println!("cargo:rerun-if-changed=src/shim/umoja_shim.h");

    let miral = pkg_config::Config::new()
        .atleast_version("3.9")
        .probe("miral")
        .expect("miral (libmiral-dev) >= 3.9 is required to build umoja-mir");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .file("src/shim/umoja_shim.cpp")
        .warnings(true)
        .flag_if_supported("-Wno-deprecated-declarations");
    for path in &miral.include_paths {
        build.include(path);
    }
    build.compile("umoja_shim");
}
