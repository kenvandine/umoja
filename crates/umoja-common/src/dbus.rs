//! Well-known D-Bus names and object paths used between Umoja components.

/// The overlay (app grid + AI search). Panel, dock and compositor call `Toggle`.
pub const OVERLAY_BUS_NAME: &str = "org.umoja.Overlay";
pub const OVERLAY_OBJECT_PATH: &str = "/org/umoja/Overlay";
pub const OVERLAY_INTERFACE: &str = "org.umoja.Overlay";

/// The top bar: `OpenMenu("calendar" | "quick-settings")` for the
/// compositor's top-edge touch gesture.
pub const PANEL_BUS_NAME: &str = "org.umoja.Panel";
pub const PANEL_OBJECT_PATH: &str = "/org/umoja/Panel";
pub const PANEL_INTERFACE: &str = "org.umoja.Panel";

/// StatusNotifier (AppIndicator) host implemented by umoja-panel.
pub const SNI_WATCHER_BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const SNI_WATCHER_OBJECT_PATH: &str = "/StatusNotifierWatcher";

/// Myna dictation daemon (`myna-desktop`, from the myna snap). Myna
/// Settings binds its shortcut (Super+J by default) as a GNOME media-keys
/// custom keybinding that calls `Toggle` here; the HUD and the top bar
/// follow its `State` property.
pub const MYNA_BUS_NAME: &str = "com.canonical.Myna.Dictation";
pub const MYNA_OBJECT_PATH: &str = "/com/canonical/Myna/Dictation";
pub const MYNA_INTERFACE: &str = "com.canonical.Myna.Dictation";

/// Window management API served by umoja-mir (window list for the overview,
/// activation, Alt+Tab clicks, tiling).
pub const COMPOSITOR_BUS_NAME: &str = "org.umoja.Compositor";
pub const COMPOSITOR_OBJECT_PATH: &str = "/org/umoja/Compositor";
pub const COMPOSITOR_INTERFACE: &str = "org.umoja.Compositor";

/// File search served by umoja-files (a local index of the home folder,
/// searched by meaning through the embedded Lemonade).
pub const FILES_BUS_NAME: &str = "org.umoja.Files";
pub const FILES_OBJECT_PATH: &str = "/org/umoja/Files";
pub const FILES_INTERFACE: &str = "org.umoja.Files";

/// Notification history, digest and quick replies served by umoja-panel
/// (next to its `org.freedesktop.Notifications` server), so the assistant
/// can answer "what did I miss?" and draft replies.
pub const NOTIFICATIONS_BUS_NAME: &str = "org.umoja.Notifications";
pub const NOTIFICATIONS_OBJECT_PATH: &str = "/org/umoja/Notifications";
pub const NOTIFICATIONS_INTERFACE: &str = "org.umoja.Notifications";

/// Media keys and the on-screen display (volume, brightness...), served by
/// umoja-panel. The compositor grabs the keys and calls `Press`; anything
/// else can show the OSD with `ShowOSD`, which takes the same dictionary as
/// gnome-shell's `org.gnome.Shell.ShowOSD`.
pub const MEDIA_KEYS_BUS_NAME: &str = "org.umoja.MediaKeys";
pub const MEDIA_KEYS_OBJECT_PATH: &str = "/org/umoja/MediaKeys";
pub const MEDIA_KEYS_INTERFACE: &str = "org.umoja.MediaKeys";
