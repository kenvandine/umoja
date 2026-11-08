//! GNOME-style screenshot UI (Print key).
//!
//! Mirrors gnome-shell's: the screen freezes and dims, you drag out an area
//! (or pick the whole screen), and a floating bottom panel offers
//! Selection / Screen / Window, Screenshot / Screencast, Show Pointer and a
//! round capture button. Captures land in `~/Pictures/Screenshots` as
//! "Screenshot From YYYY-MM-DD HH-MM-SS.png", are copied to the clipboard,
//! and a "Screenshot captured" notification is shown, exactly as on GNOME.
//!
//! Pixels come from umoja-mir via `zwlr_screencopy_manager_v1` using `grim`.
//!
//! TODO(phase 4): window picker with live thumbnails, screencast (PipeWire),
//! one surface per monitor for multi-monitor selection.

use adw::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::glib;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use umoja_common::i18n;

/// Selection rectangle in widget (logical) coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sel {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Sel {
    fn from_points(a: (f64, f64), b: (f64, f64)) -> Self {
        Self {
            x: a.0.min(b.0),
            y: a.1.min(b.1),
            w: (a.0 - b.0).abs(),
            h: (a.1 - b.1).abs(),
        }
    }

    fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 <= self.x + self.w && p.1 >= self.y && p.1 <= self.y + self.h
    }

    /// Map to image pixels, clamped to the image.
    pub fn to_pixels(self, scale: f64, img_w: i32, img_h: i32) -> (i32, i32, i32, i32) {
        let x = ((self.x * scale).round() as i32).clamp(0, img_w.max(1) - 1);
        let y = ((self.y * scale).round() as i32).clamp(0, img_h.max(1) - 1);
        let w = ((self.w * scale).round() as i32).clamp(1, img_w - x);
        let h = ((self.h * scale).round() as i32).clamp(1, img_h - y);
        (x, y, w, h)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Selection,
    Screen,
}

pub struct ScreenshotUi {
    window: gtk::ApplicationWindow,
    app: adw::Application,
    picture: gtk::Picture,
    area: gtk::DrawingArea,
    /// Frozen frames: (without pointer, with pointer), in physical pixels.
    frame: RefCell<Option<(Pixbuf, Pixbuf)>>,
    sel: Cell<Sel>,
    mode: Cell<Mode>,
    show_pointer: Cell<bool>,
    drag: Cell<Option<DragKind>>,
    /// Set while the assistant asked for an area: the capture goes here
    /// instead of to ~/Pictures (`None` if the user cancelled).
    ask: RefCell<Option<AskCallback>>,
}

pub type AskCallback = Box<dyn FnOnce(Option<Pixbuf>)>;

#[derive(Clone, Copy)]
enum DragKind {
    New((f64, f64)),
    Move { grab: (f64, f64), start: Sel },
}

