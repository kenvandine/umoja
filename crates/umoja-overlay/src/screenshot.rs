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
//! Window mode picks one of the current workspace's windows in place on the
//! frozen screen (the focused one to start with); a window that others cover
//! is raised and grabbed live, since Mir has no per-window capture source
//! yet. Screencast records the selection, screen or window with
//! [`crate::screencast::Recorder`]; Ctrl+Shift+Alt+R opens the UI in
//! screencast mode, or stops a running recording.
//!
//! TODO: GNOME's spread-out window picker once Mir offers toplevel capture
//! (ext-image-capture-source's foreign-toplevel source), and one surface per
//! monitor for multi-monitor selection.

use crate::context;
use crate::screencast::Recorder;
use crate::spread::{self, Win};
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

    /// Which edges a press at `p` grabs: corners grab two, edges one,
    /// as in gnome-shell. `None` if the press is not on the border.
    fn edges_at(&self, p: (f64, f64)) -> Option<Edges> {
        if self.w <= 0.0 || self.h <= 0.0 {
            return None;
        }
        let (l, r, t, b) = (self.x, self.x + self.w, self.y, self.y + self.h);
        let within_y = p.1 >= t - GRAB && p.1 <= b + GRAB;
        let within_x = p.0 >= l - GRAB && p.0 <= r + GRAB;
        // On a small selection the grab zones overlap; take the closer side.
        let near_l = within_y && (p.0 - l).abs() <= GRAB && (p.0 - l).abs() <= (p.0 - r).abs();
        let near_r = within_y && !near_l && (p.0 - r).abs() <= GRAB;
        let near_t = within_x && (p.1 - t).abs() <= GRAB && (p.1 - t).abs() <= (p.1 - b).abs();
        let near_b = within_x && !near_t && (p.1 - b).abs() <= GRAB;
        let e = Edges {
            left: near_l,
            right: near_r,
            top: near_t,
            bottom: near_b,
        };
        (e.left || e.right || e.top || e.bottom).then_some(e)
    }

    /// `start` with the grabbed edges moved to `p`. Dragging past the
    /// opposite edge flips the selection rather than collapsing it.
    fn resized(start: Sel, edges: Edges, p: (f64, f64)) -> Sel {
        let (mut l, mut r, mut t, mut b) = (start.x, start.x + start.w, start.y, start.y + start.h);
        if edges.left {
            l = p.0;
        }
        if edges.right {
            r = p.0;
        }
        if edges.top {
            t = p.1;
        }
        if edges.bottom {
            b = p.1;
        }
        Sel::from_points((l, t), (r, b))
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Selection,
    Screen,
    Window,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Screenshot,
    Screencast,
    /// Umoja's addition: read the area's text (OCR) onto the clipboard.
    Text,
}

const FLAG_FOCUSED: u32 = 1 << 0;
const FLAG_MINIMIZED: u32 = 1 << 1;
const FLAG_CURRENT_WORKSPACE: u32 = 1 << 2;

/// A window the Window mode can pick, in UI (logical, monitor-local) space.
#[derive(Debug, Clone)]
struct Pick {
    id: u64,
    rect: Sel,
    /// Some window above it overlaps it on screen.
    covered: bool,
}

/// The current workspace's visible windows, bottom to top, clipped to the
/// monitor at `origin` of size `size`.
fn picks(windows: &[Win], origin: (f64, f64), size: (f64, f64)) -> (Vec<Pick>, Option<usize>) {
    let shown: Vec<&Win> = windows
        .iter()
        .filter(|w| w.flags & FLAG_MINIMIZED == 0 && w.flags & FLAG_CURRENT_WORKSPACE != 0)
        .filter(|w| w.w > 0 && w.h > 0)
        .collect();
    let mut out = Vec::new();
    let mut focused = None;
    for (i, w) in shown.iter().enumerate() {
        let x0 = (w.x as f64 - origin.0).max(0.0);
        let y0 = (w.y as f64 - origin.1).max(0.0);
        let x1 = ((w.x + w.w) as f64 - origin.0).min(size.0);
        let y1 = ((w.y + w.h) as f64 - origin.1).min(size.1);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let covered = shown[i + 1..]
            .iter()
            .any(|o| o.x < w.x + w.w && w.x < o.x + o.w && o.y < w.y + w.h && w.y < o.y + o.h);
        if w.flags & FLAG_FOCUSED != 0 {
            focused = Some(out.len());
        }
        out.push(Pick {
            id: w.id,
            rect: Sel {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            },
            covered,
        });
    }
    // gnome-shell preselects the focused window, else the topmost.
    let sel = focused.or(out.len().checked_sub(1));
    (out, sel)
}

