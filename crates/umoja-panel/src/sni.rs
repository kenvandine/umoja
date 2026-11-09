//! StatusNotifierItem (AppIndicator) support.
//!
//! umoja-panel owns `org.kde.StatusNotifierWatcher` and is also the host
//! that draws the icons, so tray icons from Steam, Discord, Slack, nm-applet
//! and friends work without any legacy XEmbed wrapper.

use crate::dbusmenu;
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
/// Panel icons are 16 logical pixels, like gnome-shell's.
const ICON_SIZE: i32 = 16;

struct Item {
    button: gtk::Button,
    state: Rc<ItemState>,
    unwatch: Box<dyn FnOnce()>,
}

/// Live state of one tray item.
struct ItemState {
    conn: gio::DBusConnection,
    bus: String,
    proxy: gio::DBusProxy,
    image: gtk::Image,
    button: glib::WeakRef<gtk::Button>,
    menu_path: RefCell<Option<String>>,
    menu: RefCell<Option<Rc<dbusmenu::Menu>>>,
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
                            // Reply first: the item may block on this call,
                            // and registering talks back to it.
                            invocation.return_value(None);
                            let sender = sender.unwrap_or_default().to_string();
                            glib::idle_add_local_once(move || tray.register(&sender, &arg));
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

        let proxy = gio::DBusProxy::new_sync(
            &conn,
            gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
            None,
            Some(&bus),
            &path,
            ITEM_IFACE,
            gio::Cancellable::NONE,
        );
        let Ok(proxy) = proxy else { return };
        let image = gtk::Image::builder().pixel_size(ICON_SIZE).build();
        let button = gtk::Button::builder()
            .child(&image)
            .css_classes(["flat", "tray-item"])
            .visible(false)
            .build();
        let state = Rc::new(ItemState {
            conn: conn.clone(),
            bus: bus.clone(),
            proxy: proxy.clone(),
            image,
            button: button.downgrade(),
            menu_path: RefCell::default(),
            menu: RefCell::default(),
        });
        state.refresh();

        proxy.connect_local("g-signal", false, {
            let weak = Rc::downgrade(&state);
            move |args| {
                let signal = args[2].get::<String>().ok()?;
                if signal.starts_with("New") {
                    weak.upgrade()?.refresh();
                }
                None
            }
        });

