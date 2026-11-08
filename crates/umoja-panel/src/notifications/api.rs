//! `org.umoja.Notifications`: the message list, the missed-notification
//! digest and quick replies, for the assistant overlay and scripts.
//!
//! ```text
//! GetMissed() -> a(ussssx)          id, app name, desktop id, summary, plain body, unix time
//! GetNotifications() -> a(ussssx)   everything in the message list, newest first
//! Summarize() -> s                  digest of the missed ones (all, if none are new)
//! DraftReplies(u id) -> as          up to three suggested replies
//! Reply(u id, s text) -> b          true if the app took the reply, false if it was
//!                                   copied to the clipboard and the app raised
//! OpenReply(u id)                   open the clock menu at the reply field
//! MarkSeen()                        as if the message list had been opened
//! ```

use std::rc::Rc;

use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use umoja_common::dbus;

use super::{assistant, Center, Note};

const XML: &str = r#"
<node>
  <interface name="org.umoja.Notifications">
    <method name="GetMissed">
      <arg type="a(ussssx)" direction="out"/>
    </method>
    <method name="GetNotifications">
      <arg type="a(ussssx)" direction="out"/>
    </method>
    <method name="Summarize">
      <arg type="s" name="digest" direction="out"/>
    </method>
    <method name="DraftReplies">
      <arg type="u" name="id" direction="in"/>
      <arg type="as" name="replies" direction="out"/>
    </method>
    <method name="Reply">
      <arg type="u" name="id" direction="in"/>
      <arg type="s" name="text" direction="in"/>
      <arg type="b" name="delivered" direction="out"/>
    </method>
    <method name="OpenReply">
      <arg type="u" name="id" direction="in"/>
    </method>
    <method name="MarkSeen"/>
  </interface>
</node>"#;

type Row = (u32, String, String, String, String, i64);

fn row(n: &Note) -> Row {
    (
        n.id,
        n.app_name.clone(),
        n.app_id.clone().unwrap_or_default(),
        n.summary.clone(),
        assistant::plain_text(&n.body, 2000),
        n.time.to_unix(),
    )
}

fn rows(notes: &[Note]) -> glib::Variant {
    let v: Vec<Row> = notes.iter().map(row).collect();
    (v,).to_variant()
}

pub fn own_name(center: Rc<Center>) {
    let weak = Rc::downgrade(&center);
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::NOTIFICATIONS_INTERFACE)
            .expect("interface");
        let weak = weak.clone();
        let res = conn
            .register_object(dbus::NOTIFICATIONS_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                let Some(c) = weak.upgrade() else {
                    invocation.return_dbus_error("org.freedesktop.DBus.Error.Failed", "shutting down");
                    return;
                };
                match method {
                    "GetMissed" => invocation.return_value(Some(&rows(&c.missed()))),
                    "GetNotifications" => invocation.return_value(Some(&rows(&c.notes()))),
                    "Summarize" => {
                        let mut notes = c.missed();
                        if notes.is_empty() {
                            notes = c.notes();
                        }
                        if notes.is_empty() {
                            invocation.return_value(Some(&("No notifications.",).to_variant()));
                            return;
                        }
                        glib::MainContext::default().spawn_local(async move {
                            match assistant::digest_text(&notes).await {
                                Ok(text) => invocation.return_value(Some(&(text,).to_variant())),
                                Err(e) => invocation
                                    .return_dbus_error("org.umoja.Notifications.Error.Assistant", &e),
                            }
                        });
                    }
                    "DraftReplies" => {
                        let Some((id,)) = params.get::<(u32,)>() else {
                            invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "u");
                            return;
                        };
                        let Some(note) = c.note(id) else {
                            invocation.return_dbus_error("org.umoja.Notifications.Error.NoSuchNotification", "no such notification");
                            return;
                        };
                        glib::MainContext::default().spawn_local(async move {
                            match assistant::draft_replies(&note).await {
                                Ok(r) => invocation.return_value(Some(&(r,).to_variant())),
                                Err(e) => invocation
                                    .return_dbus_error("org.umoja.Notifications.Error.Assistant", &e),
                            }
                        });
                    }
                    "Reply" => match params.get::<(u32, String)>() {
                        Some((id, text)) if c.note(id).is_some() => {
                            let ok = c.reply(id, &text);
                            invocation.return_value(Some(&(ok,).to_variant()));
                        }
                        Some(_) => invocation.return_dbus_error(
                            "org.umoja.Notifications.Error.NoSuchNotification",
                            "no such notification",
                        ),
                        None => invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "us"),
                    },
                    "OpenReply" => {
                        if let Some((id,)) = params.get::<(u32,)>() {
                            c.request_reply(id);
                        }
                        invocation.return_value(None);
                    }
                    "MarkSeen" => {
                        c.mark_seen();
                        invocation.return_value(None);
                    }
                    _ => invocation
                        .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
                }
            })
            .build();
        if let Err(e) = res {
            tracing::error!(error = %e, "could not export org.umoja.Notifications");
        }
    };
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        dbus::NOTIFICATIONS_BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        on_bus,
        |_, _| {},
        |_, _| tracing::warn!("could not own {}", dbus::NOTIFICATIONS_BUS_NAME),
    );
}
