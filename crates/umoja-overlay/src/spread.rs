//! The Activities overview (Super key), after gnome-shell 40+.
//!
//! A search entry on top, the workspace thumbnails under it (when there is
//! more than one workspace), and the current workspace as a card with the
//! wallpaper, its windows spread out side by side and scaled down together
//! (never up), with the neighbouring workspaces peeking in at the sides.
//! Each window has its app icon at the bottom and the title and a close
//! button on hover. Clicking a window activates it; clicking a thumbnail or
//! a neighbour, scrolling, or Page Up/Down switches workspace; dragging a
//! window onto a thumbnail or a neighbour moves it there. Typing switches to
//! the app search, Escape or Super goes back. The top bar and dock stay
//! usable, as with Ubuntu Dock in GNOME's overview.
//!
//! As in gnome-shell (workspacesView.js) the main view is always on the
//! primary monitor, and every other monitor gets a card of its own with
//! the windows on it. The small windows in the thumbnails can be dragged
//! too: onto another thumbnail, the main card, another monitor, or a gap
//! between two thumbnails (a new workspace there). A drop inside a
//! thumbnail or card puts the window at that spot on the screen, which
//! GNOME doesn't do.
//!
//! Windows, workspaces and actions come from umoja-mir
//! (`org.umoja.Compositor`). Previews are cut from screen captures and kept
//! per window: one taken just before the overview opens, and one taken
//! shortly after windows settle (the compositor's `WindowsChanged`), so a
//! window that is covered when the overview opens still shows what it
//! looked like when last uncovered, as do windows on other workspaces. Only
//! a window never seen uncovered shows its app icon.
//!
//! TODO: live per-window thumbnails once Mir offers toplevel capture
//! sources (ext-image-capture-source's foreign-toplevel source).

