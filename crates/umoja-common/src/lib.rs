//! Shared building blocks for every Umoja process.
//!
//! Nothing in here depends on GTK so that the compositor, portal and other
//! headless daemons can use it without pulling in a toolkit. The
//! exceptions, [`blur`] and [`monitors`], are behind the `blur` and
//! `monitors` features.

pub mod agents;
pub mod app_scope;
pub mod apps;
pub mod assistant_backend;
pub mod calendar;
#[cfg(feature = "blur")]
pub mod blur;
pub mod clock;
pub mod crashes;
pub mod dbus;
pub mod diagnostics;
pub mod gsettings;
pub mod hidpi;
pub mod i18n;
#[cfg(feature = "icons")]
pub mod icons;
pub mod lemonade;
#[cfg(feature = "monitors")]
pub mod monitors;
pub mod recall;
pub mod screen;
pub mod terminal;
pub mod types;
#[cfg(feature = "monitors")]
pub mod wallpaper;

/// Initialise `tracing` for a component. Honours `RUST_LOG`, defaulting to `info`.
pub fn init_logging(component: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
    tracing::info!(component, version = env!("CARGO_PKG_VERSION"), "starting");
}
