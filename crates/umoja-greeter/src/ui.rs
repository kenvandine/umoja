//! The login screen.
//!
//! Ubuntu's default wallpaper (warty-final-ubuntu.png) fills the screen,
//! sharp and gently dimmed, with a large lock-screen clock in the corner, a
//! frosted-glass login bubble just under the release mascot (which is left
//! uncovered), and the other accounts as a row of pills along the bottom.
//!
//! Fingerprint login works like GDM's: the `umoja-greeter` PAM service
//! (data/pam) asks fprintd first, so for a user with enrolled fingers PAM
//! sits waiting for a finger while the password field stays usable, with
//! "(or place finger on reader)" under it. Submitting a password then
//! restarts PAM with the reader claimed by the greeter, which makes
//! pam_fprintd step aside at once and go straight to the password.

use crate::auth::{self, Greeter, MessageType, PromptType, User};
use crate::backdrop::{self, Backdrop};
use adw::prelude::*;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use umoja_common::fprint::{self, ScanType};
use umoja_common::i18n;

/// The session preselected for users who have never logged in.
pub const DEFAULT_SESSION: &str = "umoja";
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
    /// "(or place finger on reader)" under the password field.
    fp_hint: gtk::Label,
    /// fprintd's reader, if there is one.
    reader: RefCell<Option<fprint::Device>>,
    scan: Cell<ScanType>,
    /// PAM is waiting for an answer to a password prompt.
    prompt_pending: Cell<bool>,
    /// PAM is waiting for a finger (pam_fprintd said "Place your finger…").
    fingerprint: Cell<bool>,
    /// This conversation was started with the reader held by us, to skip
    /// pam_fprintd and go straight to the password.
    password_only: Cell<bool>,
    /// The reader is claimed by the greeter (see `password_only`).
    reader_claimed: Cell<bool>,
    /// A password submitted before PAM asked for it.
    queued: RefCell<Option<String>>,
    /// Bumped by every new conversation, so a stale async start gives up.
    generation: Cell<u64>,
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

    // The login screen always shows Ubuntu's default wallpaper, unblurred.
    let bg = Backdrop::new();
    bg.set_wallpaper(Some(backdrop::GREETER_WALLPAPER));

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&bg));
    // Starts transparent over the black window and fades in once mapped.
    overlay.set_opacity(0.0);

    // Card ------------------------------------------------------------------
    let avatar = adw::Avatar::new(64, None, true);
    avatar.add_css_class("user-avatar");
    avatar.set_valign(gtk::Align::Center);
    let name = gtk::Label::builder()
        .css_classes(["user-name"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
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
        .xalign(0.0)
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
        .valign(gtk::Align::Center)
        .visible(sessions.len() > 1)
        .build();

    // A short horizontal bubble under the release mascot (which sits in the
    // middle of the wallpaper): avatar | name, password, hints | session.
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 20);
    card.add_css_class("login-card");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Start);
    // Room for the card's shadow inside the fade-in revealer.
    card.set_margin_top(CARD_SHADOW_MARGIN);
    card.set_margin_bottom(CARD_SHADOW_MARGIN);
    card.set_margin_start(CARD_SHADOW_MARGIN);
    card.set_margin_end(CARD_SHADOW_MARGIN);
    let fields = gtk::Box::new(gtk::Orientation::Vertical, 8);
    fields.set_width_request(340);
    fields.set_valign(gtk::Align::Center);
    fields.append(&name);
    fields.append(&pill);
    let fp_hint = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["fingerprint-hint"])
        .visible(false)
        .build();
    fields.append(&fp_hint);
    fields.append(&message_revealer);
    card.append(&avatar);
    card.append(&fields);
    card.append(&session_menu);

    let card_reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(450)
        .child(&card)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
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

    {
        let card_reveal = card_reveal.clone();
        let user_strip = user_strip.clone();
        overlay.connect_get_child_position(move |overlay, child| {
            if child != card_reveal.upcast_ref::<gtk::Widget>() {
                return None;
            }
            let (w, h) = (overlay.width(), overlay.height());
            let strip_h = if user_strip.is_visible() && user_strip.first_child().is_some() {
                // Includes the strip's CSS margin.
                user_strip.preferred_size().1.height()
            } else {
                0
            };
            Some(card_rect(w, h, &child.preferred_size().1, strip_h))
        });
    }

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
        fp_hint,
        reader: RefCell::default(),
        scan: Cell::new(ScanType::Press),
        prompt_pending: Cell::new(false),
        fingerprint: Cell::new(false),
        password_only: Cell::new(false),
        reader_claimed: Cell::new(false),
        queued: RefCell::default(),
        generation: Cell::new(0),
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
            // wallpaper on every other screen.
            for mon in others {
                wallpaper_on(app, &window, &mon);
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
    let st = state.clone();
    window.connect_map(move |_| {
        let card_reveal = card_reveal.clone();
        let st = st.clone();
        fade(&content, 1.0, FADE_IN_MS, move || {
            card_reveal.set_reveal_child(true);
            // The entry can't take focus while the revealer hides it, and
            // with a finger enrolled PAM's password prompt (which used to
            // focus it) only comes once pam_fprintd gives up.
            focus_entry(&st);
        });
    });
    // Whenever the greeter gets the keyboard back, it goes to the password.
    let st = state.clone();
    window.connect_is_active_notify(move |w| {
        if w.is_active() {
            focus_entry(&st);
        }
    });
    window.present();
}

