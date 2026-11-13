//! The download indicator: while umoja-overlay downloads something in the
//! background (the assistant's Lemonade model, a Piper voice), a ring in
//! the bar fills with the overall progress around a download arrow; it
//! spins while the size isn't known yet. Hovering it (or clicking, for
//! touch) opens a card listing each download with its progress and a
//! Cancel button. It follows org.umoja.Overlay's DownloadsChanged, like the
//! screencast pill follows ScreencastChanged.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use libadwaita as adw;
use umoja_common::{dbus, i18n};

/// (id, kind, title, state, bytes done, bytes total or 0, cancellable), as
/// umoja-overlay's downloads.rs sends it.
pub type Item = (String, String, String, String, u64, u64, bool);

const RING_SIZE: i32 = 16;
/// How long the pointer may be off both the ring and the card before the
/// card closes, so it can travel from one to the other.
const CLOSE_DELAY: Duration = Duration::from_millis(350);

/// What the ring shows, from the whole list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ring {
    /// Fraction of all known bytes downloaded.
    Progress(f64),
    /// Downloading, size not known yet: a spinning arc.
    Spinning,
    /// Only downloads waiting for an unmetered connection.
    Paused,
    /// Everything finished.
    Done,
    /// Nothing downloading and something failed.
    Failed,
}

pub fn ring_for(items: &[Item]) -> Ring {
    let downloading: Vec<&Item> = items.iter().filter(|i| i.3 == "downloading").collect();
    if !downloading.is_empty() {
        if downloading.iter().any(|i| i.5 == 0) {
            return Ring::Spinning;
        }
        // Bytes-weighted, and counting downloads that just finished, so
        // the ring doesn't jump back when a small one completes.
        let (done, total) = items
            .iter()
            .filter(|i| (i.3 == "downloading" || i.3 == "done") && i.5 > 0)
            .fold((0u64, 0u64), |(d, t), i| (d + i.4.min(i.5), t + i.5));
        return Ring::Progress(if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        });
    }
    if items.iter().any(|i| i.3 == "waiting") {
        Ring::Paused
    } else if items.iter().any(|i| i.3 == "failed") && !items.iter().any(|i| i.3 == "done") {
        Ring::Failed
    } else {
        Ring::Done
    }
}

/// "3 min left", "40 s left", "1 h 5 min left".
pub fn time_left(secs: f64) -> String {
    let s = secs.ceil().max(1.0) as u64;
    if s < 90 {
        format!("{s} s left")
    } else if s < 3600 {
        format!("{} min left", s.div_ceil(60))
    } else {
        format!("{} h {} min left", s / 3600, s % 3600 / 60)
    }
}

/// Download speed per id, for "time left".
#[derive(Default)]
struct Speed {
    seen: Option<(Instant, u64)>,
    /// Bytes a second, smoothed.
    rate: f64,
    since: Option<Instant>,
}

impl Speed {
    fn update(&mut self, done: u64) {
        let now = Instant::now();
        match self.seen {
            Some((t, d)) if done >= d => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt >= 0.5 {
                    let r = (done - d) as f64 / dt;
                    self.rate = if self.rate > 0.0 {
                        0.8 * self.rate + 0.2 * r
                    } else {
                        r
                    };
                    self.seen = Some((now, done));
                }
            }
            _ => {
                self.seen = Some((now, done));
                self.since = Some(now);
                self.rate = 0.0;
            }
        }
    }

    /// Seconds left, once the rate has had a few seconds to settle.
    fn left(&self, done: u64, total: u64) -> Option<f64> {
        let settled = self
            .since
            .is_some_and(|t| t.elapsed() >= Duration::from_secs(3));
        (settled && self.rate > 1.0 && total > done).then(|| (total - done) as f64 / self.rate)
    }
}

struct Row {
    root: gtk::Box,
    subtitle: gtk::Label,
    bar: gtk::ProgressBar,
    cancel: gtk::Button,
    start: gtk::Button,
}

struct Indicator {
    button: gtk::Button,
    ring: gtk::DrawingArea,
    count: gtk::Label,
    popover: gtk::Popover,
    header: gtk::Label,
    list: gtk::Box,
    rows: RefCell<HashMap<String, Row>>,
    speeds: RefCell<HashMap<String, Speed>>,
    items: RefCell<Vec<Item>>,
    state: Cell<Ring>,
    spin_tick: RefCell<Option<gtk::TickCallbackId>>,
    pinned: Cell<bool>,
    hovered: Cell<bool>,
    close_timer: RefCell<Option<glib::SourceId>>,
    conn: RefCell<Option<gio::DBusConnection>>,
}

