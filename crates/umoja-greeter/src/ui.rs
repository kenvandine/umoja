//! The login screen.
//!
//! Ubuntu's default wallpaper (warty-final-ubuntu.png) fills the screen,
//! blurred and dimmed, with a large lock-screen clock in the corner, a
//! frosted-glass login card in the middle, and the other accounts as a row
//! of avatars underneath.

use crate::auth::{self, Greeter, PromptType, User};
use crate::backdrop::{self, Backdrop};
use adw::prelude::*;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use umoja_common::i18n;

/// The session preselected for users who have never logged in.
pub const DEFAULT_SESSION: &str = "umoja";
const BLUR_RADIUS: f64 = 48.0;
const FADE_IN_MS: u32 = 600;
const FADE_OUT_MS: u32 = 450;

struct State {
    greeter: Greeter,
    users: Vec<User>,
    sessions: Vec<auth::Session>,
    current: Cell<usize>,
    session: Cell<usize>,
    card: gtk::Box,
    avatar: adw::Avatar,
    name: gtk::Label,
    entry: gtk::PasswordEntry,
    submit: gtk::Button,
    message: gtk::Label,
    message_revealer: gtk::Revealer,
    session_buttons: RefCell<Vec<gtk::CheckButton>>,
    user_strip: gtk::Box,
    busy: Cell<bool>,
    /// Everything on screen; faded in at start and out before the session.
    content: gtk::Overlay,
}

pub fn build(app: &adw::Application, greeter: Greeter) {
    let (users, sessions) = if greeter.is_preview() {
        (auth::preview_users(), auth::preview_sessions())
    } else {
        (auth::users(), auth::sessions())
    };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .css_classes(["umoja-greeter"])
        .build();

    // The login screen always shows Ubuntu's default wallpaper, blurred.
    let bg = Backdrop::new(BLUR_RADIUS);
    bg.set_wallpaper(Some(backdrop::GREETER_WALLPAPER));

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&bg));
    // Starts transparent over the black window and fades in once mapped.
    overlay.set_opacity(0.0);

    // Card ------------------------------------------------------------------
    let avatar = adw::Avatar::new(128, None, true);
    avatar.add_css_class("user-avatar");
    avatar.set_halign(gtk::Align::Center);
    let name = gtk::Label::builder().css_classes(["user-name"]).build();
    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text(i18n::shell("Password"))
        .hexpand(true)
        .build();
    let submit = gtk::Button::builder()
        .icon_name("go-next-symbolic")
        .css_classes(["circular", "suggested-action", "submit"])
        .valign(gtk::Align::Center)
        .build();
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    pill.add_css_class("password-pill");
    pill.append(&entry);
    pill.append(&submit);

    let message = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["message"])
        .build();
    let message_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&message)
        .build();

    let session_menu = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text(i18n::shell("Session"))
        .css_classes(["circular", "flat", "session-button"])
        .halign(gtk::Align::Center)
        .visible(sessions.len() > 1)
        .build();

    let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
    card.add_css_class("login-card");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Center);
    card.set_width_request(380);
    card.append(&avatar);
    card.append(&name);
    card.append(&pill);
    card.append(&message_revealer);
    card.append(&session_menu);

    let card_reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(450)
        .child(&card)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    overlay.add_overlay(&card_reveal);

    // Clock -----------------------------------------------------------------
    let time = gtk::Label::builder()
        .css_classes(["big-clock"])
        .xalign(0.0)
        .build();
    let date = gtk::Label::builder()
        .css_classes(["big-date"])
        .xalign(0.0)
        .build();
    let clock = gtk::Box::new(gtk::Orientation::Vertical, 0);
    clock.add_css_class("clock-box");
    clock.set_halign(gtk::Align::Start);
    clock.set_valign(gtk::Align::End);
    clock.append(&time);
    clock.append(&date);
    overlay.add_overlay(&clock);
    tick(&time, &date);

    // Top bar ---------------------------------------------------------------
    let top = gtk::CenterBox::new();
    top.add_css_class("greeter-top-bar");
    top.set_valign(gtk::Align::Start);
    top.set_start_widget(Some(&gtk::Label::new(Some(&glib::host_name()))));
    let status = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status.append(&battery_indicator());
    status.append(&power_menu());
    top.set_end_widget(Some(&status));
    overlay.add_overlay(&top);

    // Other users -----------------------------------------------------------
    let user_strip = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    user_strip.add_css_class("user-strip");
    user_strip.set_halign(gtk::Align::Center);
    user_strip.set_valign(gtk::Align::End);
    overlay.add_overlay(&user_strip);

    window.set_content(Some(&overlay));

    let state = Rc::new(State {
        greeter,
        users,
        sessions,
        current: Cell::new(0),
        session: Cell::new(0),
        card,
        avatar,
        name,
        entry,
        submit,
        message,
        message_revealer,
        session_buttons: RefCell::default(),
        user_strip,
        busy: Cell::new(false),
        content: overlay.clone(),
    });

    session_menu.set_popover(Some(&session_popover(&state)));
    build_user_strip(&state);
    wire(&state);

    let initial = state
        .greeter
        .select_user_hint()
        .and_then(|hint| state.users.iter().position(|u| u.name == hint))
        .unwrap_or(0);
    select_user(&state, initial);

    // On the X11 fallback there is no window manager, so "fullscreen" is
    // only a hint: size the window to the monitor ourselves.
    if let Some(mon) = gtk::gdk::Display::default()
        .and_then(|d| d.monitors().item(0))
        .and_downcast::<gtk::gdk::Monitor>()
    {
        let g = mon.geometry();
        window.set_default_size(g.width(), g.height());
    }
    window.set_decorated(false);
    match login_and_other_monitors() {
        Some((login, others)) if !others.is_empty() => {
            // Like GDM: the login card on the built-in panel, the same
            // blurred wallpaper on every other screen.
            for mon in others {
                let w = adw::ApplicationWindow::builder()
                    .application(app)
                    .css_classes(["umoja-greeter"])
                    .build();
                let bg = Backdrop::new(BLUR_RADIUS);
                bg.set_wallpaper(Some(backdrop::GREETER_WALLPAPER));
                w.set_content(Some(&bg));
                w.set_decorated(false);
                w.fullscreen_on_monitor(&mon);
                follow_monitor(w.upcast_ref(), &mon);
                // A click there shouldn't take the keyboard from the card.
                let main = window.downgrade();
                w.connect_is_active_notify(move |w| {
                    if w.is_active() {
                        if let Some(main) = main.upgrade() {
                            main.present();
                        }
                    }
                });
                w.present();
            }
            window.fullscreen_on_monitor(&login);
            follow_monitor(window.upcast_ref(), &login);
        }
        _ => window.fullscreen(),
    }
    // Name a pointer explicitly rather than inherit the bare X root
    // window's on the X11 fallback.
    window.set_cursor_from_name(Some("default"));
    // Fade up from black once the window is on screen, then bring the card in.
    let content = overlay.clone();
    window.connect_map(move |_| {
        let card_reveal = card_reveal.clone();
        fade(&content, 1.0, FADE_IN_MS, move || {
            card_reveal.set_reveal_child(true)
        });
    });
    window.present();
}

