//! The Ubuntu Quick Settings menu.
//!
//! Every toggle reads and writes the same GSettings keys / D-Bus services as
//! GNOME, so flipping Dark Style here or in gnome-control-center is the same
//! operation and both stay in sync.

use crate::{battery, bluetooth, brightness, network, sound, volume};
use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::Cell;
use std::rc::Rc;
use umoja_common::{gsettings as gs, i18n};

const POWER_PROFILES_BUS: &str = "net.hadess.PowerProfiles";
const POWER_PROFILES_PATH: &str = "/net/hadess/PowerProfiles";
const POWER_PROFILES_BUS_NEW: &str = "org.freedesktop.UPower.PowerProfiles";
const POWER_PROFILES_PATH_NEW: &str = "/org/freedesktop/UPower/PowerProfiles";

/// The system status area: network, VPN, volume and battery icons (or the
/// power icon on machines without a battery), opening Quick Settings.
pub fn button() -> gtk::MenuButton {
    let popover = gtk::Popover::builder()
        .child(&menu())
        .css_classes(["quick-settings-popover"])
        .has_arrow(false)
        .build();
    crate::menu_gap::below_bar(&popover);
    umoja_common::blur::blur_behind(&popover);
    // Take the keyboard while open, like the date menu and gnome-shell's
    // Quick Settings (arrow keys, Escape). Without it a menu opened over
    // D-Bus (org.umoja.Panel.OpenMenu: the top-edge swipe, tools/demo)
    // had no grab and closed again at once.
    popover.connect_show(|p| crate::notifications::want_keyboard(p, true));
    popover.connect_hide(|p| {
        crate::notifications::want_keyboard(p, false);
        // Like gnome-shell, a tile's menu (Wi-Fi networks, Keyboard
        // Backlight...) doesn't stay expanded for the next time.
        if let Some(child) = p.child() {
            collapse_tile_menus(&child);
        }
    });
    let icons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    icons.add_css_class("system-status");
    let (net, vpn) = network::indicators();
    icons.append(&net);
    icons.append(&vpn);
    icons.append(&bluetooth::indicator());
    icons.append(&volume::indicator());
    icons.append(&volume::mic_indicator());
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

/// Close every tile arrow under `w`, which hides the menus they reveal.
fn collapse_tile_menus(w: &gtk::Widget) {
    if w.has_css_class("tile-arrow")
        && let Some(arrow) = w.downcast_ref::<gtk::ToggleButton>()
    {
        arrow.set_active(false);
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        collapse_tile_menus(&c);
        child = c.next_sibling();
    }
}

fn menu() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("quick-settings");

    root.append(&session_row());
    root.append(&stream_slider(false));
    root.append(&stream_slider(true));

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(2)
        .row_spacing(8)
        .column_spacing(8)
        .build();
    let (wifi, networks) = network::wifi_tile(tile);
    let (power, profiles) = power_profile_tile();
    let (bt, bt_devices) = bluetooth::tile(tile);
    let (vpn, vpns) = network::vpn_tile(tile);
    let (kbd, kbd_levels) = keyboard_tile();
    let tiles: Vec<gtk::Widget> = vec![
        network::wired_tile(tile).upcast(),
        wifi.upcast(),
        vpn.upcast(),
        bt.upcast(),
        power.upcast(),
        dark_style_tile().upcast(),
        night_light_tile().upcast(),
        kbd.upcast(),
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
    root.append(&vpns);
    root.append(&bt_devices);
    root.append(&profiles);
    root.append(&kbd_levels);
    root
}

pub fn tile(icon: &str, label: &str) -> gtk::ToggleButton {
    // gnome-shell 46+ QuickToggle: icon, then a title over an optional
    // subtitle (network name, connected device, active profile...).
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.append(&gtk::Image::from_icon_name(icon));
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["tile-title"])
            .build(),
    );
    text.append(
        &gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["tile-subtitle"])
            .visible(false)
            .build(),
    );
    content.append(&text);
    gtk::ToggleButton::builder()
        .child(&content)
        .css_classes(["tile"])
        .build()
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
    // gnome-shell draws dark-mode-symbolic from its own resources; icon
    // themes may not have it, so fall back to a themed moon.
    if let Some(img) = t
        .child()
        .and_then(|c| c.first_child())
        .and_downcast::<gtk::Image>()
    {
        let icon = gio::ThemedIcon::from_names(&[
            "dark-mode-symbolic",
            "weather-clear-night-symbolic",
            "night-light-symbolic",
        ]);
        img.set_from_gicon(&icon);
    }
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