/// The topmost pick under `p`.
fn pick_at(picks: &[Pick], p: (f64, f64)) -> Option<usize> {
    picks.iter().rposition(|w| w.rect.contains(p))
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
    kind: Cell<Kind>,
    show_pointer: Cell<bool>,
    /// Window mode: the windows and the picked one.
    windows: RefCell<Vec<Pick>>,
    picked: Cell<Option<usize>>,
    hovered: Cell<Option<usize>>,
    /// Bumped on every open so a late window list is dropped.
    generation: Cell<u32>,
    mode_buttons: RefCell<Vec<(Mode, gtk::ToggleButton)>>,
    kind_buttons: RefCell<Vec<(Kind, gtk::ToggleButton)>>,
    capture_button: RefCell<Option<gtk::Button>>,
    pointer_button: RefCell<Option<gtk::ToggleButton>>,
    recorder: Rc<Recorder>,
    drag: Cell<Option<DragKind>>,
    /// Set while the assistant asked for an area: the capture goes here
    /// instead of to ~/Pictures (`None` if the user cancelled).
    ask: RefCell<Option<AskCallback>>,
}

pub type AskCallback = Box<dyn FnOnce(Option<Pixbuf>)>;

/// How far (logical px) from the border a press still grabs it.
const GRAB: f64 = 12.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn cursor(self) -> &'static str {
        match (self.left, self.right, self.top, self.bottom) {
            (true, _, true, _) | (_, true, _, true) => "nwse-resize",
            (true, _, _, true) | (_, true, true, _) => "nesw-resize",
            (true, _, _, _) | (_, true, _, _) => "ew-resize",
            _ => "ns-resize",
        }
    }
}