/// Keep a fullscreen window on its monitor when the screens are rearranged
/// (the compositor lays them out side by side just as the greeter starts).
fn follow_monitor(window: &gtk::Window, monitor: &gtk::gdk::Monitor) {
    let window = window.downgrade();
    monitor.connect_geometry_notify(move |mon| {
        if let Some(w) = window.upgrade() {
            w.fullscreen_on_monitor(mon);
        }
    });
}

/// The monitor for the login card (the built-in panel if there is one,
/// as GDM does) and the rest. Only on Wayland: the X11 fallback runs
/// without a window manager to place windows per monitor.
fn login_and_other_monitors() -> Option<(gtk::gdk::Monitor, Vec<gtk::gdk::Monitor>)> {
    let display = gtk::gdk::Display::default()?;
    if display.type_().name() != "GdkWaylandDisplay" {
        return None;
    }
    let list = display.monitors();
    let mut monitors: Vec<gtk::gdk::Monitor> = (0..list.n_items())
        .filter_map(|i| list.item(i).and_downcast::<gtk::gdk::Monitor>())
        .collect();
    if monitors.is_empty() {
        return None;
    }
    let builtin = monitors.iter().position(|m| {
        m.connector()
            .is_some_and(|c| umoja_common::hidpi::is_builtin_connector(c.as_str()))
    });
    let login = monitors.remove(builtin.unwrap_or(0));
    Some((login, monitors))
}

