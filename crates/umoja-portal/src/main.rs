//! xdg-desktop-portal-umoja: portal backend for the Umoja session.
//!
//! Implements `org.freedesktop.impl.portal.{Settings,Screenshot}`. Secret is
//! delegated to gnome-keyring via `umoja-portals.conf`, and every other
//! interface falls back to the GNOME/GTK backends. Screen sharing (ScreenCast,
//! RemoteDesktop, InputCapture) is xdg-desktop-portal-gnome's, with umoja-mir
//! serving the mutter and gnome-shell APIs it talks to.

mod screenshot;
mod settings;

use zbus::connection;

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.umoja";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// Portal response codes.
pub const RESPONSE_SUCCESS: u32 = 0;
pub const RESPONSE_CANCELLED: u32 = 1;
pub const RESPONSE_OTHER: u32 = 2;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    umoja_common::init_logging("xdg-desktop-portal-umoja");

    let settings = settings::Settings::new();
    let watcher = settings.clone();

    let conn = connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, settings)?
        .serve_at(OBJECT_PATH, screenshot::Screenshot)?
        .build()
        .await?;

    settings::watch(conn.clone(), watcher);
    tracing::info!("portal backend ready");
    std::future::pending::<()>().await;
    Ok(())
}