pub fn indicator() -> gtk::Button {
    let ring = gtk::DrawingArea::builder()
        .content_width(RING_SIZE)
        .content_height(RING_SIZE)
        .valign(gtk::Align::Center)
        .build();
    let count = gtk::Label::builder()
        .css_classes(["downloads-count"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .visible(false)
        .build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&ring));
    overlay.add_overlay(&count);
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&overlay);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["flat", "downloads-indicator"])
        .visible(false)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&i18n::umoja("Downloads"))]);

    let header = gtk::Label::builder()
        .xalign(1.0)
        .hexpand(true)
        .css_classes(["dim-label", "numeric"])
        .build();
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.add_css_class("downloads-header");
    let title = gtk::Label::builder()
        .label(i18n::umoja("Downloads"))
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    head.append(&title);
    head.append(&header);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card.append(&head);
    card.append(&list);
    let popover = gtk::Popover::builder()
        .child(&card)
        .css_classes(["downloads-popover"])
        .has_arrow(false)
        // Hover card: no grab, so it never takes the keyboard or eats the
        // click that moves on to something else.
        .autohide(false)
        .build();
    popover.set_parent(&button);
    crate::menu_gap::below_bar(&popover);
    umoja_common::blur::blur_behind(&popover);

    let this = Rc::new(Indicator {
        button: button.clone(),
        ring: ring.clone(),
        count,
        popover: popover.clone(),
        header,
        list,
        rows: RefCell::default(),
        speeds: RefCell::default(),
        items: RefCell::default(),
        state: Cell::new(Ring::Done),
        spin_tick: RefCell::default(),
        pinned: Cell::new(false),
        hovered: Cell::new(false),
        close_timer: RefCell::default(),
        conn: RefCell::default(),
    });

    {
        let weak = Rc::downgrade(&this);
        ring.set_draw_func(move |area, cr, w, h| {
            if let Some(this) = weak.upgrade() {
                draw_ring(area, cr, w as f64, h as f64, this.state.get());
            }
        });
    }
    // Redraw in a new accent colour.
    {
        let ring = ring.downgrade();
        adw::StyleManager::default().connect_accent_color_rgba_notify(move |_| {
            if let Some(r) = ring.upgrade() {
                r.queue_draw();
            }
        });
    }

    // Hover opens the card; leaving both the ring and the card closes it.
    for widget in [button.upcast_ref::<gtk::Widget>(), popover.upcast_ref()] {
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&this);
        motion.connect_enter(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.hovered.set(true);
                this.cancel_close();
                this.open();
            }
        });
        let weak = Rc::downgrade(&this);
        motion.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                this.hovered.set(false);
                this.close_later();
            }
        });
        widget.add_controller(motion);
    }
    // A click (or tap) pins the card open until the next click.
    {
        let weak = Rc::downgrade(&this);
        button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            if this.pinned.get() && this.popover.is_visible() {
                this.pinned.set(false);
                this.popover.popdown();
            } else {
                this.pinned.set(true);
                this.open();
            }
        });
    }
    {
        let weak = Rc::downgrade(&this);
        popover.connect_closed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.pinned.set(false);
                this.button.unset_state_flags(gtk::StateFlags::CHECKED);
            }
        });
    }

    watch(&this);
    // The indicator lives as long as the bar.
    std::mem::forget(this);
    button
}

impl Indicator {
    fn open(&self) {
        if self.items.borrow().is_empty() {
            return;
        }
        self.button.set_state_flags(gtk::StateFlags::CHECKED, false);
        self.popover.popup();
    }

    fn cancel_close(&self) {
        if let Some(id) = self.close_timer.take() {
            id.remove();
        }
    }

    fn close_later(self: &Rc<Self>) {
        if self.pinned.get() {
            return;
        }
        self.cancel_close();
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(CLOSE_DELAY, move || {
            let Some(this) = weak.upgrade() else { return };
            this.close_timer.replace(None);
            if !this.hovered.get() && !this.pinned.get() {
                this.popover.popdown();
            }
        });
        self.close_timer.replace(Some(id));
    }

