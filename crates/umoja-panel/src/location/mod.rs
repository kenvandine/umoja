//! The GeoClue location agent, built in like gnome-shell's
//! (js/ui/status/location.js).
//!
//! GeoClue only hands a location to an app it doesn't already trust after
//! asking the user's agent, `org.freedesktop.GeoClue2.Agent.AuthorizeApp`.
//! On Ubuntu the only agent is gnome-shell itself, so under Umoja nothing
//! answered and Weather, Maps and friends were refused. The panel now
//! registers that agent on the system bus and behaves as gnome-shell does:
//!
//! - `MaxAccuracyLevel` follows Settings > Privacy > Location
//!   (`org.gnome.system.location`): Off means GeoClue gives nobody a
//!   location, On means the `max-accuracy-level` setting.
//! - An app's answer is remembered in the permission store's `location`
//!   table, the one Settings > Privacy > Location lists and edits. Unknown
//!   apps get the "Allow location access" dialog; the answer, yes or no, is
//!   stored so they are not asked again.
//! - The location icon shows among the status icons while something is
//!   using the location (GeoClue's `InUse`).

mod dialog;

use crate::bus;
use gio::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::WidgetExt;
use std::collections::HashMap;

const GEOCLUE: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER: &str = "org.freedesktop.GeoClue2.Manager";
/// GeoClue calls the agent at this fixed path.
const AGENT_PATH: &str = "/org/freedesktop/GeoClue2/Agent";
const AGENT_IFACE: &str = "org.freedesktop.GeoClue2.Agent";
/// GeoClue only accepts agents named in its config's allow-list
/// (`[agent] whitelist=` in /etc/geoclue/geoclue.conf). Umoja is the
/// desktop shell in gnome-shell's place, so it registers under the shell's
/// entry instead of needing the system config changed.
const AGENT_ID: &str = "gnome-shell";

const STORE: &str = "org.freedesktop.impl.portal.PermissionStore";
const STORE_PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";
/// gnome-shell's table and entry; Settings reads the same ones.
const TABLE: &str = "location";
const STORE_ID: &str = "location";

const SCHEMA: &str = "org.gnome.system.location";

/// GClueAccuracyLevel, by the names the permission store uses.
const LEVELS: [(&str, u32); 6] = [
    ("NONE", 0),
    ("COUNTRY", 1),
    ("CITY", 4),
    ("NEIGHBORHOOD", 5),
    ("STREET", 6),
    ("EXACT", 8),
];
const NONE: u32 = 0;
const EXACT: u32 = 8;

const XML: &str = r#"<node>
  <interface name="org.freedesktop.GeoClue2.Agent">
    <method name="AuthorizeApp">
      <arg type="s" name="desktop_id" direction="in"/>
      <arg type="u" name="req_accuracy_level" direction="in"/>
      <arg type="b" name="authorized" direction="out"/>
      <arg type="u" name="allowed_accuracy_level" direction="out"/>
    </method>
    <property name="MaxAccuracyLevel" type="u" access="read"/>
  </interface>
</node>"#;

thread_local! {
    /// Kept for the life of the panel; its change signal needs it alive.
    static SETTINGS: Option<gio::Settings> = gio::SettingsSchemaSource::default()
        .and_then(|s| s.lookup(SCHEMA, true))
        .map(|_| gio::Settings::new(SCHEMA));
}

/// The most GeoClue may give anyone: nothing while Location Services is
/// off, else the user's chosen accuracy (the schema's enum values are
/// GeoClue's levels).
fn max_level() -> u32 {
    SETTINGS.with(|s| match s {
        Some(s) if s.boolean("enabled") => u32::try_from(s.enum_("max-accuracy-level"))
            .ok()
            .filter(|&l| l > NONE)
            .unwrap_or(EXACT),
        Some(_) => NONE,
        // No schema at all: nothing to honour, behave like the default.
        None => EXACT,
    })
}

fn level_name(level: u32) -> &'static str {
    LEVELS
        .iter()
        .rev()
        .find(|(_, l)| *l <= level)
        .map_or("NONE", |(n, _)| n)
}

fn level_from_name(name: &str) -> u32 {
    LEVELS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(NONE, |(_, l)| *l)
}

struct Request {
    desktop_id: String,
    level: u32,
    invocation: gio::DBusMethodInvocation,
}