#[derive(Clone, Copy)]
enum DragKind {
    New((f64, f64)),
    Move { grab: (f64, f64), start: Sel },
    Resize { edges: Edges, start: Sel },
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
            kind: Cell::new(Kind::Screenshot),
            show_pointer: Cell::new(false),
            windows: RefCell::default(),
            picked: Cell::new(None),
            hovered: Cell::new(None),
            generation: Cell::new(0),
            mode_buttons: RefCell::default(),
            kind_buttons: RefCell::default(),
            capture_button: RefCell::default(),
            pointer_button: RefCell::default(),
            recorder: Recorder::new(app),
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
            &["screenshot-ui-area-symbolic", "edit-select-all-symbolic"],
            &i18n::shell("Selection"),
        );
        let screen = mode_button(
            &["screenshot-ui-display-symbolic", "video-display-symbolic"],
            &i18n::shell("Screen"),
        );
        let window_btn = mode_button(
            &[
                "screenshot-ui-window-symbolic",
                "focus-windows-symbolic",
                "window-new-symbolic",
                "preferences-system-windows-symbolic",
            ],
            &i18n::shell("Window"),
        );
        screen.set_group(Some(&selection));
        window_btn.set_group(Some(&selection));
        selection.set_active(true);
        for (b, m) in [
            (&selection, Mode::Selection),
            (&screen, Mode::Screen),
            (&window_btn, Mode::Window),
        ] {
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                if let (true, Some(ui)) = (b.is_active(), weak.upgrade()) {
                    ui.mode.set(m);
                    ui.area.queue_draw();
                }
            });
            self.mode_buttons.borrow_mut().push((m, b.clone()));
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
            .build();
        cast.set_group(Some(&shot));
        let text = gtk::ToggleButton::builder()
            .child(&gtk::Image::from_gicon(&gtk::gio::ThemedIcon::from_names(
                &[
                    "text-recognition-symbolic",
                    "insert-text-symbolic",
                    "edit-copy-symbolic",
                ],
            )))
            .tooltip_text(i18n::umoja("Copy Text"))
            .build();
        text.set_group(Some(&shot));
        for (b, k) in [
            (&shot, Kind::Screenshot),
            (&cast, Kind::Screencast),
            (&text, Kind::Text),
        ] {
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                if let (true, Some(ui)) = (b.is_active(), weak.upgrade()) {
                    ui.set_kind(k);
                }
            });
            self.kind_buttons.borrow_mut().push((k, b.clone()));
        }
        kinds.append(&shot);
        kinds.append(&cast);
        kinds.append(&text);
        *self.capture_button.borrow_mut() = Some(capture.clone());

        // The screenshot-ui-* icons ship inside gnome-shell, not in icon
        // themes, so fall back to a themed pointer.
        let pointer = gtk::ToggleButton::builder()
            .child(&gtk::Image::from_gicon(&gtk::gio::ThemedIcon::from_names(
                &[
                    "screenshot-ui-show-pointer-symbolic",
                    "input-mouse-symbolic",
                    "pointer-primary-click-symbolic",
                ],
            )))
            .tooltip_text(i18n::shell("Show Pointer"))
            .css_classes(["circular", "pointer-toggle"])
            .build();
        let weak = Rc::downgrade(self);
        pointer.connect_toggled(move |b| {
            if let Some(ui) = weak.upgrade() {
                ui.show_pointer.set(b.is_active());
            }
        });
        *self.pointer_button.borrow_mut() = Some(pointer.clone());

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
                Mode::Window => ui.picked_rect().unwrap_or_default(),
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
            if ui.mode.get() == Mode::Window {
                let windows = ui.windows.borrow();
                // The window under the pointer gets a faint outline...
                if let Some(r) = ui
                    .hovered
                    .get()
                    .filter(|h| Some(*h) != ui.picked.get())
                    .and_then(|i| windows.get(i))
                    .map(|p| p.rect)
                {
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.5);
                    cr.set_line_width(2.0);
                    rounded(cr, r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0, 8.0);
                    let _ = cr.stroke();
                }
                // ...and the picked one gnome-shell's white frame.
                if sel.w > 0.0 {
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                    cr.set_line_width(4.0);
                    rounded(cr, sel.x + 2.0, sel.y + 2.0, sel.w - 4.0, sel.h - 4.0, 8.0);
                    let _ = cr.stroke();
                }
            }
        });

        let drag = gtk::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.mode.get() == Mode::Window {
                let hit = pick_at(&ui.windows.borrow(), (x, y));
                if hit.is_some() {
                    ui.picked.set(hit);
                    ui.area.queue_draw();
                }
                return;
            }
            if ui.mode.get() != Mode::Selection {
                return;
            }
            let sel = ui.sel.get();
            ui.drag.set(Some(if let Some(edges) = sel.edges_at((x, y)) {
                DragKind::Resize { edges, start: sel }
            } else if sel.w > 0.0 && sel.contains((x, y)) {
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
                Some(DragKind::Resize { edges, start }) => {
                    let p = ((sx + dx).clamp(0.0, w), (sy + dy).clamp(0.0, h));
                    ui.sel.set(Sel::resized(start, edges, p));
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

        // Resize/move cursors on hover, like gnome-shell.
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.drag.get().is_some() {
                return;
            }
            let sel = ui.sel.get();
            if ui.mode.get() == Mode::Window {
                let hit = pick_at(&ui.windows.borrow(), (x, y));
                if hit != ui.hovered.get() {
                    ui.hovered.set(hit);
                    ui.area.queue_draw();
                }
            }
            let name = if ui.mode.get() == Mode::Window {
                "pointer"
            } else if ui.mode.get() != Mode::Selection {
                "default"
            } else if let Some(e) = sel.edges_at((x, y)) {
                e.cursor()
            } else if sel.w > 0.0 && sel.contains((x, y)) {
                "move"
            } else {
                "crosshair"
            };
            ui.area.set_cursor_from_name(Some(name));
        });
        self.area.add_controller(motion);
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
                // gnome-shell's keys: S, C, W pick the mode, V toggles
                // screenshot/screencast, P the pointer.
                gdk::Key::s | gdk::Key::S => ui.set_mode(Mode::Selection),
                gdk::Key::c | gdk::Key::C => ui.set_mode(Mode::Screen),
                gdk::Key::w | gdk::Key::W => ui.set_mode(Mode::Window),
                gdk::Key::v | gdk::Key::V => ui.select_kind(match ui.kind.get() {
                    Kind::Screenshot => Kind::Screencast,
                    Kind::Screencast | Kind::Text => Kind::Screenshot,
                }),
                gdk::Key::p | gdk::Key::P => {
                    if let Some(b) = ui.pointer_button.borrow().as_ref() {
                        b.set_active(!b.is_active());
                    }
                }
                _ => return glib::Propagation::Proceed,
            }
            ui.area.queue_draw();
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    /// Freeze the screen and open the UI (Print).
    pub fn open(self: &Rc<Self>) {
        self.open_as(Kind::Screenshot);
    }

    /// Ctrl+Shift+Alt+R: the UI in screencast mode, or stop the recording.
    pub fn toggle_screen_recording(self: &Rc<Self>) {
        if self.recorder.is_recording() {
            self.recorder.stop();
        } else {
            self.open_as(Kind::Screencast);
        }
    }

    /// Super+Ctrl+Print: the UI in Copy Text mode, starting on Selection.
    pub fn open_copy_text(self: &Rc<Self>) {
        self.open_as(Kind::Text);
        if self.window.is_visible() {
            self.set_mode(Mode::Selection);
        }
    }

    pub fn recorder(&self) -> &Rc<Recorder> {
        &self.recorder
    }

    fn open_as(self: &Rc<Self>, kind: Kind) {
        if self.window.is_visible() {
            self.select_kind(kind);
            return;
        }
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
        self.load_windows();
        // The assistant wants a picture, and only one recording runs at a time.
        let can_cast = self.ask.borrow().is_none() && !self.recorder.is_recording();
        let asking = self.ask.borrow().is_some();
        for (k, b) in self.kind_buttons.borrow().iter() {
            match k {
                Kind::Screencast => b.set_sensitive(can_cast),
                Kind::Text => b.set_sensitive(!asking),
                Kind::Screenshot => {}
            }
        }
        let kind = match kind {
            Kind::Screencast if !can_cast => Kind::Screenshot,
            Kind::Text if asking => Kind::Screenshot,
            k => k,
        };
        self.select_kind(kind);
        self.window.present();
        self.area.queue_draw();
    }

    /// Fetch the windows for Window mode; the UI opens without waiting.
    fn load_windows(self: &Rc<Self>) {
        self.windows.borrow_mut().clear();
        self.picked.set(None);
        self.hovered.set(None);
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let windows = spread::fetch_windows().await;
            let Some(ui) = weak.upgrade() else { return };
            if ui.generation.get() != generation {
                return;
            }
            let (origin, size) = ui.monitor_geometry();
            let (list, picked) = picks(&windows, origin, size);
            *ui.windows.borrow_mut() = list;
            ui.picked.set(picked);
            ui.area.queue_draw();
        });
    }

    fn set_mode(&self, mode: Mode) {
        for (m, b) in self.mode_buttons.borrow().iter() {
            if *m == mode {
                b.set_active(true);
            }
        }
    }

    fn select_kind(&self, kind: Kind) {
        let buttons = self.kind_buttons.borrow();
        let Some((_, b)) = buttons.iter().find(|(k, _)| *k == kind) else {
            return;
        };
        if !b.is_sensitive() {
            return;
        }
        if b.is_active() {
            self.set_kind(kind);
        } else {
            b.set_active(true);
        }
    }

    fn set_kind(&self, kind: Kind) {
        self.kind.set(kind);
        if let Some(c) = self.capture_button.borrow().as_ref() {
            // gnome-shell's capture button turns red for a screencast.
            if kind == Kind::Screencast {
                c.add_css_class("screencast");
                c.set_tooltip_text(Some(&i18n::shell("Record Screen")));
            } else {
                c.remove_css_class("screencast");
                c.set_tooltip_text(Some(&if kind == Kind::Text {
                    i18n::umoja("Copy Text")
                } else {
                    i18n::shell("Capture")
                }));
            }
        }
    }

    fn picked_rect(&self) -> Option<Sel> {
        let i = self.picked.get()?;
        self.windows.borrow().get(i).map(|p| p.rect)
    }

    fn monitor(&self) -> Option<gdk::Monitor> {
        gdk::Display::default()
            .and_then(|d| d.monitors().item(0))
            .and_downcast::<gdk::Monitor>()
    }

    fn monitor_size(&self) -> (f64, f64) {
        self.monitor_geometry().1
    }

    /// Origin and size of the monitor the UI covers, in layout pixels.
    fn monitor_geometry(&self) -> ((f64, f64), (f64, f64)) {
        self.monitor()
            .map(|m| {
                let g = m.geometry();
                (
                    (g.x() as f64, g.y() as f64),
                    (g.width() as f64, g.height() as f64),
                )
            })
            .unwrap_or(((0.0, 0.0), (1280.0, 800.0)))
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
        self.windows.borrow_mut().clear();
        self.generation.set(self.generation.get().wrapping_add(1));
        let ask = self.ask.borrow_mut().take();
        if let Some(done) = ask {
            done(None);
        }
    }

    /// The area to capture, in UI (logical, monitor-local) pixels.
    fn target(&self) -> Option<Sel> {
        let (w, h) = (self.area.width() as f64, self.area.height() as f64);
        let sel = match self.mode.get() {
            Mode::Screen => Sel {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            Mode::Selection => self.sel.get(),
            Mode::Window => self.picked_rect()?,
        };
        (sel.w >= 1.0 && sel.h >= 1.0).then_some(sel)
    }

    fn capture(self: &Rc<Self>) {
        let Some(sel) = self.target() else { return };
        if self.kind.get() == Kind::Screencast {
            self.start_screencast(sel);
            return;
        }
        // A covered window is raised and grabbed live.
        if self.mode.get() == Mode::Window {
            let covered = self.picked.get().and_then(|i| {
                let w = self.windows.borrow();
                w.get(i).filter(|p| p.covered).map(|p| p.id)
            });
            if let Some(id) = covered {
                self.capture_raised(id, sel);
                return;
            }
        }
        let frame = self.frame.borrow().as_ref().map(|(plain, cursor)| {
            if self.show_pointer.get() {
                cursor.clone()
            } else {
                plain.clone()
            }
        });
        let Some(full) = frame else { return };
        let pixbuf = if self.mode.get() == Mode::Screen {
            full
        } else {
            // Frozen frame is in physical pixels; the UI is logical.
            let scale = full.width() as f64 / self.area.width().max(1) as f64;
            let (x, y, w, h) = sel.to_pixels(scale, full.width(), full.height());
            full.new_subpixbuf(x, y, w, h)
        };
        let ask = self.ask.borrow_mut().take();
        let kind = self.kind.get();
        self.close();
        match ask {
            Some(done) => done(Some(pixbuf)),
            None if kind == Kind::Text => copy_text(&self.app, &pixbuf),
            None => {
                save_and_notify(&self.app, &pixbuf);
            }
        }
    }

    /// `sel` in layout pixels for grim and wf-recorder.
    fn layout_region(&self, sel: Sel) -> (i32, i32, i32, i32) {
        let ((ox, oy), _) = self.monitor_geometry();
        (
            (sel.x + ox).round() as i32,
            (sel.y + oy).round() as i32,
            sel.w.round() as i32,
            sel.h.round() as i32,
        )
    }

    fn capture_raised(self: &Rc<Self>, id: u64, sel: Sel) {
        let region = self.layout_region(sel);
        let cursor = self.show_pointer.get() && self.kind.get() != Kind::Text;
        let ask = self.ask.borrow_mut().take();
        let kind = self.kind.get();
        self.close();
        activate_window(id);
        let app = self.app.clone();
        // Give the compositor a few frames to raise it and drop our surface.
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            let path = glib::user_runtime_dir()
                .join(format!("umoja-shot-{}-window.png", std::process::id()));
            let pixbuf = grim(&path, Some(region), cursor)
                .then(|| Pixbuf::from_file(&path).ok())
                .flatten();
            let _ = std::fs::remove_file(&path);
            match (ask, pixbuf) {
                (Some(done), p) => done(p),
                (None, Some(p)) if kind == Kind::Text => copy_text(&app, &p),
                (None, Some(p)) => {
                    save_and_notify(&app, &p);
                }
                (None, None) => tracing::warn!("window screenshot failed"),
            }
        });
    }

    fn start_screencast(self: &Rc<Self>, sel: Sel) {
        let region = self.layout_region(sel);
        let raise = (self.mode.get() == Mode::Window)
            .then(|| self.picked.get())
            .flatten()
            .and_then(|i| self.windows.borrow().get(i).map(|p| p.id));
        self.close();
        if let Some(id) = raise {
            activate_window(id);
        }
        let recorder = self.recorder.clone();
        // Start once our surface is gone, so it is not in the first frame.
        glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
            recorder.start(region)
        });
    }

    /// Shift+Print: whole screen, no UI.
    pub fn capture_screen(&self) {
        save_and_notify_file(&self.app, None);
    }

    /// Alt+Print: the focused window's rectangle from the compositor.
    pub fn capture_area(&self, x: i32, y: i32, w: i32, h: i32) {
        save_and_notify_file(&self.app, Some((x, y, w, h)));
    }

    /// For the assistant: open the UI to pick an area (selection mode);
    /// `done` gets the picked pixels, or `None` if the user cancelled.
    pub fn pick_area(self: &Rc<Self>, done: AskCallback) {
        self.open_for_ask(done);
        if self.window.is_visible() {
            self.set_mode(Mode::Selection);
        }
    }

    /// For the assistant: save `pixbuf` like a capture from the UI
    /// (~/Pictures/Screenshots, clipboard, notification).
    pub fn save_pixbuf(&self, pixbuf: &Pixbuf) -> Option<PathBuf> {
        save_and_notify(&self.app, pixbuf)
    }

    /// For the assistant: copy and announce a capture already saved with
    /// [`grab_to_file`].
    pub fn announce(&self, path: &Path) {
        match Pixbuf::from_file(path) {
            Ok(p) => finish(&self.app, path, &p),
            Err(e) => tracing::warn!(error = %e, "could not load the screenshot"),
        }
    }

    /// The first monitor's area in layout pixels, for a full-screen recording.
    pub fn screen_region(&self) -> (i32, i32, i32, i32) {
        let ((x, y), (w, h)) = self.monitor_geometry();
        (x as i32, y as i32, w as i32, h as i32)
    }
}

