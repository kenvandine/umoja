//! Window-management chrome drawn for umoja-mir: the tile preview and the
//! layout picker shown while dragging a window, and the Alt+Tab switcher.
//!
//! The compositor decides everything (see umoja-mir's `snap.rs`,
//! `switcher.rs`); these surfaces only draw what they are told, so they never
//! take input focus. They follow gnome-shell and Ubuntu's Tiling Assistant:
//!
//! * the tile preview is an accent-coloured translucent rectangle that eases
//!   from the window's frame to the tile it will fill (250 ms, ease-out);
//! * the layout picker peeks out from under the top bar during a drag and
//!   slides down when the pointer reaches it, its hovered tile filled in; it
//!   is the blurred see-through shell card with accent-coloured tiles;
//! * the switcher is the dark rounded popup with app icons and names that
//!   appears after 150 ms, so a quick Alt+Tab never flashes.

use adw::prelude::*;
use gio::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;
use std::time::Duration;
use umoja_common::dbus;

/// gnome-shell's WINDOW_ANIMATION_TIME.
const ANIMATION_MS: f64 = 250.0;
/// switcherPopup.js POPUP_DELAY_TIMEOUT.
const SWITCHER_DELAY_MS: u64 = 150;

// Layout picker metrics: keep in sync with umoja-mir's snap.rs.
const ICON: f64 = 64.0;
const SPACING: f64 = 15.0;
const INNER_PAD: f64 = 15.0;
const OUTER_PAD: f64 = 20.0;
const PICKER_W: f64 = 4.0 * ICON + 3.0 * SPACING + 2.0 * (INNER_PAD + OUTER_PAD);
const PICKER_H: f64 = ICON + 2.0 * (INNER_PAD + OUTER_PAD);

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct R {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl R {
    fn lerp(a: R, b: R, t: f64) -> R {
        R {
            x: a.x + (b.x - a.x) * t,
            y: a.y + (b.y - a.y) * t,
            w: a.w + (b.w - a.w) * t,
            h: a.h + (b.h - a.h) * t,
        }
    }
}

fn ease_out_quad(t: f64) -> f64 {
    1.0 - (1.0 - t) * (1.0 - t)
}

fn monitor_at(x: i32, y: i32) -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    let mut first = None;
    for i in 0..monitors.n_items() {
        let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
            continue;
        };
        let g = m.geometry();
        if x >= g.x() && x < g.x() + g.width() && y >= g.y() && y < g.y() + g.height() {
            return Some(m);
        }
        first.get_or_insert(m);
    }
    first
}

/// A transparent, click-through layer surface covering one monitor.
fn chrome_window(app: &adw::Application, layer: Layer, namespace: &str) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .css_classes(["umoja-chrome"])
        .build();
    window.init_layer_shell();
    window.set_layer(layer);
    window.set_namespace(namespace);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_exclusive_zone(-1);
    window.connect_realize(|w| {
        if let Some(s) = w.surface() {
            s.set_input_region(&gtk::cairo::Region::create());
        }
    });
    window
}

fn accent() -> gdk::RGBA {
    adw::StyleManager::default().accent_color_rgba()
}

fn rounded(cr: &gtk::cairo::Context, r: R, radius: [f64; 4]) {
    // radius: top-left, top-right, bottom-right, bottom-left
    let [tl, tr, br, bl] = radius;
    cr.new_sub_path();
    cr.arc(r.x + r.w - tr, r.y + tr, tr, -PI / 2.0, 0.0);
    cr.arc(r.x + r.w - br, r.y + r.h - br, br, 0.0, PI / 2.0);
    cr.arc(r.x + bl, r.y + r.h - bl, bl, PI / 2.0, PI);
    cr.arc(r.x + tl, r.y + tl, tl, PI, 1.5 * PI);
    cr.close_path();
}

// ---- tile preview -------------------------------------------------------------

