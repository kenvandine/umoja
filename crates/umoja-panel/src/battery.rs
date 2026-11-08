//! Battery status from UPower's DisplayDevice, drawn like gnome-shell:
//! `battery-level-N[-charging]-symbolic` icons and, when
//! `org.gnome.desktop.interface show-battery-percentage` is on, the
//! percentage next to it.

use gio::prelude::*;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::RefCell;
use std::rc::Rc;
use umoja_common::{gsettings as gs, i18n};

const UPOWER: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";

// UpDeviceKind / UpDeviceState
const KIND_BATTERY: u32 = 2;
const STATE_CHARGING: u32 = 1;
const STATE_DISCHARGING: u32 = 2;
const STATE_FULLY_CHARGED: u32 = 4;
const STATE_PENDING_CHARGE: u32 = 5;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryState {
    pub present: bool,
    pub percentage: f64,
    pub state: u32,
    pub time_to_empty: i64,
    pub time_to_full: i64,
}

impl BatteryState {
    fn charging(&self) -> bool {
        self.state == STATE_CHARGING
    }

    /// gnome-shell's power indicator icon.
    pub fn icon(&self) -> String {
        let level = (10.0 * (self.percentage / 10.0).floor()).clamp(0.0, 100.0) as u32;
        let charged = self.state == STATE_FULLY_CHARGED || (self.charging() && level == 100);
        if charged {
            "battery-level-100-charged-symbolic".into()
        } else {
            let charging = if self.charging() || self.state == STATE_PENDING_CHARGE {
                "-charging"
            } else {
                ""
            };
            format!("battery-level-{level}{charging}-symbolic")
        }
    }

    pub fn percent_label(&self) -> String {
        format!("{}\u{2009}%", self.percentage.round() as i64)
    }

    /// "1∶05 Remaining (42 %)", the same wording as gnome-shell.
    pub fn status_text(&self) -> String {
        let pct = self.percentage.round() as i64;
        let fmt = |secs: i64| {
            let mins = (secs as f64 / 60.0).round() as i64;
            format!("{}\u{2236}{:02}", mins / 60, mins % 60)
        };
        match self.state {
            STATE_FULLY_CHARGED => i18n::shell("Fully Charged"),
            STATE_DISCHARGING if self.time_to_empty > 60 => {
                format!(
                    "{} {} ({pct}\u{2009}%)",
                    fmt(self.time_to_empty),
                    i18n::shell("Remaining")
                )
            }
            STATE_CHARGING if self.time_to_full > 60 => {
                format!(
                    "{} {} ({pct}\u{2009}%)",
                    fmt(self.time_to_full),
                    i18n::shell("Until Full")
                )
            }
            STATE_PENDING_CHARGE => format!("{} ({pct}\u{2009}%)", i18n::shell("Not Charging")),
            _ => format!("{pct}\u{2009}%"),
        }
    }
}

type Listener = Box<dyn Fn(&BatteryState)>;

pub struct Battery {
    proxy: Option<gio::DBusProxy>,
    listeners: RefCell<Vec<Listener>>,
}

thread_local! {
    static BATTERY: Rc<Battery> = Battery::start();
}

impl Battery {
    pub fn get() -> Rc<Self> {
        BATTERY.with(Rc::clone)
    }