    fn update(self: &Rc<Self>, items: Vec<Item>) {
        // Rows for downloads that are gone.
        {
            let ids: Vec<&str> = items.iter().map(|i| i.0.as_str()).collect();
            let mut rows = self.rows.borrow_mut();
            rows.retain(|id, row| {
                let keep = ids.contains(&id.as_str());
                if !keep {
                    self.list.remove(&row.root);
                }
                keep
            });
            self.speeds
                .borrow_mut()
                .retain(|id, _| ids.contains(&id.as_str()));
        }
        for item in &items {
            if item.3 == "downloading" {
                self.speeds
                    .borrow_mut()
                    .entry(item.0.clone())
                    .or_default()
                    .update(item.4);
            }
            let mut rows = self.rows.borrow_mut();
            let row = rows.entry(item.0.clone()).or_insert_with(|| {
                let row = self.new_row(item);
                self.list.append(&row.root);
                row
            });
            self.fill_row(row, item);
        }

        let ring = ring_for(&items);
        let active = items
            .iter()
            .filter(|i| i.3 == "downloading" || i.3 == "waiting")
            .count();
        self.count.set_text(&active.to_string());
        self.count.set_visible(active > 1);
        self.header.set_text(&match ring {
            Ring::Progress(f) => format!("{} · {}%", active_label(active), (f * 100.0).floor()),
            Ring::Spinning => active_label(active),
            Ring::Paused => i18n::umoja("Paused"),
            Ring::Done => i18n::umoja("Ready"),
            Ring::Failed => i18n::umoja("Failed"),
        });
        self.set_ring(ring);
        let empty = items.is_empty();
        self.items.replace(items);
        if empty {
            self.pinned.set(false);
            self.popover.popdown();
            // Fade out rather than vanish; the check has had its moment.
            let button = self.button.downgrade();
            let weak = Rc::downgrade(self);
            crate::fade::fade(&self.button, 0.0, 400, move || {
                let (Some(b), Some(this)) = (button.upgrade(), weak.upgrade()) else {
                    return;
                };
                if this.items.borrow().is_empty() {
                    b.set_visible(false);
                }
                b.set_opacity(1.0);
            });
        } else {
            self.button.set_opacity(1.0);
            self.button.set_visible(true);
        }
    }

    fn set_ring(&self, ring: Ring) {
        self.state.set(ring);
        let spinning = ring == Ring::Spinning;
        let ticking = self.spin_tick.borrow().is_some();
        if spinning && !ticking {
            let id = self.ring.add_tick_callback(|w, _| {
                w.queue_draw();
                glib::ControlFlow::Continue
            });
            self.spin_tick.replace(Some(id));
        } else if !spinning && let Some(id) = self.spin_tick.take() {
            id.remove();
        }
        self.ring.queue_draw();
    }

