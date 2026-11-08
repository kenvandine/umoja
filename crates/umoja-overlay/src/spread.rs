//! The Activities overview's window spread (Super key), after gnome-shell.
//!
//! The windows of the current workspace are laid out side by side over the
//! wallpaper, scaled down together (never up), with the app icon at the
//! bottom of each, the title and a close button on hover, and a search
//! entry on top. Clicking a window activates it; typing switches to the app
//! search, Escape or Super goes back. The top bar and dock stay usable, as
//! with Ubuntu Dock in GNOME's overview.
//!
//! Window list and activation come from umoja-mir (`org.umoja.Compositor`).
//! Previews are cut from a screen capture taken just before the overview
//! opens; a window that other windows cover (or that is minimized) shows its
//! app icon instead.
//!
//! TODO: live per-window thumbnails once Mir offers toplevel capture
//! sources (ext-image-capture-source's foreign-toplevel source).

use crate::view::Overlay;
use adw::prelude::*;
use gio::prelude::*;
use gtk::gdk_pixbuf::{Pixbuf, PixbufLoader};
use gtk::{gdk, glib};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::time::Duration;
use umoja_common::gsettings as gs;
use umoja_common::{apps, dbus, i18n};

const FLAG_FOCUSED: u32 = 1 << 0;
const FLAG_MINIMIZED: u32 = 1 << 1;
const FLAG_CURRENT_WORKSPACE: u32 = 1 << 2;

/// Gap between previews and around the spread, in logical pixels.
const GAP: f64 = 32.0;
/// Room for the search entry above the spread.
const SEARCH_ROOM: f64 = 96.0;

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
}

impl Win {
    fn intersects(&self, o: &Win) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// Scale and row assignment for `sizes` inside `avail` (w, h): every window
/// gets the same scale, rows keep the given order, and the row count that
/// allows the largest scale wins. Never scales up.
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

pub struct Spread {
    window: gtk::ApplicationWindow,
    backdrop: gtk::Picture,
    entry: gtk::SearchEntry,
    rows: gtk::Box,
    overlay: Weak<Overlay>,
    opening: Cell<bool>,
}

impl Spread {
    pub fn new(app: &adw::Application, overlay: &Rc<Overlay>) -> Rc<Self> {
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

        let backdrop = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dim.add_css_class("spread-dim");

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(i18n::umoja("Type to search"))
            .width_request(400)
            .halign(gtk::Align::Center)
            .build();
        let rows = gtk::Box::new(gtk::Orientation::Vertical, GAP as i32);
        rows.set_valign(gtk::Align::Center);
        rows.set_halign(gtk::Align::Center);
        rows.set_vexpand(true);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_top(24);
        content.set_margin_bottom(GAP as i32);
        content.set_margin_start(GAP as i32);
        content.set_margin_end(GAP as i32);
        content.append(&entry);
        content.append(&rows);

        let stack = gtk::Overlay::new();
        stack.set_child(Some(&backdrop));
        stack.add_overlay(&dim);
        stack.add_overlay(&content);
        window.set_child(Some(&stack));

        let this = Rc::new(Self {
            window,
            backdrop,
            entry,
            rows,
            overlay: Rc::downgrade(overlay),
            opening: Cell::new(false),
        });
        this.connect_signals(&dim);
        this
    }

