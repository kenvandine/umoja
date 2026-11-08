//! Shared building blocks for every Umoja process.
//!
//! Nothing in here depends on GTK so that the compositor, portal and other
//! headless daemons can use it without pulling in a toolkit.

pub mod apps;
pub mod dbus;
pub mod gsettings;
pub mod hidpi;
pub mod i18n;
pub mod lemonade;
pub mod types;

/// Initialise `tracing` for a component. Honours `RUST_LOG`, defaulting to `info`.
pub fn init_logging(component: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
    tracing::info!(component, version = env!("CARGO_PKG_VERSION"), "starting");
}
