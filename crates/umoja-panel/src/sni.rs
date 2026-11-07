//! StatusNotifierItem (AppIndicator) support.
//!
//! umoja-panel owns `org.kde.StatusNotifierWatcher` and is also the host
//! that draws the icons, so tray icons from Steam, Discord, Slack, nm-applet
//! and friends work without any legacy XEmbed wrapper.

use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use umoja_common::dbus;

const WATCHER_XML: &str = r#"
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem"><arg name="service" type="s" direction="in"/></method>
    <method name="RegisterStatusNotifierHost"><arg name="service" type="s" direction="in"/></method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
    <signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
    <signal name="StatusNotifierHostRegistered"/>
  </interface>
</node>"#;

const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";

struct Item {
    button: gtk::Button,
    unwatch: Box<dyn FnOnce()>,
}

pub struct Tray {
    container: gtk::Box,
    items: RefCell<HashMap<String, Item>>,
    connection: RefCell<Option<gio::DBusConnection>>,
}

impl Tray {
    pub fn start() -> Rc<Self> {
        let tray = Rc::new(Self {
            container: gtk::Box::new(gtk::Orientation::Horizontal, 2),
            items: RefCell::default(),
            connection: RefCell::default(),
        });

        let on_bus = {
            let tray = tray.clone();
            move |conn: gio::DBusConnection, _: &str| tray.export(conn)
        };
        // Never released; the watcher lives as long as the panel.
        let _ = gio::bus_own_name(
            gio::BusType::Session,
            dbus::SNI_WATCHER_BUS_NAME,
            gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
            on_bus,
            |_, name| tracing::info!(name, "tray watcher active"),
            |_, name| tracing::warn!(name, "lost tray watcher name"),
        );
        tray
    }

    pub fn widget(&self) -> gtk::Widget {
        self.container.clone().upcast()
    }

    fn export(self: &Rc<Self>, conn: gio::DBusConnection) {
        let info = gio::DBusNodeInfo::for_xml(WATCHER_XML).expect("valid introspection XML");
        let iface = info
            .lookup_interface("org.kde.StatusNotifierWatcher")
            .expect("interface present");
        *self.connection.borrow_mut() = Some(conn.clone());

        let methods = Rc::downgrade(self);
        let props = Rc::downgrade(self);
        let result = conn
            .register_object(dbus::SNI_WATCHER_OBJECT_PATH, &iface)
            .method_call(
                move |_conn, sender, _path, _iface, method, params, invocation| {
                    let Some(tray) = methods.upgrade() else {
                        return;
                    };
                    let arg = params.get::<(String,)>().map(|(s,)| s).unwrap_or_default();
                    match method {
                        "RegisterStatusNotifierItem" => {
                            tray.register(sender.unwrap_or_default(), &arg);
                            invocation.return_value(None);
                        }
                        "RegisterStatusNotifierHost" => invocation.return_value(None),
                        _ => invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
                    }
                },
            )
            .property(move |_conn, _sender, _path, _iface, name| {
                let Some(tray) = props.upgrade() else {
                    return false.to_variant();
                };
                match name {
                    "RegisteredStatusNotifierItems" => tray
                        .items
                        .borrow()
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .to_variant(),
                    "IsStatusNotifierHostRegistered" => true.to_variant(),
                    "ProtocolVersion" => 0i32.to_variant(),
                    _ => glib::Variant::from(()),
                }
            })
            .build();
        if let Err(e) = result {
            tracing::error!(error = %e, "could not export StatusNotifierWatcher");
            return;
        }
        self.emit("StatusNotifierHostRegistered", None);
    }

    fn emit(&self, signal: &str, arg: Option<&str>) {
        if let Some(conn) = self.connection.borrow().as_ref() {
            let params = arg.map(|a| (a,).to_variant());
            let _ = conn.emit_signal(
                None,
                dbus::SNI_WATCHER_OBJECT_PATH,
                "org.kde.StatusNotifierWatcher",
                signal,
                params.as_ref(),
            );
        }
    }