use crate::appgrid::AppGrid;
use crate::texpane::TexPane;
use adw::prelude::*;
use gio::prelude::*;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::{gdk, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Duration;
use umoja_common::gsettings as gs;
use umoja_common::{apps, dbus, i18n};

const FLAG_FOCUSED: u32 = 1 << 0;
const FLAG_MINIMIZED: u32 = 1 << 1;
const FLAG_CURRENT_WORKSPACE: u32 = 1 << 2;

/// How long window previews are kept after the overview closes.
const KEEP_PREVIEWS: Duration = Duration::from_secs(60);

/// Gap between previews and around the spread, in logical pixels.
const GAP: f64 = 32.0;
/// Workspace thumbnail height as a share of the monitor (GNOME's thumbnails
/// are about this size), and its bounds.
const THUMB_SHARE: f64 = 0.07;
const THUMB_MIN: f64 = 48.0;
const THUMB_MAX: f64 = 120.0;
/// How far the neighbouring workspaces peek in at the sides.
const PEEK: f64 = 56.0;
/// Space between the workspace card's edge and the window spread.
const CARD_PAD: f64 = 24.0;
/// Background preview capture: wait this long after the last window change
/// (apps redraw after a raise or resize), and capture at most this often.
const SNAP_SETTLE: Duration = Duration::from_millis(700);
const SNAP_INTERVAL: Duration = Duration::from_secs(10);
/// Capture scale (of the logical layout) when the overview opens: previews
/// are shown scaled down, so logical size is already sharp.
const OPEN_SCALE: f64 = 1.0;
/// Capture scale for the background preview refresh: half the logical
/// size, a few MB, enough for a preview shown until a fresh one replaces it.
const SNAP_SCALE: f64 = 0.5;
/// Pointer travel before a press on a window becomes a drag.
const DRAG_THRESHOLD: f64 = 8.0;

#[derive(Debug, Clone)]
pub struct Win {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub flags: u32,
    /// 0-based workspace, -1 for a window on every workspace.
    pub workspace: i32,
}

impl Win {
    fn intersects(&self, o: &Win) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// Scale and row assignment for `sizes` inside `avail` (w, h): every window
/// gets the same scale, rows keep the given order, and the row count that
/// allows the largest scale wins. Never scales up.
/// A touch drag this far sideways is a workspace swipe, not a tap.
const TOUCH_SWIPE_CLAIM: f64 = 16.0;
/// ...and this far switches workspace when the finger lifts.
const TOUCH_SWIPE_SWITCH: f64 = 80.0;

pub fn layout(sizes: &[(f64, f64)], avail: (f64, f64), gap: f64) -> (f64, Vec<Vec<usize>>) {
    let n = sizes.len();
    if n == 0 {
        return (1.0, Vec::new());
    }
    let mut best = (0.0, Vec::new());
    for rows in 1..=n {
        let per = n.div_ceil(rows);
        let groups: Vec<Vec<usize>> = (0..n)
            .collect::<Vec<_>>()
            .chunks(per)
            .map(|c| c.to_vec())
            .collect();
        let mut s = 1.0f64;
        let mut total_h = 0.0;
        for g in &groups {
            let w: f64 = g.iter().map(|&i| sizes[i].0).sum();
            let room = avail.0 - gap * (g.len() as f64 - 1.0);
            if w > 0.0 {
                s = s.min(room / w);
            }
            total_h += g.iter().map(|&i| sizes[i].1).fold(0.0, f64::max);
        }
        let room_h = avail.1 - gap * (groups.len() as f64 - 1.0);
        if total_h > 0.0 {
            s = s.min(room_h / total_h);
        }
        if s > best.0 {
            best = (s, groups);
        }
    }
    best
}

/// Size of the current-workspace card: the monitor's aspect, as large as
/// fits `avail` (w, h) with room for the neighbours peeking at the sides.
pub fn card_size(avail: (f64, f64), aspect: f64, peeks: bool) -> (f64, f64) {
    let side = if peeks { 2.0 * (PEEK + GAP / 2.0) } else { 0.0 };
    let max_w = (avail.0 - side).max(100.0);
    let h = avail.1.max(60.0);
    let aspect = if aspect > 0.0 { aspect } else { 16.0 / 10.0 };
    if h * aspect <= max_w {
        (h * aspect, h)
    } else {
        (max_w, max_w / aspect)
    }
}

/// Where a window sits inside a workspace thumbnail of `scale` (thumbnail
/// pixels per logical pixel), relative to the monitor at `origin`. `None`
/// for a window that isn't on that monitor or would be too small to show.
pub fn thumb_rect(
    w: &Win,
    origin: (i32, i32),
    mon: (i32, i32),
    scale: f64,
) -> Option<(f64, f64, f64, f64)> {
    let cx = w.x + w.w / 2 - origin.0;
    let cy = w.y + w.h / 2 - origin.1;
    if cx < 0 || cy < 0 || cx >= mon.0 || cy >= mon.1 {
        return None;
    }
    let x = ((w.x - origin.0).max(0) as f64) * scale;
    let y = ((w.y - origin.1).max(0) as f64) * scale;
    let rw = (w.w.min(mon.0) as f64 * scale).max(4.0);
    let rh = (w.h.min(mon.1) as f64 * scale).max(4.0);
    Some((x, y, rw, rh))
}

/// Which workspace the window shows in: its own, or the current one for a
/// window on every workspace.
fn shown_on(w: &Win, current: u32) -> u32 {
    if w.workspace < 0 {
        current
    } else {
        w.workspace as u32
    }
}

/// A monitor's (x, y, width, height) in layout pixels.
type Geo = (i32, i32, i32, i32);

/// What a dragged window is over.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DropKind {
    /// A neighbouring workspace peeking in at the side.
    Peek(u32),
    /// A workspace thumbnail of the primary monitor.
    Thumb { index: u32, mon: Geo },
    /// The workspace card of a monitor.
    Card { index: u32, mon: Geo },
    /// Between two thumbnails: a new workspace at this index.
    Gap(u32),
    /// Another monitor's view, at this point in layout pixels.
    Screen { mon: Geo, x: f64, y: f64 },
}

/// The monitor a window belongs to: the one holding its centre, else the
/// first (primary).
fn monitor_of((x, y, w, h): Geo, mons: &[Geo]) -> Geo {
    let (cx, cy) = (x + w / 2, y + h / 2);
    mons.iter()
        .copied()
        .find(|&(x, y, mw, mh)| cx >= x && cy >= y && cx < x + mw && cy < y + mh)
        .or_else(|| mons.first().copied())
        .unwrap_or((0, 0, 1280, 800))
}

/// Where a window's top left goes so the point grabbed at `grab` (a
/// fraction of its size) lands on `(px, py)`.
pub fn grab_place(px: f64, py: f64, grab: (f64, f64), size: (i32, i32)) -> (i32, i32) {
    (
        (px - grab.0 * size.0 as f64).round() as i32,
        (py - grab.1 * size.1 as f64).round() as i32,
    )
}

/// The layout point for `(fx, fy)`, fractions of a view of `mon`.
pub fn map_fraction(fx: f64, fy: f64, mon: Geo) -> (f64, f64) {
    (
        mon.0 as f64 + fx.clamp(0.0, 1.0) * mon.2 as f64,
        mon.1 as f64 + fy.clamp(0.0, 1.0) * mon.3 as f64,
    )
}

/// The same place on another monitor, proportionally.
pub fn translate(x: i32, y: i32, from: Geo, to: Geo) -> (i32, i32) {
    let fx = (x - from.0) as f64 / from.2.max(1) as f64;
    let fy = (y - from.1) as f64 / from.3.max(1) as f64;
    (
        to.0 + (fx * to.2 as f64).round() as i32,
        to.1 + (fy * to.3 as f64).round() as i32,
    )
}

/// The previews' order in the spread: top to bottom, then left to right
/// (GNOME keeps a window at the bottom left at the bottom left).
pub fn spread_order(rects: &[(i32, i32, i32, i32)]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by_key(|&i| {
        let (x, y, w, h) = rects[i];
        (y + h / 2, x + w / 2)
    });
    order
}

/// An overview card on a monitor other than the primary.
struct Side {
    window: gtk::ApplicationWindow,
    stack: gtk::Overlay,
    main: gtk::Box,
    drag_layer: gtk::Fixed,
    connector: String,
    geo: Cell<Geo>,
}

/// What the overview shows, for tools/test/umoja-wm-test.
#[derive(Default)]
struct Shown {
    monitor: String,
    card: Vec<u64>,
    thumbs: Vec<Vec<u64>>,
    sides: Vec<(String, Vec<u64>)>,
}

#[derive(Default)]
struct State {
    windows: Vec<Win>,
    current: u32,
    count: u32,
}

/// Where an app dropped from the dock opens.
#[derive(Clone, Copy, Debug)]
enum AppDrop {
    Workspace(u32),
    NewWorkspace(u32),
}

/// The dragged app icon's size over the overview.
const APP_GHOST: i32 = 64;

/// How long a dropped app has to open its window.
const LAUNCH_WAIT: Duration = Duration::from_secs(15);

struct Drag {
    id: u64,
    workspace: i32,
    /// The window's rectangle on screen and its monitor.
    rect: Geo,
    mon: Geo,
    /// Where it was picked up, as a fraction of its preview.
    grab: (f64, f64),
    /// The overview surface the drag started on, and its drag layer.
    root: gtk::Widget,
    layer: gtk::Fixed,
    cell: gtk::Widget,
    ghost: gtk::Widget,
    size: (f64, f64),
    target: Option<(gtk::Widget, DropKind)>,
    /// The pointer, in `root`'s coordinates.
    at: (f64, f64),
    /// What the ghost shows, for its copy on another monitor's view.
    thumb: Option<gdk::Texture>,
    icon: gio::Icon,
    /// That copy, while the pointer is over another monitor's view.
    remote: Option<(gtk::Fixed, gtk::Widget)>,
}

/// The copy of a dragged window that follows the pointer.
fn ghost_widget(thumb: Option<&gdk::Texture>, icon: &gio::Icon, (w, h): (f64, f64)) -> gtk::Widget {
    let ghost: gtk::Widget = match thumb {
        Some(t) => TexPane::new(Some(t), false, 1, 1).upcast(),
        None => {
            let img = gtk::Image::from_gicon(icon);
            img.set_pixel_size(64);
            img.add_css_class("spread-card");
            img.upcast()
        }
    };
    ghost.set_size_request(w as i32, h as i32);
    ghost.add_css_class("spread-drag-ghost");
    ghost.set_can_target(false);
    ghost
}

pub struct Spread {
    window: gtk::ApplicationWindow,
    entry: gtk::SearchEntry,
    thumbs: gtk::Box,
    main: gtk::Box,
    drag_layer: gtk::Fixed,
    overlay: Weak<AppGrid>,
    opening: Cell<bool>,
    wallpaper: RefCell<Option<gdk::Texture>>,
    /// The file `wallpaper` was decoded from, to notice a new one.
    wallpaper_path: RefCell<Option<std::path::PathBuf>>,
    previews: Rc<RefCell<HashMap<u64, gdk::Texture>>>,
    /// Drops `previews` a while after the overview closes (see `hide`).
    drop_previews: Rc<RefCell<Option<glib::SourceId>>>,
    /// Whether umoja-mir can render single windows (CaptureWindow, needs
    /// Umoja's Mir): None until the first try. With it every window gets a
    /// real preview, covered, minimized or on another workspace; without it
    /// previews are cut from screen captures.
    window_capture: Cell<Option<bool>>,
    state: RefCell<State>,
    refresh_pending: Cell<bool>,
    /// Pending background preview capture (see `watch_windows`).
    snap_timer: RefCell<Option<glib::SourceId>>,
    /// When the last background capture ran (monotonic µs).
    last_snap: Cell<i64>,
    /// The windows on screen at the last background capture (`layout_key`).
    last_snap_layout: RefCell<Vec<(u64, i32, i32, i32, i32)>>,
    last_scroll: Cell<i64>,
    scroll_acc: Cell<f64>,
    drag: RefCell<Option<Drag>>,
    /// The views on the other monitors.
    sides: RefCell<Vec<Rc<Side>>>,
    /// Drop targets of the current layout.
    targets: RefCell<Vec<(glib::WeakRef<gtk::Widget>, DropKind)>>,
    /// The main card's previews by row, for the arrow keys, and the one
    /// they selected.
    nav: RefCell<Vec<Vec<(u64, gtk::Button)>>>,
    selected: Cell<Option<(usize, usize)>>,
    stack: gtk::Overlay,
    shown: RefCell<Shown>,
    /// An app icon dragged out of the dock: the target it is over, and a
    /// counter so a pointer answer arriving after the drop is ignored.
    app_target: RefCell<Option<(gtk::Widget, DropKind)>>,
    app_drag_seq: Cell<u64>,
    /// The dragged app's icon under the pointer, and the layer it is on.
    app_ghost: RefCell<Option<(gtk::Fixed, gtk::Widget)>>,
}

/// Fade a view in (gnome-shell animates into the overview; a fade is the
/// cheap half of it). libadwaita skips it when animations are off.
fn fade_in(widget: &gtk::Widget) {
    widget.set_opacity(0.0);
    let w = widget.clone();
    let target = adw::CallbackAnimationTarget::new(move |v| w.set_opacity(v));
    let anim = adw::TimedAnimation::builder()
        .widget(widget)
        .value_from(0.0)
        .value_to(1.0)
        .duration(200)
        .easing(adw::Easing::EaseOutCubic)
        .target(&target)
        .build();
    anim.play();
}

/// The monitors, the primary first.
fn monitor_geos() -> Vec<(gdk::Monitor, Geo)> {
    let primary = umoja_common::monitors::primary();
    let mut all: Vec<(gdk::Monitor, Geo)> = umoja_common::monitors::all()
        .into_iter()
        .map(|m| {
            let g = m.geometry();
            let geo = (g.x(), g.y(), g.width(), g.height());
            (m, geo)
        })
        .collect();
    if let Some(p) = primary
        && let Some(i) = all.iter().position(|(m, _)| *m == p)
    {
        let first = all.remove(i);
        all.insert(0, first);
    }
    all
}

impl Spread {
    pub fn new(app: &adw::Application, overlay: &Rc<AppGrid>) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-spread"])
            .build();
        window.init_layer_shell();
        // The Top layer with a normal exclusive zone keeps the top bar and the
        // dock visible and clickable around the overview.
        window.set_layer(Layer::Top);
        window.set_namespace("umoja-overview");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(0);
        window.set_keyboard_mode(KeyboardMode::Exclusive);

        // The overview's own background; a click on it leaves the overview.
        let background = gtk::Box::new(gtk::Orientation::Vertical, 0);
        background.add_css_class("spread-background");
        background.set_hexpand(true);
        background.set_vexpand(true);

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(i18n::umoja("Type to search"))
            .width_request(400)
            .halign(gtk::Align::Center)
            .build();
        // No spacing: the gaps between thumbnails are drop targets.
        let thumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        thumbs.set_halign(gtk::Align::Center);
        thumbs.set_margin_top(16);
        thumbs.add_css_class("workspace-thumbnails");
        let main = gtk::Box::new(gtk::Orientation::Horizontal, (GAP / 2.0) as i32);
        main.set_halign(gtk::Align::Center);
        main.set_valign(gtk::Align::Center);
        main.set_vexpand(true);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_top(24);
        content.set_margin_bottom(GAP as i32);
        content.append(&entry);
        content.append(&thumbs);
        content.append(&main);
        // Let clicks on the gaps fall through to the background.
        content.set_can_target(true);

        let drag_layer = gtk::Fixed::new();
        drag_layer.set_can_target(false);

        let stack = gtk::Overlay::new();
        stack.set_child(Some(&background));
        stack.add_overlay(&content);
        stack.add_overlay(&drag_layer);
        window.set_child(Some(&stack));
        umoja_common::monitors::start();

        let this = Rc::new(Self {
            window,
            entry,
            thumbs,
            main,
            drag_layer,
            overlay: Rc::downgrade(overlay),
            opening: Cell::new(false),
            wallpaper: RefCell::default(),
            wallpaper_path: RefCell::default(),
            previews: Rc::default(),
            drop_previews: Rc::default(),
            window_capture: Cell::new(None),
            state: RefCell::default(),
            refresh_pending: Cell::new(false),
            snap_timer: RefCell::default(),
            last_snap: Cell::new(0),
            last_snap_layout: RefCell::default(),
            last_scroll: Cell::new(0),
            scroll_acc: Cell::new(0.0),
            drag: RefCell::default(),
            sides: RefCell::default(),
            targets: RefCell::default(),
            nav: RefCell::default(),
            selected: Cell::new(None),
            stack,
            shown: RefCell::default(),
            app_target: RefCell::default(),
            app_drag_seq: Cell::new(0),
            app_ghost: RefCell::default(),
        });
        this.connect_signals(&content);
        this.watch_workspaces();
        this.watch_windows();
        // Decode the wallpaper now, so the first Super press doesn't wait.
        this.load_wallpaper();
        this
    }

    /// Keep a downscaled copy of the current wallpaper, decoded off the
    /// main thread. Opening the overview used to decode the full-size image
    /// (about 180 ms for 3840x2160) every time before it could show.
    fn load_wallpaper(self: &Rc<Self>) {
        let path = gs::wallpaper_path(gs::prefers_dark());
        if *self.wallpaper_path.borrow() == path {
            return;
        }
        self.wallpaper_path.replace(path.clone());
        let Some(path) = path else {
            self.wallpaper.replace(None);
            return;
        };
        // Cards are smaller than the monitor: half its pixels is plenty.
        let (w, h) = umoja_common::wallpaper::largest_monitor_pixels().unwrap_or((3840, 2160));
        let need = ((w / 2).max(1), (h / 2).max(1));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let p = path.clone();
            let pixels =
                gio::spawn_blocking(move || umoja_common::wallpaper::decode_cover_pixels(&p, need))
                    .await
                    .ok()
                    .flatten();
            let Some(s) = weak.upgrade() else { return };
            if s.wallpaper_path.borrow().as_deref() != Some(path.as_path()) {
                return; // a newer wallpaper is on its way
            }
            s.wallpaper.replace(pixels.map(|p| p.texture()));
            if s.is_visible() {
                s.populate();
            }
        });
    }

    fn connect_signals(self: &Rc<Self>, content: &gtk::Box) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(s) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let plain = !mods.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            );
            match key {
                gdk::Key::Escape => {
                    s.hide();
                    glib::Propagation::Stop
                }
                // Page Up/Down switch workspace in GNOME's overview.
                gdk::Key::Page_Up if plain => {
                    s.step_workspace(-1);
                    glib::Propagation::Stop
                }
                gdk::Key::Page_Down if plain => {
                    s.step_workspace(1);
                    glib::Propagation::Stop
                }
                // Arrow keys pick a window, Enter goes to it (GNOME's
                // keyboard navigation between previews).
                gdk::Key::Left | gdk::Key::Right | gdk::Key::Up | gdk::Key::Down
                    if plain && s.entry.text().is_empty() =>
                {
                    let (dx, dy) = match key {
                        gdk::Key::Left => (-1, 0),
                        gdk::Key::Right => (1, 0),
                        gdk::Key::Up => (0, -1),
                        _ => (0, 1),
                    };
                    s.move_selection(dx, dy);
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter if s.selected.get().is_some() => {
                    s.activate_selected();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(keys);

        // Typing goes to app search, as in GNOME's overview.
        let weak = Rc::downgrade(self);
        self.entry.connect_search_changed(move |e| {
            let text = e.text().to_string();
            if text.is_empty() {
                return;
            }
            let Some(s) = weak.upgrade() else { return };
            e.set_text("");
            s.hide();
            if let Some(o) = s.overlay.upgrade() {
                o.show_with_text(&text);
            }
        });

        // A click on the empty background leaves the overview. The content
        // box covers everything, so the click is taken there unless a child
        // (window, thumbnail, entry) claimed it.
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |g, _, x, y| {
            let Some(s) = weak.upgrade() else { return };
            let Some(w) = g.widget() else { return };
            let hit = w.pick(x, y, gtk::PickFlags::DEFAULT);
            let on_background = hit.is_none_or(|h| {
                h == w
                    || h == s.main.clone().upcast::<gtk::Widget>()
                    || h == s.thumbs.clone().upcast::<gtk::Widget>()
                    || h.has_css_class("card-wallpaper")
                    || h.has_css_class("spread-rows")
            });
            if on_background {
                s.hide();
            }
        });
        content.add_controller(click);

        // On a touchscreen a one-finger sideways drag across the overview
        // (not on a window, which drags the window) switches workspace,
        // as in GNOME.
        let swipe = gtk::GestureDrag::new();
        swipe.set_touch_only(true);
        swipe.connect_drag_update(|g, dx, dy| {
            if dx.abs() >= TOUCH_SWIPE_CLAIM && dx.abs() > dy.abs() {
                g.set_state(gtk::EventSequenceState::Claimed);
            }
        });
        let weak = Rc::downgrade(self);
        swipe.connect_drag_end(move |_, dx, dy| {
            let Some(s) = weak.upgrade() else { return };
            if dx.abs() >= TOUCH_SWIPE_SWITCH && dx.abs() > dy.abs() {
                // Fingers moving left bring in the workspace on the right.
                s.step_workspace(if dx < 0.0 { 1 } else { -1 });
            }
        });
        content.add_controller(swipe);

        // Scrolling switches workspace, one step per notch or swipe.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, dx, dy| {
            let Some(s) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            // Wheel notches come as ±1, touchpads as many small deltas.
            let d = if dy.abs() >= dx.abs() { dy } else { dx };
            let acc = s.scroll_acc.get() + d;
            let now = glib::monotonic_time();
            if acc.abs() >= 1.0 {
                s.scroll_acc.set(0.0);
                if now - s.last_scroll.get() > 250_000 {
                    s.last_scroll.set(now);
                    s.step_workspace(if acc > 0.0 { 1 } else { -1 });
                }
            } else {
                s.scroll_acc.set(acc);
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(scroll);
    }

    /// Follow workspace switches made while the overview is up (keyboard
    /// shortcuts, dynamic workspaces appearing or going away).
    fn watch_workspaces(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                return;
            };
            let w = weak.clone();
            let id = conn.signal_subscribe(
                Some(dbus::COMPOSITOR_BUS_NAME),
                Some(dbus::COMPOSITOR_INTERFACE),
                Some("WorkspacesChanged"),
                Some(dbus::COMPOSITOR_OBJECT_PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, _| {
                    if let Some(s) = w.upgrade() {
                        if s.is_visible() {
                            s.queue_refresh();
                        }
                    }
                },
            );
            // Lives as long as the overview, which is the whole process.
            std::mem::forget(id);
            std::mem::forget(conn);
        });
    }

    /// Keep previews fresh while the overview is closed: shortly after
    /// windows open, move, get raised or focused, capture the screen and
    /// update the previews of the windows that are uncovered now. Without
    /// this a window that happened to be covered when the overview opened
    /// (Text Editor partly behind Files) had no preview at all.
    fn watch_windows(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
                return;
            };
            let w = weak.clone();
            let id = conn.signal_subscribe(
                Some(dbus::COMPOSITOR_BUS_NAME),
                Some(dbus::COMPOSITOR_INTERFACE),
                Some("WindowsChanged"),
                Some(dbus::COMPOSITOR_OBJECT_PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, _| {
                    // Windows opening, closing or changing while the
                    // overview is up show at once, as in GNOME.
                    match w.upgrade() {
                        Some(s) if s.is_visible() => s.queue_refresh(),
                        Some(s) => s.queue_snapshot(),
                        None => {}
                    }
                },
            );
            std::mem::forget(id);
            std::mem::forget(conn);
        });
    }

    /// (Re)start the countdown to a background capture: SNAP_SETTLE after
    /// the last change, and no sooner than SNAP_INTERVAL after the last one.
    fn queue_snapshot(self: &Rc<Self>) {
        if let Some(t) = self.snap_timer.take() {
            t.remove();
        }
        let since = glib::monotonic_time() - self.last_snap.get();
        let wait = SNAP_SETTLE
            .max(SNAP_INTERVAL.saturating_sub(Duration::from_micros(since.max(0) as u64)));
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(wait, move || {
            let Some(s) = weak.upgrade() else { return };
            s.snap_timer.take();
            glib::spawn_future_local(async move { s.snapshot().await });
        });
        self.snap_timer.replace(Some(id));
    }

    async fn snapshot(self: &Rc<Self>) {
        // Windows are captured one by one when the overview opens.
        if self.window_capture.get() == Some(true) {
            return;
        }
        if self.opening.get() || self.is_visible() || self.own_window_shown() {
            return;
        }
        if !screen_shows_desktop().await {
            return;
        }
        self.last_snap.set(glib::monotonic_time());
        let state = fetch_state().await;
        let here = state
            .windows
            .iter()
            .any(|w| w.flags & FLAG_CURRENT_WORKSPACE != 0 && w.flags & FLAG_MINIMIZED == 0);
        if !here {
            return;
        }
        // Focus moved between windows, or a window on another workspace
        // changed: what is on screen is what the last capture saw, and every
        // window it could see already has a preview. Capturing again costs
        // the compositor a full-screen render for nothing.
        let layout = layout_key(&state);
        if *self.last_snap_layout.borrow() == layout && {
            let previews = self.previews.borrow();
            uncovered(&state).all(|w| previews.contains_key(&w.id))
        } {
            return;
        }
        let frame = capture(SNAP_SCALE).await;
        // Something of ours may have come up meanwhile (Super pressed).
        if self.opening.get() || self.is_visible() || self.own_window_shown() {
            return;
        }
        if frame.is_some() {
            *self.last_snap_layout.borrow_mut() = layout;
        }
        self.remember_previews(&state, frame.as_ref());
    }

    /// Is any of this process's windows (assistant, screenshot UI, tile
    /// preview, shortcuts...) on screen? It would be in the capture.
    fn own_window_shown(&self) -> bool {
        self.window
            .application()
            .map(|app| app.windows().iter().any(|w| w.is_visible()))
            .unwrap_or(false)
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        self.cancel_drag();
        self.set_app_target(None);
        self.move_app_ghost("", None);
        let was_visible = self.window.is_visible();
        self.window.set_visible(false);
        for s in self.sides.borrow().iter() {
            s.window.set_visible(false);
        }
        self.set_selected(None);
        // Window previews are up to 4 MB each and there is one for every
        // window on every workspace. Keep them for a while, so opening the
        // overview again is instant, then let them go: umoja-mir renders
        // them afresh in a moment. (Without CaptureWindow they are cut from
        // screen captures taken in the background and have to be kept.)
        if was_visible && self.window_capture.get() == Some(true) {
            if let Some(t) = self.drop_previews.take() {
                t.remove();
            }
            let previews = self.previews.clone();
            let slot = self.drop_previews.clone();
            let id = glib::timeout_add_local_once(KEEP_PREVIEWS, move || {
                slot.take();
                previews.borrow_mut().clear();
            });
            self.drop_previews.replace(Some(id));
        }
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    pub fn show(self: &Rc<Self>) {
        if self.opening.replace(true) {
            return;
        }
        if let Some(o) = self.overlay.upgrade() {
            o.hide();
        }
        if let Some(t) = self.drop_previews.take() {
            t.remove();
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(s) = weak.upgrade() else { return };
            if s.window_capture.get() != Some(false) {
                // The windows on this workspace before showing, the rest
                // (other workspaces, minimized) while it is up.
                let state = fetch_state().await;
                let here: Vec<u64> = state
                    .windows
                    .iter()
                    .filter(|w| {
                        w.flags & FLAG_CURRENT_WORKSPACE != 0 && w.flags & FLAG_MINIMIZED == 0
                    })
                    .map(|w| w.id)
                    .collect();
                // Opened again while the last previews are still here: show
                // at once with those, and bring them up to date behind.
                // Rendering every window first cost a GPU pass and a 4 MB
                // D-Bus reply each before the overview could appear.
                let cached = s.window_capture.get() == Some(true) && !here.is_empty() && {
                    let previews = s.previews.borrow();
                    here.iter().all(|id| previews.contains_key(id))
                };
                if cached {
                    s.previews
                        .borrow_mut()
                        .retain(|id, _| state.windows.iter().any(|w| w.id == *id));
                    let rest: Vec<u64> = state
                        .windows
                        .iter()
                        .map(|w| w.id)
                        .filter(|id| !here.contains(id))
                        .collect();
                    s.opening.set(false);
                    s.load_wallpaper();
                    *s.state.borrow_mut() = state;
                    s.populate();
                    s.present_views();
                    if s.capture_windows(&here).await > 0 {
                        s.queue_refresh();
                    }
                    if !rest.is_empty() && s.capture_windows(&rest).await > 0 {
                        s.queue_refresh();
                    }
                    return;
                }
                let got = s.capture_windows(&here).await;
                if got > 0 || (here.is_empty() && s.window_capture.get() == Some(true)) {
                    s.window_capture.set(Some(true));
                    s.previews
                        .borrow_mut()
                        .retain(|id, _| state.windows.iter().any(|w| w.id == *id));
                    let rest: Vec<u64> = state
                        .windows
                        .iter()
                        .map(|w| w.id)
                        .filter(|id| !here.contains(id))
                        .collect();
                    s.opening.set(false);
                    s.load_wallpaper();
                    *s.state.borrow_mut() = state;
                    s.populate();
                    s.present_views();
                    if !rest.is_empty() && s.capture_windows(&rest).await > 0 {
                        s.queue_refresh();
                    }
                    return;
                }
                if !here.is_empty() {
                    s.window_capture.set(Some(false));
                }
            }
            // Capture before mapping so the overview isn't in the picture.
            let (state, frame) = futures_util::join!(fetch_state(), capture(OPEN_SCALE));
            s.opening.set(false);
            s.load_wallpaper();
            s.remember_previews(&state, frame.as_ref());
            *s.state.borrow_mut() = state;
            s.populate();
            s.present_views();
        });
    }

    /// Render these windows one by one through umoja-mir into `previews`;
    /// how many it managed.
    async fn capture_windows(&self, ids: &[u64]) -> usize {
        let shots = futures_util::future::join_all(
            ids.iter()
                .map(|&id| umoja_common::thumbnails::capture(id, umoja_common::thumbnails::LARGE)),
        )
        .await;
        let mut previews = self.previews.borrow_mut();
        let mut got = 0;
        for (id, shot) in ids.iter().zip(shots) {
            if let Some(t) = shot {
                previews.insert(*id, t);
                got += 1;
            }
        }
        got
    }

    /// Re-read windows and workspaces and redraw, coalescing bursts.
    fn queue_refresh(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(40), move || {
            let Some(s) = weak.upgrade() else { return };
            glib::spawn_future_local(async move {
                let state = fetch_state().await;
                s.refresh_pending.set(false);
                if !s.is_visible() || s.drag.borrow().is_some() {
                    return;
                }
                *s.state.borrow_mut() = state;
                s.populate();
                // A window that opened while the overview is up has no
                // preview yet.
                if s.window_capture.get() == Some(true) {
                    let missing: Vec<u64> = {
                        let previews = s.previews.borrow();
                        s.state
                            .borrow()
                            .windows
                            .iter()
                            .map(|w| w.id)
                            .filter(|id| !previews.contains_key(id))
                            .collect()
                    };
                    if !missing.is_empty() && s.capture_windows(&missing).await > 0 && s.is_visible()
                    {
                        s.populate();
                    }
                }
            });
        });
    }

    fn step_workspace(self: &Rc<Self>, delta: i32) {
        let (current, count) = {
            let st = self.state.borrow();
            (st.current as i32, st.count as i32)
        };
        let target = (current + delta).clamp(0, (count - 1).max(0));
        if target != current {
            self.show_workspace(target as u32);
        }
    }

    fn show_workspace(self: &Rc<Self>, index: u32) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            compositor_call("ShowWorkspace", (index,)).await;
            if let Some(s) = weak.upgrade() {
                s.queue_refresh();
            }
        });
    }

    fn send_to_workspace(self: &Rc<Self>, id: u64, index: u32) {
        self.call_then_refresh("SendWindowToWorkspace", (id, index));
    }

    fn place_window(self: &Rc<Self>, id: u64, workspace: i32, x: i32, y: i32) {
        self.call_then_refresh("PlaceWindow", (id, workspace, x, y));
    }

    fn call_then_refresh<A: ToVariant + 'static>(self: &Rc<Self>, method: &'static str, args: A) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            compositor_call(method, args).await;
            if let Some(s) = weak.upgrade() {
                s.queue_refresh();
            }
        });
    }

    /// Keep a preview for every window that is on screen and uncovered now;
    /// forget windows that are gone.
    fn remember_previews(&self, state: &State, frame: Option<&Pixbuf>) {
        let mut previews = self.previews.borrow_mut();
        previews.retain(|id, _| state.windows.iter().any(|w| w.id == *id));
        let Some(frame) = frame else { return };
        let origin = layout_origin();
        for w in uncovered(state) {
            if let Some(t) = crop(frame, w, origin) {
                previews.insert(w.id, t);
            }
        }
    }

    /// The primary monitor, where the main view always is (as in
    /// gnome-shell). Mir used to choose the surface's monitor, and after
    /// some interactions chose the other one, with thumbnails computed for
    /// the wrong origin (empty).
    fn monitor(&self) -> Geo {
        monitor_geos()
            .first()
            .map(|(_, g)| *g)
            .unwrap_or((0, 0, 1280, 800))
    }

    /// Room for the overview: its last size, or the monitor less the top bar
    /// and dock before it was ever shown.
    fn area(&self) -> (f64, f64) {
        let (w, h) = (self.window.width(), self.window.height());
        if w > 0 && h > 0 {
            return (w as f64, h as f64);
        }
        let (_, _, mw, mh) = self.monitor();
        ((mw - 64).max(320) as f64, (mh - 32).max(240) as f64)
    }

    fn present_views(self: &Rc<Self>) {
        if let Some(m) = umoja_common::monitors::primary() {
            self.window.set_monitor(&m);
        }
        let was = self.window.is_visible();
        self.window.present();
        self.entry.grab_focus();
        if !was {
            fade_in(self.stack.upcast_ref());
        }
        for side in self.sides.borrow().iter() {
            let was = side.window.is_visible();
            side.window.present();
            if !was {
                fade_in(side.stack.upcast_ref());
            }
        }
    }

    /// One view per monitor besides the primary, made or dropped as
    /// monitors come and go.
    fn ensure_sides(self: &Rc<Self>, geos: &[(gdk::Monitor, Geo)]) {
        let others = geos.get(1..).unwrap_or_default();
        let name = |m: &gdk::Monitor| m.connector().map(|c| c.to_string()).unwrap_or_default();
        let mut sides = self.sides.borrow_mut();
        sides.retain(|s| {
            let keep = others.iter().any(|(m, _)| name(m) == s.connector);
            if !keep {
                s.window.destroy();
            }
            keep
        });
        let Some(app) = self.window.application() else {
            return;
        };
        for (m, geo) in others {
            let connector = name(m);
            if let Some(s) = sides.iter().find(|s| s.connector == connector) {
                s.geo.set(*geo);
                if !s.window.is_visible() {
                    s.window.set_monitor(m);
                }
                continue;
            }
            sides.push(self.new_side(&app, m, connector, *geo));
        }
    }

    fn new_side(self: &Rc<Self>, app: &gtk::Application, m: &gdk::Monitor, connector: String, geo: Geo) -> Rc<Side> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-spread"])
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Top);
        window.set_namespace("umoja-overview-monitor");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        // The whole monitor, so the surface's origin is the monitor's
        // (drops across monitors need it). The keyboard stays with the
        // main view.
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_monitor(m);
        let background = gtk::Box::new(gtk::Orientation::Vertical, 0);
        background.add_css_class("spread-background");
        background.set_hexpand(true);
        background.set_vexpand(true);
        let main = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        main.set_halign(gtk::Align::Center);
        main.set_valign(gtk::Align::Center);
        main.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_top(GAP as i32);
        content.set_margin_bottom(GAP as i32);
        content.append(&main);
        let drag_layer = gtk::Fixed::new();
        drag_layer.set_can_target(false);
        let stack = gtk::Overlay::new();
        stack.set_child(Some(&background));
        stack.add_overlay(&content);
        stack.add_overlay(&drag_layer);
        window.set_child(Some(&stack));
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        let main_w = main.clone();
        click.connect_released(move |g, _, x, y| {
            let Some(s) = weak.upgrade() else { return };
            let Some(w) = g.widget() else { return };
            let hit = w.pick(x, y, gtk::PickFlags::DEFAULT);
            let on_background = hit.is_none_or(|h| {
                h == w
                    || h == main_w.clone().upcast::<gtk::Widget>()
                    || h.has_css_class("card-wallpaper")
                    || h.has_css_class("spread-rows")
            });
            if on_background {
                s.hide();
            }
        });
        content.add_controller(click);
        Rc::new(Side {
            window,
            stack,
            main,
            drag_layer,
            connector,
            geo: Cell::new(geo),
        })
    }

    fn register(&self, w: &impl IsA<gtk::Widget>, kind: DropKind) {
        self.targets
            .borrow_mut()
            .push((w.upcast_ref::<gtk::Widget>().downgrade(), kind));
    }

    fn kind_of(&self, w: &gtk::Widget) -> Option<DropKind> {
        self.targets
            .borrow()
            .iter()
            .find(|(t, _)| t.upgrade().as_ref() == Some(w))
            .map(|(_, k)| *k)
    }

    fn populate(self: &Rc<Self>) {
        for b in [&self.thumbs, &self.main] {
            while let Some(c) = b.first_child() {
                b.remove(&c);
            }
        }
        self.targets.borrow_mut().clear();
        let geos = monitor_geos();
        self.ensure_sides(&geos);
        let mons: Vec<Geo> = geos.iter().map(|(_, g)| *g).collect();
        let mut shown = Shown {
            monitor: geos
                .first()
                .and_then(|(m, _)| m.connector())
                .map(|c| c.to_string())
                .unwrap_or_default(),
            ..Default::default()
        };
        let st = self.state.borrow();
        let primary = mons.first().copied().unwrap_or((0, 0, 1280, 800));
        let (mx, my, mw, mh) = primary;
        let aspect = mw.max(1) as f64 / mh.max(1) as f64;
        let (aw, ah) = self.area();
        let count = st.count.max(1);
        let current = st.current.min(count - 1);
        let per_workspace: Vec<usize> = (0..count)
            .map(|i| {
                st.windows
                    .iter()
                    .filter(|w| shown_on(w, current) == i)
                    .count()
            })
            .collect();
        tracing::info!(
            "overview populated current={current} count={count} per_workspace={per_workspace:?} monitors={}",
            mons.len()
        );

        // Thumbnails, when there is more than one workspace (as in GNOME),
        // with a drop gap before, between and after them.
        let mut used_h = 24.0 + 40.0 + GAP;
        if count > 1 {
            let th = (mh as f64 * THUMB_SHARE).clamp(THUMB_MIN, THUMB_MAX);
            let tw = th * aspect;
            for i in 0..count {
                self.thumbs.append(&self.gap(i, th));
                let (t, ids) = self.thumbnail(&st, i, i == current, (tw, th), (mx, my), (mw, mh));
                self.thumbs.append(&t);
                shown.thumbs.push(ids);
            }
            self.thumbs.append(&self.gap(count, th));
            self.thumbs.set_visible(true);
            used_h += 16.0 + th;
        } else {
            self.thumbs.set_visible(false);
        }

        let peeks = count > 1;
        let (cw, ch) = card_size((aw - 2.0 * GAP, ah - used_h - GAP), aspect, peeks);
        if peeks {
            let left = (current > 0).then(|| current - 1);
            self.main.append(&self.peek(left, ch, true));
        }
        let (card, nav, ids) = self.workspace_card(&st, current, (cw, ch), primary, &mons);
        self.main.append(&card);
        shown.card = ids;
        if peeks {
            let right = (current + 1 < count).then_some(current + 1);
            self.main.append(&self.peek(right, ch, false));
        }
        let selected = self.selected.take();
        *self.nav.borrow_mut() = nav;
        // Keep the keyboard selection across a refresh when it still exists.
        if let Some((r, c)) = selected
            && self.nav.borrow().get(r).is_some_and(|row| c < row.len())
        {
            self.set_selected(Some((r, c)));
        }

        // The other monitors: their windows on the current workspace.
        for side in self.sides.borrow().iter() {
            while let Some(c) = side.main.first_child() {
                side.main.remove(&c);
            }
            let geo = side.geo.get();
            let aspect = geo.2.max(1) as f64 / geo.3.max(1) as f64;
            let avail = (geo.2 as f64 - 2.0 * GAP, geo.3 as f64 - 3.0 * GAP);
            let size = card_size(avail, aspect, false);
            let (card, _, ids) = self.workspace_card(&st, current, size, geo, &mons);
            side.main.append(&card);
            shown.sides.push((side.connector.clone(), ids));
        }
        *self.shown.borrow_mut() = shown;
    }

    /// Between two thumbnails: dropping a window here makes a workspace.
    fn gap(&self, index: u32, height: f64) -> gtk::Widget {
        let g = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        g.add_css_class("workspace-gap");
        g.set_size_request(6, height.round() as i32);
        self.register(&g, DropKind::Gap(index));
        g.upcast()
    }

    fn wallpaper(&self, w: f64, h: f64) -> TexPane {
        let pane = TexPane::new(
            self.wallpaper.borrow().as_ref(),
            true,
            w.round() as i32,
            h.round() as i32,
        );
        pane.add_css_class("workspace-wallpaper");
        pane
    }

    /// A neighbouring workspace's edge, peeking in beside the current one.
    fn peek(self: &Rc<Self>, index: Option<u32>, height: f64, left: bool) -> gtk::Widget {
        let frame = gtk::Overlay::new();
        frame.set_size_request(PEEK as i32, (height * 0.85) as i32);
        frame.set_valign(gtk::Align::Center);
        frame.set_overflow(gtk::Overflow::Hidden);
        let Some(index) = index else {
            // Keep the card centred when there is no neighbour on this side.
            frame.set_opacity(0.0);
            frame.set_can_target(false);
            return frame.upcast();
        };
        frame.set_child(Some(&self.wallpaper(PEEK, height * 0.85)));
        frame.add_css_class("workspace-peek");
        frame.add_css_class(if left { "left" } else { "right" });
        frame.set_widget_name(&format!("ws-{index}"));
        frame.set_cursor_from_name(Some("pointer"));
        self.register(&frame, DropKind::Peek(index));
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(s) = weak.upgrade() {
                s.show_workspace(index);
            }
        });
        frame.add_controller(click);
        frame.upcast()
    }

    /// GNOME's WorkspaceThumbnail: the wallpaper with the workspace's
    /// windows at their places, the active one outlined. Each small window
    /// can be dragged (GNOME's WindowClone).
    fn thumbnail(
        self: &Rc<Self>,
        st: &State,
        index: u32,
        active: bool,
        (tw, th): (f64, f64),
        origin: (i32, i32),
        mon: (i32, i32),
    ) -> (gtk::Widget, Vec<u64>) {
        let scale = tw / mon.0.max(1) as f64;
        let fixed = gtk::Fixed::new();
        let previews = self.previews.borrow();
        let mut ids = Vec::new();
        for w in st.windows.iter() {
            if w.workspace < 0 || w.workspace as u32 != index || w.flags & FLAG_MINIMIZED != 0 {
                continue;
            }
            let Some((x, y, rw, rh)) = thumb_rect(w, origin, mon, scale) else {
                continue;
            };
            let icon = apps::app_for_window(&w.app_id)
                .and_then(|i| i.icon())
                .unwrap_or_else(|| gio::ThemedIcon::new("application-x-executable").upcast());
            let thumb = previews.get(&w.id).cloned();
            let child: gtk::Widget = match &thumb {
                Some(t) => TexPane::new(Some(t), false, 1, 1).upcast(),
                None => {
                    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    card.add_css_class("thumb-window-card");
                    let img = gtk::Image::from_gicon(&icon);
                    img.set_pixel_size(((rw.min(rh)) * 0.5).clamp(8.0, 32.0) as i32);
                    img.set_vexpand(true);
                    img.set_can_target(false);
                    card.append(&img);
                    card.upcast()
                }
            };
            child.set_size_request(rw.round() as i32, rh.round() as i32);
            child.add_css_class("thumb-window");
            child.set_overflow(gtk::Overflow::Hidden);
            child.set_tooltip_text(Some(&w.title));
            child.set_widget_name(&format!("win-{}", w.id));
            self.make_draggable(&child, w, thumb, &icon, (rw.round() as i32, rh.round() as i32));
            fixed.put(&child, x, y);
            ids.push(w.id);
        }

        let frame = gtk::Overlay::new();
        frame.set_child(Some(&self.wallpaper(tw, th)));
        frame.add_overlay(&fixed);
        frame.set_size_request(tw.round() as i32, th.round() as i32);
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.add_css_class("workspace-thumbnail");
        if active {
            frame.add_css_class("active");
        }
        frame.set_widget_name(&format!("ws-{index}"));
        frame.set_tooltip_text(Some(
            &i18n::shell("Workspace %d").replace("%d", &(index + 1).to_string()),
        ));
        self.register(
            &frame,
            DropKind::Thumb {
                index,
                mon: (origin.0, origin.1, mon.0, mon.1),
            },
        );
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
            let Some(s) = weak.upgrade() else { return };
            // "A click on the already current workspace should go back to
            // the main view" (workspaceThumbnail.js).
            if s.state.borrow().current == index {
                s.hide();
            } else {
                s.show_workspace(index);
            }
        });
        frame.add_controller(click);
        (frame.upcast(), ids)
    }

    /// A workspace on one monitor: the wallpaper with the windows that are
    /// on that monitor spread out. The previews by row, and their ids.
    #[allow(clippy::type_complexity)]
    fn workspace_card(
        self: &Rc<Self>,
        st: &State,
        index: u32,
        (cw, ch): (f64, f64),
        mon: Geo,
        mons: &[Geo],
    ) -> (gtk::Widget, Vec<Vec<(u64, gtk::Button)>>, Vec<u64>) {
        let rows = gtk::Box::new(gtk::Orientation::Vertical, GAP as i32);
        rows.set_valign(gtk::Align::Center);
        rows.set_halign(gtk::Align::Center);
        rows.add_css_class("spread-rows");

        let here: Vec<&Win> = st
            .windows
            .iter()
            .filter(|w| {
                shown_on(w, st.current) == index && monitor_of((w.x, w.y, w.w, w.h), mons) == mon
            })
            .collect();
        let rects: Vec<Geo> = here.iter().map(|w| (w.x, w.y, w.w, w.h)).collect();
        let order = spread_order(&rects);
        let mut nav = Vec::new();
        let mut ids = Vec::new();
        if !here.is_empty() {
            let sizes: Vec<(f64, f64)> = order
                .iter()
                .map(|&i| (here[i].w.max(1) as f64, here[i].h.max(1) as f64))
                .collect();
            // Leave room under each window for its title.
            let avail = (
                (cw - 2.0 * CARD_PAD).max(100.0),
                (ch - 2.0 * CARD_PAD - 32.0).max(60.0),
            );
            let (scale, groups) = layout(&sizes, avail, GAP);
            let previews = self.previews.borrow();
            for mut g in groups {
                // Rows go top to bottom; inside a row, left to right.
                g.sort_by_key(|&k| {
                    let w = here[order[k]];
                    w.x + w.w / 2
                });
                let row = gtk::Box::new(gtk::Orientation::Horizontal, GAP as i32);
                row.set_halign(gtk::Align::Center);
                row.add_css_class("spread-rows");
                let mut nav_row = Vec::new();
                for k in g {
                    let w = here[order[k]];
                    // Minimized windows too: umoja-mir renders them, and
                    // GNOME shows their last contents.
                    let thumb = previews.get(&w.id).cloned();
                    let (cell, button) = self.preview(w, scale, thumb);
                    row.append(&cell);
                    nav_row.push((w.id, button));
                    ids.push(w.id);
                }
                rows.append(&row);
                nav.push(nav_row);
            }
        }

        let card = gtk::Overlay::new();
        let wallpaper = self.wallpaper(cw, ch);
        // Clicks on the bare wallpaper leave the overview, as in GNOME.
        wallpaper.add_css_class("card-wallpaper");
        card.set_child(Some(&wallpaper));
        card.add_overlay(&rows);
        card.set_size_request(cw.round() as i32, ch.round() as i32);
        card.set_overflow(gtk::Overflow::Hidden);
        card.add_css_class("workspace-card");
        card.set_widget_name(&format!("ws-{index}"));
        self.register(&card, DropKind::Card { index, mon });
        (card.upcast(), nav, ids)
    }

    fn preview(
        self: &Rc<Self>,
        w: &Win,
        scale: f64,
        thumb: Option<gdk::Texture>,
    ) -> (gtk::Widget, gtk::Button) {
        let width = (w.w as f64 * scale).round().max(48.0) as i32;
        let height = (w.h as f64 * scale).round().max(48.0) as i32;
        let info = apps::app_for_window(&w.app_id);
        let icon = info
            .as_ref()
            .and_then(|i| i.icon())
            .unwrap_or_else(|| gio::ThemedIcon::new("application-x-executable").upcast());

        let body: gtk::Widget = match thumb.as_ref() {
            Some(t) => TexPane::new(Some(t), false, width, height).upcast(),
            None => {
                let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
                card.add_css_class("spread-card");
                let big = gtk::Image::from_gicon(&icon);
                big.set_pixel_size((height / 3).clamp(32, 128));
                big.set_vexpand(true);
                big.set_valign(gtk::Align::Center);
                card.append(&big);
                card.upcast()
            }
        };
        body.set_size_request(width, height);
        body.set_overflow(gtk::Overflow::Hidden);
        body.add_css_class("spread-thumb");
        if w.flags & FLAG_MINIMIZED != 0 {
            body.add_css_class("minimized");
        }

        let badge = gtk::Image::from_gicon(&icon);
        badge.set_pixel_size(48);
        badge.set_halign(gtk::Align::Center);
        badge.set_valign(gtk::Align::End);
        badge.set_margin_bottom(8);
        badge.add_css_class("spread-icon");

        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["circular", "spread-close"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_end(6)
            .opacity(0.0)
            .focusable(false)
            .build();
        let id = w.id;
        close.connect_clicked(move |b| {
            call_compositor("CloseWindow", (id,));
            if let Some(cell) = b.ancestor(gtk::Box::static_type()) {
                cell.set_visible(false);
            }
        });

        let stack = gtk::Overlay::new();
        stack.set_child(Some(&body));
        stack.add_overlay(&badge);
        stack.add_overlay(&close);
        stack.set_overflow(gtk::Overflow::Visible);

        let button = gtk::Button::builder()
            .child(&stack)
            .css_classes(["spread-window"])
            .tooltip_text(&w.title)
            .build();
        button.set_widget_name(&format!("win-{id}"));
        if w.flags & FLAG_FOCUSED != 0 {
            button.add_css_class("focused");
        }
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            call_compositor("ActivateWindow", (id,));
            if let Some(s) = weak.upgrade() {
                s.hide();
            }
        });

        let title = gtk::Label::builder()
            .label(&w.title)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(((width / 8).max(8)) as i32)
            .css_classes(["spread-title"])
            .opacity(0.0)
            .build();

        let cell = gtk::Box::new(gtk::Orientation::Vertical, 18);
        cell.append(&button);
        cell.append(&title);

        // Title and close button appear on hover.
        let hover = gtk::EventControllerMotion::new();
        let (t1, c1) = (title.clone(), close.clone());
        hover.connect_enter(move |_, _, _| {
            t1.set_opacity(1.0);
            c1.set_opacity(1.0);
        });
        let (t2, c2) = (title, close);
        hover.connect_leave(move |_| {
            t2.set_opacity(0.0);
            c2.set_opacity(0.0);
        });
        cell.add_controller(hover);

        // Drag the window onto another workspace, monitor or spot.
        self.make_draggable(cell.upcast_ref(), w, thumb, &icon, (width, height));
        (cell.upcast(), button)
    }

    fn set_selected(&self, sel: Option<(usize, usize)>) {
        let nav = self.nav.borrow();
        let at = |s: Option<(usize, usize)>| {
            s.and_then(|(r, c)| nav.get(r).and_then(|row| row.get(c)))
                .map(|(_, b)| b.clone())
        };
        if let Some(b) = at(self.selected.get()) {
            b.remove_css_class("selected");
        }
        if let Some(b) = at(sel) {
            b.add_css_class("selected");
        }
        self.selected.set(sel);
    }

    fn move_selection(&self, dx: i32, dy: i32) {
        let next = {
            let nav = self.nav.borrow();
            if nav.is_empty() {
                return;
            }
            match self.selected.get() {
                None => (0, 0),
                Some((r, c)) => {
                    let r2 = (r as i32 + dy).clamp(0, nav.len() as i32 - 1) as usize;
                    let len = nav[r2].len().max(1);
                    let c2 = if dy != 0 {
                        c.min(len - 1)
                    } else {
                        (c as i32 + dx).clamp(0, len as i32 - 1) as usize
                    };
                    (r2, c2)
                }
            }
        };
        self.set_selected(Some(next));
    }

    fn activate_selected(&self) {
        let button = self.selected.get().and_then(|(r, c)| {
            self.nav
                .borrow()
                .get(r)
                .and_then(|row| row.get(c))
                .map(|(_, b)| b.clone())
        });
        if let Some(b) = button {
            b.emit_clicked();
        }
    }

    fn layer_for(&self, root: &gtk::Widget) -> Option<gtk::Fixed> {
        if root == self.window.upcast_ref::<gtk::Widget>() {
            return Some(self.drag_layer.clone());
        }
        self.sides
            .borrow()
            .iter()
            .find(|s| s.window.upcast_ref::<gtk::Widget>() == root)
            .map(|s| s.drag_layer.clone())
    }

    /// Where an overview surface's (0, 0) is in the layout. Layer surfaces
    /// aren't told; the other monitors' views cover their monitor, and the
    /// main view keeps out of the top bar's exclusive zone, so what it
    /// lacks of the monitor is above it.
    fn surface_origin(&self, root: &gtk::Widget) -> Option<(i32, i32)> {
        if root == self.window.upcast_ref::<gtk::Widget>() {
            let (x, y, w, h) = self.monitor();
            let (ww, wh) = (self.window.width(), self.window.height());
            if ww <= 0 || wh <= 0 {
                return Some((x, y));
            }
            return Some((x + (w - ww).max(0), y + (h - wh).max(0)));
        }
        self.sides
            .borrow()
            .iter()
            .find(|s| s.window.upcast_ref::<gtk::Widget>() == root)
            .map(|s| {
                let g = s.geo.get();
                (g.0, g.1)
            })
    }

    fn make_draggable(
        self: &Rc<Self>,
        widget: &gtk::Widget,
        w: &Win,
        thumb: Option<gdk::Texture>,
        icon: &gio::Icon,
        (width, height): (i32, i32),
    ) {
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (id, workspace) = (w.id, w.workspace);
        let rect = (w.x, w.y, w.w, w.h);
        let weak = Rc::downgrade(self);
        let icon = icon.clone();
        let cell_w = widget.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(s) = weak.upgrade() else { return };
            let Some((sx, sy)) = g.start_point() else {
                return;
            };
            let Some(root) = cell_w.root().map(|r| r.upcast::<gtk::Widget>()) else {
                return;
            };
            if s.drag.borrow().is_none() {
                if dx.hypot(dy) < DRAG_THRESHOLD {
                    return;
                }
                let Some(layer) = s.layer_for(&root) else {
                    return;
                };
                // Now a drag: the button underneath must not see a click.
                g.set_state(gtk::EventSequenceState::Claimed);
                // Small windows from a thumbnail grow to a usable size.
                let gw = (width as f64).clamp(120.0, 240.0);
                let gh = gw * height.max(1) as f64 / width.max(1) as f64;
                let ghost = ghost_widget(thumb.as_ref(), &icon, (gw, gh));
                layer.put(&ghost, 0.0, 0.0);
                cell_w.set_opacity(0.3);
                let mons: Vec<Geo> = monitor_geos().iter().map(|(_, g)| *g).collect();
                let grab = (
                    (sx / width.max(1) as f64).clamp(0.0, 1.0),
                    (sy / height.max(1) as f64).clamp(0.0, 1.0),
                );
                *s.drag.borrow_mut() = Some(Drag {
                    id,
                    workspace,
                    rect,
                    mon: monitor_of(rect, &mons),
                    grab,
                    root: root.clone(),
                    layer,
                    cell: cell_w.clone(),
                    ghost,
                    size: (gw, gh),
                    target: None,
                    at: (0.0, 0.0),
                    thumb: thumb.clone(),
                    icon: icon.clone(),
                    remote: None,
                });
            }
            let Some(p) = cell_w.compute_point(
                &root,
                &gtk::graphene::Point::new((sx + dx) as f32, (sy + dy) as f32),
            ) else {
                return;
            };
            s.drag_motion(&root, p.x() as f64, p.y() as f64);
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(s) = weak.upgrade() {
                s.drop_window();
            }
        });
        widget.add_controller(drag);
    }

    /// The drop target under (x, y) of overview surface `root`; past its
    /// edges, the monitor there.
    fn target_at(&self, root: &gtk::Widget, x: f64, y: f64) -> Option<(gtk::Widget, DropKind)> {
        let (rw, rh) = (root.width() as f64, root.height() as f64);
        if x >= 0.0 && y >= 0.0 && x < rw && y < rh {
            let mut hit = root.pick(x, y, gtk::PickFlags::DEFAULT);
            while let Some(w) = hit {
                if let Some(kind) = self.kind_of(&w) {
                    return Some((w, kind));
                }
                if &w == root {
                    break;
                }
                hit = w.parent();
            }
            return None;
        }
        // The pointer left this surface while held: another monitor's view.
        let origin = self.surface_origin(root)?;
        let (gx, gy) = (origin.0 as f64 + x, origin.1 as f64 + y);
        let mon = monitor_geos().into_iter().map(|(_, g)| g).find(|&(mx, my, mw, mh)| {
            gx >= mx as f64 && gy >= my as f64 && gx < (mx + mw) as f64 && gy < (my + mh) as f64
        })?;
        let card = self.targets.borrow().iter().find_map(|(w, k)| match k {
            DropKind::Card { mon: m, .. } if *m == mon => w.upgrade(),
            _ => None,
        })?;
        Some((card, DropKind::Screen { mon, x: gx, y: gy }))
    }

    fn drag_motion(&self, root: &gtk::Widget, x: f64, y: f64) {
        let target = self.target_at(root, x, y);
        // Past the edge of this view: the other monitor's view under the
        // pointer, to show the ghost there too.
        let inside = x >= 0.0 && y >= 0.0 && x < root.width() as f64 && y < root.height() as f64;
        let elsewhere = if inside {
            None
        } else {
            self.surface_origin(root).and_then(|(ox, oy)| {
                self.app_target_at(ox as f64 + x, oy as f64 + y)
                    .filter(|(r, _, _)| r != root)
                    .and_then(|(r, p, _)| Some((self.layer_for(&r)?, r, p)))
            })
        };
        let mut drag = self.drag.borrow_mut();
        let Some(d) = drag.as_mut() else { return };
        d.at = (x, y);
        let to = (x - d.grab.0 * d.size.0, y - d.grab.1 * d.size.1);
        if let Some(p) = root.compute_point(
            &d.layer,
            &gtk::graphene::Point::new(to.0 as f32, to.1 as f32),
        ) {
            d.layer.move_(&d.ghost, p.x() as f64, p.y() as f64);
        }
        if d.remote.as_ref().map(|(l, _)| l) != elsewhere.as_ref().map(|(l, _, _)| l)
            && let Some((l, g)) = d.remote.take()
        {
            l.remove(&g);
        }
        if let Some((layer, other, (ox, oy))) = elsewhere {
            if d.remote.is_none() {
                let g = ghost_widget(d.thumb.as_ref(), &d.icon, d.size);
                layer.put(&g, 0.0, 0.0);
                d.remote = Some((layer.clone(), g));
            }
            let to = (ox - d.grab.0 * d.size.0, oy - d.grab.1 * d.size.1);
            if let Some((l, g)) = d.remote.as_ref()
                && let Some(p) = other.compute_point(l, &gtk::graphene::Point::new(to.0 as f32, to.1 as f32))
            {
                l.move_(g, p.x() as f64, p.y() as f64);
            }
        }
        // Dropping where the window already is does nothing.
        let (workspace, mon) = (d.workspace, d.mon);
        let new = target.filter(|(_, k)| match *k {
            DropKind::Peek(i) => i as i32 != workspace,
            DropKind::Card { index, mon: m } => {
                !(m == mon && (workspace < 0 || index as i32 == workspace))
            }
            DropKind::Screen { mon: m, .. } => m != mon,
            _ => true,
        });
        let old_w = d.target.as_ref().map(|t| t.0.clone());
        let new_w = new.as_ref().map(|t| t.0.clone());
        if old_w != new_w {
            if let Some(o) = old_w {
                o.remove_css_class("drop-target");
            }
            if let Some(n) = &new_w {
                n.add_css_class("drop-target");
            }
        }
        d.target = new;
    }

    fn drop_window(self: &Rc<Self>) {
        let Some(d) = self.drag.borrow_mut().take() else {
            return;
        };
        d.layer.remove(&d.ghost);
        if let Some((l, g)) = &d.remote {
            l.remove(g);
        }
        let Some((widget, kind)) = d.target.clone() else {
            d.cell.set_opacity(1.0);
            return;
        };
        widget.remove_css_class("drop-target");
        let size = (d.rect.2, d.rect.3);
        let primary = self.monitor();
        match kind {
            DropKind::Peek(i) => {
                if d.workspace >= 0 {
                    self.send_to_workspace(d.id, i);
                } else {
                    // On every workspace (another monitor): to the same
                    // place on the primary one, on that workspace.
                    let (x, y) = translate(d.rect.0, d.rect.1, d.mon, primary);
                    self.place_window(d.id, i as i32, x, y);
                }
            }
            DropKind::Thumb { index, mon } | DropKind::Card { index, mon } => {
                let p = d.root.compute_point(
                    &widget,
                    &gtk::graphene::Point::new(d.at.0 as f32, d.at.1 as f32),
                );
                let (fx, fy) = p
                    .map(|p| {
                        (
                            p.x() as f64 / widget.width().max(1) as f64,
                            p.y() as f64 / widget.height().max(1) as f64,
                        )
                    })
                    .unwrap_or((0.5, 0.5));
                let (px, py) = map_fraction(fx, fy, mon);
                let (x, y) = grab_place(px, py, d.grab, size);
                let ws = if index as i32 == d.workspace {
                    -1
                } else {
                    index as i32
                };
                self.place_window(d.id, ws, x, y);
            }
            DropKind::Gap(i) => self.call_then_refresh("SendWindowToNewWorkspace", (d.id, i)),
            DropKind::Screen { x, y, .. } => {
                let (x, y) = grab_place(x, y, d.grab, size);
                self.place_window(d.id, -1, x, y);
            }
        }
    }

    /// An app icon dragged out of the dock (umoja-dock's AppDrag): phase 0
    /// it moved, 1 it was dropped, 2 the drag ended without a drop. Dropped
    /// on a workspace thumbnail, a peek or a card, a new window of the app
    /// opens there; between two thumbnails, on a new workspace there
    /// (gnome-shell's app drop on a workspace).
    pub fn app_drag(self: &Rc<Self>, desktop_id: String, phase: u32) {
        let seq = self.app_drag_seq.get() + 1;
        self.app_drag_seq.set(seq);
        if phase > 1 || !self.is_visible() {
            self.set_app_target(None);
            self.move_app_ghost(&desktop_id, None);
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let p = compositor_query::<(bool, i32, i32)>("GetPointer", "(bii)").await;
            let Some(s) = weak.upgrade() else { return };
            if s.app_drag_seq.get() != seq {
                return;
            }
            let at = match p {
                Some((true, x, y)) => s.app_target_at(x as f64, y as f64),
                _ => None,
            };
            let target = at.as_ref().and_then(|(_, _, t)| t.clone());
            if phase == 0 {
                s.move_app_ghost(&desktop_id, at.as_ref().map(|(root, p, _)| (root, *p)));
                s.set_app_target(target);
                return;
            }
            s.move_app_ghost(&desktop_id, None);
            s.set_app_target(None);
            let Some((_, kind)) = target else { return };
            let to = match kind {
                DropKind::Peek(i) | DropKind::Thumb { index: i, .. } | DropKind::Card { index: i, .. } => {
                    AppDrop::Workspace(i)
                }
                DropKind::Gap(i) => AppDrop::NewWorkspace(i),
                DropKind::Screen { .. } => return,
            };
            s.launch_on(&desktop_id, to);
        });
    }

    /// The overview surface at (`x`, `y`) in layout pixels, the point in
    /// it, and the drop target there.
    #[allow(clippy::type_complexity)]
    fn app_target_at(
        &self,
        x: f64,
        y: f64,
    ) -> Option<(gtk::Widget, (f64, f64), Option<(gtk::Widget, DropKind)>)> {
        let mut roots: Vec<gtk::Widget> = vec![self.window.clone().upcast()];
        roots.extend(self.sides.borrow().iter().map(|s| s.window.clone().upcast()));
        roots.into_iter().find_map(|root| {
            if !root.is_visible() {
                return None;
            }
            let (ox, oy) = self.surface_origin(&root)?;
            let (lx, ly) = (x - ox as f64, y - oy as f64);
            if lx < 0.0 || ly < 0.0 || lx >= root.width() as f64 || ly >= root.height() as f64 {
                return None;
            }
            let target = self
                .target_at(&root, lx, ly)
                .filter(|(_, k)| !matches!(k, DropKind::Screen { .. }));
            Some((root, (lx, ly), target))
        })
    }

    /// Show the dragged app's icon at `at` (a point in an overview
    /// surface), or take it away.
    fn move_app_ghost(&self, desktop_id: &str, at: Option<(&gtk::Widget, (f64, f64))>) {
        let layer = at.and_then(|(root, _)| self.layer_for(root));
        let mut ghost = self.app_ghost.borrow_mut();
        if ghost.as_ref().map(|(l, _)| l) != layer.as_ref()
            && let Some((l, g)) = ghost.take()
        {
            l.remove(&g);
        }
        let (Some(layer), Some((root, (x, y)))) = (layer, at) else {
            return;
        };
        if ghost.is_none() {
            let icon = gio::DesktopAppInfo::new(desktop_id)
                .and_then(|a| a.icon())
                .unwrap_or_else(|| gio::ThemedIcon::new("application-x-executable").upcast());
            let img = gtk::Image::from_gicon(&icon);
            img.set_pixel_size(APP_GHOST);
            img.add_css_class("spread-drag-ghost");
            img.set_can_target(false);
            layer.put(&img, 0.0, 0.0);
            *ghost = Some((layer.clone(), img.upcast()));
        }
        let half = APP_GHOST as f32 / 2.0;
        if let Some((l, g)) = ghost.as_ref()
            && let Some(p) = root.compute_point(l, &gtk::graphene::Point::new(x as f32 - half, y as f32 - half))
        {
            l.move_(g, p.x() as f64, p.y() as f64);
        }
    }

    fn set_app_target(&self, new: Option<(gtk::Widget, DropKind)>) {
        let mut cur = self.app_target.borrow_mut();
        let old_w = cur.as_ref().map(|t| t.0.clone());
        let new_w = new.as_ref().map(|t| t.0.clone());
        if old_w != new_w {
            if let Some(o) = old_w {
                o.remove_css_class("drop-target");
            }
            if let Some(n) = &new_w {
                n.add_css_class("drop-target");
            }
        }
        *cur = new;
    }

    /// Open a new window of `desktop_id` and move it to the workspace
    /// `to` names once it maps (it opens on the current one first; the
    /// overview shows it move).
    fn launch_on(self: &Rc<Self>, desktop_id: &str, to: AppDrop) {
        let Some(app) = gio::DesktopAppInfo::new(desktop_id) else {
            tracing::warn!(desktop_id, "dropped app not found");
            return;
        };
        let before: Vec<u64> = self.state.borrow().windows.iter().map(|w| w.id).collect();
        crate::appgrid::launch_app(self.window.upcast_ref(), app.upcast_ref());
        let want = desktop_id.to_string();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut before = before;
            if before.is_empty() {
                before = fetch_windows().await.iter().map(|w| w.id).collect();
            }
            for _ in 0..(LAUNCH_WAIT.as_millis() / 250) {
                glib::timeout_future(Duration::from_millis(250)).await;
                let new = fetch_windows().await.into_iter().find(|w| {
                    !before.contains(&w.id)
                        && apps::app_for_window(&w.app_id)
                            .and_then(|a| a.id())
                            .is_some_and(|id| id.as_str() == want)
                });
                let Some(w) = new else { continue };
                let Some(s) = weak.upgrade() else { return };
                match to {
                    AppDrop::Workspace(i) => s.send_to_workspace(w.id, i),
                    AppDrop::NewWorkspace(i) => {
                        s.call_then_refresh("SendWindowToNewWorkspace", (w.id, i))
                    }
                }
                return;
            }
            tracing::info!(desktop_id = want, "no new window to move");
        });
    }

    fn cancel_drag(&self) {
        if let Some(d) = self.drag.borrow_mut().take() {
            d.layer.remove(&d.ghost);
            if let Some((l, g)) = &d.remote {
                l.remove(g);
            }
            d.cell.set_opacity(1.0);
            if let Some((t, _)) = d.target {
                t.remove_css_class("drop-target");
            }
        }
    }

    /// What is on screen, for tools/test/umoja-wm-test: ids per view and
    /// where previews, thumbnails and gaps are in layout pixels.
    pub fn test_state(&self, d: &glib::VariantDict) {
        d.insert_value("visible", &self.is_visible().to_variant());
        let shown = self.shown.borrow();
        d.insert_value("monitor", &shown.monitor.to_variant());
        {
            let st = self.state.borrow();
            d.insert_value("current", &st.current.to_variant());
            d.insert_value("count", &st.count.to_variant());
        }
        d.insert_value("card", &shown.card.to_variant());
        d.insert_value("thumbs", &shown.thumbs.to_variant());
        d.insert_value("sides", &shown.sides.to_variant());
        let sides_up = self.sides.borrow().iter().filter(|s| s.window.is_visible()).count() as u32;
        d.insert_value("sides_visible", &sides_up.to_variant());
        let sel = self
            .selected
            .get()
            .and_then(|(r, c)| self.nav.borrow().get(r).and_then(|row| row.get(c)).map(|(id, _)| *id))
            .unwrap_or(0);
        d.insert_value("selected", &sel.to_variant());

        let global = |w: &gtk::Widget| -> Option<Geo> {
            let root = w.root()?.upcast::<gtk::Widget>();
            let (ox, oy) = self.surface_origin(&root)?;
            let b = w.compute_bounds(&root)?;
            Some((
                ox + b.x() as i32,
                oy + b.y() as i32,
                b.width() as i32,
                b.height() as i32,
            ))
        };
        let mut thumbnails: Vec<(u32, i32, i32, i32, i32)> = Vec::new();
        let mut gaps: Vec<(u32, i32, i32, i32, i32)> = Vec::new();
        let mut cards: Vec<(u32, i32, i32, i32, i32)> = Vec::new();
        let mut previews: Vec<(u64, i32, i32, i32, i32)> = Vec::new();
        let mut small: Vec<(u64, i32, i32, i32, i32)> = Vec::new();
        for (w, k) in self.targets.borrow().iter() {
            let Some(w) = w.upgrade() else { continue };
            let Some((x, y, wd, ht)) = global(&w) else { continue };
            match k {
                DropKind::Thumb { index, .. } => {
                    thumbnails.push((*index, x, y, wd, ht));
                    // The small windows inside it.
                    let mut c = w.first_child();
                    while let Some(child) = c {
                        let mut g = child.first_child();
                        while let Some(win) = g {
                            if let Some(id) = win
                                .widget_name()
                                .strip_prefix("win-")
                                .and_then(|n| n.parse::<u64>().ok())
                                && let Some((x, y, wd, ht)) = global(&win)
                            {
                                small.push((id, x, y, wd, ht));
                            }
                            g = win.next_sibling();
                        }
                        c = child.next_sibling();
                    }
                }
                DropKind::Gap(i) => gaps.push((*i, x, y, wd, ht)),
                DropKind::Card { index, .. } => {
                    cards.push((*index, x, y, wd, ht));
                    collect_previews(&w, &global, &mut previews);
                }
                _ => {}
            }
        }
        d.insert_value("thumbnail_rects", &thumbnails.to_variant());
        d.insert_value("gap_rects", &gaps.to_variant());
        d.insert_value("card_rects", &cards.to_variant());
        d.insert_value("preview_rects", &previews.to_variant());
        d.insert_value("thumb_window_rects", &small.to_variant());
    }
}

