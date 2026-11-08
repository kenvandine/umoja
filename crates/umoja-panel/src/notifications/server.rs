//! The `org.freedesktop.Notifications` D-Bus interface (spec 1.2).

use std::collections::HashMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;

use super::{Center, CloseReason, Icon, Note, Urgency};

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
pub const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
pub const INTERFACE: &str = "org.freedesktop.Notifications";

const XML: &str = r#"
<node>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities">
      <arg type="as" direction="out"/>
    </method>
    <method name="Notify">
      <arg type="s" name="app_name" direction="in"/>
      <arg type="u" name="replaces_id" direction="in"/>
      <arg type="s" name="app_icon" direction="in"/>
      <arg type="s" name="summary" direction="in"/>
      <arg type="s" name="body" direction="in"/>
      <arg type="as" name="actions" direction="in"/>
      <arg type="a{sv}" name="hints" direction="in"/>
      <arg type="i" name="expire_timeout" direction="in"/>
      <arg type="u" direction="out"/>
    </method>
    <method name="CloseNotification">
      <arg type="u" name="id" direction="in"/>
    </method>
    <method name="GetServerInformation">
      <arg type="s" name="name" direction="out"/>
      <arg type="s" name="vendor" direction="out"/>
      <arg type="s" name="version" direction="out"/>
      <arg type="s" name="spec_version" direction="out"/>
    </method>
    <signal name="NotificationClosed">
      <arg type="u" name="id"/>
      <arg type="u" name="reason"/>
    </signal>
    <signal name="ActionInvoked">
      <arg type="u" name="id"/>
      <arg type="s" name="action_key"/>
    </signal>
  </interface>
</node>"#;

/// What we support, as gnome-shell reports it, plus notify-osd's gauge and
/// synchronous bubbles.
const CAPABILITIES: &[&str] = &[
    "actions",
    "body",
    "body-hyperlinks",
    "body-markup",
    "icon-static",
    "persistence",
    "x-canonical-private-synchronous",
];

pub fn own_name(center: Rc<Center>) {
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("valid XML");
        let iface = info.lookup_interface(INTERFACE).expect("interface");
        let c = center.clone();
        let res = conn
            .register_object(OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                match method {
                    "GetCapabilities" => {
                        let caps: Vec<&str> = CAPABILITIES.to_vec();
                        invocation.return_value(Some(&(caps,).to_variant()));
                    }
                    "GetServerInformation" => invocation.return_value(Some(
                        &("Umoja", "Umoja", env!("CARGO_PKG_VERSION"), "1.2").to_variant(),
                    )),
                    "CloseNotification" => {
                        if let Some((id,)) = params.get::<(u32,)>() {
                            c.close(id, CloseReason::Closed);
                        }
                        invocation.return_value(None);
                    }
                    "Notify" => match parse_notify(&params) {
                        Some((note, replaces)) => {
                            let id = c.notify(note, replaces);
                            invocation.return_value(Some(&(id,).to_variant()));
                        }
                        None => invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "expected (susssasa{sv}i)",
                        ),
                    },
                    _ => invocation
                        .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
                }
            })
            .build();
        match res {
            Ok(_) => *center.conn.borrow_mut() = Some(conn),
            Err(e) => tracing::error!(error = %e, "could not export notifications"),
        }
    };
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        on_bus,
        |_, _| tracing::info!("notification server ready"),
        |_, _| tracing::warn!("another notification server owns {BUS_NAME}"),
    );
}

type NotifyArgs = (
    String,
    u32,
    String,
    String,
    String,
    Vec<String>,
    HashMap<String, glib::Variant>,
    i32,
);