/// Capture `region` (all outputs when `None`) to a new file in
/// ~/Pictures/Screenshots. Blocking; safe off the main thread.
pub fn grab_to_file(region: Option<(i32, i32, i32, i32)>) -> Result<PathBuf, String> {
    let path = screenshot_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if grim(&path, region, false) {
        Ok(path)
    } else {
        Err("the screen couldn't be captured (is grim installed?)".into())
    }
}

/// Raise and focus a window through umoja-mir (for the assistant).
pub fn raise_window(id: u64) {
    activate_window(id);
}

/// Raise and focus a window through umoja-mir.
fn activate_window(id: u64) {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(umoja_common::dbus::COMPOSITOR_BUS_NAME),
            umoja_common::dbus::COMPOSITOR_OBJECT_PATH,
            umoja_common::dbus::COMPOSITOR_INTERFACE,
            "ActivateWindow",
            Some(&(id,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |_| {},
        );
    });
}

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// `icons` are tried in order; the first one the icon theme has is drawn.
fn mode_button(icons: &[&str], label: &str) -> gtk::ToggleButton {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.append(
        &gtk::Image::builder()
            .gicon(&gtk::gio::ThemedIcon::from_names(icons))
            .pixel_size(24)
            .build(),
    );
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

fn save_and_notify(app: &adw::Application, pixbuf: &Pixbuf) -> Option<PathBuf> {
    let path = screenshot_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = pixbuf.savev(&path, "png", &[]) {
        tracing::warn!(error = %e, "could not save screenshot");
        return None;
    }
    finish(app, &path, pixbuf);
    Some(path)
}

/// Copy Text: OCR `pixbuf` off the GTK thread, put the text on the
/// clipboard and say so (or why it failed) in a notification.
fn copy_text(app: &adw::Application, pixbuf: &Pixbuf) {
    let path = context::temp_png();
    if let Err(e) = pixbuf.savev(&path, "png", &[]) {
        tracing::warn!(error = %e, "could not save the area for OCR");
        return;
    }
    let image = context::TempImage(path);
    let app = app.clone();
    glib::spawn_future_local(async move {
        let result = context::blocking(move || {
            if !context::ocr_available() {
                return Err(context::OcrError::NotInstalled);
            }
            context::ocr_file(&image.0)
        })
        .await;
        let n = match result {
            Ok(text) => {
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(&text);
                }
                let n = gio::Notification::new(&i18n::umoja("Text copied"));
                n.set_body(Some(&text_preview(&text)));
                n
            }
            Err(e) => {
                let n = gio::Notification::new(&i18n::umoja("Couldn't copy the text"));
                n.set_body(Some(&e.message()));
                n
            }
        };
        n.set_icon(&gio::ThemedIcon::new("edit-copy-symbolic"));
        app.send_notification(Some("copy-text"), &n);
    });
}