/// The window previews (buttons named win-<id>) under `w`.
fn collect_previews(
    w: &gtk::Widget,
    global: &dyn Fn(&gtk::Widget) -> Option<Geo>,
    out: &mut Vec<(u64, i32, i32, i32, i32)>,
) {
    let mut c = w.first_child();
    while let Some(child) = c {
        if child.is::<gtk::Button>()
            && let Some(id) = child
                .widget_name()
                .strip_prefix("win-")
                .and_then(|n| n.parse::<u64>().ok())
        {
            if let Some((x, y, wd, ht)) = global(&child) {
                out.push((id, x, y, wd, ht));
            }
        } else {
            collect_previews(&child, global, out);
        }
        c = child.next_sibling();
    }
}

/// Is the plain desktop on screen: not blanked and not locked? Otherwise a
/// capture would show those instead of the windows. (umoja-mir also sends
/// no WindowsChanged while the screensaver shows.)
async fn screen_shows_desktop() -> bool {
    let Ok(session) = gio::bus_get_future(gio::BusType::Session).await else {
        return false;
    };
    let blanked = session.call_future(
        Some("org.freedesktop.ScreenSaver"),
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
        "GetActive",
        None,
        Some(glib::VariantTy::new("(b)").unwrap()),
        gio::DBusCallFlags::NO_AUTO_START,
        1000,
    );
    let locked = async {
        let system = gio::bus_get_future(gio::BusType::System).await.ok()?;
        system
            .call_future(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&("org.freedesktop.login1.Session", "LockedHint").to_variant()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                1000,
            )
            .await
            .ok()
            .and_then(|v| v.child_value(0).as_variant())
            .and_then(|v| v.get::<bool>())
    };
    let (blanked, locked) = futures_util::join!(blanked, locked);
    let yes = |r: Result<glib::Variant, glib::Error>| {
        matches!(r.ok().and_then(|v| v.get::<(bool,)>()), Some((true,)))
    };
    // No ScreenSaver service (test compositor) counts as on.
    !yes(blanked) && locked != Some(true)
}

