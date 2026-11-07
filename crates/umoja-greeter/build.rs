fn main() {
    pkg_config::Config::new()
        .atleast_version("1.30")
        .probe("liblightdm-gobject-1")
        .expect("liblightdm-gobject-1-dev is required to build umoja-greeter");
}