const PROFILES: [(&str, &str, &str); 3] = [
    (
        "performance",
        "Performance",
        "power-profile-performance-symbolic",
    ),
    ("balanced", "Balanced", "power-profile-balanced-symbolic"),
    (
        "power-saver",
        "Power Saver",
        "power-profile-power-saver-symbolic",
    ),
];

/// power-profiles-daemon renamed its bus name in 0.20; try the new one first.
fn power_profiles_proxy() -> Option<gio::DBusProxy> {
    [
        (POWER_PROFILES_BUS_NEW, POWER_PROFILES_PATH_NEW),
        (POWER_PROFILES_BUS, POWER_PROFILES_PATH),
    ]
    .into_iter()
    .find_map(|(bus, path)| {
        gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None,
            bus,
            path,
            bus,
            gio::Cancellable::NONE,
        )
        .ok()
        .filter(|p| p.name_owner().is_some())
    })
}

/// gnome-shell's Power Mode tile: shows the active profile, and the tile
/// or its arrow opens a list of the three profiles to pick from.
fn power_profile_tile() -> (gtk::Box, gtk::Revealer) {
    let toggle = tile(
        "power-profile-balanced-symbolic",
        &i18n::shell("Power Mode"),
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
    list.append(
        &gtk::Label::builder()
            .label(i18n::shell("Power Mode"))
            .xalign(0.0)
            .css_classes(["heading", "network-list-header"])
            .build(),
    );
    let revealer = gtk::Revealer::builder()
        .child(&list)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();

    let Some(proxy) = power_profiles_proxy() else {
        tracing::info!("power-profiles-daemon not running; Power Mode disabled");
        split.set_sensitive(false);
        return (split, revealer);
    };
    let bus = proxy.interface_name().to_string();

    let mut rows = Vec::new();
    for (id, label, icon) in PROFILES {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Image::from_icon_name(icon));
        row.append(
            &gtk::Label::builder()
                .label(i18n::shell(label))
                .xalign(0.0)
                .hexpand(true)
                .build(),
        );
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        row.append(&check);
        let button = gtk::Button::builder()
            .child(&row)
            .css_classes(["flat", "network-row"])
            .build();
        button.connect_clicked({
            let (proxy, bus) = (proxy.clone(), bus.clone());
            move |_| {
                // id.to_variant() is already boxed as `v` inside the tuple;
                // an extra from_variant made it <<'id'>>, which ppd rejects.
                let args = (bus.as_str(), "ActiveProfile", id.to_variant());
                proxy.call(
                    "org.freedesktop.DBus.Properties.Set",
                    Some(&args.to_variant()),
                    gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
                    -1,
                    gio::Cancellable::NONE,
                    |r| {
                        if let Err(e) = r {
                            tracing::warn!(error = %e, "could not change power profile");
                        }
                    },
                );
            }
        });
        list.append(&button);
        rows.push((id, button, check));
    }

    let updating = Rc::new(Cell::new(false));
    let sync = {
        let (toggle, updating) = (toggle.clone(), updating.clone());
        move |p: &gio::DBusProxy| {
            let active = p
                .cached_property("ActiveProfile")
                .and_then(|v| v.get::<String>())
                .unwrap_or_else(|| "balanced".into());
            // Only offer profiles this machine has (performance needs
            // platform support).
            let available: Vec<String> = p
                .cached_property("Profiles")
                .map(|v| {
                    v.iter()
                        .filter_map(|d| {
                            glib::VariantDict::new(Some(&d))
                                .lookup_value("Profile", None)
                                .and_then(|p| p.get::<String>())
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (id, button, check) in &rows {
                button.set_visible(available.is_empty() || available.iter().any(|a| a == id));
                check.set_opacity(if *id == active { 1.0 } else { 0.0 });
            }
            let (_, label, icon) = PROFILES
                .iter()
                .find(|(id, _, _)| *id == active)
                .copied()
                .unwrap_or(PROFILES[1]);
            network::set_tile_icon(&toggle, icon);
            network::set_tile_subtitle(&toggle, &i18n::shell(label));
            updating.set(true);
            toggle.set_active(active != "balanced");
            updating.set(false);
        }
    };
    sync(&proxy);
    proxy.connect_local("g-properties-changed", false, move |args| {
        if let Ok(p) = args[0].get::<gio::DBusProxy>() {
            sync(&p);
        }
        None
    });

    // The tile body opens the list too, like gnome-shell's menu toggle.
    toggle.connect_clicked({
        let arrow = arrow.clone();
        move |t| {
            if updating.get() {
                return;
            }
            // Keep showing the profile state, not the click.
            updating.set(true);
            t.set_active(!t.is_active());
            updating.set(false);
            arrow.set_active(!arrow.is_active());
        }
    });
    arrow.connect_toggled({
        let revealer = revealer.clone();
        move |b| revealer.set_reveal_child(b.is_active())
    });
    (split, revealer)
}

/// gnome-shell's Keyboard backlight tile, only on machines that have one:
/// the tile turns it off and back on, the arrow offers Off/Low/High for
/// keyboards with three levels or fewer and a slider for the rest.
fn keyboard_tile() -> (gtk::Box, gtk::Revealer) {
    let toggle = tile("keyboard-brightness-symbolic", &i18n::shell("Keyboard"));
    let (split, arrow) = network::split_tile(&toggle);
    let (list, revealer) = network::tile_menu(&arrow);
    let kbd = brightness::Brightness::get();
    let max = kbd.kbd_max();
    if max <= 0 {
        split.set_visible(false);
        return (split, revealer);
    }
    list.append(&network::menu_header(&i18n::shell("Keyboard Backlight")));

    // (level, button, check) for the discrete levels.
    let mut levels: Vec<(i32, gtk::Button, gtk::Image)> = Vec::new();
    let mut slider = None;
    if max <= 2 {
        let names: &[(&str, &str)] = if max == 1 {
            &[
                ("Off", "keyboard-brightness-off-symbolic"),
                ("On", "keyboard-brightness-high-symbolic"),
            ]
        } else {
            &[
                ("Off", "keyboard-brightness-off-symbolic"),
                ("Low", "keyboard-brightness-medium-symbolic"),
                ("High", "keyboard-brightness-high-symbolic"),
            ]
        };
        for (level, (label, icon)) in names.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let image = gtk::Image::new();
            image.set_from_gicon(&gio::ThemedIcon::from_names(&[
                icon,
                "keyboard-brightness-symbolic",
            ]));
            row.append(&image);
            row.append(
                &gtk::Label::builder()
                    .label(i18n::shell(label))
                    .xalign(0.0)
                    .hexpand(true)
                    .build(),
            );
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            row.append(&check);
            let button = gtk::Button::builder()
                .child(&row)
                .css_classes(["flat", "network-row"])
                .build();
            let level = level as i32;
            button.connect_clicked(move |_| brightness::Brightness::get().set_keyboard(level));
            list.append(&button);
            levels.push((level, button, check));
        }
    } else {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, max as f64, 1.0);
        scale.set_hexpand(true);
        scale.set_round_digits(0);
        list.append(&scale);
        slider = Some(scale);
    }

    let updating = Rc::new(Cell::new(false));
    kbd.subscribe_keyboard({
        let (toggle, slider, updating) = (toggle.clone(), slider.clone(), updating.clone());
        move |level, max| {
            updating.set(true);
            toggle.set_active(level > 0);
            if let Some(s) = &slider
                && s.value().round() as i32 != level
            {
                s.set_value(level as f64);
            }
            updating.set(false);
            for (l, _, check) in &levels {
                check.set_opacity(if *l == level { 1.0 } else { 0.0 });
            }
            let subtitle = match level {
                0 => i18n::shell("Off"),
                _ if max <= 2 && level == max => i18n::shell(if max == 1 { "On" } else { "High" }),
                _ if max <= 2 => i18n::shell("Low"),
                _ => format!("{}%", brightness::to_percent(0, max as i64, level as i64)),
            };
            network::set_tile_subtitle(&toggle, &subtitle);
        }
    });
    if let Some(scale) = &slider {
        let updating = updating.clone();
        scale.connect_value_changed(move |s| {
            if !updating.get() {
                brightness::Brightness::get().set_keyboard(s.value().round() as i32);
            }
        });
    }
    toggle.connect_toggled(move |_| {
        if !updating.get() {
            brightness::Brightness::get().toggle_keyboard();
        }
    });
    (split, revealer)
}

/// gnome-shell's OutputStreamSlider / InputStreamSlider: the icon mutes,
/// and an arrow (shown with more than one device) opens "Sound Output" /
/// "Sound Input" to pick the device. The input slider is only shown while
/// an app is recording.
fn stream_slider(input: bool) -> gtk::Box {
    let volume = if input {
        volume::Volume::mic()
    } else {
        volume::Volume::get()
    };
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    section.append(&row);
    let mute = gtk::Button::builder()
        .icon_name(if input {
            "microphone-sensitivity-high-symbolic"
        } else {
            "audio-volume-high-symbolic"
        })
        .css_classes(["flat", "circular"])
        .tooltip_text(if input {
            i18n::shell("Microphone")
        } else {
            i18n::shell("Volume")
        })
        .build();
    mute.connect_clicked({
        let volume = volume.clone();
        move |_| volume.toggle_mute()
    });
    row.append(&mute);
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    scale.set_hexpand(true);
    row.append(&scale);
    let arrow = gtk::ToggleButton::builder()
        .icon_name("go-next-symbolic")
        .css_classes(["flat", "circular", "tile-arrow", "slider-menu-arrow"])
        .visible(false)
        .build();
    row.append(&arrow);

    let (list, revealer) = network::tile_menu(&arrow);
    section.append(&revealer);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("network-list-header");
    header.append(&gtk::Image::from_icon_name(if input {
        "audio-input-microphone-symbolic"
    } else {
        "audio-headphones-symbolic"
    }));
    header.append(
        &gtk::Label::builder()
            .label(i18n::shell(if input {
                "Sound Input"
            } else {
                "Sound Output"
            }))
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    let devices = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let settings = gtk::Button::builder()
        .label(i18n::shell("Sound Settings"))
        .css_classes(["flat", "network-row"])
        .build();
    if let Some(l) = settings.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    settings.connect_clicked(|b| {
        close_menu(b);
        open_settings("sound");
    });
    list.append(&header);
    list.append(&devices);
    list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    list.append(&settings);

    // Rebuild the device rows only when the list itself changes.
    let shown: Rc<std::cell::RefCell<Vec<(sound::Device, gtk::Image)>>> = Rc::default();
    let recording = Rc::new(Cell::new(false));
    let available = Rc::new(Cell::new(false));
    let sync_visible = {
        let (section, arrow, recording, available) = (
            section.clone(),
            arrow.clone(),
            recording.clone(),
            available.clone(),
        );
        move || {
            let visible = !input || (recording.get() && available.get());
            section.set_visible(visible);
            if !visible {
                arrow.set_active(false);
            }
        }
    };
    let sync_visible = Rc::new(sync_visible);
    sound::Sound::get().subscribe({
        let (arrow, devices, shown, recording, sync_visible) = (
            arrow.clone(),
            devices.clone(),
            shown.clone(),
            recording.clone(),
            sync_visible.clone(),
        );
        move |s| {
            let list = s.devices(input);
            let same = {
                let shown = shown.borrow();
                shown.len() == list.len() && shown.iter().zip(list).all(|((d, _), n)| d == n)
            };
            if !same {
                while let Some(c) = devices.first_child() {
                    devices.remove(&c);
                }
                let mut rows = Vec::new();
                for dev in list {
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    row.append(&gtk::Image::from_icon_name(dev.icon));
                    // Port over card, like a tile's title and subtitle.
                    // max-width-chars 1 keeps a long name from widening
                    // the panel: it takes the width there is and ends in "…".
                    let text = gtk::Box::builder()
                        .orientation(gtk::Orientation::Vertical)
                        .valign(gtk::Align::Center)
                        .hexpand(true)
                        .build();
                    let label = |s: &str, class: &str| {
                        gtk::Label::builder()
                            .label(s)
                            .xalign(0.0)
                            .max_width_chars(1)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes([class])
                            .tooltip_text(&dev.description)
                            .build()
                    };
                    text.append(&label(&dev.port, "device-port"));
                    if !dev.card.is_empty() {
                        text.append(&label(&dev.card, "tile-subtitle"));
                    }
                    row.append(&text);
                    let check = gtk::Image::from_icon_name("object-select-symbolic");
                    row.append(&check);
                    let button = gtk::Button::builder()
                        .child(&row)
                        .css_classes(["flat", "network-row"])
                        .build();
                    let d = dev.clone();
                    button.connect_clicked(move |_| sound::Sound::get().set_default(input, &d));
                    devices.append(&button);
                    rows.push((dev.clone(), check));
                }
                shown.replace(rows);
            }
            let default = s.default_name(input);
            for (d, check) in shown.borrow().iter() {
                check.set_opacity(if Some(d.name.as_str()) == default {
                    1.0
                } else {
                    0.0
                });
            }
            // gnome-shell: menuEnabled = this._deviceItems.size > 1.
            arrow.set_visible(list.len() > 1);
            if list.len() <= 1 {
                arrow.set_active(false);
            }
            recording.set(s.recording);
            sync_visible();
        }
    });

    let updating = Rc::new(Cell::new(false));
    volume.subscribe({
        let (row, scale, updating) = (row.clone(), scale.clone(), updating.clone());
        move |s| {
            row.set_sensitive(s.available);
            mute.set_icon_name(if input { volume::mic_icon(s) } else { s.icon() });
            if (scale.value() - s.volume).abs() > 0.005 {
                updating.set(true);
                scale.set_value(s.volume);
                updating.set(false);
            }
            available.set(s.available);
            sync_visible();
        }
    });
    scale.connect_value_changed(move |s| {
        if !updating.get() {
            volume.set_volume(s.value());
        }
    });
    section
}

fn session_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&battery::menu_button());
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    row.append(&spacer);
    let items: [(&str, String, &str, fn()); 4] = [
        (
            "emblem-system-symbolic",
            i18n::control_center("Settings"),
            "settings",
            || open_settings(""),
        ),
        (
            "system-lock-screen-symbolic",
            i18n::shell("Lock"),
            "lock",
            || crate::lock::locker().lock(),
        ),
        (
            "system-log-out-symbolic",
            i18n::shell("Log Out"),
            "log-out",
            crate::fade::log_out,
        ),
        (
            "system-shutdown-symbolic",
            i18n::shell("Power Off"),
            "power-off",
            crate::power_dialog::open,
        ),
    ];
    for (icon, label, name, action) in items {
        let b = gtk::Button::from_icon_name(icon);
        b.set_tooltip_text(Some(&label));
        b.add_css_class("circular");
        b.connect_clicked(move |b| {
            tracing::info!(button = name, "quick settings action");
            close_menu(b);
            action();
        });
        row.append(&b);
    }
    row
}

/// Close Quick Settings before acting, as gnome-shell does, so the menu
/// is not left open over a new window or the lock screen.
pub fn close_menu(w: &impl IsA<gtk::Widget>) {
    if let Some(p) = w
        .ancestor(gtk::Popover::static_type())
        .and_downcast::<gtk::Popover>()
    {
        p.popdown();
    }
}

/// Opens GNOME Settings (optionally on a panel) through its desktop file,
/// with an activation token so the compositor focuses the new window.
pub fn open_settings(panel: &str) {
    let ctx = gtk::gdk::Display::default().map(|d| d.app_launch_context());
    let app = gio::DesktopAppInfo::new("org.gnome.Settings.desktop");
    let mut argv = vec!["gnome-control-center".to_string()];
    if !panel.is_empty() {
        argv.push(panel.to_string());
    }
    let token = match (&ctx, &app) {
        (Some(ctx), Some(app)) => ctx.startup_notify_id(Some(app), &[]),
        _ => None,
    };
    crate::launch::spawn_app("org.gnome.Settings", &argv, token.as_deref());
}

pub fn run(argv: &[&str]) {
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    if let Err(e) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
        tracing::warn!(cmd = ?argv[0], error = %e, "could not run");
    }
}