    fn connect_signals(self: &Rc<Self>, dim: &gtk::Box) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(s) = weak.upgrade() {
                    s.hide();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
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

        // A click on the empty background leaves the overview.
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            if let Some(s) = weak.upgrade() {
                s.hide();
            }
        });
        dim.add_controller(click);
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
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
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // Capture before mapping so the overview isn't in the picture.
            let (windows, frame) = futures_util::join!(fetch_windows(), capture());
            let Some(s) = weak.upgrade() else { return };
            s.opening.set(false);
            s.populate(windows, frame);
            s.window.present();
            s.entry.grab_focus();
        });
    }

    fn populate(self: &Rc<Self>, windows: Vec<Win>, frame: Option<Pixbuf>) {
        self.backdrop.set_paintable(
            gs::wallpaper_path(gs::prefers_dark())
                .and_then(|p| gdk::Texture::from_filename(&p).ok())
                .as_ref(),
        );
        while let Some(c) = self.rows.first_child() {
            self.rows.remove(&c);
        }

        // Stacking order (bottom to top) for occlusion, then left to right.
        let here: Vec<Win> = windows
            .into_iter()
            .filter(|w| w.flags & FLAG_CURRENT_WORKSPACE != 0)
            .collect();
        let covered: Vec<bool> = here
            .iter()
            .enumerate()
            .map(|(i, w)| {
                w.flags & FLAG_MINIMIZED != 0
                    || here[i + 1..]
                        .iter()
                        .any(|o| o.flags & FLAG_MINIMIZED == 0 && o.intersects(w))
            })
            .collect();
        let mut order: Vec<usize> = (0..here.len()).collect();
        order.sort_by_key(|&i| (here[i].x + here[i].w / 2, here[i].y));

        if here.is_empty() {
            return;
        }
        let (mw, mh) = self.available();
        let sizes: Vec<(f64, f64)> = order
            .iter()
            .map(|&i| (here[i].w.max(1) as f64, here[i].h.max(1) as f64))
            .collect();
        let (scale, groups) = layout(&sizes, (mw, mh), GAP);
        let origin = layout_origin();

        for g in groups {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, GAP as i32);
            row.set_halign(gtk::Align::Center);
            for k in g {
                let i = order[k];
                let w = &here[i];
                let thumb = if covered[i] {
                    None
                } else {
                    frame.as_ref().and_then(|f| crop(f, w, origin))
                };
                row.append(&self.preview(w, scale, thumb));
            }
            self.rows.append(&row);
        }
    }

    /// Room for the spread on the monitor the overview is on.
    fn available(&self) -> (f64, f64) {
        let geometry = gdk::Display::default()
            .and_then(|d| d.monitors().item(0).and_downcast::<gdk::Monitor>())
            .map(|m| m.geometry());
        let (w, h) = geometry
            .map(|g| (g.width() as f64, g.height() as f64))
            .unwrap_or((1280.0, 800.0));
        // Leave room for the top bar, dock, search entry and margins.
        (
            (w - 2.0 * GAP - 96.0).max(200.0),
            (h - SEARCH_ROOM - 2.0 * GAP - 48.0).max(200.0),
        )
    }

    fn preview(self: &Rc<Self>, w: &Win, scale: f64, thumb: Option<gdk::Texture>) -> gtk::Widget {
        let width = (w.w as f64 * scale).round().max(48.0) as i32;
        let height = (w.h as f64 * scale).round().max(48.0) as i32;
        let info = apps::app_for_window(&w.app_id);
        let icon = info
            .as_ref()
            .and_then(|i| i.icon())
            .unwrap_or_else(|| gio::ThemedIcon::new("application-x-executable").upcast());

        let body: gtk::Widget = match thumb {
            Some(t) => gtk::Picture::builder()
                .paintable(&t)
                .content_fit(gtk::ContentFit::Fill)
                .can_shrink(true)
                .build()
                .upcast(),
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
        cell.upcast()
    }
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
    let sub = frame.new_subpixbuf(x, y, cw, ch);
    #[allow(deprecated)]
    Some(gdk::Texture::for_pixbuf(&sub))
}

/// Grab the screen through grim (zwlr_screencopy), as PPM for speed.
async fn capture() -> Option<Pixbuf> {
    let proc = gio::Subprocess::newv(
        &["grim".as_ref(), "-t".as_ref(), "ppm".as_ref(), "-".as_ref()],
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
    let loader = PixbufLoader::new();
    loader.write(&bytes).ok()?;
    loader.close().ok()?;
    loader.pixbuf()
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
        })
        .collect()
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