fn session_popover(state: &Rc<State>) -> gtk::Popover {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let mut group: Option<gtk::CheckButton> = None;
    let mut buttons = Vec::new();
    for (i, s) in state.sessions.iter().enumerate() {
        let b = gtk::CheckButton::with_label(&s.name);
        if let Some(g) = &group {
            b.set_group(Some(g));
        } else {
            group = Some(b.clone());
        }
        let st = state.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                st.session.set(i);
            }
        });
        list.append(&b);
        buttons.push(b);
    }
    *state.session_buttons.borrow_mut() = buttons;
    gtk::Popover::builder().child(&list).build()
}

fn build_user_strip(state: &Rc<State>) {
    if state.users.len() < 2 {
        return;
    }
    for (i, u) in state.users.iter().enumerate() {
        let avatar = adw::Avatar::new(56, Some(&u.display_name), true);
        avatar.set_custom_image(
            u.image
                .as_ref()
                .and_then(|p| gtk::gdk::Texture::from_filename(p).ok())
                .as_ref(),
        );
        let label = gtk::Label::builder()
            .label(&u.display_name)
            .css_classes(["caption"])
            .build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.append(&avatar);
        col.append(&label);
        let b = gtk::Button::builder()
            .child(&col)
            .css_classes(["flat", "user-chip"])
            .build();
        if u.logged_in {
            b.set_tooltip_text(Some(&i18n::shell("Logged in")));
        }
        let st = state.clone();
        b.connect_clicked(move |_| select_user(&st, i));
        state.user_strip.append(&b);
    }
}

fn wire(state: &Rc<State>) {
    let s = state.clone();
    state.greeter.on_prompt(move |_text, kind| {
        s.busy.set(false);
        s.entry.set_sensitive(true);
        s.submit.set_sensitive(true);
        s.entry.set_text("");
        // A non-secret prompt is a username question; we always pre-fill it.
        if kind == PromptType::Question {
            if let Some(u) = s.users.get(s.current.get()) {
                let _ = s.greeter.respond(&u.name);
            }
        } else {
            s.entry.grab_focus();
        }
    });

    let s = state.clone();
    state
        .greeter
        .on_message(move |text| show_message(&s, &text));

    let s = state.clone();
    state.greeter.on_authentication_complete(move || {
        if s.greeter.is_authenticated() {
            let key = s
                .sessions
                .get(s.session.get())
                .map(|x| x.key.clone())
                .unwrap_or_else(|| DEFAULT_SESSION.into());
            // Fade to black first so the hand-over to the session's display
            // server (which also starts black) is not a hard cut.
            let st = s.clone();
            fade(&s.content, 0.0, FADE_OUT_MS, move || {
                if let Err(e) = st.greeter.start_session(&key) {
                    st.content.set_opacity(1.0);
                    show_message(&st, &e.to_string());
                }
            });
        } else {
            show_message(
                &s,
                &i18n::shell("Sorry, that didn’t work. Please try again."),
            );
            shake(&s.card);
            let i = s.current.get();
            select_user(&s, i);
        }
    });

    let submit = {
        let s = state.clone();
        move || {
            if s.busy.replace(true) {
                return;
            }
            s.entry.set_sensitive(false);
            s.submit.set_sensitive(false);
            s.message_revealer.set_reveal_child(false);
            if let Err(err) = s.greeter.respond(&s.entry.text()) {
                show_message(&s, &err.to_string());
            }
        }
    };
    let on_enter = submit.clone();
    state.entry.connect_activate(move |_| on_enter());
    state.submit.connect_clicked(move |_| submit());
}

/// Animates `widget`'s opacity to `to`, then runs `done`. Animations are
/// skipped (and `done` runs at once) when GTK animations are turned off.
fn fade(widget: &impl IsA<gtk::Widget>, to: f64, ms: u32, done: impl Fn() + 'static) {
    let widget = widget.as_ref();
    let target = adw::PropertyAnimationTarget::new(widget, "opacity");
    let anim = adw::TimedAnimation::new(widget, widget.opacity(), to, ms, target);
    anim.set_easing(adw::Easing::EaseInOutCubic);
    anim.connect_done(move |_| done());
    anim.play();
}

fn show_message(state: &State, text: &str) {
    state.message.set_text(text);
    state.message_revealer.set_reveal_child(!text.is_empty());
}

fn shake(card: &gtk::Box) {
    card.remove_css_class("shake");
    card.add_css_class("shake");
    let card = card.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(450), move || {
        card.remove_css_class("shake")
    });
}