/// Register as GeoClue's agent for this user, and again whenever GeoClue
/// (which exits when idle and is started on demand) comes back.
pub fn start() {
    let Some(conn) = bus::system_bus() else {
        return;
    };
    let (tx, rx) = async_channel::unbounded::<Request>();
    if let Err(e) = export(&conn, tx) {
        tracing::warn!(error = %e, "could not export the location agent");
        return;
    }
    // One request at a time, so two asks from the same app give one
    // dialog: the second finds the first's answer in the store.
    glib::MainContext::default().spawn_local(async move {
        while let Ok(req) = rx.recv().await {
            let allowed = authorize(&req.desktop_id, req.level).await;
            tracing::info!(
                app = req.desktop_id,
                requested = req.level,
                allowed,
                "location request"
            );
            req.invocation
                .return_value(Some(&(allowed != NONE, allowed).to_variant()));
        }
    });
    SETTINGS.with(|s| {
        if let Some(s) = s {
            let conn = conn.clone();
            s.connect_changed(None, move |_, key| {
                if matches!(key, "enabled" | "max-accuracy-level") {
                    max_level_changed(&conn);
                }
            });
        }
    });
    // Watched for the life of the panel.
    let _watch = gio::bus_watch_name_on_connection(
        &conn,
        GEOCLUE,
        gio::BusNameWatcherFlags::NONE,
        |conn, _, _| add_agent(&conn),
        |_, _| tracing::debug!("geoclue went away"),
    );
}

fn export(
    conn: &gio::DBusConnection,
    tx: async_channel::Sender<Request>,
) -> Result<(), glib::Error> {
    let info = gio::DBusNodeInfo::for_xml(XML)?;
    let iface = info
        .lookup_interface(AGENT_IFACE)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no interface"))?;
    // The registration lives as long as the connection.
    let _id = conn
        .register_object(AGENT_PATH, &iface)
        .method_call(move |conn, sender, _, _, method, params, invocation| {
            // Only GeoClue gets to put location dialogs on screen.
            if !bus::sent_by_owner(&conn, sender, GEOCLUE) {
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.AccessDenied",
                    "only geoclue may call the location agent",
                );
                return;
            }
            if method != "AuthorizeApp" {
                invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                return;
            }
            let Some((desktop_id, level)) = params.get::<(String, u32)>() else {
                invocation
                    .return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "expected (su)");
                return;
            };
            let req = Request {
                desktop_id,
                level,
                invocation,
            };
            if let Err(e) = tx.try_send(req) {
                e.into_inner().invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.Failed",
                    "location agent is shutting down",
                );
            }
        })
        .property(|_, _, _, _, prop| match prop {
            "MaxAccuracyLevel" => max_level().to_variant(),
            _ => NONE.to_variant(),
        })
        .build()?;
    Ok(())
}