fn parse_notify(params: &glib::Variant) -> Option<(Note, u32)> {
    let (app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout): NotifyArgs =
        params.get()?;

    let hint_str = |k: &str| hints.get(k).and_then(|v| v.get::<String>());
    let hint_bool = |k: &str| {
        hints
            .get(k)
            .and_then(|v| v.get::<bool>().or_else(|| v.get::<u8>().map(|b| b != 0)))
            .unwrap_or(false)
    };

    let app_id = hint_str("desktop-entry")
        .map(|d| d.trim_end_matches(".desktop").to_string())
        .filter(|d| !d.is_empty());

    let urgency = match hints.get("urgency").and_then(|v| v.get::<u8>()) {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    };
    let value = hints
        .get("value")
        .and_then(|v| v.get::<i32>().or_else(|| v.get::<u32>().map(|u| u as i32)))
        .map(|v| v.clamp(0, 100));

    // Spec order: image-data, image-path, app_icon; then the app's icon.
    let icon = hints
        .get("image-data")
        .or_else(|| hints.get("image_data"))
        .or_else(|| hints.get("icon_data"))
        .and_then(image_data)
        .map(Icon::Texture)
        .or_else(|| {
            hint_str("image-path")
                .or_else(|| hint_str("image_path"))
                .filter(|s| !s.is_empty())
                .map(icon_from_string)
        })
        .or_else(|| (!app_icon.is_empty()).then(|| icon_from_string(app_icon.clone())))
        .or_else(|| {
            let app = gio::DesktopAppInfo::new(&format!("{}.desktop", app_id.as_deref()?))?;
            let gicon = app.icon()?;
            let names = gicon.downcast_ref::<gio::ThemedIcon>()?.names();
            names.first().map(|n| Icon::Name(n.to_string()))
        });

    let actions = actions
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect();

    let note = Note {
        id: 0,
        app_name,
        app_id,
        summary,
        body: sanitize_body(&body),
        icon,
        actions,
        urgency,
        value,
        sync_tag: hint_str("x-canonical-private-synchronous")
            .or_else(|| hint_str("synchronous"))
            .or_else(|| hint_str("private-synchronous")),
        transient: hint_bool("transient"),
        resident: hint_bool("resident"),
        expire_timeout,
        time: glib::DateTime::now_local().ok()?,
    };
    Some((note, replaces_id))
}

fn icon_from_string(s: String) -> Icon {
    if let Some(path) = s.strip_prefix("file://") {
        Icon::File(glib::Uri::unescape_string(path, None::<&str>).map_or(path.to_string(), |p| p.to_string()))
    } else if s.starts_with('/') {
        Icon::File(s)
    } else {
        Icon::Name(s)
    }
}

/// `(iiibiiay)`: width, height, rowstride, has_alpha, bits, channels, data.
fn image_data(v: &glib::Variant) -> Option<gdk::Texture> {
    let (w, h, stride, alpha, bits, channels, data): (i32, i32, i32, bool, i32, i32, Vec<u8>) =
        v.get()?;
    if w <= 0 || h <= 0 || bits != 8 || !(channels == 3 || channels == 4) {
        return None;
    }
    if data.len() < (stride as usize) * (h as usize - 1) + (w * channels) as usize {
        return None;
    }
    let format = if alpha && channels == 4 {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    let tex = gdk::MemoryTexture::new(w, h, format, &glib::Bytes::from_owned(data), stride as usize);
    Some(tex.upcast())
}

/// Bodies may carry a small markup subset (b, i, u, a, img). Keep what
/// Pango accepts, drop <img>, and escape anything malformed.
pub fn sanitize_body(body: &str) -> String {
    let mut s = body.to_string();
    while let Some(start) = s.find("<img") {
        match s[start..].find('>') {
            Some(end) => s.replace_range(start..start + end + 1, ""),
            None => break,
        }
    }
    if gtk::pango::parse_markup(&s, '\0').is_ok() {
        s
    } else {
        glib::markup_escape_text(body).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_sanitized() {
        assert_eq!(sanitize_body("<b>bold</b> text"), "<b>bold</b> text");
        assert_eq!(sanitize_body("a <img src=\"x.png\"/> b"), "a  b");
        assert_eq!(sanitize_body("1 < 2 & 3"), "1 &lt; 2 &amp; 3");
    }

    #[test]
    fn icon_strings() {
        assert!(matches!(icon_from_string("dialog-information".into()), Icon::Name(_)));
        assert!(matches!(icon_from_string("/tmp/a.png".into()), Icon::File(p) if p == "/tmp/a.png"));
        assert!(matches!(icon_from_string("file:///tmp/a%20b.png".into()), Icon::File(p) if p == "/tmp/a b.png"));
    }

    #[test]
    fn notify_round_trip() {
        let mut hints: HashMap<String, glib::Variant> = HashMap::new();
        hints.insert("urgency".into(), 2u8.to_variant());
        hints.insert("value".into(), 42i32.to_variant());
        hints.insert("x-canonical-private-synchronous".into(), "volume".to_variant());
        let args: NotifyArgs = (
            "app".into(),
            7,
            "audio-volume-medium".into(),
            "Volume".into(),
            "".into(),
            vec!["default".into(), "Open".into(), "mute".into(), "Mute".into()],
            hints,
            -1,
        );
        let (n, replaces) = parse_notify(&args.to_variant()).unwrap();
        assert_eq!(replaces, 7);
        assert_eq!(n.urgency, Urgency::Critical);
        assert_eq!(n.value, Some(42));
        assert_eq!(n.sync_tag.as_deref(), Some("volume"));
        assert!(n.default_action());
        assert_eq!(n.buttons().count(), 1);
    }
}
