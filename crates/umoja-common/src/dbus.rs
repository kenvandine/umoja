//! Well-known D-Bus names and object paths used between Umoja components.

/// The overlay (app grid + AI search). Panel, dock and compositor call `Toggle`.
pub const OVERLAY_BUS_NAME: &str = "org.umoja.Overlay";
pub const OVERLAY_OBJECT_PATH: &str = "/org/umoja/Overlay";
pub const OVERLAY_INTERFACE: &str = "org.umoja.Overlay";

/// StatusNotifier (AppIndicator) host implemented by umoja-panel.
pub const SNI_WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const SNI_WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";

/// Myna dictation daemon.
pub const MYNA_BUS_NAME: &str = "org.canonical.Myna";
pub const MYNA_OBJECT_PATH: &str = "/org/canonical/Myna";
pub const MYNA_SOCKET_PATH: &str = "/tmp/myna.sock";

/// Window management API served by umoja-mir (window list for the overview,
/// activation, Alt+Tab clicks, tiling).
pub const COMPOSITOR_BUS_NAME: &str = "org.umoja.Compositor";
pub const COMPOSITOR_OBJECT_PATH: &str = "/org/umoja/Compositor";
pub const COMPOSITOR_INTERFACE: &str = "org.umoja.Compositor";
