//! The Ubuntu Quick Settings menu.
//!
//! Every toggle reads and writes the same GSettings keys / D-Bus services as
//! GNOME, so flipping Dark Style here or in gnome-control-center is the same
//! operation and both stay in sync.

use crate::{battery, network, volume};
use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;
use umoja_common::{gsettings as gs, i18n};

const POWER_PROFILES_BUS: &str = "net.hadess.PowerProfiles";
const POWER_PROFILES_PATH: &str = "/net/hadess/PowerProfiles";

/// The system status area: network, VPN, volume and battery icons (or the
/// power icon on machines without a battery), opening Quick Settings.
pub fn button() -> gtk::MenuButton {
    let popover = gtk::Popover::builder()
        .child(&menu())
        .css_classes(["quick-settings-popover"])
        .build();
    let icons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    icons.add_css_class("system-status");
    let (net, vpn) = network::indicators();
    icons.append(&net);
    icons.append(&vpn);
    icons.append(&volume::indicator());
    icons.append(&battery::indicator());
    let power = gtk::Image::from_icon_name("system-shutdown-symbolic");
    battery::Battery::get().subscribe({
        let power = power.clone();
        move |s| power.set_visible(!s.present)
    });
    icons.append(&power);
    gtk::MenuButton::builder()
        .child(&icons)
        .popover(&popover)
        .css_classes(["flat", "quick-settings-button"])
        .build()
}

fn menu() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("quick-settings");

    root.append(&session_row());
    root.append(&volume_row());

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(2)
        .row_spacing(8)
        .column_spacing(8)
        .build();
    let (wifi, networks) = network::wifi_tile(tile);
    let tiles: Vec<gtk::Widget> = vec![
        network::wired_tile(tile).upcast(),
        wifi.upcast(),
        settings_tile(
            "bluetooth-active-symbolic",
            &i18n::control_center("Bluetooth"),
            "bluetooth",
        )
        .upcast(),
        power_profile_tile().upcast(),
        dark_style_tile().upcast(),
        night_light_tile().upcast(),
    ];
    for t in tiles {
        flow.append(&t);
        // A hidden tile must not leave a hole: hide its FlowBoxChild too.
        let sync = |w: &gtk::Widget| {
            if let Some(p) = w.parent() {
                p.set_visible(w.is_visible());
            }
        };
        sync(&t);
        t.connect_visible_notify(sync);
    }
    root.append(&flow);
    root.append(&networks);
    root
}

pub fn tile(icon: &str, label: &str) -> gtk::ToggleButton {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(
        &gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    gtk::ToggleButton::builder()
        .child(&content)
        .css_classes(["tile"])
        .build()
}

/// A tile that opens a gnome-control-center panel.
/// TODO: drive BlueZ directly for on/off state.
fn settings_tile(icon: &str, label: &str, panel: &'static str) -> gtk::ToggleButton {
    let t = tile(icon, label);
    t.connect_clicked(move |b| {
        b.set_active(false);
        open_settings(panel);
    });
    t
}

/// Bind a tile to a boolean key and keep it in sync both ways.
fn bool_tile(icon: &str, label: &str, schema: &str, key: &'static str) -> gtk::ToggleButton {
    let t = tile(icon, label);
    match gs::settings(schema).filter(|s| gs::has_key(s, key)) {
        Some(settings) => settings.bind(key, &t, "active").build(),
        None => t.set_sensitive(false),
    }
    t
}

fn dark_style_tile() -> gtk::ToggleButton {
    let t = tile("dark-mode-symbolic", &i18n::shell("Dark Style"));
    let Some(settings) = gs::settings(gs::DESKTOP_INTERFACE) else {
        t.set_sensitive(false);
        return t;
    };
    let sync = {
        let t = t.clone();
        move |s: &gio::Settings| t.set_active(s.string("color-scheme") == "prefer-dark")
    };
    sync(&settings);
    settings.connect_changed(Some("color-scheme"), move |s, _| sync(s));
    t.connect_toggled(move |b| {
        // Mirror gnome-shell: Dark Style also switches the Yaru GTK theme.
        let (scheme, theme) = if b.is_active() {
            ("prefer-dark", "Yaru-dark")
        } else {
            ("default", "Yaru")
        };
        let _ = settings.set_string("color-scheme", scheme);
        let _ = settings.set_string("gtk-theme", theme);
    });
    t
}

fn night_light_tile() -> gtk::ToggleButton {
    bool_tile(
        "night-light-symbolic",
        &i18n::shell("Night Light"),
        gs::NIGHT_LIGHT,
        "night-light-enabled",
    )
}

/// Cycles power-saver / balanced / performance through power-profiles-daemon.
fn power_profile_tile() -> gtk::ToggleButton {
    let t = tile(
        "power-profile-balanced-symbolic",
        &i18n::shell("Power Mode"),
    );
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        POWER_PROFILES_BUS,
        POWER_PROFILES_PATH,
        POWER_PROFILES_BUS,
        gio::Cancellable::NONE,
    );
    let Ok(proxy) = proxy else {
        t.set_sensitive(false);
        return t;
    };
    let current = move |p: &gio::DBusProxy| {
        p.cached_property("ActiveProfile")
            .and_then(|v| v.get::<String>())
            .unwrap_or_else(|| "balanced".into())
    };
    let sync = {
        let t = t.clone();
        move |p: &gio::DBusProxy| {
            let profile = current(p);
            t.set_active(profile != "balanced");
            t.set_tooltip_text(Some(&profile));
        }
    };
    sync(&proxy);
    proxy.connect_local("g-properties-changed", false, {
        let sync = sync.clone();
        move |args| {
            if let Ok(p) = args[0].get::<gio::DBusProxy>() {
                sync(&p);
            }
            None
        }
    });
    t.connect_clicked(move |_| {
        let next = match current(&proxy).as_str() {
            "power-saver" => "balanced",
            "balanced" => "performance",
            _ => "power-saver",
        };
        let args = (
            POWER_PROFILES_BUS,
            "ActiveProfile",
            glib::Variant::from_variant(&next.to_variant()),
        );
        proxy.call(
            "org.freedesktop.DBus.Properties.Set",
            Some(&args.to_variant()),
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |r| {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "could not change power profile");
                }
            },
        );
    });
    t
}