    fn new_row(self: &Rc<Self>, item: &Item) -> Row {
        let icon_names: &[&str] = if item.1 == "voice" {
            &["audio-speakers-symbolic", "audio-volume-high-symbolic"]
        } else {
            &["applications-science-symbolic", "folder-download-symbolic"]
        };
        let icon = gtk::Image::from_gicon(&gio::ThemedIcon::from_names(icon_names));
        icon.add_css_class("download-icon");
        icon.set_valign(gtk::Align::Center);
        let title = gtk::Label::builder()
            .label(&item.2)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .hexpand(true)
            .css_classes(["download-title"])
            .build();
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .css_classes(["dim-label", "numeric", "download-subtitle"])
            .build();
        let bar = gtk::ProgressBar::builder()
            .css_classes(["download-progress"])
            .build();
        let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        text.append(&title);
        text.append(&subtitle);
        text.append(&bar);
        let cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "download-cancel"])
            .tooltip_text(i18n::umoja("Cancel download"))
            .valign(gtk::Align::Center)
            .build();
        let start = gtk::Button::builder()
            .label(i18n::umoja("Download Now"))
            .css_classes(["suggested-action", "download-start"])
            .valign(gtk::Align::Center)
            .build();
        let id = item.0.clone();
        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.call("CancelDownload", &id);
            }
        });
        let id = item.0.clone();
        let weak = Rc::downgrade(self);
        start.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.call("StartDownload", &id);
            }
        });
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        root.add_css_class("download-row");
        root.append(&icon);
        root.append(&text);
        root.append(&cancel);
        root.append(&start);
        Row {
            root,
            subtitle,
            bar,
            cancel,
            start,
        }
    }

    fn fill_row(&self, row: &Row, item: &Item) {
        let (_, _, _, state, done, total, cancellable) = item;
        let (done, total) = (*done, *total);
        let speeds = self.speeds.borrow();
        let left = speeds.get(&item.0).and_then(|s| s.left(done, total));
        let text = match state.as_str() {
            "downloading" if total > 0 => {
                let mut t = format!(
                    "{} of {}",
                    glib::format_size(done),
                    glib::format_size(total)
                );
                if let Some(s) = left {
                    t.push_str(" · ");
                    t.push_str(&time_left(s));
                }
                t
            }
            "downloading" if done > 0 => glib::format_size(done).to_string(),
            "downloading" => i18n::umoja("Starting…"),
            "waiting" => i18n::umoja("Waiting for an unmetered connection"),
            "done" => i18n::umoja("Ready to use"),
            _ => i18n::umoja("Couldn't download"),
        };
        row.subtitle.set_text(&text);
        let downloading = state == "downloading";
        row.bar.set_visible(downloading);
        if downloading {
            if total > 0 {
                row.bar
                    .set_fraction((done as f64 / total as f64).clamp(0.0, 1.0));
            } else {
                row.bar.pulse();
            }
        }
        row.cancel.set_visible(downloading && *cancellable);
        row.start.set_visible(state == "waiting");
        for (class, on) in [("done", state == "done"), ("failed", state == "failed")] {
            if on {
                row.root.add_css_class(class);
            } else {
                row.root.remove_css_class(class);
            }
        }
    }

    fn call(&self, method: &str, id: &str) {
        let Some(conn) = self.conn.borrow().clone() else {
            return;
        };
        let name = method.to_owned();
        conn.call(
            Some(dbus::OVERLAY_BUS_NAME),
            dbus::OVERLAY_OBJECT_PATH,
            dbus::OVERLAY_INTERFACE,
            method,
            Some(&(id,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |res| {
                if let Err(e) = res {
                    tracing::warn!(error = %e, method = name, "download request failed");
                }
            },
        );
    }
}

fn active_label(n: usize) -> String {
    format!("{n} {}", i18n::umoja("active"))
}

/// Follow DownloadsChanged, and fetch the list whenever umoja-overlay
/// (re)appears on the bus; clear it when the overlay goes away.
fn watch(this: &Rc<Indicator>) {
    let weak = Rc::downgrade(this);
    glib::spawn_future_local(async move {
        let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else {
            return;
        };
        let Some(this) = weak.upgrade() else { return };
        this.conn.replace(Some(conn.clone()));
        drop(this);
        let w = weak.clone();
        let id = conn.signal_subscribe(
            Some(dbus::OVERLAY_BUS_NAME),
            Some(dbus::OVERLAY_INTERFACE),
            Some("DownloadsChanged"),
            Some(dbus::OVERLAY_OBJECT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                if let (Some(this), Some((items,))) = (w.upgrade(), params.get::<(Vec<Item>,)>()) {
                    this.update(items);
                }
            },
        );
        std::mem::forget(id);
        let (appeared, vanished) = (weak.clone(), weak.clone());
        let watch = gio::bus_watch_name_on_connection(
            &conn,
            dbus::OVERLAY_BUS_NAME,
            gio::BusNameWatcherFlags::NONE,
            move |conn, _, _| {
                let weak = appeared.clone();
                conn.call(
                    Some(dbus::OVERLAY_BUS_NAME),
                    dbus::OVERLAY_OBJECT_PATH,
                    dbus::OVERLAY_INTERFACE,
                    "GetDownloads",
                    None,
                    Some(glib::VariantTy::new("(a(ssssttb))").expect("type")),
                    gio::DBusCallFlags::NONE,
                    2000,
                    gio::Cancellable::NONE,
                    move |res| {
                        // An older overlay without GetDownloads: no indicator.
                        let Ok(v) = res else { return };
                        if let (Some(this), Some((items,))) =
                            (weak.upgrade(), v.get::<(Vec<Item>,)>())
                        {
                            this.update(items);
                        }
                    },
                );
            },
            move |_, _| {
                if let Some(this) = vanished.upgrade() {
                    this.update(Vec::new());
                }
            },
        );
        std::mem::forget(watch);
    });
}

/// The ring: a track, the accent-coloured progress (or a spinning arc),
/// and a glyph in the middle: an arrow, a check, a pause sign or "!".
fn draw_ring(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: f64, h: f64, ring: Ring) {
    use std::f64::consts::{FRAC_PI_2, TAU};
    let (cx, cy) = (w / 2.0, h / 2.0);
    let r = w.min(h) / 2.0 - 1.5;
    #[allow(deprecated)]
    let fg = area.color();
    let accent = adw::StyleManager::default().accent_color_rgba();
    let set = |c: &gdk::RGBA, a: f64| {
        cr.set_source_rgba(
            c.red() as f64,
            c.green() as f64,
            c.blue() as f64,
            c.alpha() as f64 * a,
        )
    };
    cr.set_line_width(2.0);
    cr.set_line_cap(gtk::cairo::LineCap::Round);

    // Track.
    set(&fg, 0.25);
    cr.arc(cx, cy, r, 0.0, TAU);
    let _ = cr.stroke();

    // Progress.
    match ring {
        Ring::Progress(f) if f > 0.0 => {
            set(&accent, 1.0);
            cr.arc(cx, cy, r, -FRAC_PI_2, -FRAC_PI_2 + TAU * f.min(1.0));
            let _ = cr.stroke();
        }
        Ring::Spinning => {
            let t = area
                .frame_clock()
                .map(|c| c.frame_time() as f64 / 1_000_000.0)
                .unwrap_or(0.0);
            let start = (t / 1.1).fract() * TAU;
            set(&accent, 1.0);
            cr.arc(cx, cy, r, start, start + TAU * 0.28);
            let _ = cr.stroke();
        }
        Ring::Done => {
            set(&accent, 1.0);
            cr.arc(cx, cy, r, 0.0, TAU);
            let _ = cr.stroke();
        }
        _ => {}
    }

    // Glyph, in the bar's foreground colour.
    set(&fg, 1.0);
    cr.set_line_width(1.6);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    let s = w.min(h) / 16.0;
    let p = |x: f64, y: f64| (cx + (x - 8.0) * s, cy + (y - 8.0) * s);
    let line = |pts: &[(f64, f64)]| {
        for (i, &(x, y)) in pts.iter().enumerate() {
            let (x, y) = p(x, y);
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        let _ = cr.stroke();
    };
    match ring {
        Ring::Done => line(&[(5.3, 8.2), (7.2, 10.1), (10.8, 6.3)]),
        Ring::Paused => {
            line(&[(6.6, 5.6), (6.6, 10.4)]);
            line(&[(9.4, 5.6), (9.4, 10.4)]);
        }
        Ring::Failed => {
            line(&[(8.0, 4.8), (8.0, 8.6)]);
            line(&[(8.0, 11.0), (8.0, 11.1)]);
        }
        _ => {
            line(&[(8.0, 4.6), (8.0, 10.6)]);
            line(&[(5.6, 8.4), (8.0, 10.8), (10.4, 8.4)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(state: &str, done: u64, total: u64) -> Item {
        (
            "id".into(),
            "model".into(),
            "t".into(),
            state.into(),
            done,
            total,
            true,
        )
    }

    #[test]
    fn ring_states() {
        assert_eq!(
            ring_for(&[item("downloading", 25, 100)]),
            Ring::Progress(0.25)
        );
        // Bytes-weighted: a small download barely moves a big one.
        assert_eq!(
            ring_for(&[item("downloading", 0, 900), item("downloading", 100, 100)]),
            Ring::Progress(0.1)
        );
        // A finished one still counts, so the ring doesn't jump back.
        assert_eq!(
            ring_for(&[item("downloading", 0, 100), item("done", 100, 100)]),
            Ring::Progress(0.5)
        );
        assert_eq!(
            ring_for(&[item("downloading", 10, 0), item("downloading", 5, 100)]),
            Ring::Spinning
        );
        assert_eq!(ring_for(&[item("waiting", 0, 0)]), Ring::Paused);
        assert_eq!(ring_for(&[item("done", 1, 1)]), Ring::Done);
        assert_eq!(ring_for(&[item("failed", 0, 1)]), Ring::Failed);
        assert_eq!(
            ring_for(&[item("failed", 0, 1), item("done", 1, 1)]),
            Ring::Done
        );
    }

    #[test]
    fn times() {
        assert_eq!(time_left(0.2), "1 s left");
        assert_eq!(time_left(42.0), "42 s left");
        assert_eq!(time_left(180.0), "3 min left");
        assert_eq!(time_left(3900.0), "1 h 5 min left");
    }
}
