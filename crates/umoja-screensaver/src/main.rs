//! umoja-screensaver: the Umoja screensaver.
//!
//! umoja-mir starts it (`--managed`) once the session has been idle for
//! Settings > Power > Screen Blank, and stops it when the user comes back
//! (locking first if Screen Lock says so) or when the screen blanks
//! `org.umoja.shell screensaver-duration` seconds later. See
//! `crates/umoja-mir/src/idle.rs`.
//!
//! From a terminal:
//!
//! ```text
//! umoja-screensaver              preview on every monitor; any input quits
//! umoja-screensaver --activate   run the real idle path now (then blank, lock)
//! umoja-screensaver --window     preview in a normal window
//! umoja-screensaver --effect rain
//! umoja-screensaver --list-effects
//! ```

mod art;

use gtk::prelude::*;
use gtk::{cairo, gdk, gio, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::Cell;
use std::f64::consts::TAU;
use std::rc::Rc;
use std::time::{Duration, Instant};
use umoja_common::dbus;

const APP_ID: &str = "org.umoja.Screensaver";
/// Ignore input this soon after starting (the key that launched a preview).
const GRACE: Duration = Duration::from_millis(1200);
/// Pointer jitter that doesn't count as the user coming back.
const MOTION_SLOP: f64 = 8.0;

#[derive(Clone, Default)]
struct Options {
    managed: bool,
    window: bool,
    effect: Option<usize>,
}

fn usage() {
    println!(
        "Usage: umoja-screensaver [--window] [--effect NAME]\n\
         \x20      umoja-screensaver --activate\n\
         \x20      umoja-screensaver --list-effects\n\n\
         Without options, shows the screensaver on every monitor until a key,\n\
         click or mouse move. --activate starts it the way idling does, so the\n\
         screen also blanks after screensaver-duration seconds and the session\n\
         locks on the way out if Screen Lock is on.\n\n\
         Effects: {}",
        art::EFFECTS.join(", ")
    );
}

fn main() -> glib::ExitCode {
    let mut opts = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--managed" => opts.managed = true,
            "--window" => opts.window = true,
            "--effect" => {
                let name = args.next().unwrap_or_default();
                match art::EFFECTS.iter().position(|e| *e == name) {
                    Some(i) => opts.effect = Some(i),
                    None => {
                        eprintln!("unknown effect {name:?}; try --list-effects");
                        return glib::ExitCode::FAILURE;
                    }
                }
            }
            "--list-effects" => {
                for e in art::EFFECTS {
                    println!("{e}");
                }
                return glib::ExitCode::SUCCESS;
            }
            "--activate" => return activate(),
            "-h" | "--help" => {
                usage();
                return glib::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown option {other:?}");
                usage();
                return glib::ExitCode::FAILURE;
            }
        }
    }

    umoja_common::init_logging("umoja-screensaver");
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| build(app, &opts));
    // Our options are already parsed; don't let GTK see them.
    app.run_with_args::<&str>(&[])
}

/// Ask umoja-mir to start the screensaver as if the session had idled.
fn activate() -> glib::ExitCode {
    let conn = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("no session bus: {e}");
            return glib::ExitCode::FAILURE;
        }
    };
    let reply = conn.call_sync(
        Some(dbus::COMPOSITOR_BUS_NAME),
        dbus::COMPOSITOR_OBJECT_PATH,
        dbus::COMPOSITOR_INTERFACE,
        "ActivateScreensaver",
        None,
        Some(glib::VariantTy::new("(b)").unwrap()),
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
    );
    match reply.map(|v| v.get::<(bool,)>()) {
        Ok(Some((true,))) => glib::ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!("umoja-mir didn't start the screensaver (is the session locked?)");
            glib::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("umoja-mir isn't answering (is this an Umoja session?): {e}");
            glib::ExitCode::FAILURE
        }
    }
}