fn add_agent(conn: &gio::DBusConnection) {
    conn.call(
        Some(GEOCLUE),
        MANAGER_PATH,
        MANAGER,
        "AddAgent",
        Some(&(AGENT_ID,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
        |r| match r {
            Ok(_) => tracing::info!("registered as the geoclue location agent"),
            Err(e) => tracing::warn!(error = %e, "could not register the geoclue agent"),
        },
    );
}

/// Tell GeoClue (its agent proxy caches the property) about a new limit.
fn max_level_changed(conn: &gio::DBusConnection) {
    let mut changed: HashMap<&str, glib::Variant> = HashMap::new();
    changed.insert("MaxAccuracyLevel", max_level().to_variant());
    let args = (AGENT_IFACE, changed, Vec::<String>::new()).to_variant();
    if let Err(e) = conn.emit_signal(
        None,
        AGENT_PATH,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        Some(&args),
    ) {
        tracing::warn!(error = %e, "could not announce the location accuracy");
    }
}

/// gnome-shell's AppAuthorizer: the stored answer, else the user's.
async fn authorize(desktop_id: &str, requested: u32) -> u32 {
    let max = max_level();
    if max == NONE {
        // Location Services is off; GeoClue refuses everyone anyway, and
        // asking (or storing a "no") would be pointless.
        return NONE;
    }
    // gnome-shell only asks for apps it can name.
    let Some(app) = find_app(desktop_id) else {
        tracing::warn!(app = desktop_id, "location request from an unknown app");
        return NONE;
    };
    let Ok(session) = gio::bus_get_future(gio::BusType::Session).await else {
        return NONE;
    };
    let level = match lookup(&session, desktop_id).await {
        Stored::Level(level) => level,
        // No permission store on this system: nowhere to keep an answer,
        // so (like gnome-shell) don't nag; let the app have what it asked.
        Stored::NoStore => requested,
        Stored::Unknown => {
            let granted = dialog::ask(&app).await;
            let level = if granted { requested } else { NONE };
            save(&session, desktop_id, level).await;
            level
        }
    };
    level.min(max)
}

/// Desktop files are looked up by GeoClue's desktop id. Snaps install
/// theirs as `<snap>_<app>.desktop`, so look for that too.
fn find_app(desktop_id: &str) -> Option<gio::DesktopAppInfo> {
    let file = format!("{desktop_id}.desktop");
    if let Some(app) = gio::DesktopAppInfo::new(&file) {
        return Some(app);
    }
    let suffix = format!("_{file}");
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|a| a.downcast::<gio::DesktopAppInfo>().ok())
        .find(|a| a.id().is_some_and(|id| id.ends_with(&suffix)))
}

enum Stored {
    Level(u32),
    Unknown,
    NoStore,
}

async fn lookup(session: &gio::DBusConnection, desktop_id: &str) -> Stored {
    let reply = session
        .call_future(
            Some(STORE),
            STORE_PATH,
            STORE,
            "Lookup",
            Some(&(TABLE, STORE_ID).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await;
    match reply {
        Ok(reply) => reply
            .child_value(0)
            .get::<HashMap<String, Vec<String>>>()
            .and_then(|perms| perms.get(desktop_id)?.first().map(|l| level_from_name(l)))
            .map_or(Stored::Unknown, Stored::Level),
        // The store answers NotFound until the table has its first entry;
        // a bus-level error (no such service) means there is no store.
        Err(e) => match gio::DBusError::remote_error(&e) {
            Some(name) if name.starts_with("org.freedesktop.DBus.Error.") => {
                tracing::warn!(error = %e, "no permission store");
                Stored::NoStore
            }
            _ => Stored::Unknown,
        },
    }
}

async fn save(session: &gio::DBusConnection, desktop_id: &str, level: u32) {
    let args = (TABLE, true, STORE_ID, desktop_id, vec![level_name(level)]).to_variant();
    if let Err(e) = session
        .call_future(
            Some(STORE),
            STORE_PATH,
            STORE,
            "SetPermission",
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await
    {
        tracing::warn!(app = desktop_id, error = %e, "could not store the location answer");
    }
}

/// The status icon shown while an app is using the location, like
/// gnome-shell's location indicator.
pub fn indicator() -> gtk::Image {
    let icon = gtk::Image::from_icon_name("location-services-active-symbolic");
    icon.set_visible(false);
    let Some(conn) = bus::system_bus() else {
        return icon;
    };
    let weak = icon.downgrade();
    conn.signal_subscribe(
        Some(GEOCLUE),
        Some("org.freedesktop.DBus.Properties"),
        Some("PropertiesChanged"),
        Some(MANAGER_PATH),
        Some(MANAGER),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            let changed = bus::Props::from_dict(&params.child_value(1));
            if let (Some(icon), Some(in_use)) = (weak.upgrade(), changed.bool("InUse")) {
                icon.set_visible(in_use);
            }
        },
    );
    let weak = icon.downgrade();
    let _watch = gio::bus_watch_name_on_connection(
        &conn,
        GEOCLUE,
        gio::BusNameWatcherFlags::NONE,
        move |conn, _, _| {
            let in_use = bus::get_all(&conn, GEOCLUE, MANAGER_PATH, MANAGER)
                .and_then(|p| p.bool("InUse"))
                .unwrap_or(false);
            if let Some(icon) = weak.upgrade() {
                icon.set_visible(in_use);
            }
        },
        {
            let weak = icon.downgrade();
            move |_, _| {
                if let Some(icon) = weak.upgrade() {
                    icon.set_visible(false);
                }
            }
        },
    );
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_round_trip() {
        for (name, level) in LEVELS {
            assert_eq!(level_name(level), name);
            assert_eq!(level_from_name(name), level);
        }
        // In-between levels round down, as GeoClue's do.
        assert_eq!(level_name(7), "STREET");
        assert_eq!(level_name(2), "COUNTRY");
        assert_eq!(level_from_name("bogus"), NONE);
    }
}
