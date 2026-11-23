//! Desktop notifications for approvals and results, with action buttons.
//! The Umoja panel is the notification server; ActionInvoked comes back here.

use std::collections::HashMap;

use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
pub trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;
}

pub const ICON: &str = "umoja-agents-symbolic";

/// Sends one; returns its id (0 when there is no notification server).
pub async fn send(
    conn: &zbus::Connection,
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
    urgent: bool,
) -> u32 {
    let Ok(proxy) = NotificationsProxy::new(conn).await else { return 0 };
    let flat: Vec<&str> = actions.iter().flat_map(|(k, l)| [*k, *l]).collect();
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("desktop-entry", Value::from("org.umoja.Agents"));
    hints.insert("urgency", Value::from(if urgent { 2u8 } else { 1u8 }));
    if urgent {
        // Stays until answered, like gnome-shell's critical notifications.
        hints.insert("resident", Value::from(true));
    }
    proxy
        .notify(&crate::catalog::active().name, 0, ICON, summary, body, &flat, hints, if urgent { 0 } else { -1 })
        .await
        .unwrap_or(0)
}

pub async fn close(conn: &zbus::Connection, id: u32) {
    if id == 0 {
        return;
    }
    if let Ok(proxy) = NotificationsProxy::new(conn).await {
        let _ = proxy.close_notification(id).await;
    }
}
