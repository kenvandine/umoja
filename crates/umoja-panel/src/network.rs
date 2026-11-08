//! Network status from NetworkManager, the way gnome-shell's network
//! indicator shows it: one icon for the primary connection (Wi-Fi signal
//! strength, wired, mobile...), a separate VPN icon, plus the Wi-Fi and
//! Wired tiles and the network list in Quick Settings.

use crate::bus::{self, Props};
use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use umoja_common::i18n;

pub const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const SETTINGS_CONN: &str = "org.freedesktop.NetworkManager.Settings.Connection";

// NMDeviceType
const TYPE_ETHERNET: u32 = 1;
const TYPE_WIFI: u32 = 2;
// NMDeviceState
pub const DEV_DISCONNECTED: u32 = 30;
pub const DEV_ACTIVATED: u32 = 100;
// NMConnectivityState
const CONNECTIVITY_FULL: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wifi,
    Wired,
    Vpn,
    Mobile,
    Bluetooth,
    Other,
}

impl Kind {
    fn from_nm(t: &str) -> Self {
        match t {
            "802-11-wireless" => Kind::Wifi,
            "802-3-ethernet" | "pppoe" => Kind::Wired,
            "vpn" | "wireguard" => Kind::Vpn,
            "gsm" | "cdma" => Kind::Mobile,
            "bluetooth" => Kind::Bluetooth,
            _ => Kind::Other,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WifiDevice {
    pub path: String,
    pub state: u32,
    pub ssid: Option<String>,
    pub strength: u8,
}

#[derive(Debug, Clone, Default)]
pub struct WiredDevice {
    pub path: String,
    pub state: u32,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActiveConn {
    pub kind: Kind,
    pub id: String,
    pub activating: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NetState {
    pub running: bool,
    pub wireless_enabled: bool,
    pub wireless_hw_enabled: bool,
    /// 0 when connectivity checking is off, which we treat as "full".
    pub connectivity: u32,
    pub wifi: Option<WifiDevice>,
    pub wired: Option<WiredDevice>,
    pub primary: Option<ActiveConn>,
    pub vpn: Option<String>,
}

impl NetState {
    fn limited(&self) -> bool {
        self.connectivity != 0 && self.connectivity < CONNECTIVITY_FULL
    }

    /// The top bar icon, as gnome-shell picks it. `None` hides the icon.
    pub fn icon(&self) -> Option<String> {
        if !self.running {
            return None;
        }
        let wifi_icon = |w: &WifiDevice, activating: bool| {
            if activating {
                "network-wireless-acquiring-symbolic".to_string()
            } else if self.limited() {
                "network-wireless-no-route-symbolic".to_string()
            } else {
                signal_icon(w.strength)
            }
        };
        let wired_icon = |activating: bool| {
            if activating {
                "network-wired-acquiring-symbolic"
            } else if self.limited() {
                "network-wired-no-route-symbolic"
            } else {
                "network-wired-symbolic"
            }
            .to_string()
        };
        if let Some(p) = &self.primary {
            match p.kind {
                Kind::Wifi => {
                    let w = self.wifi.clone().unwrap_or_default();
                    return Some(wifi_icon(&w, p.activating));
                }
                Kind::Wired => return Some(wired_icon(p.activating)),
                Kind::Mobile => return Some("network-cellular-connected-symbolic".into()),
                Kind::Bluetooth => return Some("bluetooth-active-symbolic".into()),
                // A VPN as primary rides on some device; show that device.
                Kind::Vpn | Kind::Other => {}
            }
        }
        if let Some(w) = self.wifi.as_ref().filter(|w| w.state == DEV_ACTIVATED) {
            return Some(wifi_icon(w, false));
        }
        if self
            .wired
            .as_ref()
            .is_some_and(|w| w.state == DEV_ACTIVATED)
        {
            return Some(wired_icon(false));
        }
        if let Some(w) = self.wifi.as_ref().filter(|w| w.state > DEV_DISCONNECTED) {
            return Some(wifi_icon(w, true));
        }
        if self
            .wired
            .as_ref()
            .is_some_and(|w| w.state > DEV_DISCONNECTED)
        {
            return Some(wired_icon(true));
        }
        if self.primary.as_ref().is_some_and(|p| p.kind == Kind::Vpn) {
            return Some("network-vpn-symbolic".into());
        }
        if self.wifi.is_some() && self.wireless_enabled && self.wireless_hw_enabled {
            return Some("network-wireless-offline-symbolic".into());
        }
        if self
            .wired
            .as_ref()
            .is_some_and(|w| w.state == DEV_DISCONNECTED)
        {
            return Some("network-wired-disconnected-symbolic".into());
        }
        None
    }

    pub fn tooltip(&self) -> Option<String> {
        match &self.primary {
            Some(p) => Some(p.id.clone()),
            None if self.running => Some(i18n::shell("Disconnected")),
            None => None,
        }
    }
}

/// gnome-shell's signalToIcon().
pub fn signal_icon(strength: u8) -> String {
    let level = match strength {
        0..20 => "none",
        20..40 => "weak",
        40..50 => "ok",
        50..80 => "good",
        _ => "excellent",
    };
    format!("network-wireless-signal-{level}-symbolic")
}

#[derive(Debug, Clone)]
pub struct AccessPoint {
    pub path: String,
    pub ssid: String,
    pub strength: u8,
    pub secure: bool,
}

type Listener = Box<dyn Fn(&NetState)>;

/// Shared NetworkManager model, refreshed on every NM PropertiesChanged.
pub struct Network {
    conn: Option<gio::DBusConnection>,
    state: RefCell<NetState>,
    listeners: RefCell<Vec<Listener>>,
    pending: Cell<bool>,
}

thread_local! {
    static NETWORK: Rc<Network> = Network::start();
}

impl Network {
    pub fn get() -> Rc<Self> {
        NETWORK.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let net = Rc::new(Self {
            conn: bus::system_bus(),
            state: RefCell::default(),
            listeners: RefCell::default(),
            pending: Cell::new(false),
        });
        let Some(conn) = net.conn.clone() else {
            return net;
        };
        // Both the standard PropertiesChanged and NM's legacy per-interface
        // one; between them every device, AP and connection change arrives.
        let weak = Rc::downgrade(&net);
        #[allow(deprecated)]
        conn.signal_subscribe(
            Some(NM),
            None,
            Some("PropertiesChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| {
                if let Some(n) = weak.upgrade() {
                    n.schedule_refresh();
                }
            },
        );
        let weak = Rc::downgrade(&net);
        #[allow(deprecated)]
        conn.signal_subscribe(
            Some(NM),
            Some(NM),
            None,
            Some(NM_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| {
                if let Some(n) = weak.upgrade() {
                    n.schedule_refresh();
                }
            },
        );
        let (up, down) = (Rc::downgrade(&net), Rc::downgrade(&net));
        // Leaked on purpose: lives as long as the panel.
        let _ = gio::bus_watch_name_on_connection(
            &conn,
            NM,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, _| {
                if let Some(n) = up.upgrade() {
                    n.schedule_refresh();
                }
            },
            move |_, _| {
                if let Some(n) = down.upgrade() {
                    n.schedule_refresh();
                }
            },
        );
        net.refresh();
        net
    }

    pub fn state(&self) -> NetState {
        self.state.borrow().clone()
    }

    /// Calls `f` now and after every change.
    pub fn subscribe(&self, f: impl Fn(&NetState) + 'static) {
        f(&self.state.borrow());
        self.listeners.borrow_mut().push(Box::new(f));
    }

    /// NM sends bursts of signals (one per property per object); coalesce.
    fn schedule_refresh(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            if let Some(n) = weak.upgrade() {
                n.pending.set(false);
                n.refresh();
            }
        });
    }

    fn refresh(&self) {
        let state = self.conn.as_ref().map(read_state).unwrap_or_default();
        *self.state.borrow_mut() = state;
        let state = self.state.borrow();
        for l in self.listeners.borrow().iter() {
            l(&state);
        }
    }

    pub fn set_wireless_enabled(&self, on: bool) {
        if let Some(c) = &self.conn {
            bus::set_property(c, NM, NM_PATH, NM, "WirelessEnabled", on.to_variant());
        }
    }

    pub fn request_scan(&self) {
        let (Some(c), Some(w)) = (&self.conn, self.state.borrow().wifi.clone()) else {
            return;
        };
        let opts: HashMap<String, glib::Variant> = HashMap::new();
        bus::call(
            c,
            NM,
            &w.path,
            WIRELESS,
            "RequestScan",
            Some((opts,).to_variant()),
        );
    }

    /// Visible networks, one per SSID (strongest first), plus the saved
    /// connection for each SSID that has one.
    pub fn access_points(&self) -> (Vec<AccessPoint>, HashMap<String, String>) {
        let (Some(c), Some(w)) = (&self.conn, self.state.borrow().wifi.clone()) else {
            return Default::default();
        };
        let paths = c
            .call_sync(
                Some(NM),
                &w.path,
                WIRELESS,
                "GetAllAccessPoints",
                None,
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .map(|v| {
                v.child_value(0)
                    .iter()
                    .filter_map(|p| p.str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut by_ssid: HashMap<String, AccessPoint> = HashMap::new();
        for path in paths {
            let Some(p) = bus::get_all(c, NM, &path, ACCESS_POINT) else {
                continue;
            };
            let ssid = String::from_utf8_lossy(&p.bytes("Ssid")).into_owned();
            if ssid.is_empty() {
                continue; // hidden network
            }
            let ap = AccessPoint {
                path,
                strength: p.u8("Strength").unwrap_or(0),
                secure: p.u32("Flags").unwrap_or(0) & 1 != 0
                    || p.u32("WpaFlags").unwrap_or(0) != 0
                    || p.u32("RsnFlags").unwrap_or(0) != 0,
                ssid: ssid.clone(),
            };
            match by_ssid.get(&ssid) {
                Some(old) if old.strength >= ap.strength => {}
                _ => {
                    by_ssid.insert(ssid, ap);
                }
            }
        }
        let mut aps: Vec<_> = by_ssid.into_values().collect();
        aps.sort_by(|a, b| b.strength.cmp(&a.strength).then(a.ssid.cmp(&b.ssid)));
        (aps, saved_wifi(c))
    }

    /// Connect the way gnome-shell does: reuse a saved profile, create one
    /// for open networks, and hand secured new networks to Settings, which
    /// asks for the password.
    pub fn connect_to(&self, ap: &AccessPoint, saved: Option<&str>) {
        let (Some(c), Some(w)) = (&self.conn, self.state.borrow().wifi.clone()) else {
            return;
        };
        let dev = bus::object_path(&w.path);
        let specific = bus::object_path(&ap.path);
        if let Some(profile) = saved {
            let args = (bus::object_path(profile), dev, specific).to_variant();
            bus::call(c, NM, NM_PATH, NM, "ActivateConnection", Some(args));
        } else if !ap.secure {
            let empty: HashMap<String, HashMap<String, glib::Variant>> = HashMap::new();
            let args = (empty, dev, specific).to_variant();
            bus::call(c, NM, NM_PATH, NM, "AddAndActivateConnection", Some(args));
        } else {
            crate::quick_settings::open_settings("wifi");
        }
    }

    /// The Wired tile: disconnect when up, otherwise let NM pick a profile.
    pub fn toggle_wired(&self) {
        let (Some(c), Some(w)) = (&self.conn, self.state.borrow().wired.clone()) else {
            return;
        };
        if w.state > DEV_DISCONNECTED {
            bus::call(c, NM, &w.path, DEVICE, "Disconnect", None);
        } else {
            let args = (
                bus::object_path("/"),
                bus::object_path(&w.path),
                bus::object_path("/"),
            )
                .to_variant();
            bus::call(c, NM, NM_PATH, NM, "ActivateConnection", Some(args));
        }
    }
}

fn read_state(c: &gio::DBusConnection) -> NetState {
    let Some(root) = bus::get_all(c, NM, NM_PATH, NM) else {
        return NetState::default();
    };
    let mut s = NetState {
        running: true,
        wireless_enabled: root.bool("WirelessEnabled").unwrap_or(false),
        wireless_hw_enabled: root.bool("WirelessHardwareEnabled").unwrap_or(true),
        connectivity: root.u32("Connectivity").unwrap_or(0),
        ..Default::default()
    };

    let active = |path: &str| bus::get_all(c, NM, path, ACTIVE);
    let primary_path = root
        .obj("PrimaryConnection")
        .or_else(|| root.obj("ActivatingConnection"));
    if let Some(p) = primary_path.as_deref().and_then(active) {
        s.primary = Some(active_conn(&p));
    }
    for path in root.objs("ActiveConnections") {
        if let Some(p) = active(&path) {
            let a = active_conn(&p);
            if a.kind == Kind::Vpn && !a.activating {
                s.vpn = Some(a.id);
            }
        }
    }

    for path in root.objs("Devices") {
        let Some(d) = bus::get_all(c, NM, &path, DEVICE) else {
            continue;
        };
        if d.bool("Managed") == Some(false) {
            continue;
        }
        let state = d.u32("State").unwrap_or(0);
        let conn_name = d
            .obj("ActiveConnection")
            .and_then(|a| active(&a))
            .and_then(|a| a.str("Id"));
        match d.u32("DeviceType") {
            Some(TYPE_WIFI) if s.wifi.is_none() => {
                let mut w = WifiDevice {
                    path: path.clone(),
                    state,
                    ..Default::default()
                };
                if let Some(ap) = bus::get_all(c, NM, &path, WIRELESS)
                    .and_then(|wl| wl.obj("ActiveAccessPoint"))
                    .and_then(|ap| bus::get_all(c, NM, &ap, ACCESS_POINT))
                {
                    w.strength = ap.u8("Strength").unwrap_or(0);
                    w.ssid = Some(String::from_utf8_lossy(&ap.bytes("Ssid")).into_owned())
                        .filter(|s| !s.is_empty());
                }
                if w.ssid.is_none() && state == DEV_ACTIVATED {
                    w.ssid = conn_name;
                }
                s.wifi = Some(w);
            }
            Some(TYPE_ETHERNET) if s.wired.is_none() || state > s.wired.as_ref().unwrap().state => {
                s.wired = Some(WiredDevice {
                    path,
                    state,
                    name: conn_name,
                });
            }
            _ => {}
        }
    }
    s
}

fn active_conn(p: &Props) -> ActiveConn {
    let kind = if p.bool("Vpn") == Some(true) {
        Kind::Vpn
    } else {
        Kind::from_nm(&p.str("Type").unwrap_or_default())
    };
    ActiveConn {
        kind,
        id: p.str("Id").unwrap_or_default(),
        // NMActiveConnectionState: 1 activating, 2 activated
        activating: p.u32("State") == Some(1),
    }
}

/// SSID -> saved connection path for every Wi-Fi profile.
fn saved_wifi(c: &gio::DBusConnection) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(list) = c.call_sync(
        Some(NM),
        NM_SETTINGS_PATH,
        "org.freedesktop.NetworkManager.Settings",
        "ListConnections",
        None,
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    ) else {
        return out;
    };
    for path in list
        .child_value(0)
        .iter()
        .filter_map(|p| p.str().map(String::from))
    {
        let Ok(reply) = c.call_sync(
            Some(NM),
            &path,
            SETTINGS_CONN,
            "GetSettings",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        ) else {
            continue;
        };
        // a{sa{sv}}: not an a{sv}, so VariantDict would abort the panel.
        let settings = reply.child_value(0);
        let Some(wifi) = settings.iter().find_map(|e| {
            (e.child_value(0).str() == Some("802-11-wireless")).then(|| e.child_value(1))
        }) else {
            continue;
        };
        let wifi = glib::VariantDict::new(Some(&wifi));
        if let Some(ssid) = wifi
            .lookup_value("ssid", None)
            .and_then(|v| v.get::<Vec<u8>>())
        {
            out.entry(String::from_utf8_lossy(&ssid).into_owned())
                .or_insert(path);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Widgets

/// Top bar icons: the primary connection and, when one is up, the VPN.
pub fn indicators() -> (gtk::Image, gtk::Image) {
    let main = gtk::Image::new();
    let vpn = gtk::Image::from_icon_name("network-vpn-symbolic");
    Network::get().subscribe({
        let (main, vpn) = (main.clone(), vpn.clone());
        move |s| {
            match s.icon() {
                Some(icon) => {
                    main.set_icon_name(Some(&icon));
                    main.set_visible(true);
                }
                None => main.set_visible(false),
            }
            main.set_tooltip_text(s.tooltip().as_deref());
            vpn.set_visible(s.vpn.is_some());
            vpn.set_tooltip_text(s.vpn.as_deref());
        }
    });
    (main, vpn)
}

/// Wi-Fi tile (toggle + arrow) and the network list it reveals.
pub fn wifi_tile(tile: impl Fn(&str, &str) -> gtk::ToggleButton) -> (gtk::Box, gtk::Revealer) {
    let toggle = tile("network-wireless-symbolic", &i18n::control_center("Wi-Fi"));
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

    let net = Network::get();
    let updating = Rc::new(Cell::new(false));
    net.subscribe({
        let (toggle, arrow, split, revealer, updating) = (
            toggle.clone(),
            arrow.clone(),
            split.clone(),
            revealer.clone(),
            updating.clone(),
        );
        move |s| {
            let Some(w) = &s.wifi else {
                split.set_visible(false);
                return;
            };
            split.set_visible(true);
            updating.set(true);
            toggle.set_active(s.wireless_enabled);
            updating.set(false);
            toggle.set_sensitive(s.wireless_hw_enabled);
            arrow.set_sensitive(s.wireless_enabled);
            if !s.wireless_enabled {
                arrow.set_active(false);
                revealer.set_reveal_child(false);
            }
            let title = match (&w.ssid, w.state) {
                (Some(ssid), DEV_ACTIVATED) => ssid.clone(),
                _ => i18n::control_center("Wi-Fi"),
            };
            set_tile_label(&toggle, &title);
            set_tile_icon(
                &toggle,
                &if w.state == DEV_ACTIVATED {
                    signal_icon(w.strength)
                } else {
                    "network-wireless-symbolic".into()
                },
            );
            if revealer.reveals_child() {
                fill_networks(&list_of(&revealer), s);
            }
        }
    });
    toggle.connect_toggled({
        let net = net.clone();
        move |b| {
            if !updating.get() {
                net.set_wireless_enabled(b.is_active());
            }
        }
    });
    arrow.connect_toggled({
        let revealer = revealer.clone();
        move |b| {
            let open = b.is_active();
            if open {
                let net = Network::get();
                net.request_scan();
                fill_networks(&list_of(&revealer), &net.state());
            }
            revealer.set_reveal_child(open);
        }
    });
    (split, revealer)
}

fn list_of(revealer: &gtk::Revealer) -> gtk::Box {
    revealer.child().and_downcast().expect("network list")
}

fn fill_networks(list: &gtk::Box, s: &NetState) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let header = gtk::Label::builder()
        .label(i18n::shell("Wi-Fi"))
        .xalign(0.0)
        .css_classes(["heading", "network-list-header"])
        .build();
    list.append(&header);

    let net = Network::get();
    let (aps, saved) = net.access_points();
    let current = s
        .wifi
        .as_ref()
        .filter(|w| w.state == DEV_ACTIVATED)
        .and_then(|w| w.ssid.clone());
    for ap in aps.into_iter().take(8) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Image::from_icon_name(&signal_icon(ap.strength)));
        row.append(
            &gtk::Label::builder()
                .label(&ap.ssid)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        if current.as_deref() == Some(ap.ssid.as_str()) {
            row.append(&gtk::Image::from_icon_name("object-select-symbolic"));
        } else if ap.secure {
            row.append(&gtk::Image::from_icon_name(
                "network-wireless-encrypted-symbolic",
            ));
        }
        let button = gtk::Button::builder()
            .child(&row)
            .css_classes(["flat", "network-row"])
            .build();
        let profile = saved.get(&ap.ssid).cloned();
        button.connect_clicked(move |_| Network::get().connect_to(&ap, profile.as_deref()));
        list.append(&button);
    }
    let all = gtk::Button::builder()
        .label(i18n::shell("All Networks"))
        .css_classes(["flat", "network-row"])
        .build();
    if let Some(l) = all.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    all.connect_clicked(|_| crate::quick_settings::open_settings("wifi"));
    list.append(&all);
}

/// Wired tile, shown only when there is an Ethernet device.
pub fn wired_tile(tile: impl Fn(&str, &str) -> gtk::ToggleButton) -> gtk::ToggleButton {
    let t = tile("network-wired-symbolic", &i18n::control_center("Wired"));
    let updating = Rc::new(Cell::new(false));
    Network::get().subscribe({
        let (t, updating) = (t.clone(), updating.clone());
        move |s| {
            let Some(w) = s.wired.as_ref().filter(|w| w.state >= DEV_DISCONNECTED) else {
                t.set_visible(false);
                return;
            };
            t.set_visible(true);
            updating.set(true);
            t.set_active(w.state == DEV_ACTIVATED);
            updating.set(false);
            let title = match (&w.name, w.state) {
                (Some(n), DEV_ACTIVATED) => n.clone(),
                _ => i18n::control_center("Wired"),
            };
            set_tile_label(&t, &title);
        }
    });
    t.connect_toggled(move |_| {
        if !updating.get() {
            Network::get().toggle_wired();
        }
    });
    t
}

fn tile_content(t: &gtk::ToggleButton) -> Option<gtk::Box> {
    t.child().and_downcast()
}

pub fn set_tile_label(t: &gtk::ToggleButton, text: &str) {
    if let Some(l) = tile_content(t)
        .and_then(|b| b.last_child())
        .and_downcast::<gtk::Label>()
    {
        l.set_label(text);
    }
}

pub fn set_tile_icon(t: &gtk::ToggleButton, icon: &str) {
    if let Some(i) = tile_content(t)
        .and_then(|b| b.first_child())
        .and_downcast::<gtk::Image>()
    {
        i.set_icon_name(Some(icon));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wifi(state: u32, strength: u8) -> Option<WifiDevice> {
        Some(WifiDevice {
            path: "/d/1".into(),
            state,
            ssid: Some("home".into()),
            strength,
        })
    }

    fn primary(kind: Kind, activating: bool) -> Option<ActiveConn> {
        Some(ActiveConn {
            kind,
            id: "x".into(),
            activating,
        })
    }

    #[test]
    fn signal_levels() {
        assert_eq!(signal_icon(5), "network-wireless-signal-none-symbolic");
        assert_eq!(signal_icon(30), "network-wireless-signal-weak-symbolic");
        assert_eq!(signal_icon(45), "network-wireless-signal-ok-symbolic");
        assert_eq!(signal_icon(70), "network-wireless-signal-good-symbolic");
        assert_eq!(
            signal_icon(95),
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn nm_not_running_hides_icon() {
        assert_eq!(NetState::default().icon(), None);
    }

    #[test]
    fn wifi_connected() {
        let s = NetState {
            running: true,
            connectivity: 4,
            wireless_enabled: true,
            wifi: wifi(DEV_ACTIVATED, 85),
            primary: primary(Kind::Wifi, false),
            ..Default::default()
        };
        assert_eq!(
            s.icon().unwrap(),
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn wifi_limited_and_connecting() {
        let mut s = NetState {
            running: true,
            connectivity: 3,
            wireless_enabled: true,
            wifi: wifi(DEV_ACTIVATED, 85),
            primary: primary(Kind::Wifi, false),
            ..Default::default()
        };
        assert_eq!(s.icon().unwrap(), "network-wireless-no-route-symbolic");
        s.primary = primary(Kind::Wifi, true);
        assert_eq!(s.icon().unwrap(), "network-wireless-acquiring-symbolic");
    }

    #[test]
    fn wired_and_vpn() {
        let s = NetState {
            running: true,
            wired: Some(WiredDevice {
                path: "/d/2".into(),
                state: DEV_ACTIVATED,
                name: None,
            }),
            primary: primary(Kind::Vpn, false),
            vpn: Some("work".into()),
            ..Default::default()
        };
        assert_eq!(s.icon().unwrap(), "network-wired-symbolic");
    }

    #[test]
    fn disconnected() {
        let mut s = NetState {
            running: true,
            wireless_enabled: true,
            wireless_hw_enabled: true,
            wifi: wifi(DEV_DISCONNECTED, 0),
            ..Default::default()
        };
        assert_eq!(s.icon().unwrap(), "network-wireless-offline-symbolic");
        s.wireless_enabled = false;
        assert_eq!(s.icon(), None);
    }
}
