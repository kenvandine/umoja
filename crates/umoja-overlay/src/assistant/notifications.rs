//! Notification tools for the assistant: "what did I miss?" and replies,
//! backed by umoja-panel's `org.umoja.Notifications` (see
//! crates/umoja-panel/src/notifications/api.rs).

use super::tools::{Run, ToolSpec, arg_bool, arg_str, arg_u64};
use gio::prelude::*;
use gtk4::glib;
use serde_json::{Value, json};
use umoja_common::dbus;

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "list_notifications",
            "List the user's notifications, newest first, with their ids. Use for \"what did I \
             miss?\", \"any messages?\" or before replying to one. By default only those that \
             arrived since the user last opened the notification list; set all to true for \
             every notification. Summarize the result for the user, leading with anything that \
             asks them a question or needs action.",
            json!({"type": "object", "properties": {
                "all": {"type": "boolean", "description": "Include notifications already seen."}
            }}),
            Run::Main(|a| Box::pin(list(a))),
        )
        .with_summary(|_| "Check your notifications".into()),
        ToolSpec::new(
            "reply_to_notification",
            "Send a reply to a chat or mail notification (id from list_notifications). Apps that \
             support it receive the reply directly; otherwise it is copied to the clipboard and \
             the app is opened so the user can paste it. Write the reply in the user's voice.",
            json!({"type": "object", "properties": {
                "id": {"type": "integer", "description": "Notification id."},
                "text": {"type": "string", "description": "The reply to send."}
            }, "required": ["id", "text"]}),
            Run::Main(|a| Box::pin(reply(a))),
        )
        // It sends a message to someone outside the desktop: always ask.
        .confirm()
        .with_summary(|a| format!("Reply \"{}\"", arg_str(a, "text").unwrap_or(""))),
        ToolSpec::new(
            "open_notification_reply",
            "Open the notification list at a notification's reply field, with replies drafted \
             by the assistant, so the user can pick or edit one and send it themselves.",
            json!({"type": "object", "properties": {
                "id": {"type": "integer", "description": "Notification id."}
            }, "required": ["id"]}),
            Run::Main(|a| Box::pin(open_reply(a))),
        )
        .with_summary(|_| "Open the reply field".into()),
    ]
}

async fn call(
    method: &str,
    args: Option<glib::Variant>,
    reply: Option<&str>,
) -> Result<glib::Variant, String> {
    let c = gio::bus_get_future(gio::BusType::Session)
        .await
        .map_err(|e| e.to_string())?;
    c.call_future(
        Some(dbus::NOTIFICATIONS_BUS_NAME),
        dbus::NOTIFICATIONS_OBJECT_PATH,
        dbus::NOTIFICATIONS_INTERFACE,
        method,
        args.as_ref(),
        reply.map(|r| glib::VariantTy::new(r).unwrap()),
        gio::DBusCallFlags::NONE,
        3000,
    )
    .await
    .map_err(|e| {
        tracing::warn!(method, error = %e, "notifications call failed");
        if e.to_string().contains("NoSuchNotification") {
            "NoSuchNotification".to_string()
        } else {
            "the top bar's notification service didn't respond".to_string()
        }
    })
}

type Row = (u32, String, String, String, String, i64);

/// One line per notification for the model.
pub fn format_rows(rows: &[Row], now: i64) -> String {
    rows.iter()
        .map(|(id, app, _, summary, body, time)| {
            let mins = (now - time).max(0) / 60;
            let ago = match mins {
                0 => "just now".to_string(),
                m if m < 60 => format!("{m} min ago"),
                m if m < 24 * 60 => format!("{} h ago", m / 60),
                m => format!("{} days ago", m / (24 * 60)),
            };
            let app = if app.is_empty() { "System" } else { app };
            let body: String = body.chars().take(300).collect();
            if body.is_empty() {
                format!("id {id} · {app} · {ago}: {summary}")
            } else {
                format!("id {id} · {app} · {ago}: {summary} — {body}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn list(args: Value) -> String {
    let all = arg_bool(&args, "all").unwrap_or(false);
    let method = if all { "GetNotifications" } else { "GetMissed" };
    let v = match call(method, None, Some("(a(ussssx))")).await {
        Ok(v) => v,
        Err(e) => return format!("error: {e}"),
    };
    let (rows,) = v.get::<(Vec<Row>,)>().unwrap_or_default();
    if rows.is_empty() {
        return if all {
            "There are no notifications.".into()
        } else {
            "Nothing new since the user last looked at their notifications.".into()
        };
    }
    let now = glib::DateTime::now_local().map_or(0, |d| d.to_unix());
    let note = if all { "" } else { " new since the user last looked" };
    format!("{} notification(s){note}:\n{}", rows.len(), format_rows(&rows, now))
}

async fn reply(args: Value) -> String {
    let Some(id) = arg_u64(&args, "id") else { return "error: id is required".into() };
    let Some(text) = arg_str(&args, "text") else { return "error: text is required".into() };
    match call("Reply", Some((id as u32, text).to_variant()), Some("(b)")).await {
        Ok(v) => match v.get::<(bool,)>() {
            Some((true,)) => "Sent.".into(),
            _ => "The app can't take replies directly, so the reply was copied to the \
                  clipboard and the app opened; tell the user to paste it."
                .into(),
        },
        Err(e) if e.contains("NoSuchNotification") => {
            "error: that notification is gone".into()
        }
        Err(e) => format!("error: {e}"),
    }
}

async fn open_reply(args: Value) -> String {
    let Some(id) = arg_u64(&args, "id") else { return "error: id is required".into() };
    match call("OpenReply", Some((id as u32,).to_variant()), None).await {
        Ok(_) => {
            super::hide_when_done();
            "Opened the reply field with drafted replies.".into()
        }
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_for_the_model() {
        let rows = vec![
            (4, "Chat".into(), "".into(), "Bob".into(), "Did you push?".into(), 1000),
            (2, "".into(), "".into(), "Updates".into(), "".into(), 1000 - 7200),
        ];
        let s = format_rows(&rows, 1000 + 300);
        assert_eq!(
            s,
            "id 4 · Chat · 5 min ago: Bob — Did you push?\nid 2 · System · 2 h ago: Updates"
        );
    }
}