fn select_user(state: &Rc<State>, index: usize) {
    state.current.set(index);
    let Some(user) = state.users.get(index) else {
        show_message(state, &i18n::umoja("No user accounts found"));
        return;
    };
    state.name.set_text(&user.display_name);
    state.avatar.set_text(Some(&user.display_name));
    state.avatar.set_custom_image(
        user.image
            .as_ref()
            .and_then(|p| gtk::gdk::Texture::from_filename(p).ok())
            .as_ref(),
    );

    // Highlight the chosen avatar in the strip.
    let mut child = state.user_strip.first_child();
    let mut i = 0;
    while let Some(c) = child {
        if i == index {
            c.add_css_class("selected");
        } else {
            c.remove_css_class("selected");
        }
        child = c.next_sibling();
        i += 1;
    }

    // Preselect the user's last session, else LightDM's hint, else Umoja.
    let wanted = user
        .session
        .clone()
        .or_else(|| state.greeter.default_session_hint())
        .unwrap_or_else(|| DEFAULT_SESSION.into());
    if let Some(i) = state
        .sessions
        .iter()
        .position(|s| s.key == wanted)
        .or_else(|| state.sessions.iter().position(|s| s.key == DEFAULT_SESSION))
    {
        state.session.set(i);
        if let Some(b) = state.session_buttons.borrow().get(i) {
            b.set_active(true);
        }
    }

    state.greeter.cancel();
    if let Err(e) = state.greeter.authenticate(&user.name) {
        show_message(state, &e.to_string());
    }
}

/// Battery level from UPower's display device; hidden on desktops.
fn battery_indicator() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let icon = gtk::Image::new();
    let label = gtk::Label::new(None);
    row.append(&icon);
    row.append(&label);
    row.set_visible(false);
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower/devices/DisplayDevice",
        "org.freedesktop.UPower.Device",
        gio::Cancellable::NONE,
    );
    if let Ok(proxy) = proxy {
        let update = {
            let (row, icon, label) = (row.clone(), icon.clone(), label.clone());
            move |p: &gio::DBusProxy| {
                let present = p
                    .cached_property("IsPresent")
                    .and_then(|v| v.get::<bool>())
                    .unwrap_or(false);
                row.set_visible(present);
                if let Some(pct) = p.cached_property("Percentage").and_then(|v| v.get::<f64>()) {
                    label.set_text(&format!("{pct:.0}%"));
                }
                if let Some(name) = p
                    .cached_property("IconName")
                    .and_then(|v| v.get::<String>())
                {
                    icon.set_icon_name(Some(&name));
                }
            }
        };
        update(&proxy);
        proxy.connect_local("g-properties-changed", false, move |args| {
            if let Ok(p) = args[0].get::<gio::DBusProxy>() {
                update(&p);
            }
            None
        });
    }
    row
}

fn power_menu() -> gtk::MenuButton {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let items: [(&str, String, &'static [&'static str]); 3] = [
        (
            "weather-clear-night-symbolic",
            i18n::shell("Suspend"),
            &["systemctl", "suspend"],
        ),
        (
            "system-reboot-symbolic",
            i18n::shell("Restart…"),
            &["systemctl", "reboot"],
        ),
        (
            "system-shutdown-symbolic",
            i18n::shell("Power Off…"),
            &["systemctl", "poweroff"],
        ),
    ];
    for (icon, label, argv) in items {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        content.append(&gtk::Image::from_icon_name(icon));
        content.append(&gtk::Label::new(Some(&label)));
        let b = gtk::Button::builder()
            .child(&content)
            .css_classes(["flat"])
            .build();
        b.connect_clicked(move |_| {
            let _ = Command::new(argv[0]).args(&argv[1..]).spawn();
        });
        col.append(&b);
    }
    gtk::MenuButton::builder()
        .icon_name("system-shutdown-symbolic")
        .popover(&gtk::Popover::builder().child(&col).build())
        .css_classes(["flat", "circular"])
        .build()
}

fn tick(time: &gtk::Label, date: &gtk::Label) {
    let update = {
        let (time, date) = (time.clone(), date.clone());
        move || {
            let Ok(now) = glib::DateTime::now_local() else {
                return;
            };
            let twelve = umoja_common::gsettings::get_string(
                "org.gnome.desktop.interface",
                "clock-format",
                "24h",
            ) == "12h";
            if let Ok(t) = now.format(if twelve { "%-l:%M" } else { "%H:%M" }) {
                time.set_text(&t);
            }
            if let Ok(d) = now.format("%A, %B %-d") {
                date.set_text(&d);
            }
        }
    };
    update();
    glib::timeout_add_seconds_local(5, move || {
        update();
        glib::ControlFlow::Continue
    });
}