        // Like Ubuntu's AppIndicator extension: left or right click opens
        // the menu, Activate only for items without one. Middle click:
        // SecondaryActivate.
        // GtkButton's own gesture claims the primary button, so a left click
        // never reaches the GestureClick below: handle it as "clicked".
        button.connect_clicked({
            let weak = Rc::downgrade(&state);
            move |_| {
                let Some(st) = weak.upgrade() else { return };
                if st.has_menu() {
                    st.show_menu();
                } else {
                    st.activate("Activate", 0, 0);
                }
            }
        });
        let click = gtk::GestureClick::builder().button(0).build();
        click.connect_released({
            let weak = Rc::downgrade(&state);
            move |g, _, x, y| {
                let Some(st) = weak.upgrade() else { return };
                let (rx, ry) = st.root_coords(x, y);
                match g.current_button() {
                    2 => st.call("SecondaryActivate", Some((rx, ry).to_variant())),
                    3 if st.has_menu() => st.show_menu(),
                    3 => st.call("ContextMenu", Some((rx, ry).to_variant())),
                    _ => {}
                }
            }
        });
        button.add_controller(click);

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.connect_scroll({
            let weak = Rc::downgrade(&state);
            move |_, dx, dy| {
                let Some(st) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if dy != 0.0 {
                    st.call(
                        "Scroll",
                        Some(((dy * -120.0) as i32, "vertical").to_variant()),
                    );
                }
                if dx != 0.0 {
                    st.call(
                        "Scroll",
                        Some(((dx * 120.0) as i32, "horizontal").to_variant()),
                    );
                }
                glib::Propagation::Stop
            }
        });
        button.add_controller(scroll);

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
                state,
                unwatch: Box::new(move || gio::bus_unwatch_name(watch)),
            },
        );
        tracing::info!(item = key, "tray item registered");
        self.emit("StatusNotifierItemRegistered", Some(&key));
    }

    fn unregister(&self, key: &str) {
        let removed = self.items.borrow_mut().remove(key);
        if let Some(item) = removed {
            // Unparents the menu popover before its button goes away.
            item.state.menu.borrow_mut().take();
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

impl ItemState {
    fn has_menu(&self) -> bool {
        self.menu_path.borrow().is_some()
    }

    fn root_coords(&self, x: f64, y: f64) -> (i32, i32) {
        // Wayland clients cannot know global coordinates; items only use
        // these to place their own menus, so the local point is good enough.
        (x as i32, y as i32)
    }

    fn call(&self, method: &str, args: Option<glib::Variant>) {
        self.proxy.call(
            method,
            args.as_ref(),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            |_| {},
        );
    }

    /// Activate, falling back to the menu when the item does not implement
    /// it (many AppIndicators only have a menu).
    fn activate(self: &Rc<Self>, method: &str, x: i32, y: i32) {
        let weak = Rc::downgrade(self);
        self.proxy.call(
            method,
            Some(&(x, y).to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |res| {
                if res.is_err()
                    && let Some(st) = weak.upgrade()
                    && st.has_menu()
                {
                    st.show_menu();
                }
            },
        );
    }

    fn show_menu(&self) {
        let Some(path) = self.menu_path.borrow().clone() else {
            return;
        };
        let Some(button) = self.button.upgrade() else {
            return;
        };
        let mut menu = self.menu.borrow_mut();
        if menu.is_none() {
            *menu = dbusmenu::Menu::new(&self.conn, &self.bus, &path, button.upcast_ref());
        }
        if let Some(m) = menu.as_ref() {
            m.popup();
        }
    }

    /// SNI items rarely emit PropertiesChanged, so fetch properties
    /// explicitly on every New* signal.
    fn refresh(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.proxy.call(
            "org.freedesktop.DBus.Properties.GetAll",
            Some(&(ITEM_IFACE,).to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |res| {
                let (Ok(v), Some(st)) = (res, weak.upgrade()) else {
                    return;
                };
                st.apply(&crate::bus::Props::from_dict(&v.child_value(0)));
            },
        );
    }

    fn apply(&self, props: &crate::bus::Props) {
        let Some(button) = self.button.upgrade() else {
            return;
        };
        let text = |k: &str| props.str(k).filter(|s| !s.is_empty());
        let status = text("Status").unwrap_or_default();
        button.set_visible(status != "Passive");
        let menu_path = props.obj("Menu");
        if *self.menu_path.borrow() != menu_path {
            self.menu.borrow_mut().take();
            *self.menu_path.borrow_mut() = menu_path;
        }

        let theme_dir = text("IconThemePath");
        // Electron/Chromium write each tray icon as <IconThemePath>/<IconName>.png
        // and several apps reuse the same name (e.g. "chrome_status_icon_1"),
        // so look in the item's own directory first; the shared icon theme
        // would hand every such app the first one it found.
        let local_file = |name: &str| -> Option<std::path::PathBuf> {
            let dir = std::path::Path::new(theme_dir.as_deref()?);
            ["png", "svg", "xpm"]
                .iter()
                .map(|ext| dir.join(format!("{name}.{ext}")))
                .find(|p| p.is_file())
        };
        if let Some(dir) = theme_dir.as_deref()
            && let Some(display) = gtk::gdk::Display::default()
        {
            let theme = gtk::IconTheme::for_display(&display);
            if !theme
                .search_path()
                .iter()
                .any(|p| p.as_os_str() == dir.as_str())
            {
                theme.add_search_path(&dir);
            }
        }

        let attention = status == "NeedsAttention";
        let (name_key, pixmap_key) = if attention {
            ("AttentionIconName", "AttentionIconPixmap")
        } else {
            ("IconName", "IconPixmap")
        };
        let name = text(name_key).or_else(|| text("IconName"));
        let scale = button.scale_factor();
        let pixmap = props
            .get(pixmap_key)
            .or_else(|| props.get("IconPixmap"))
            .and_then(|v| best_pixmap(v, ICON_SIZE * scale));
        let file = name.as_deref().and_then(local_file);
        match (name, pixmap) {
            (Some(n), _) if n.starts_with('/') => self.image.set_from_file(Some(&n)),
            (Some(_), _) if file.is_some() => self.image.set_from_file(file.as_ref()),
            (Some(n), Some(p)) => {
                let theme = gtk::gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d));
                if theme.is_some_and(|t| t.has_icon(&n)) {
                    self.image.set_icon_name(Some(&n));
                } else {
                    self.image.set_paintable(Some(&p));
                }
            }
            (Some(n), None) => self.image.set_icon_name(Some(&n)),
            (None, Some(p)) => self.image.set_paintable(Some(&p)),
            (None, None) => self
                .image
                .set_icon_name(Some("application-x-executable-symbolic")),
        }

        let tooltip = props
            .get("ToolTip")
            .and_then(tooltip_text)
            .or_else(|| text("Title"));
        button.set_tooltip_markup(tooltip.as_deref());
    }
}

/// `ToolTip` is `(s icon, a(iiay) pixmaps, s title, s description)`.
fn tooltip_text(v: &glib::Variant) -> Option<String> {
    let title = v.try_child_value(2)?.str()?.to_string();
    let body = v
        .try_child_value(3)
        .and_then(|b| b.str().map(String::from))
        .unwrap_or_default();
    let title = glib::markup_escape_text(&title).to_string();
    match (title.is_empty(), body.is_empty()) {
        (true, true) => None,
        (false, true) => Some(title),
        (true, false) => Some(body),
        (false, false) => Some(format!("<b>{title}</b>\n{body}")),
    }
}

/// Picks the smallest `a(iiay)` ARGB32 pixmap at least `size` pixels wide
/// (or the largest one) and converts it to a texture.
fn best_pixmap(v: &glib::Variant, size: i32) -> Option<gtk::gdk::MemoryTexture> {
    let mut pixmaps: Vec<(i32, i32, Vec<u8>)> = v
        .iter()
        .filter_map(|p| {
            let w = p.try_child_value(0)?.get::<i32>()?;
            let h = p.try_child_value(1)?.get::<i32>()?;
            let data = p.try_child_value(2)?.get::<Vec<u8>>()?;
            (w > 0 && h > 0 && data.len() >= (w * h * 4) as usize).then_some((w, h, data))
        })
        .collect();
    pixmaps.sort_by_key(|(w, _, _)| *w);
    let (w, h, data) = pixmaps
        .iter()
        .find(|(w, _, _)| *w >= size)
        .or(pixmaps.last())?
        .clone();
    // SNI pixmaps are ARGB32 in network byte order: A, R, G, B per pixel.
    let bytes = glib::Bytes::from_owned(data);
    Some(gtk::gdk::MemoryTexture::new(
        w,
        h,
        gtk::gdk::MemoryFormat::A8r8g8b8,
        &bytes,
        (w * 4) as usize,
    ))
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