/// `~/Pictures/Screenshots/Screenshot From 2026-11-07 21-05-33.png`
pub fn screenshot_path() -> PathBuf {
    let dir = glib::user_special_dir(glib::UserDirectory::Pictures)
        .unwrap_or_else(|| glib::home_dir().join("Pictures"))
        // gnome-shell uses the translated folder name; "Screenshots" is the C locale.
        .join(i18n::shell("Screenshots"));
    let stamp = glib::DateTime::now_local()
        .and_then(|d| d.format("%Y-%m-%d %H-%M-%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    // "Screenshot From %s" is gnome-shell's own (translated) file name pattern.
    dir.join(format!(
        "{}.png",
        i18n::shell("Screenshot From %s").replace("%s", &stamp)
    ))
}

fn grim(path: &Path, region: Option<(i32, i32, i32, i32)>, cursor: bool) -> bool {
    let mut cmd = Command::new("grim");
    if cursor {
        cmd.arg("-c");
    }
    if let Some((x, y, w, h)) = region {
        cmd.args(["-g", &format!("{x},{y} {w}x{h}")]);
    }
    matches!(cmd.arg(path).status(), Ok(s) if s.success())
}

impl ScreenshotUi {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-screenshot"])
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-screenshot");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Fill)
            .build();
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&area);

        let ui = Rc::new(Self {
            window: window.clone(),
            app: app.clone(),
            picture,
            area: area.clone(),
            frame: RefCell::default(),
            sel: Cell::new(Sel::default()),
            mode: Cell::new(Mode::Selection),
            show_pointer: Cell::new(false),
            drag: Cell::new(None),
            ask: RefCell::new(None),
        });

        overlay.add_overlay(&ui.panel());
        window.set_child(Some(&overlay));
        ui.connect_drawing();
        ui.connect_keys();
        ui
    }

    /// The floating bottom panel, laid out like gnome-shell's.
    fn panel(self: &Rc<Self>) -> gtk::Box {
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 12);
        panel.add_css_class("screenshot-panel");
        panel.set_halign(gtk::Align::Center);
        panel.set_valign(gtk::Align::End);

        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["circular", "close"])
            .halign(gtk::Align::End)
            .build();
        let weak = Rc::downgrade(self);
        close.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.close();
            }
        });
        panel.append(&close);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 24);
        row.set_halign(gtk::Align::Center);

        // Selection | Screen | Window
        let modes = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        modes.add_css_class("mode-group");
        let selection = mode_button(
            "screenshot-ui-area-symbolic",
            "edit-select-all-symbolic",
            &i18n::shell("Selection"),
        );
        let screen = mode_button(
            "screenshot-ui-display-symbolic",
            "video-display-symbolic",
            &i18n::shell("Screen"),
        );
        let window_btn = mode_button(
            "screenshot-ui-window-symbolic",
            "focus-windows-symbolic",
            &i18n::shell("Window"),
        );
        screen.set_group(Some(&selection));
        window_btn.set_group(Some(&selection));
        window_btn.set_sensitive(false); // TODO(phase 4): window picker
        selection.set_active(true);
        for (b, m) in [(&selection, Mode::Selection), (&screen, Mode::Screen)] {
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                if let (true, Some(ui)) = (b.is_active(), weak.upgrade()) {
                    ui.mode.set(m);
                    ui.area.queue_draw();
                }
            });
        }
        modes.append(&selection);
        modes.append(&screen);
        modes.append(&window_btn);

        let capture = gtk::Button::builder()
            .css_classes(["capture-button"])
            .tooltip_text(i18n::shell("Capture"))
            .build();
        let weak = Rc::downgrade(self);
        capture.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.capture();
            }
        });

        // Screenshot | Screencast
        let kinds = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        kinds.add_css_class("type-group");
        let shot = gtk::ToggleButton::builder()
            .icon_name("camera-photo-symbolic")
            .tooltip_text(i18n::shell("Screenshot"))
            .active(true)
            .build();
        let cast = gtk::ToggleButton::builder()
            .icon_name("camera-video-symbolic")
            .tooltip_text(i18n::shell("Screencast"))
            .sensitive(false)
            .build();
        cast.set_group(Some(&shot));
        kinds.append(&shot);
        kinds.append(&cast);

        let pointer = gtk::ToggleButton::builder()
            .icon_name("screenshot-ui-show-pointer-symbolic")
            .tooltip_text(i18n::shell("Show Pointer"))
            .css_classes(["circular", "pointer-toggle"])
            .build();
        let weak = Rc::downgrade(self);
        pointer.connect_toggled(move |b| {
            if let Some(ui) = weak.upgrade() {
                ui.show_pointer.set(b.is_active());
            }
        });

        row.append(&modes);
        row.append(&capture);
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        right.append(&kinds);
        right.append(&pointer);
        row.append(&right);
        panel.append(&row);
        panel
    }

    fn connect_drawing(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.area.set_draw_func(move |_, cr, w, h| {
            let Some(ui) = weak.upgrade() else { return };
            let (w, h) = (w as f64, h as f64);
            let sel = match ui.mode.get() {
                Mode::Screen => Sel {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
                Mode::Selection => ui.sel.get(),
            };
            // Dim everything outside the selection.
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
            cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
            cr.rectangle(0.0, 0.0, w, h);
            cr.rectangle(sel.x, sel.y, sel.w, sel.h);
            let _ = cr.fill();
            if ui.mode.get() == Mode::Selection && sel.w > 0.0 {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                cr.set_line_width(1.0);
                cr.rectangle(sel.x + 0.5, sel.y + 0.5, sel.w - 1.0, sel.h - 1.0);
                let _ = cr.stroke();
                // Corner handles, as in gnome-shell.
                for (hx, hy) in [
                    (sel.x, sel.y),
                    (sel.x + sel.w, sel.y),
                    (sel.x, sel.y + sel.h),
                    (sel.x + sel.w, sel.y + sel.h),
                ] {
                    cr.arc(hx, hy, 6.0, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                }
            }
        });

        let drag = gtk::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.mode.get() != Mode::Selection {
                return;
            }
            let sel = ui.sel.get();
            ui.drag.set(Some(if sel.w > 0.0 && sel.contains((x, y)) {
                DragKind::Move {
                    grab: (x, y),
                    start: sel,
                }
            } else {
                DragKind::New((x, y))
            }));
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(ui) = weak.upgrade() else { return };
            let Some((sx, sy)) = g.start_point() else {
                return;
            };
            let (w, h) = (ui.area.width() as f64, ui.area.height() as f64);
            match ui.drag.get() {
                Some(DragKind::New(origin)) => {
                    let end = ((sx + dx).clamp(0.0, w), (sy + dy).clamp(0.0, h));
                    ui.sel.set(Sel::from_points(origin, end));
                }
                Some(DragKind::Move { grab, start }) => {
                    let nx = (start.x + (sx + dx - grab.0)).clamp(0.0, w - start.w);
                    let ny = (start.y + (sy + dy - grab.1)).clamp(0.0, h - start.h);
                    ui.sel.set(Sel {
                        x: nx,
                        y: ny,
                        ..start
                    });
                }
                None => {}
            }
            ui.area.queue_draw();
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(ui) = weak.upgrade() {
                ui.drag.set(None);
            }
        });
        self.area.add_controller(drag);
    }

    fn connect_keys(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(ui) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => ui.close(),
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => ui.capture(),
                gdk::Key::s | gdk::Key::S => ui.mode.set(Mode::Selection),
                gdk::Key::c | gdk::Key::C => ui.mode.set(Mode::Screen),
                _ => return glib::Propagation::Proceed,
            }
            ui.area.queue_draw();
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    /// Freeze the screen and open the UI (Print).
    pub fn open(self: &Rc<Self>) {
        // Grab with and without the pointer up front, so "Show Pointer" can be
        // toggled after the fact like in gnome-shell.
        let dir = glib::user_runtime_dir();
        let plain = dir.join(format!("umoja-shot-{}.png", std::process::id()));
        let cursor = dir.join(format!("umoja-shot-{}-cursor.png", std::process::id()));
        let ok = grim(&plain, None, false) && grim(&cursor, None, true);
        let frames = ok.then(|| {
            (
                Pixbuf::from_file(&plain).ok(),
                Pixbuf::from_file(&cursor).ok(),
            )
        });
        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(&cursor);
        let Some((Some(plain), Some(cursor))) = frames else {
            tracing::warn!("grim failed; is it installed?");
            return;
        };
        self.picture
            .set_paintable(Some(&gdk::Texture::for_pixbuf(&plain)));
        *self.frame.borrow_mut() = Some((plain, cursor));
        // gnome-shell starts with a centred default selection.
        let (w, h) = self.monitor_size();
        self.sel.set(Sel {
            x: w * 0.25,
            y: h * 0.25,
            w: w * 0.5,
            h: h * 0.5,
        });
        self.window.present();
        self.area.queue_draw();
    }

    fn monitor_size(&self) -> (f64, f64) {
        gdk::Display::default()
            .and_then(|d| d.monitors().item(0))
            .and_downcast::<gdk::Monitor>()
            .map(|m| (m.geometry().width() as f64, m.geometry().height() as f64))
            .unwrap_or((1280.0, 800.0))
    }

    /// Open the same UI to pick an area for the assistant to read.
    pub fn open_for_ask(self: &Rc<Self>, done: AskCallback) {
        *self.ask.borrow_mut() = Some(done);
        self.open();
        if !self.window.is_visible() {
            // grim failed
            if let Some(done) = self.ask.borrow_mut().take() {
                done(None);
            }
        }
    }

    fn close(&self) {
        self.window.set_visible(false);
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.frame.borrow_mut().take();
        let ask = self.ask.borrow_mut().take();
        if let Some(done) = ask {
            done(None);
        }
    }

    fn capture(&self) {
        let frame = self.frame.borrow().as_ref().map(|(plain, cursor)| {
            if self.show_pointer.get() {
                cursor.clone()
            } else {
                plain.clone()
            }
        });
        let Some(full) = frame else { return };
        let pixbuf = match self.mode.get() {
            Mode::Screen => full,
            Mode::Selection => {
                let sel = self.sel.get();
                if sel.w < 1.0 || sel.h < 1.0 {
                    return;
                }
                // Frozen frame is in physical pixels; the UI is logical.
                let scale = full.width() as f64 / self.area.width().max(1) as f64;
                let (x, y, w, h) = sel.to_pixels(scale, full.width(), full.height());
                full.new_subpixbuf(x, y, w, h)
            }
        };
        let ask = self.ask.borrow_mut().take();
        self.close();
        match ask {
            Some(done) => done(Some(pixbuf)),
            None => save_and_notify(&self.app, &pixbuf),
        }
    }

    /// Shift+Print: whole screen, no UI.
    pub fn capture_screen(&self) {
        save_and_notify_file(&self.app, None);
    }

    /// Alt+Print: the focused window's rectangle from the compositor.
    pub fn capture_area(&self, x: i32, y: i32, w: i32, h: i32) {
        save_and_notify_file(&self.app, Some((x, y, w, h)));
    }
}