/// Top-left of the whole monitor layout, which is (0, 0) of a grim capture.
fn layout_origin() -> (i32, i32, i32) {
    let Some(display) = gdk::Display::default() else {
        return (0, 0, 0);
    };
    let monitors = display.monitors();
    let (mut x0, mut y0, mut x1) = (i32::MAX, i32::MAX, i32::MIN);
    for i in 0..monitors.n_items() {
        if let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() {
            let g = m.geometry();
            x0 = x0.min(g.x());
            y0 = y0.min(g.y());
            x1 = x1.max(g.x() + g.width());
        }
    }
    if x0 == i32::MAX {
        (0, 0, 0)
    } else {
        (x0, y0, x1 - x0)
    }
}

fn crop(frame: &Pixbuf, w: &Win, (ox, oy, layout_w): (i32, i32, i32)) -> Option<gdk::Texture> {
    if layout_w <= 0 {
        return None;
    }
    let s = frame.width() as f64 / layout_w as f64;
    let x = (((w.x - ox) as f64) * s).round() as i32;
    let y = (((w.y - oy) as f64) * s).round() as i32;
    let x = x.clamp(0, frame.width() - 1);
    let y = y.clamp(0, frame.height() - 1);
    let cw = ((w.w as f64 * s).round() as i32).min(frame.width() - x);
    let ch = ((w.h as f64 * s).round() as i32).min(frame.height() - y);
    if cw < 8 || ch < 8 {
        return None;
    }
    // A copy, not the subpixbuf itself: a subpixbuf shares (and keeps
    // alive) the whole screen frame, so previews taken at different times
    // pinned a full frame each.
    let sub = frame.new_subpixbuf(x, y, cw, ch).copy()?;
    #[allow(deprecated)]
    Some(gdk::Texture::for_pixbuf(&sub))
}

