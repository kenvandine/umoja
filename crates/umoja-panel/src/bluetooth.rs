//! Bluetooth through BlueZ, like gnome-shell's Bluetooth quick toggle: the
//! tile powers the adapter on and off, its arrow lists paired devices
//! (click to connect or disconnect), and the top bar shows the Bluetooth
//! icon while a device is connected.

use crate::bus::{self, Props};
use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::i18n;

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Device {
    pub path: String,
    pub name: String,
    pub icon: String,
    pub connected: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BtState {
    /// Object path of the (first) adapter, if there is one.
    pub adapter: Option<String>,
    pub powered: bool,
    /// Paired devices, connected first, then by name.
    pub devices: Vec<Device>,
}

impl BtState {
    pub fn connected(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|d| d.connected)
    }

    /// gnome-shell's tile subtitle: the device name for one connected
    /// device, a count for several.
    /// The tile's second line: the connected device, or how many.
    pub fn subtitle(&self) -> String {
        if self.connected().next().is_none() {
            String::new()
        } else {
            self.title()
        }
    }

    pub fn title(&self) -> String {
        let connected: Vec<_> = self.connected().collect();
        match connected.as_slice() {
            [one] => one.name.clone(),
            [] => i18n::control_center("Bluetooth"),
            many => format!("{} {}", many.len(), i18n::shell("Connected")),
        }
    }
}

/// Builds the state from `GetManagedObjects` output.
fn parse_objects(v: &glib::Variant) -> BtState {
    let mut s = BtState::default();
    for entry in v.iter() {
        let path = entry.child_value(0).str().unwrap_or_default().to_string();
        let ifaces = entry.child_value(1);
        for iface in ifaces.iter() {
            let name = iface.child_value(0).str().unwrap_or_default().to_string();
            let props = Props::from_dict(&iface.child_value(1));
            match name.as_str() {
                ADAPTER if s.adapter.is_none() => {
                    s.adapter = Some(path.clone());
                    s.powered = props.bool("Powered").unwrap_or(false);
                }
                DEVICE if props.bool("Paired") == Some(true) => s.devices.push(Device {
                    path: path.clone(),
                    name: props
                        .str("Alias")
                        .or_else(|| props.str("Name"))
                        .unwrap_or_else(|| path.clone()),
                    icon: props.str("Icon").unwrap_or_default(),
                    connected: props.bool("Connected").unwrap_or(false),
                }),
                _ => {}
            }
        }
    }
    s.devices
        .sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
    s
}

type Listener = Box<dyn Fn(&BtState)>;

pub struct Bluetooth {
    conn: Option<gio::DBusConnection>,
    state: RefCell<BtState>,
    listeners: RefCell<Vec<Listener>>,
    pending: Cell<bool>,
}

thread_local! {
    static BT: Rc<Bluetooth> = Bluetooth::start();
}

impl Bluetooth {
    pub fn get() -> Rc<Self> {
        BT.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let bt = Rc::new(Self {
            conn: bus::system_bus(),
            state: RefCell::default(),
            listeners: RefCell::default(),
            pending: Cell::new(false),
        });
        let Some(conn) = bt.conn.clone() else {
            return bt;
        };
        // PropertiesChanged, InterfacesAdded/Removed: anything from BlueZ.
        let weak = Rc::downgrade(&bt);
        #[allow(deprecated)]
        conn.signal_subscribe(
            Some(BLUEZ),
            None,
            None,
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| {
                if let Some(b) = weak.upgrade() {
                    b.schedule_refresh();
                }
            },
        );
        let (up, down) = (Rc::downgrade(&bt), Rc::downgrade(&bt));
        let _ = gio::bus_watch_name_on_connection(
            &conn,
            BLUEZ,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if let Some(b) = up.upgrade() {
                    b.schedule_refresh();
                }
            },
            move |_, _| {
                if let Some(b) = down.upgrade() {
                    b.schedule_refresh();
                }
            },
        );
        bt.refresh();
        bt
    }

    pub fn subscribe(&self, f: impl Fn(&BtState) + 'static) {
        f(&self.state.borrow());
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn schedule_refresh(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            if let Some(b) = weak.upgrade() {
                b.pending.set(false);
                b.refresh();
            }
        });
    }

    fn refresh(&self) {
        let state = self
            .conn
            .as_ref()
            .and_then(|c| {
                c.call_sync(
                    Some(BLUEZ),
                    "/",
                    "org.freedesktop.DBus.ObjectManager",
                    "GetManagedObjects",
                    None,
                    None,
                    gio::DBusCallFlags::NO_AUTO_START,
                    1000,
                    gio::Cancellable::NONE,
                )
                .ok()
            })
            .map(|v| parse_objects(&v.child_value(0)))
            .unwrap_or_default();
        if *self.state.borrow() == state {
            return;
        }
        *self.state.borrow_mut() = state;
        let state = self.state.borrow();
        for l in self.listeners.borrow().iter() {
            l(&state);
        }
    }

    pub fn set_powered(&self, on: bool) {
        let (Some(c), Some(a)) = (&self.conn, self.state.borrow().adapter.clone()) else {
            return;
        };
        bus::set_property(c, BLUEZ, &a, ADAPTER, "Powered", on.to_variant());
    }

    pub fn toggle_device(&self, d: &Device) {
        let Some(c) = &self.conn else { return };
        let method = if d.connected { "Disconnect" } else { "Connect" };
        tracing::info!(device = d.name, method, "bluetooth");
        bus::call(c, BLUEZ, &d.path, DEVICE, method, None);
    }
}