/// The first couple of lines of OCR text, for the notification body.
fn text_preview(text: &str) -> String {
    const MAX: usize = 120;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        flat
    } else {
        let cut: String = flat.chars().take(MAX).collect();
        format!("{}…", cut.trim_end())
    }
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

    fn win(id: u64, x: i32, y: i32, w: i32, h: i32, flags: u32) -> Win {
        Win {
            id,
            app_id: String::new(),
            title: String::new(),
            x,
            y,
            w,
            h,
            flags,
            workspace: 0,
        }
    }

    #[test]
    fn copy_text_preview() {
        assert_eq!(
            text_preview("Call  me\n at 555-0100"),
            "Call me at 555-0100"
        );
        let long = "word ".repeat(60);
        let p = text_preview(&long);
        assert!(p.ends_with('…'));
        assert!(p.chars().count() <= 121);
    }

    #[test]
    fn window_picks() {
        let cur = FLAG_CURRENT_WORKSPACE;
        let windows = [
            win(1, 0, 0, 800, 600, cur),
            win(2, 500, 400, 800, 600, cur | FLAG_FOCUSED),
            win(3, 0, 0, 100, 100, cur | FLAG_MINIMIZED),
            win(4, 0, 0, 100, 100, 0), // other workspace
            win(5, 1500, 100, 200, 200, cur),
        ];
        let (p, sel) = picks(&windows, (0.0, 0.0), (1600.0, 900.0));
        assert_eq!(p.iter().map(|p| p.id).collect::<Vec<_>>(), [1, 2, 5]);
        assert_eq!(sel, Some(1)); // focused
        assert!(p[0].covered && !p[1].covered && !p[2].covered);
        // Clipped to the monitor.
        assert_eq!(
            p[1].rect,
            Sel {
                x: 500.0,
                y: 400.0,
                w: 800.0,
                h: 500.0
            }
        );
        assert_eq!(p[2].rect.w, 100.0);
        // Topmost wins where they overlap.
        assert_eq!(pick_at(&p, (600.0, 500.0)), Some(1));
        assert_eq!(pick_at(&p, (100.0, 100.0)), Some(0));
        assert_eq!(pick_at(&p, (1400.0, 50.0)), None);
        // No focused window: the topmost.
        let (_, sel) = picks(&windows[..1], (0.0, 0.0), (1600.0, 900.0));
        assert_eq!(sel, Some(0));
    }

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

    #[test]
    fn corners_and_edges_resize() {
        let s = Sel {
            x: 100.0,
            y: 100.0,
            w: 200.0,
            h: 100.0,
        };
        let br = s.edges_at((302.0, 198.0)).unwrap();
        assert!(br.right && br.bottom && !br.left && !br.top);
        let tl = s.edges_at((95.0, 104.0)).unwrap();
        assert!(tl.left && tl.top && !tl.right && !tl.bottom);
        let top = s.edges_at((200.0, 99.0)).unwrap();
        assert_eq!(
            top,
            Edges {
                top: true,
                ..Default::default()
            }
        );
        // Inside, away from the border: not a resize (it moves).
        assert_eq!(s.edges_at((200.0, 150.0)), None);
        // Far outside: not a resize (it starts a new selection).
        assert_eq!(s.edges_at((500.0, 500.0)), None);

        // Drag the bottom-right corner out; top-left stays put.
        let r = Sel::resized(s, br, (400.0, 250.0));
        assert_eq!(
            r,
            Sel {
                x: 100.0,
                y: 100.0,
                w: 300.0,
                h: 150.0
            }
        );
        // Drag the left edge past the right edge: flips, no collapse.
        let left = Edges {
            left: true,
            ..Default::default()
        };
        let r = Sel::resized(s, left, (350.0, 0.0));
        assert_eq!(
            r,
            Sel {
                x: 300.0,
                y: 100.0,
                w: 50.0,
                h: 100.0
            }
        );
    }
}