fn mode_button(icon: &str, fallback: &str, label: &str) -> gtk::ToggleButton {
    let theme = gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d));
    let name = if theme.is_some_and(|t| t.has_icon(icon)) {
        icon
    } else {
        fallback
    };
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.append(&gtk::Image::builder().icon_name(name).pixel_size(24).build());
    content.append(&gtk::Label::new(Some(label)));
    gtk::ToggleButton::builder()
        .child(&content)
        .css_classes(["mode-button"])
        .build()
}

fn save_and_notify_file(app: &adw::Application, region: Option<(i32, i32, i32, i32)>) {
    let path = screenshot_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if !grim(&path, region, false) {
        tracing::warn!("screenshot failed");
        return;
    }
    if let Ok(p) = Pixbuf::from_file(&path) {
        finish(app, &path, &p);
    }
}

fn save_and_notify(app: &adw::Application, pixbuf: &Pixbuf) {
    let path = screenshot_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = pixbuf.savev(&path, "png", &[]) {
        tracing::warn!(error = %e, "could not save screenshot");
        return;
    }
    finish(app, &path, pixbuf);
}

/// Clipboard + "Screenshot captured" notification, as gnome-shell does.
fn finish(app: &adw::Application, path: &Path, pixbuf: &Pixbuf) {
    if let Some(display) = gdk::Display::default() {
        display
            .clipboard()
            .set_texture(&gdk::Texture::for_pixbuf(pixbuf));
    }
    let n = gio::Notification::new(&i18n::shell("Screenshot captured"));
    n.set_body(Some(&i18n::shell(
        "You can paste the image from the clipboard.",
    )));
    if let Ok(file_icon) =
        gio::FileIcon::new(&gio::File::for_path(path)).dynamic_cast::<gio::Icon>()
    {
        n.set_icon(&file_icon);
    }
    app.send_notification(Some("screenshot"), &n);
    tracing::info!(path = %path.display(), "screenshot saved");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_maps_to_pixels() {
        let s = Sel::from_points((300.0, 200.0), (100.0, 50.0));
        assert_eq!(
            s,
            Sel {
                x: 100.0,
                y: 50.0,
                w: 200.0,
                h: 150.0
            }
        );
        // 2x HiDPI frame.
        assert_eq!(s.to_pixels(2.0, 3840, 2160), (200, 100, 400, 300));
        // Clamped at the right/bottom edge.
        let edge = Sel {
            x: 1900.0,
            y: 1000.0,
            w: 100.0,
            h: 100.0,
        };
        assert_eq!(edge.to_pixels(1.0, 1920, 1080), (1900, 1000, 20, 80));
    }
}