/// panel.css's @shell_bg, the dark surface shared by the bar, menus, OSD,
/// notification bubbles and dock.
const SHELL_BG: (f64, f64, f64) = (22.0 / 255.0, 22.0 / 255.0, 24.0 / 255.0);
const PREVIEW_INSET: f64 = 6.0;
const PREVIEW_RADIUS: f64 = 18.0;

struct Preview {
    window: gtk::ApplicationWindow,
    area: crate::canvas::Canvas,
    monitor: Cell<(i32, i32)>,
    from: Cell<R>,
    to: Cell<R>,
    /// Rectangle drawn on the last frame, in global coordinates.
    shown: Cell<Option<R>>,
    opacity_from: Cell<f64>,
    opacity: Cell<f64>,
    start: Cell<i64>,
    target_visible: Cell<bool>,
    ticking: Cell<bool>,
}

impl Preview {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = chrome_window(app, Layer::Top, "umoja-tile-preview");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        let area = crate::canvas::Canvas::new();
        window.set_child(Some(&area));
        let this = Rc::new(Self {
            window,
            area,
            monitor: Cell::new((0, 0)),
            from: Cell::default(),
            to: Cell::default(),
            shown: Cell::new(None),
            opacity_from: Cell::new(0.0),
            opacity: Cell::new(0.0),
            start: Cell::new(0),
            target_visible: Cell::new(false),
            ticking: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.area.set_draw(move |snapshot, _, _| {
            if let Some(p) = weak.upgrade() {
                p.draw(snapshot);
            }
        });
        this
    }

    fn progress(&self) -> f64 {
        let elapsed = (glib::monotonic_time() - self.start.get()) as f64 / 1000.0;
        ease_out_quad((elapsed / ANIMATION_MS).clamp(0.0, 1.0))
    }

    fn draw(&self, snapshot: &gtk::Snapshot) {
        use crate::canvas::{fill_rounded, rect, stroke_rounded};
        let t = self.progress();
        let target_opacity = if self.target_visible.get() { 1.0 } else { 0.0 };
        let opacity = self.opacity_from.get() + (target_opacity - self.opacity_from.get()) * t;
        self.opacity.set(opacity);
        let r = R::lerp(self.from.get(), self.to.get(), t);
        self.shown.set(Some(r));
        if opacity <= 0.0 {
            return;
        }
        let (mx, my) = self.monitor.get();
        let r = R {
            x: r.x - mx as f64,
            y: r.y - my as f64,
            ..r
        };
        // The shell's dark translucent card (panel.css @shell_bg), inset
        // and rounded, outlined in the accent color so the target reads.
        let r = R {
            x: r.x + PREVIEW_INSET,
            y: r.y + PREVIEW_INSET,
            w: (r.w - 2.0 * PREVIEW_INSET).max(0.0),
            h: (r.h - 2.0 * PREVIEW_INSET).max(0.0),
        };
        let radius = PREVIEW_RADIUS.min(r.w / 2.0).min(r.h / 2.0);
        let bg = gdk::RGBA::new(
            SHELL_BG.0 as f32,
            SHELL_BG.1 as f32,
            SHELL_BG.2 as f32,
            (0.55 * opacity) as f32,
        );
        fill_rounded(snapshot, rect(r.x, r.y, r.w, r.h), radius, &bg);
        let a = accent();
        let line = gdk::RGBA::new(a.red(), a.green(), a.blue(), (0.9 * opacity) as f32);
        // A 2 px line just inside the card's edge.
        stroke_rounded(
            snapshot,
            rect(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 2.0),
            (radius - 1.0).max(0.0),
            2.0,
            &line,
        );
    }

    fn animate(self: &Rc<Self>) {
        self.start.set(glib::monotonic_time());
        if self.ticking.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        // A plain timer rather than a tick callback: the frame clock of a
        // click-through layer surface may idle between commits.
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(p) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            p.area.queue_draw();
            if p.progress() >= 1.0 {
                p.ticking.set(false);
                if !p.target_visible.get() {
                    p.window.set_visible(false);
                    p.shown.set(None);
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    fn show(self: &Rc<Self>, to: R, window: R) {
        if self.target_visible.get() && self.to.get() == to {
            return;
        }
        let was_visible = self.window.is_visible() && self.shown.get().is_some();
        if !was_visible {
            if let Some(m) = monitor_at((to.x + to.w / 2.0) as i32, (to.y + to.h / 2.0) as i32) {
                let g = m.geometry();
                self.monitor.set((g.x(), g.y()));
                self.window.set_monitor(&m);
            }
            self.window.present();
        }
        // Start from where the preview is now, or from the window itself.
        self.from
            .set(self.shown.get().filter(|_| was_visible).unwrap_or(window));
        self.opacity_from
            .set(if was_visible { self.opacity.get() } else { 0.0 });
        self.to.set(to);
        self.target_visible.set(true);
        self.animate();
    }

    fn hide(self: &Rc<Self>) {
        if !self.target_visible.replace(false) {
            return;
        }
        self.from.set(self.shown.get().unwrap_or(self.to.get()));
        self.opacity_from.set(self.opacity.get());
        self.animate();
    }
}

// ---- layout picker ------------------------------------------------------------

struct Picker {
    window: gtk::ApplicationWindow,
    /// Clips the card at the top bar's bottom edge, so it slides out from
    /// under the bar.
    clip: gtk::Fixed,
    /// The see-through shell card (`.layout-picker` in overlay.css): a real
    /// widget rather than cairo so GTK's backdrop-filter blurs behind it,
    /// like the notification bubbles and the Alt+Tab switcher.
    card: gtk::Box,
    /// The four layout icons, drawn in the accent colour.
    icons: gtk::DrawingArea,
    /// Visibility the compositor asked for: 0 hidden, 1 peek, 2 shown.
    visibility: Cell<u32>,
    tile: Cell<u32>,
    translation_from: Cell<f64>,
    translation: Cell<f64>,
    start: Cell<i64>,
    ticking: Cell<bool>,
}

fn picker_translation(visibility: u32) -> f64 {
    match visibility {
        1 => INNER_PAD + OUTER_PAD,
        2 => PICKER_H,
        _ => 0.0,
    }
}

impl Picker {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = chrome_window(app, Layer::Overlay, "umoja-layout-picker");
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        // Only acts on GTK < 4.24; newer GTK blurs via the card's CSS.
        umoja_common::blur::blur_behind(&window);
        let clip = gtk::Fixed::new();
        clip.set_size_request(PICKER_W as i32, PICKER_H as i32);
        clip.set_overflow(gtk::Overflow::Hidden);
        clip.set_valign(gtk::Align::Start);
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        card.add_css_class("layout-picker");
        card.set_size_request(
            (PICKER_W - 2.0 * OUTER_PAD) as i32,
            (PICKER_H - 2.0 * OUTER_PAD) as i32,
        );
        let icons = gtk::DrawingArea::new();
        icons.set_content_width((4.0 * ICON + 3.0 * SPACING) as i32);
        icons.set_content_height(ICON as i32);
        icons.set_hexpand(true);
        icons.set_halign(gtk::Align::Center);
        icons.set_valign(gtk::Align::Center);
        card.append(&icons);
        clip.put(&card, OUTER_PAD, -PICKER_H);
        window.set_child(Some(&clip));
        let this = Rc::new(Self {
            window,
            clip,
            card,
            icons,
            visibility: Cell::new(0),
            tile: Cell::new(0),
            translation_from: Cell::new(0.0),
            translation: Cell::new(0.0),
            start: Cell::new(0),
            ticking: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.icons.set_draw_func(move |_, cr, _, _| {
            if let Some(p) = weak.upgrade() {
                p.draw_icons(cr);
            }
        });
        // Follow the accent colour live, like the tile preview.
        let weak = Rc::downgrade(&this);
        adw::StyleManager::default().connect_accent_color_rgba_notify(move |_| {
            if let Some(p) = weak.upgrade() {
                p.icons.queue_draw();
            }
        });
        this
    }

    fn progress(&self) -> f64 {
        let elapsed = (glib::monotonic_time() - self.start.get()) as f64 / 1000.0;
        ease_out_quad((elapsed / ANIMATION_MS).clamp(0.0, 1.0))
    }

    /// Moves the card to this frame's slide position.
    fn place(&self) {
        let to = picker_translation(self.visibility.get());
        let from = self.translation_from.get();
        let t = from + (to - from) * self.progress();
        self.translation.set(t);
        self.card.set_visible(t > 0.0);
        self.clip
            .move_(&self.card, OUTER_PAD, t - PICKER_H + OUTER_PAD);
    }

    fn draw_icons(&self, cr: &gtk::cairo::Context) {
        // Icons in tiling-assistant's order: vertical, horizontal, quarter, maximize.
        let tile = self.tile.get();
        let accent = accent();
        for i in 0..4 {
            let x = i as f64 * (ICON + SPACING);
            draw_tile_icon(cr, x, 0.0, i, tile, &accent);
        }
    }

    fn update(self: &Rc<Self>, visibility: u32, tile: u32, x: i32, area_y: i32) {
        if visibility != 0 {
            if let Some(m) = monitor_at(x + PICKER_W as i32 / 2, area_y) {
                let g = m.geometry();
                let bar = (area_y - g.y()).max(0) as f64;
                self.window.set_monitor(&m);
                self.window.set_margin(Edge::Left, x - g.x());
                self.clip.set_margin_top(bar as i32);
            }
            if !self.window.is_visible() {
                self.translation.set(0.0);
                self.place();
                self.window.present();
            }
        }
        let changed_vis = self.visibility.replace(visibility) != visibility;
        if self.tile.replace(tile) != tile {
            self.icons.queue_draw();
        }
        if changed_vis {
            self.translation_from.set(self.translation.get());
            self.start.set(glib::monotonic_time());
            self.tick();
        }
        self.place();
    }

    fn tick(self: &Rc<Self>) {
        if self.ticking.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        // A plain timer rather than a tick callback: the frame clock of a
        // click-through layer surface may idle between commits.
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(p) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            p.place();
            if p.progress() >= 1.0 {
                p.ticking.set(false);
                if p.visibility.get() == 0 {
                    p.window.set_visible(false);
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

/// One layout picker icon (64×64), after tiling-assistant's media/tile-*.svg:
/// each tile is a 3px outline with rounded outer corners, and the hovered
/// tile gets a solid fill inset by 5px, both in the accent colour.
fn draw_tile_icon(
    cr: &gtk::cairo::Context,
    x: f64,
    y: f64,
    icon: usize,
    hovered: u32,
    accent: &gdk::RGBA,
) {
    const H: f64 = 31.0; // half, with a 2px gap between tiles
    const O: f64 = 33.0;
    // (rect in icon space, picker tile number)
    let tiles: &[(R, u32)] = match icon {
        0 => &[
            (
                R {
                    x: 0.0,
                    y: 0.0,
                    w: 64.0,
                    h: H,
                },
                3,
            ),
            (
                R {
                    x: 0.0,
                    y: O,
                    w: 64.0,
                    h: H,
                },
                4,
            ),
        ],
        1 => &[
            (
                R {
                    x: 0.0,
                    y: 0.0,
                    w: H,
                    h: 64.0,
                },
                1,
            ),
            (
                R {
                    x: O,
                    y: 0.0,
                    w: H,
                    h: 64.0,
                },
                2,
            ),
        ],
        2 => &[
            (
                R {
                    x: 0.0,
                    y: 0.0,
                    w: H,
                    h: H,
                },
                6,
            ),
            (
                R {
                    x: O,
                    y: 0.0,
                    w: H,
                    h: H,
                },
                5,
            ),
            (
                R {
                    x: 0.0,
                    y: O,
                    w: H,
                    h: H,
                },
                7,
            ),
            (
                R {
                    x: O,
                    y: O,
                    w: H,
                    h: H,
                },
                8,
            ),
        ],
        _ => &[(
            R {
                x: 0.0,
                y: 0.0,
                w: 64.0,
                h: 64.0,
            },
            9,
        )],
    };
    for &(t, number) in tiles {
        let r = R {
            x: x + t.x,
            y: y + t.y,
            ..t
        };
        let corner = |cx: bool, cy: bool| if cx && cy { 8.0 } else { 0.0 };
        let left = t.x == 0.0;
        let right = t.x + t.w == 64.0;
        let topc = t.y == 0.0;
        let bottom = t.y + t.h == 64.0;
        let radii = [
            corner(left, topc),
            corner(right, topc),
            corner(right, bottom),
            corner(left, bottom),
        ];
        cr.set_source_rgba(
            accent.red().into(),
            accent.green().into(),
            accent.blue().into(),
            1.0,
        );
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        rounded(cr, r, radii);
        let inner = R {
            x: r.x + 3.0,
            y: r.y + 3.0,
            w: r.w - 6.0,
            h: r.h - 6.0,
        };
        rounded(cr, inner, radii.map(|v| (v - 3.0).max(0.0)));
        let _ = cr.fill();
        cr.set_fill_rule(gtk::cairo::FillRule::Winding);
        if number == hovered {
            cr.set_source_rgba(
                accent.red().into(),
                accent.green().into(),
                accent.blue().into(),
                1.0,
            );
            let fill = R {
                x: r.x + 5.0,
                y: r.y + 5.0,
                w: r.w - 10.0,
                h: r.h - 10.0,
            };
            rounded(cr, fill, radii.map(|v| (v - 5.0).max(0.0)));
            let _ = cr.fill();
        }
    }
}

// ---- Alt+Tab switcher -----------------------------------------------------------

/// (app_id, title, window titles) per item, as umoja-mir sends them.
pub type SwitcherItems = Vec<(String, String, Vec<String>)>;

struct Switcher {
    window: gtk::ApplicationWindow,
    list: gtk::Box,
    windows_row: gtk::Box,
    items: RefCell<Vec<gtk::Button>>,
    data: RefCell<SwitcherItems>,
    mode: Cell<u32>,
    selected: Cell<u32>,
    sub: Cell<i32>,
    pending: RefCell<Option<glib::SourceId>>,
}

fn icon_for(app_id: &str) -> (gio::Icon, String) {
    match umoja_common::apps::app_for_window(app_id) {
        Some(info) => (
            info.icon()
                .unwrap_or_else(|| gio::ThemedIcon::new("application-x-executable").upcast()),
            info.name().to_string(),
        ),
        None => (
            gio::ThemedIcon::new("application-x-executable").upcast(),
            String::new(),
        ),
    }
}

impl Switcher {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-chrome"])
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-switcher");
        // The see-through list blurs what is behind it, like the bubbles.
        umoja_common::blur::blur_behind(&window);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
        let list = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        list.add_css_class("switcher-list");
        let windows_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        windows_row.add_css_class("switcher-list");
        windows_row.add_css_class("switcher-windows");
        windows_row.set_halign(gtk::Align::Center);
        windows_row.set_visible(false);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
        column.add_css_class("switcher-popup");
        column.append(&list);
        column.append(&windows_row);
        window.set_child(Some(&column));
        Rc::new(Self {
            window,
            list,
            windows_row,
            items: RefCell::default(),
            data: RefCell::default(),
            mode: Cell::new(0),
            selected: Cell::new(0),
            sub: Cell::new(-1),
            pending: RefCell::default(),
        })
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let data = self.data.borrow();
        let n = data.len().max(1);
        // switcherPopup.js shrinks icons when many apps don't fit.
        let size = if n > 12 {
            48
        } else if n > 8 {
            64
        } else {
            96
        };
        let mut buttons = Vec::new();
        for (i, (app_id, title, windows)) in data.iter().enumerate() {
            // Mode 3 is Ctrl+Alt+Tab: an icon name and a gnome-shell msgid.
            let (icon, name) = if self.mode.get() == 3 {
                // focus-top-bar-symbolic ships inside gnome-shell, not in
                // icon themes, so give it a themed fallback.
                let mut names = vec![app_id.as_str()];
                if app_id == "focus-top-bar-symbolic" {
                    names.extend(["go-top-symbolic", "go-up-symbolic"]);
                }
                (
                    gio::ThemedIcon::from_names(&names).upcast(),
                    umoja_common::i18n::shell(title),
                )
            } else {
                icon_for(app_id)
            };
            let label_text = if self.mode.get() == 3 {
                name
            } else if self.mode.get() == 0 {
                if name.is_empty() { title.clone() } else { name }
            } else {
                windows.first().cloned().unwrap_or_else(|| title.clone())
            };
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(size);
            let label = gtk::Label::builder()
                .label(&label_text)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(if self.mode.get() == 0 { 12 } else { 24 })
                .build();
            let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
            column.append(&image);
            column.append(&label);
            // An arrow under apps with several windows, as in GNOME.
            let arrow = gtk::Label::new(Some(if windows.len() > 1 { "▾" } else { " " }));
            arrow.add_css_class("switcher-arrow");
            if self.mode.get() == 0 {
                column.append(&arrow);
            }
            let button = gtk::Button::builder()
                .child(&column)
                .css_classes(["item-box"])
                .focusable(false)
                .build();
            let index = i as u32;
            button.connect_clicked(move |_| pick(index));
            self.list.append(&button);
            buttons.push(button);
        }
        drop(data);
        *self.items.borrow_mut() = buttons;
        self.highlight();
    }

    fn highlight(&self) {
        for (i, b) in self.items.borrow().iter().enumerate() {
            if i as u32 == self.selected.get() {
                b.add_css_class("selected");
            } else {
                b.remove_css_class("selected");
            }
        }
        // Window list for the selected app (Alt+` inside the app switcher).
        while let Some(c) = self.windows_row.first_child() {
            self.windows_row.remove(&c);
        }
        let sub = self.sub.get();
        let data = self.data.borrow();
        let show = self.mode.get() == 0 && sub >= 0;
        if let (true, Some((app_id, _, titles))) = (show, data.get(self.selected.get() as usize)) {
            let (icon, _) = icon_for(app_id);
            for (i, t) in titles.iter().enumerate() {
                let image = gtk::Image::from_gicon(&icon);
                image.set_pixel_size(48);
                let label = gtk::Label::builder()
                    .label(t)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(24)
                    .build();
                let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
                column.append(&image);
                column.append(&label);
                let item = gtk::Box::new(gtk::Orientation::Vertical, 0);
                item.add_css_class("item-box");
                if i as i32 == sub {
                    item.add_css_class("selected");
                }
                item.append(&column);
                self.windows_row.append(&item);
            }
        }
        self.windows_row.set_visible(show);
    }

    fn show(self: &Rc<Self>, mode: u32, items: SwitcherItems, selected: u32, sub: i32) {
        self.mode.set(mode);
        *self.data.borrow_mut() = items;
        self.selected.set(selected);
        self.sub.set(sub);
        self.rebuild();
        if self.window.is_visible() || self.pending.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id =
            glib::timeout_add_local_once(Duration::from_millis(SWITCHER_DELAY_MS), move || {
                let Some(s) = weak.upgrade() else { return };
                s.pending.borrow_mut().take();
                // No monitor set: the compositor puts it on the active output.
                s.window.present();
            });
        *self.pending.borrow_mut() = Some(id);
    }

    fn select(&self, selected: u32, sub: i32) {
        self.selected.set(selected);
        self.sub.set(sub);
        self.highlight();
    }

    fn hide(&self) {
        if let Some(id) = self.pending.borrow_mut().take() {
            id.remove();
        }
        self.window.set_visible(false);
    }
}

fn pick(index: u32) {
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let _ = conn
            .call_future(
                Some(dbus::COMPOSITOR_BUS_NAME),
                dbus::COMPOSITOR_OBJECT_PATH,
                dbus::COMPOSITOR_INTERFACE,
                "SwitcherPick",
                Some(&(index,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
            )
            .await;
    });
}

// ---- workspace switcher popup -------------------------------------------------

/// gnome-shell's WorkspaceSwitcherPopup: a row of pills, the active one
/// wider, shown low on the screen for a moment after a keyboard switch.
struct WorkspacePopup {
    window: gtk::ApplicationWindow,
    row: gtk::Box,
    hide_id: RefCell<Option<glib::SourceId>>,
}

/// switcherPopup DISPLAY_TIMEOUT in workspaceSwitcherPopup.js.
const WORKSPACE_POPUP_MS: u64 = 600;

impl WorkspacePopup {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = chrome_window(app, Layer::Overlay, "umoja-workspace-switcher");
        window.set_exclusive_zone(0);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 120);
        umoja_common::blur::blur_behind(&window);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("workspace-switcher");
        window.set_child(Some(&row));
        Rc::new(Self {
            window,
            row,
            hide_id: RefCell::default(),
        })
    }

    fn show(self: &Rc<Self>, active: u32, count: u32) {
        while let Some(c) = self.row.first_child() {
            self.row.remove(&c);
        }
        for i in 0..count.max(1) {
            let pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            pill.add_css_class("ws-indicator");
            if i == active {
                pill.add_css_class("active");
            }
            self.row.append(&pill);
        }
        self.window.present();
        if let Some(id) = self.hide_id.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id =
            glib::timeout_add_local_once(Duration::from_millis(WORKSPACE_POPUP_MS), move || {
                if let Some(p) = weak.upgrade() {
                    p.hide_id.borrow_mut().take();
                    p.window.set_visible(false);
                }
            });
        *self.hide_id.borrow_mut() = Some(id);
    }
}

// ---- public face ------------------------------------------------------------------

pub struct Chrome {
    preview: Rc<Preview>,
    picker: Rc<Picker>,
    switcher: Rc<Switcher>,
    workspaces: Rc<WorkspacePopup>,
}

impl Chrome {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        Rc::new(Self {
            preview: Preview::new(app),
            picker: Picker::new(app),
            switcher: Switcher::new(app),
            workspaces: WorkspacePopup::new(app),
        })
    }

    pub fn tile_preview(&self, show: bool, to: (i32, i32, i32, i32), window: (i32, i32, i32, i32)) {
        if show {
            let r = |(x, y, w, h): (i32, i32, i32, i32)| R {
                x: x as f64,
                y: y as f64,
                w: w as f64,
                h: h as f64,
            };
            self.preview.show(r(to), r(window));
        } else {
            self.preview.hide();
        }
    }

    pub fn layout_picker(&self, visibility: u32, tile: u32, x: i32, area_y: i32) {
        self.picker.update(visibility, tile, x, area_y);
    }

    pub fn show_switcher(&self, mode: u32, items: SwitcherItems, selected: u32, sub: i32) {
        self.switcher.show(mode, items, selected, sub);
    }

    pub fn switcher_select(&self, selected: u32, sub: i32) {
        self.switcher.select(selected, sub);
    }

    pub fn hide_switcher(&self) {
        self.switcher.hide();
    }

    pub fn workspace_switched(&self, active: u32, count: u32) {
        self.workspaces.show(active, count);
    }
}