/// Top bar icon, shown while a device is connected.
pub fn indicator() -> gtk::Image {
    let img = gtk::Image::from_icon_name("bluetooth-active-symbolic");
    Bluetooth::get().subscribe({
        let img = img.clone();
        move |s| {
            img.set_visible(s.powered && s.connected().next().is_some());
            img.set_tooltip_text(Some(&s.title()));
        }
    });
    img
}

/// The Bluetooth tile (toggle + arrow) and the device list it reveals.
pub fn tile(make: impl Fn(&str, &str) -> gtk::ToggleButton) -> (gtk::Box, gtk::Revealer) {
    let toggle = make(
        "bluetooth-active-symbolic",
        &i18n::control_center("Bluetooth"),
    );
    toggle.set_hexpand(true);
    let arrow = gtk::ToggleButton::builder()
        .icon_name("go-next-symbolic")
        .css_classes(["tile", "tile-arrow"])
        .build();
    let split = gtk::Box::builder()
        .css_classes(["linked", "split-tile"])
        .build();
    split.append(&toggle);
    split.append(&arrow);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("network-list");
    let revealer = gtk::Revealer::builder()
        .child(&list)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();

    let updating = Rc::new(Cell::new(false));
    Bluetooth::get().subscribe({
        let (toggle, arrow, split, revealer, list, updating) = (
            toggle.clone(),
            arrow.clone(),
            split.clone(),
            revealer.clone(),
            list.clone(),
            updating.clone(),
        );
        move |s| {
            split.set_visible(s.adapter.is_some());
            updating.set(true);
            toggle.set_active(s.powered);
            updating.set(false);
            crate::network::set_tile_subtitle(&toggle, &s.subtitle());
            crate::network::set_tile_icon(
                &toggle,
                if s.powered {
                    "bluetooth-active-symbolic"
                } else {
                    "bluetooth-disabled-symbolic"
                },
            );
            arrow.set_sensitive(s.powered);
            if !s.powered {
                arrow.set_active(false);
                revealer.set_reveal_child(false);
            }
            fill_devices(&list, s);
        }
    });
    toggle.connect_toggled(move |b| {
        if !updating.get() {
            Bluetooth::get().set_powered(b.is_active());
        }
    });
    arrow.connect_toggled({
        let revealer = revealer.clone();
        move |b| revealer.set_reveal_child(b.is_active())
    });
    (split, revealer)
}

fn fill_devices(list: &gtk::Box, s: &BtState) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    list.append(
        &gtk::Label::builder()
            .label(i18n::shell("Devices"))
            .xalign(0.0)
            .css_classes(["heading", "network-list-header"])
            .build(),
    );
    if s.devices.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label(i18n::shell("No Paired Devices"))
                .xalign(0.0)
                .css_classes(["dim-label", "network-list-header"])
                .build(),
        );
    }
    for d in &s.devices {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = if d.icon.is_empty() {
            "bluetooth-active-symbolic".to_string()
        } else {
            format!("{}-symbolic", d.icon)
        };
        row.append(&gtk::Image::from_gicon(&gio::ThemedIcon::from_names(&[
            icon.as_str(),
            "bluetooth-active-symbolic",
        ])));
        row.append(
            &gtk::Label::builder()
                .label(&d.name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        if d.connected {
            row.append(&gtk::Image::from_icon_name("object-select-symbolic"));
        }
        let button = gtk::Button::builder()
            .child(&row)
            .css_classes(["flat", "network-row"])
            .tooltip_text(if d.connected {
                i18n::shell("Disconnect")
            } else {
                i18n::shell("Connect")
            })
            .build();
        let dev = d.clone();
        button.connect_clicked(move |_| Bluetooth::get().toggle_device(&dev));
        list.append(&button);
    }
    let settings = gtk::Button::builder()
        .label(i18n::shell("Bluetooth Settings"))
        .css_classes(["flat", "network-row"])
        .build();
    if let Some(l) = settings.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    settings.connect_clicked(|b| {
        crate::quick_settings::close_menu(b);
        crate::quick_settings::open_settings("bluetooth");
    });
    list.append(&settings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_managed_objects() {
        type Ifaces = HashMap<String, HashMap<String, glib::Variant>>;
        let mut objects: HashMap<glib::variant::ObjectPath, Ifaces> = HashMap::new();
        let p = |k: &str, v: glib::Variant| (k.to_string(), v);
        objects.insert(
            bus::object_path("/org/bluez/hci0"),
            [(
                ADAPTER.to_string(),
                [p("Powered", true.to_variant())].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
        );
        for (path, name, connected, paired) in [
            ("/org/bluez/hci0/dev_1", "Zed", false, true),
            ("/org/bluez/hci0/dev_2", "Buds", true, true),
            ("/org/bluez/hci0/dev_3", "Stranger", false, false),
        ] {
            objects.insert(
                bus::object_path(path),
                [(
                    DEVICE.to_string(),
                    [
                        p("Alias", name.to_variant()),
                        p("Connected", connected.to_variant()),
                        p("Paired", paired.to_variant()),
                        p("Icon", "audio-headphones".to_variant()),
                    ]
                    .into_iter()
                    .collect(),
                )]
                .into_iter()
                .collect(),
            );
        }
        let s = parse_objects(&objects.to_variant());
        assert_eq!(s.adapter.as_deref(), Some("/org/bluez/hci0"));
        assert!(s.powered);
        let names: Vec<_> = s.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Buds", "Zed"]);
        assert_eq!(s.title(), "Buds");
    }
}