/// The wallpaper alone on `monitor`: a background layer surface
/// filling the screen (a fullscreen toplevel there didn't always end up
/// covering it), or a fullscreen window without layer shell.
fn wallpaper_on(
    app: &adw::Application,
    main: &adw::ApplicationWindow,
    monitor: &gtk::gdk::Monitor,
) {
    use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
    let w = gtk::Window::builder()
        .application(app)
        .css_classes(["umoja-greeter"])
        .decorated(false)
        .build();
    let bg = Backdrop::new();
    bg.set_wallpaper(Some(backdrop::GREETER_WALLPAPER));
    w.set_child(Some(&bg));
    if gtk4_layer_shell::is_supported() {
        w.init_layer_shell();
        w.set_namespace("umoja-greeter-wallpaper");
        w.set_layer(Layer::Background);
        w.set_monitor(monitor);
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            w.set_anchor(edge, true);
        }
        w.set_exclusive_zone(-1);
        w.set_keyboard_mode(KeyboardMode::None);
    } else {
        w.fullscreen_on_monitor(monitor);
        follow_monitor(&w, monitor);
        // A click there shouldn't take the keyboard from the card.
        let main = main.downgrade();
        w.connect_is_active_notify(move |w| {
            if w.is_active() {
                if let Some(main) = main.upgrade() {
                    main.present();
                }
            }
        });
    }
    w.present();
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

/// The wallpaper's mascot fills the middle 40% of the image's height
/// (warty-final-ubuntu.png, every release so far).
const MASCOT_HALF_HEIGHT: f32 = 0.2;
/// Clear space between the mascot and the card.
const MASCOT_GAP: i32 = 24;
const CARD_SHADOW_MARGIN: i32 = 48;

/// Places the login card just below the mascot, centred, without letting it
/// run into the user strip or off the bottom of the screen.
fn card_rect(w: i32, h: i32, nat: &gtk::Requisition, strip_h: i32) -> gtk::gdk::Rectangle {
    let cw = nat.width().min(w);
    let ch = nat.height().min(h);
    // The backdrop is a "cover" fit of a 16:9 image, so on wide screens it
    // overflows top and bottom and the mascot is taller than 40% of `h`.
    let image_h = (h as f32).max(w as f32 * 9.0 / 16.0);
    let mascot_bottom = (h as f32 / 2.0 + image_h * MASCOT_HALF_HEIGHT) as i32;
    let want = mascot_bottom + MASCOT_GAP - CARD_SHADOW_MARGIN;
    // The visible card (inside the shadow margin) stays MASCOT_GAP above
    // the user strip.
    let max = (h - strip_h - MASCOT_GAP - ch + CARD_SHADOW_MARGIN).max(0);
    gtk::gdk::Rectangle::new((w - cw) / 2, want.min(max), cw, ch)
}