/// The word "umoja" as a grid of cells, from Ubuntu Sans Bold.
fn sample_word() -> art::Word {
    const SIZE: f64 = 140.0;
    const CELL: f64 = 6.0;
    const TEXT: &str = "umoja";
    let Ok(mut surface) = cairo::ImageSurface::create(cairo::Format::A8, 1400, 320) else {
        return art::word_from_mask(1, 1, |_, _| true);
    };
    let (w, h) = {
        let Ok(cr) = cairo::Context::new(&surface) else {
            return art::word_from_mask(1, 1, |_, _| true);
        };
        cr.select_font_face(
            "Ubuntu Sans",
            cairo::FontSlant::Normal,
            cairo::FontWeight::Bold,
        );
        cr.set_font_size(SIZE);
        let Ok(ext) = cr.text_extents(TEXT) else {
            return art::word_from_mask(1, 1, |_, _| true);
        };
        cr.move_to(CELL - ext.x_bearing(), CELL - ext.y_bearing());
        let _ = cr.show_text(TEXT);
        (ext.width() + 2.0 * CELL, ext.height() + 2.0 * CELL)
    };
    surface.flush();
    let stride = surface.stride() as usize;
    let (sw, sh) = (surface.width() as usize, surface.height() as usize);
    let Ok(data) = surface.data() else {
        return art::word_from_mask(1, 1, |_, _| true);
    };
    let cols = (w / CELL).ceil() as usize;
    let rows = (h / CELL).ceil() as usize;
    let at = |x: usize, y: usize| {
        if x < sw && y < sh {
            data[y * stride + x]
        } else {
            0
        }
    };
    art::word_from_mask(cols, rows, |c, r| {
        let x = (c as f64 * CELL + CELL / 2.0) as usize;
        let y = (r as f64 * CELL + CELL / 2.0) as usize;
        at(x, y) > 110
    })
}

fn build(app: &gtk::Application, opts: &Options) {
    let word = Rc::new(sample_word());
    tracing::info!(dots = word.dots.len(), "screensaver word");
    let start = Instant::now();

    if opts.window {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Umoja Screensaver")
            .default_width(1280)
            .default_height(720)
            .build();
        window.set_child(Some(&canvas(word, start, opts.effect)));
        let keys = gtk::EventControllerKey::new();
        let w = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                w.close();
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);
        window.present();
        return;
    }

    let Some(display) = gdk::Display::default() else {
        return;
    };
    let monitors: Vec<gdk::Monitor> = display
        .monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .collect();
    for (i, monitor) in monitors.iter().enumerate() {
        let window = gtk::ApplicationWindow::builder().application(app).build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-screensaver");
        window.set_monitor(monitor);
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(if i == 0 {
            KeyboardMode::Exclusive
        } else {
            KeyboardMode::OnDemand
        });
        window.set_cursor_from_name(Some("none"));
        window.set_child(Some(&canvas(word.clone(), start, opts.effect)));
        if !opts.managed {
            quit_on_input(app, &window, start);
        }
        window.present();
    }
}

/// A preview ends on any key, click, scroll or real pointer move. (When
/// umoja-mir runs it, umoja-mir decides: it may lock the session first.)
fn quit_on_input(app: &gtk::Application, window: &gtk::ApplicationWindow, start: Instant) {
    let quit = {
        let app = app.clone();
        move || {
            if start.elapsed() >= GRACE {
                app.quit();
            }
        }
    };
    let keys = gtk::EventControllerKey::new();
    let q = quit.clone();
    keys.connect_key_pressed(move |_, _, _, _| {
        q();
        glib::Propagation::Stop
    });
    window.add_controller(keys);

    let click = gtk::GestureClick::new();
    click.set_button(0);
    let q = quit.clone();
    click.connect_pressed(move |_, _, _, _| q());
    window.add_controller(click);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let q = quit.clone();
    scroll.connect_scroll(move |_, _, _| {
        q();
        glib::Propagation::Stop
    });
    window.add_controller(scroll);

    let motion = gtk::EventControllerMotion::new();
    let origin: Rc<Cell<Option<(f64, f64)>>> = Rc::default();
    motion.connect_motion(move |_, x, y| match origin.get() {
        None => origin.set(Some((x, y))),
        Some((ox, oy)) if (x - ox).hypot(y - oy) > MOTION_SLOP => quit(),
        Some(_) => {}
    });
    window.add_controller(motion);
}

fn canvas(word: Rc<art::Word>, start: Instant, effect: Option<usize>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_draw_func(move |_, cr, w, h| {
        paint(
            cr,
            &word,
            w as f64,
            h as f64,
            start.elapsed().as_secs_f64(),
            effect,
        );
    });
    area.add_tick_callback(|area, _| {
        area.queue_draw();
        glib::ControlFlow::Continue
    });
    area
}

fn paint(
    cr: &cairo::Context,
    word: &art::Word,
    w: f64,
    h: f64,
    elapsed: f64,
    effect: Option<usize>,
) {
    cr.set_source_rgb(0.0, 0.0, 0.0);
    let _ = cr.paint();
    let frame = art::frame(word, w, h, elapsed, effect);
    for m in &frame.marks {
        if m.rgba.3 <= 0.005 || m.radius <= 0.1 {
            continue;
        }
        cr.set_source_rgba(m.rgba.0, m.rgba.1, m.rgba.2, m.rgba.3);
        cr.arc(m.x, m.y, m.radius, 0.0, TAU);
        let _ = cr.fill();
    }
}
