//! `org.gnome.Shell.Introspect`: the window list behind the portal's
//! "share a window" picker.
//!
//! xdg-desktop-portal-gnome lists windows (title and app icon) from
//! gnome-shell's Introspect service before it asks for a window stream
//! (screencast.rs). Like gnome-shell, only the portal backends may ask:
//! window titles are nobody else's business. `UMOJA_UNSAFE_MODE=1` lifts
//! that for tests, as gnome-shell's unsafe mode does.

use crate::shell::WindowRow;
use gio::prelude::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, fdo, interface};

pub const BUS_NAME: &str = "org.gnome.Shell.Introspect";
const OBJECT_PATH: &str = "/org/gnome/Shell/Introspect";
/// gnome-shell's INTROSPECT_DBUS_API_VERSION.
const VERSION: u32 = 3;

/// Who may list windows (gnome-shell's APP_ALLOWLIST, plus our own portal).
const ALLOWED: &[&str] = &[
    "org.freedesktop.impl.portal.desktop.gtk",
    "org.freedesktop.impl.portal.desktop.gnome",
    "org.freedesktop.impl.portal.desktop.umoja",
];

const FOCUSED: u32 = 1 << 0;
const MINIMIZED: u32 = 1 << 1;

/// Wayland app-id -> desktop file id, looked up once per app.
static DESKTOP_IDS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// The desktop file id gnome-shell would give (`org.gnome.TextEditor.desktop`,
/// `firefox_firefox.desktop`): the portal finds the icon with it.
fn desktop_id(app_id: &str) -> String {
    if let Some(id) = DESKTOP_IDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(app_id)
    {
        return id.clone();
    }
    let id = umoja_common::apps::app_for_window(app_id)
        .and_then(|info| info.id())
        .map(|id| id.to_string())
        .unwrap_or_else(|| {
            if app_id.is_empty() {
                String::new()
            } else {
                format!("{app_id}.desktop")
            }
        });
    let mut cache = DESKTOP_IDS.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() > 512 {
        cache.clear();
    }
    cache.insert(app_id.to_string(), id.clone());
    id
}

fn prop<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values convert")
}

type WindowProps = HashMap<String, OwnedValue>;

/// GetWindows' dictionary for one window.
fn window_props(row: &WindowRow, desktop: &str) -> WindowProps {
    let (_, app_id, title, _, _, w, h, flags) = row;
    let mut props = HashMap::from([
        ("app-id".to_string(), prop(desktop)),
        // Mutter's META_WINDOW_CLIENT_TYPE_WAYLAND; Xwayland windows look
        // the same to the portal.
        ("client-type".to_string(), prop(0u32)),
        ("is-hidden".to_string(), prop(flags & MINIMIZED != 0)),
        ("has-focus".to_string(), prop(flags & FOCUSED != 0)),
        ("width".to_string(), prop((*w).max(0) as u32)),
        ("height".to_string(), prop((*h).max(0) as u32)),
    ]);
    if !title.is_empty() {
        props.insert("title".to_string(), prop(title.as_str()));
    }
    if !app_id.is_empty() {
        props.insert("wm-class".to_string(), prop(app_id.as_str()));
    }
    props
}

async fn windows() -> Vec<(WindowRow, String)> {
    tokio::task::spawn_blocking(|| {
        crate::shell::list_windows(false)
            .into_iter()
            .map(|row| {
                let id = desktop_id(&row.1);
                (row, id)
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

async fn check_sender(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    if std::env::var("UMOJA_UNSAFE_MODE").is_ok_and(|v| v == "1") {
        return Ok(());
    }
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("No sender".into()))?;
    let dbus = fdo::DBusProxy::new(conn).await?;
    for name in ALLOWED {
        let Ok(name) = zbus::names::BusName::try_from(*name) else {
            continue;
        };
        if let Ok(owner) = dbus.get_name_owner(name).await
            && owner.as_str() == sender.as_str()
        {
            return Ok(());
        }
    }
    Err(fdo::Error::AccessDenied(format!(
        "{BUS_NAME} is not allowed for {sender}"
    )))
}

struct Introspect;

#[interface(name = "org.gnome.Shell.Introspect")]
impl Introspect {
    /// Running apps by desktop id; the focused one is active on seat0.
    async fn get_running_applications(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<HashMap<String, HashMap<String, OwnedValue>>> {
        check_sender(conn, &header).await?;
        let mut apps: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();
        for (row, desktop) in windows().await {
            if desktop.is_empty() {
                continue;
            }
            let entry = apps.entry(desktop).or_default();
            if row.7 & FOCUSED != 0 {
                entry.insert("active-on-seats".to_string(), prop(vec!["seat0"]));
            }
        }
        Ok(apps)
    }

    async fn get_windows(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<HashMap<u64, WindowProps>> {
        check_sender(conn, &header).await?;
        Ok(windows()
            .await
            .iter()
            .map(|(row, desktop)| (row.0, window_props(row, desktop)))
            .collect())
    }

    #[zbus(signal)]
    async fn running_applications_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn windows_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn animations_enabled(&self) -> bool {
        umoja_common::gsettings::get_bool("org.gnome.desktop.interface", "enable-animations", true)
    }

    /// The bounding box of all monitors, in layout coordinates.
    #[zbus(property)]
    fn screen_size(&self) -> (i32, i32) {
        let outputs = crate::display_config::outputs();
        let w = outputs
            .values()
            .map(|o| o.rect.x + o.rect.w)
            .max()
            .unwrap_or(0);
        let h = outputs
            .values()
            .map(|o| o.rect.y + o.rect.h)
            .max()
            .unwrap_or(0);
        (w, h)
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        VERSION
    }
}

/// Serve Introspect on `conn`, with WindowsChanged and
/// RunningApplicationsChanged following the compositor's window changes.
pub async fn serve(conn: &Connection) -> zbus::Result<()> {
    conn.object_server().at(OBJECT_PATH, Introspect).await?;
    let mut rx = crate::shell::subscribe_windows();
    let conn = conn.clone();
    crate::session::RUNTIME.spawn(async move {
        while rx.changed().await.is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            rx.borrow_and_update();
            let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) else {
                continue;
            };
            let _ = Introspect::windows_changed(&emitter).await;
            let _ = Introspect::running_applications_changed(&emitter).await;
        }
    });
    Ok(())
}