    fn start() -> Rc<Self> {
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None,
            UPOWER,
            DISPLAY_DEVICE,
            DEVICE_IFACE,
            gio::Cancellable::NONE,
        )
        .inspect_err(|e| tracing::info!(error = %e, "no UPower; battery hidden"))
        .ok();
        let b = Rc::new(Self {
            proxy,
            listeners: RefCell::default(),
        });
        if let Some(p) = &b.proxy {
            let weak = Rc::downgrade(&b);
            p.connect_local("g-properties-changed", false, move |_| {
                if let Some(b) = weak.upgrade() {
                    b.notify();
                }
                None
            });
            // UPower restarting drops the cache; re-read when it comes back.
            let weak = Rc::downgrade(&b);
            p.connect_notify_local(Some("g-name-owner"), move |_, _| {
                if let Some(b) = weak.upgrade() {
                    b.notify();
                }
            });
        }
        b
    }

    pub fn state(&self) -> BatteryState {
        let Some(p) = &self.proxy else {
            return BatteryState::default();
        };
        let get = |k: &str| p.cached_property(k);
        BatteryState {
            present: get("Type").and_then(|v| v.get::<u32>()) == Some(KIND_BATTERY)
                && get("IsPresent")
                    .and_then(|v| v.get::<bool>())
                    .unwrap_or(false),
            percentage: get("Percentage").and_then(|v| v.get()).unwrap_or(0.0),
            state: get("State").and_then(|v| v.get()).unwrap_or(0),
            time_to_empty: get("TimeToEmpty").and_then(|v| v.get()).unwrap_or(0),
            time_to_full: get("TimeToFull").and_then(|v| v.get()).unwrap_or(0),
        }
    }

    pub fn subscribe(&self, f: impl Fn(&BatteryState) + 'static) {
        f(&self.state());
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn notify(&self) {
        let s = self.state();
        for l in self.listeners.borrow().iter() {
            l(&s);
        }
    }
}

/// Top bar battery icon plus the optional percentage label.
pub fn indicator() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let icon = gtk::Image::new();
    let label = gtk::Label::new(None);
    label.add_css_class("battery-percentage");
    b.append(&icon);
    b.append(&label);

    let show_pct = Rc::new(std::cell::Cell::new(gs::get_bool(
        gs::DESKTOP_INTERFACE,
        "show-battery-percentage",
        false,
    )));
    let update = {
        let (b, icon, label, show_pct) = (b.clone(), icon.clone(), label.clone(), show_pct.clone());
        move |s: &BatteryState| {
            b.set_visible(s.present);
            icon.set_icon_name(Some(&s.icon()));
            label.set_label(&s.percent_label());
            label.set_visible(show_pct.get());
            b.set_tooltip_text(Some(&s.status_text()));
        }
    };
    let settings = gs::watch(gs::DESKTOP_INTERFACE, "show-battery-percentage", {
        let update = update.clone();
        move |s: &gio::Settings, _: &str| {
            show_pct.set(s.boolean("show-battery-percentage"));
            update(&Battery::get().state());
        }
    });
    // The panel never drops its indicator; keep the watch alive with it.
    std::mem::forget(settings);
    Battery::get().subscribe(update);
    b
}

/// The battery pill at the top of Quick Settings; opens Settings › Power.
pub fn menu_button() -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let icon = gtk::Image::new();
    let label = gtk::Label::new(None);
    content.append(&icon);
    content.append(&label);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["battery-button"])
        .build();
    button.connect_clicked(|_| crate::quick_settings::open_settings("power"));
    Battery::get().subscribe({
        let button = button.clone();
        move |s| {
            button.set_visible(s.present);
            icon.set_icon_name(Some(&s.icon()));
            label.set_label(&s.percent_label());
            button.set_tooltip_text(Some(&s.status_text()));
        }
    });
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(percentage: f64, state: u32) -> BatteryState {
        BatteryState {
            present: true,
            percentage,
            state,
            ..Default::default()
        }
    }

    #[test]
    fn icons_match_gnome_shell() {
        assert_eq!(
            st(57.0, STATE_DISCHARGING).icon(),
            "battery-level-50-symbolic"
        );
        assert_eq!(
            st(9.0, STATE_DISCHARGING).icon(),
            "battery-level-0-symbolic"
        );
        assert_eq!(
            st(83.0, STATE_CHARGING).icon(),
            "battery-level-80-charging-symbolic"
        );
        assert_eq!(
            st(100.0, STATE_CHARGING).icon(),
            "battery-level-100-charged-symbolic"
        );
        assert_eq!(
            st(98.0, STATE_FULLY_CHARGED).icon(),
            "battery-level-100-charged-symbolic"
        );
        assert_eq!(
            st(100.0, STATE_DISCHARGING).icon(),
            "battery-level-100-symbolic"
        );
    }
}