fn build_user_strip(state: &Rc<State>) {
    if state.users.len() < 2 {
        return;
    }
    for (i, u) in state.users.iter().enumerate() {
        let avatar = adw::Avatar::new(32, Some(&u.display_name), true);
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
        let col = gtk::Box::new(gtk::Orientation::Horizontal, 10);
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
        // A non-secret prompt is a username question; we always pre-fill it.
        if kind == PromptType::Question {
            if let Some(u) = s.users.get(s.current.get()) {
                let _ = s.greeter.respond(&u.name);
            }
            return;
        }
        // Past pam_fprintd (no fingers, gave up, or skipped): password time.
        set_fingerprint(&s, false);
        if let Some(password) = s.queued.borrow_mut().take() {
            let _ = s.greeter.respond(&password);
            return;
        }
        s.prompt_pending.set(true);
        s.busy.set(false);
        s.entry.set_sensitive(true);
        s.submit.set_sensitive(true);
        focus_entry(&s);
    });

    let s = state.clone();
    state.greeter.on_message(move |text, kind| {
        if kind == MessageType::Info && !s.prompt_pending.get() {
            // Before the password prompt only pam_fprintd talks: it is
            // waiting for a finger. Say so the GNOME way, under the field.
            if s.password_only.get() {
                tracing::warn!("pam_fprintd ran although the reader was claimed");
            }
            set_fingerprint(&s, true);
            return;
        }
        show_message(&s, &text);
        if kind == MessageType::Error && s.fingerprint.get() {
            shake(&s.card);
        }
    });

    let s = state.clone();
    state.greeter.on_authentication_complete(move || {
        release_reader(&s);
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
            s.entry.set_text("");
            s.busy.set(false);
            s.entry.set_sensitive(true);
            s.submit.set_sensitive(true);
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
            let password = s.entry.text().to_string();
            if s.prompt_pending.replace(false) {
                if let Err(err) = s.greeter.respond(&password) {
                    show_message(&s, &err.to_string());
                }
            } else {
                // PAM hasn't asked yet; answer when it does. If it is busy
                // waiting for a finger, start over without the reader.
                *s.queued.borrow_mut() = Some(password);
                if s.fingerprint.get() {
                    start_auth(&s, true);
                }
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

    // A different user starts afresh; a retry keeps nothing typed either.
    state.queued.borrow_mut().take();
    start_auth(state, false);
    // Picking someone from the strip leaves focus on their chip.
    focus_entry(state);
}

/// Put the keyboard in the password field. Typing works while PAM waits
/// for a finger too: submitting then restarts without the reader.
fn focus_entry(state: &State) {
    // GtkText inside holds the focus; grabbing again would select all.
    let focused = state
        .entry
        .state_flags()
        .contains(gtk::StateFlags::FOCUS_WITHIN);
    if state.entry.is_sensitive() && !focused {
        state.entry.grab_focus();
    }
}

/// Start a PAM conversation for the current user. `password_only` claims
/// the fingerprint reader first so pam_fprintd skips straight to the
/// password (also used when fingerprint login is turned off).
fn start_auth(state: &Rc<State>, password_only: bool) {
    let Some(user) = state.users.get(state.current.get()).map(|u| u.name.clone()) else {
        return;
    };
    state.greeter.cancel();
    state.prompt_pending.set(false);
    set_fingerprint(state, false);
    let password_only = password_only || !fprint::enabled_by_settings();
    state.password_only.set(password_only);
    let generation = state.generation.get() + 1;
    state.generation.set(generation);
    if state.greeter.is_preview() {
        return;
    }
    let s = state.clone();
    glib::spawn_future_local(async move {
        if s.reader.borrow().is_none() {
            let reader = fprint::Device::default().await;
            if let Some(r) = &reader {
                s.scan.set(r.scan_type());
            }
            *s.reader.borrow_mut() = reader;
        }
        let reader = s.reader.borrow().clone();
        if let Some(reader) = reader {
            if password_only {
                claim_reader(&s, &reader, generation).await;
            } else if s.reader_claimed.replace(false) {
                reader.release().await;
            }
        }
        if s.generation.get() != generation {
            return;
        }
        if let Err(e) = s.greeter.authenticate(&user) {
            show_message(&s, &e.to_string());
        }
    });
}

/// Claim the reader for the greeter. Right after a cancel the old PAM
/// process may still hold it for a moment, so try for about a second.
async fn claim_reader(state: &Rc<State>, reader: &fprint::Device, generation: u64) {
    if state.reader_claimed.get() {
        return;
    }
    for _ in 0..10 {
        if state.generation.get() != generation {
            return;
        }
        match reader.claim("").await {
            Ok(()) => {
                state.reader_claimed.set(true);
                return;
            }
            Err(e) => tracing::debug!(error = %e, "fingerprint reader busy"),
        }
        glib::timeout_future(std::time::Duration::from_millis(100)).await;
    }
    tracing::warn!("could not claim the fingerprint reader; PAM may wait for it");
}

fn release_reader(state: &Rc<State>) {
    if !state.reader_claimed.replace(false) {
        return;
    }
    if let Some(reader) = state.reader.borrow().clone() {
        glib::spawn_future_local(async move { reader.release().await });
    }
}

fn set_fingerprint(state: &State, on: bool) {
    state.fingerprint.set(on);
    if on {
        state.fp_hint.set_text(&fprint::or_hint(state.scan.get()));
    }
    state.fp_hint.set_visible(on);
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
            // The greeter user has no dconf settings of its own, so
            // this is the system locale's 12/24-hour choice.
            let twelve = umoja_common::clock::twelve_hour();
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