/// Output volume slider; the speaker icon mutes, like gnome-shell.
fn volume_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let mute = gtk::Button::builder()
        .icon_name("audio-volume-high-symbolic")
        .css_classes(["flat", "circular"])
        .build();
    mute.connect_clicked(|_| volume::Volume::get().toggle_mute());
    row.append(&mute);
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    scale.set_hexpand(true);
    row.append(&scale);

    let updating = Rc::new(Cell::new(false));
    volume::Volume::get().subscribe({
        let (row, scale, updating) = (row.clone(), scale.clone(), updating.clone());
        move |s| {
            row.set_sensitive(s.available);
            mute.set_icon_name(s.icon());
            if (scale.value() - s.volume).abs() > 0.005 {
                updating.set(true);
                scale.set_value(s.volume);
                updating.set(false);
            }
        }
    });
    scale.connect_value_changed(move |s| {
        if !updating.get() {
            volume::Volume::get().set_volume(s.value());
        }
    });
    row
}

fn session_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&battery::menu_button());
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    row.append(&spacer);
    let items: [(&str, String, fn()); 4] = [
        (
            "emblem-system-symbolic",
            i18n::control_center("Settings"),
            || open_settings(""),
        ),
        ("system-lock-screen-symbolic", i18n::shell("Lock"), || {
            run(&["loginctl", "lock-session"])
        }),
        ("system-log-out-symbolic", i18n::shell("Log Out"), log_out),
        ("system-shutdown-symbolic", i18n::shell("Power Off"), || {
            run(&["systemctl", "poweroff"])
        }),
    ];
    for (icon, label, action) in items {
        let b = gtk::Button::from_icon_name(icon);
        b.set_tooltip_text(Some(&label));
        b.add_css_class("circular");
        b.connect_clicked(move |_| action());
        row.append(&b);
    }
    row
}

fn log_out() {
    match std::env::var("XDG_SESSION_ID") {
        Ok(id) => run(&["loginctl", "terminate-session", &id]),
        Err(_) => run(&["systemctl", "--user", "stop", "umoja-session.target"]),
    }
}

pub fn open_settings(panel: &str) {
    let mut cmd = Command::new("gnome-control-center");
    if !panel.is_empty() {
        cmd.arg(panel);
    }
    if let Err(e) = cmd.spawn() {
        tracing::warn!(error = %e, "could not open Settings");
    }
}

fn run(argv: &[&str]) {
    if let Err(e) = Command::new(argv[0]).args(&argv[1..]).spawn() {
        tracing::warn!(cmd = argv[0], error = %e, "could not run");
    }
}