/// Grab the screen through grim (zwlr_screencopy), as PPM for speed, at
/// `scale` times the logical layout size (grim's default is the largest
/// output scale, tens of megapixels with a HiDPI laptop and an ultrawide).
/// The frame is parsed off the main thread.
async fn capture(scale: f64) -> Option<Pixbuf> {
    let scale = format!("{scale}");
    let proc = gio::Subprocess::newv(
        &[
            "grim".as_ref(),
            "-s".as_ref(),
            scale.as_ref(),
            "-t".as_ref(),
            "ppm".as_ref(),
            "-".as_ref(),
        ],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;
    let read = proc.communicate_future(None);
    let timeout = glib::timeout_future(Duration::from_millis(1500));
    futures_util::pin_mut!(read, timeout);
    let (stdout, _) = match futures_util::future::select(read, timeout).await {
        futures_util::future::Either::Left((Ok(out), _)) => out,
        _ => {
            proc.force_exit();
            return None;
        }
    };
    let bytes = stdout?;
    let rgb = gio::spawn_blocking(move || crate::screenshot::parse_ppm(bytes.to_vec()))
        .await
        .ok()
        .flatten()?;
    Some(crate::screenshot::rgb_pixbuf(rgb))
}

pub async fn fetch_windows() -> Vec<Win> {
    let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
        return Vec::new();
    };
    let reply = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            "GetWindows",
            None,
            Some(glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await;
    let Ok(v) = reply else {
        return Vec::new();
    };
    let Some((rows,)) = v.get::<(Vec<(u64, String, String, i32, i32, i32, i32, u32)>,)>() else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|(id, app_id, title, x, y, w, h, flags)| Win {
            id,
            app_id,
            title,
            x,
            y,
            w,
            h,
            flags,
            workspace: if flags & FLAG_CURRENT_WORKSPACE != 0 {
                -1
            } else {
                0
            },
        })
        .collect()
}