    /// `service` is either a bus name (path defaults to /StatusNotifierItem),
    /// a bus name followed by a path, or just an object path (Ayatana style,
    /// in which case the item lives on the sender's connection).
    fn register(self: &Rc<Self>, sender: &str, service: &str) {
        let (bus, path) = split_service(sender, service);
        let key = format!("{bus}{path}");
        if self.items.borrow().contains_key(&key) {
            return;
        }
        let Some(conn) = self.connection.borrow().clone() else {
            return;
        };

        let button = gtk::Button::builder()
            .css_classes(["flat", "tray-item"])
            .build();
        let proxy = gio::DBusProxy::new_sync(
            &conn,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            Some(&bus),
            &path,
            ITEM_IFACE,
            gio::Cancellable::NONE,
        );
        let Ok(proxy) = proxy else { return };
        refresh_icon(&proxy, &button);

        proxy.connect_local("g-signal", false, {
            let button = button.downgrade();
            move |args| {
                let proxy = args[0].get::<gio::DBusProxy>().ok()?;
                let signal = args[2].get::<String>().ok()?;
                if let (Some(b), "NewIcon" | "NewTitle" | "NewStatus" | "NewToolTip") =
                    (button.upgrade(), signal.as_str())
                {
                    refresh_icon(&proxy, &b);
                }
                None
            }
        });
        button.connect_clicked({
            let proxy = proxy.clone();
            move |_| call(&proxy, "Activate")
        });
        let secondary = gtk::GestureClick::builder().button(3).build();
        secondary.connect_pressed(move |_, _, _, _| {
            // TODO(phase 2): render com.canonical.dbusmenu menus natively.
            call(&proxy, "ContextMenu")
        });
        button.add_controller(secondary);

        let watch = {
            let weak = Rc::downgrade(self);
            let key = key.clone();
            gio::bus_watch_name_on_connection(
                &conn,
                &bus,
                gio::BusNameWatcherFlags::NONE,
                |_, _, _| {},
                move |_, _| {
                    if let Some(tray) = weak.upgrade() {
                        tray.unregister(&key);
                    }
                },
            )
        };

        self.container.append(&button);
        self.items.borrow_mut().insert(
            key.clone(),
            Item {
                button,
                unwatch: Box::new(move || gio::bus_unwatch_name(watch)),
            },
        );
        tracing::info!(item = key, "tray item registered");
        self.emit("StatusNotifierItemRegistered", Some(&key));
    }

    fn unregister(&self, key: &str) {
        let removed = self.items.borrow_mut().remove(key);
        if let Some(item) = removed {
            self.container.remove(&item.button);
            (item.unwatch)();
            self.emit("StatusNotifierItemUnregistered", Some(key));
        }
    }
}

fn split_service(sender: &str, service: &str) -> (String, String) {
    if service.starts_with('/') {
        (sender.to_string(), service.to_string())
    } else if let Some(idx) = service.find('/') {
        (service[..idx].to_string(), service[idx..].to_string())
    } else {
        (service.to_string(), "/StatusNotifierItem".to_string())
    }
}

/// SNI items rarely emit PropertiesChanged, so fetch properties explicitly.
fn refresh_icon(proxy: &gio::DBusProxy, button: &gtk::Button) {
    let button = button.downgrade();
    proxy.call(
        "org.freedesktop.DBus.Properties.GetAll",
        Some(&(ITEM_IFACE,).to_variant()),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
        move |res| {
            let (Ok(v), Some(button)) = (res, button.upgrade()) else {
                return;
            };
            let Some((props,)) = v.get::<(HashMap<String, glib::Variant>,)>() else {
                return;
            };
            let text = |k: &str| {
                props
                    .get(k)
                    .and_then(|v| v.as_variant().unwrap_or(v.clone()).get::<String>())
            };
            let status = text("Status").unwrap_or_default();
            button.set_visible(status != "Passive");
            let icon = if status == "NeedsAttention" {
                text("AttentionIconName").or_else(|| text("IconName"))
            } else {
                text("IconName")
            };
            match icon.filter(|i| !i.is_empty()) {
                Some(name) => button.set_icon_name(&name),
                None => button.set_label(&text("Title").unwrap_or_else(|| "•".into())),
            }
            if let Some(title) = text("Title") {
                button.set_tooltip_text(Some(&title));
            }
        },
    );
}

fn call(proxy: &gio::DBusProxy, method: &str) {
    proxy.call(
        method,
        Some(&(0i32, 0i32).to_variant()),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
        |_| {},
    );
}

#[cfg(test)]
mod tests {
    use super::split_service;

    #[test]
    fn service_forms() {
        assert_eq!(
            split_service(":1.5", "/org/ayatana/x"),
            (":1.5".into(), "/org/ayatana/x".into())
        );
        assert_eq!(
            split_service(":1.5", "org.x.Y"),
            ("org.x.Y".into(), "/StatusNotifierItem".into())
        );
        assert_eq!(
            split_service(":1.5", ":1.9/Item"),
            (":1.9".into(), "/Item".into())
        );
    }
}