/// The windows a background capture would see, bottom to top, with where
/// they are: the uncovered ones get a preview from it.
fn layout_key(state: &State) -> Vec<(u64, i32, i32, i32, i32)> {
    state
        .windows
        .iter()
        .filter(|w| w.flags & FLAG_CURRENT_WORKSPACE != 0 && w.flags & FLAG_MINIMIZED == 0)
        .map(|w| (w.id, w.x, w.y, w.w, w.h))
        .collect()
}

/// Windows on the current workspace that no window above overlaps: a
/// screen capture shows all of them.
fn uncovered(state: &State) -> impl Iterator<Item = &Win> {
    let here: Vec<&Win> = state
        .windows
        .iter()
        .filter(|w| w.flags & FLAG_CURRENT_WORKSPACE != 0 && w.flags & FLAG_MINIMIZED == 0)
        .collect();
    let open: Vec<&Win> = (0..here.len())
        .filter(|&i| !here[i + 1..].iter().any(|o| o.intersects(here[i])))
        .map(|i| here[i])
        .collect();
    open.into_iter()
}

async fn fetch_state() -> State {
    let (mut windows, ws, (current, count)) = futures_util::join!(
        fetch_windows(),
        compositor_query::<(Vec<(u64, i32)>,)>("GetWindowWorkspaces", "(a(ti))"),
        async {
            compositor_query::<(u32, u32)>("GetWorkspaces", "(uu)")
                .await
                .unwrap_or((0, 1))
        },
    );
    let map: HashMap<u64, i32> = ws.map(|(v,)| v.into_iter().collect()).unwrap_or_default();
    for w in &mut windows {
        // Without the per-window list (an older compositor) everything
        // counts as the current workspace, as before.
        w.workspace = map
            .get(&w.id)
            .copied()
            .unwrap_or(if w.flags & FLAG_CURRENT_WORKSPACE != 0 {
                current as i32
            } else {
                -2
            });
    }
    windows.retain(|w| w.workspace >= -1);
    State {
        windows,
        current,
        count: count.max(1),
    }
}

async fn compositor_query<T: glib::variant::FromVariant>(method: &str, ty: &str) -> Option<T> {
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    conn.call_future(
        Some(dbus::COMPOSITOR_BUS_NAME),
        dbus::COMPOSITOR_OBJECT_PATH,
        dbus::COMPOSITOR_INTERFACE,
        method,
        None,
        Some(glib::VariantTy::new(ty).ok()?),
        gio::DBusCallFlags::NONE,
        1000,
    )
    .await
    .ok()?
    .get::<T>()
}

/// A compositor call whose completion matters (the overview redraws after).
async fn compositor_call<A: ToVariant>(method: &str, args: A) {
    let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
        return;
    };
    if let Err(e) = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            method,
            Some(&args.to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await
    {
        tracing::warn!(method, error = %e, "compositor call failed");
    }
}

fn call_compositor<A: ToVariant + 'static>(method: &'static str, args: A) {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        if let Err(e) = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                method,
                Some(&args.to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
            )
            .await
        {
            tracing::warn!(method, error = %e, "compositor call failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(x: i32, y: i32, w: i32, h: i32) -> Win {
        Win {
            id: 1,
            app_id: String::new(),
            title: String::new(),
            x,
            y,
            w,
            h,
            flags: 0,
            workspace: 0,
        }
    }

    #[test]
    fn card_keeps_monitor_aspect() {
        let (w, h) = card_size((1600.0, 700.0), 16.0 / 10.0, true);
        assert!((w / h - 1.6).abs() < 1e-9);
        assert_eq!(h, 700.0);
        // Too wide for the room: limited by width, peeks included.
        let (w, h) = card_size((800.0, 700.0), 16.0 / 10.0, true);
        assert!(w <= 800.0 - 2.0 * PEEK);
        assert!((w / h - 1.6).abs() < 1e-9);
    }

    #[test]
    fn thumbnail_places_windows_on_their_monitor() {
        let mon = (1728, 1080);
        let r = thumb_rect(&win(100, 50, 864, 540), (0, 0), mon, 0.1).unwrap();
        assert_eq!(r, (10.0, 5.0, 86.4, 54.0));
        // A window on a monitor to the right is not in this thumbnail.
        assert!(thumb_rect(&win(1900, 50, 400, 300), (0, 0), mon, 0.1).is_none());
        // Same monitor offset in the layout.
        assert!(thumb_rect(&win(1900, 50, 400, 300), (1728, 0), mon, 0.1).is_some());
    }

    #[test]
    fn sticky_windows_show_on_the_current_workspace() {
        let mut w = win(0, 0, 10, 10);
        w.workspace = -1;
        assert_eq!(shown_on(&w, 3), 3);
        w.workspace = 1;
        assert_eq!(shown_on(&w, 3), 1);
    }

    #[test]
    fn windows_belong_to_the_monitor_holding_their_centre() {
        let mons = [(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];
        assert_eq!(monitor_of((100, 100, 800, 600), &mons), mons[0]);
        assert_eq!(monitor_of((1800, 100, 800, 600), &mons), mons[1]);
        // Off every monitor: the primary.
        assert_eq!(monitor_of((-5000, 0, 10, 10), &mons), mons[0]);
    }

    #[test]
    fn drops_keep_the_grab_point_under_the_pointer() {
        // Grabbed at its middle, dropped at (1000, 500): centred there.
        assert_eq!(grab_place(1000.0, 500.0, (0.5, 0.5), (400, 300)), (800, 350));
        // A quarter into a thumbnail of a 1920x1080 monitor at x 1920.
        assert_eq!(map_fraction(0.25, 0.5, (1920, 0, 1920, 1080)), (2400.0, 540.0));
        assert_eq!(map_fraction(2.0, -1.0, (0, 0, 100, 100)), (100.0, 0.0));
        assert_eq!(translate(960, 540, (0, 0, 1920, 1080), (1920, 0, 2560, 1440)), (3200, 720));
    }

    #[test]
    fn spread_goes_top_to_bottom_then_left_to_right() {
        let rects = [(900, 600, 400, 300), (100, 600, 400, 300), (500, 0, 400, 300)];
        assert_eq!(spread_order(&rects), vec![2, 1, 0]);
    }

    #[test]
    fn single_window_is_never_scaled_up() {
        let (s, rows) = layout(&[(400.0, 300.0)], (1600.0, 900.0), 32.0);
        assert_eq!(s, 1.0);
        assert_eq!(rows, vec![vec![0]]);
    }

    #[test]
    fn many_windows_wrap_into_rows() {
        let sizes = vec![(1200.0, 800.0); 6];
        let (s, rows) = layout(&sizes, (1600.0, 900.0), 32.0);
        assert!(rows.len() >= 2, "{rows:?}");
        assert!(s < 1.0 && s > 0.2);
        let total: usize = rows.iter().map(|r| r.len()).sum();
        assert_eq!(total, 6);
    }
}
