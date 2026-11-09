//! The on-screen display for volume, brightness and other hardware keys,
//! a copy of gnome-shell's `OsdWindow` (js/ui/osdWindow.js).
//!
//! A dark pill centred at the bottom of each monitor, 4em above the edge:
//! a 32px icon, then an optional bold label above a 6px level bar. It
//! fades in over 100ms, the level eases to its new value over 100ms, and
//! it fades out 1.5s after the last update. A level above 100% (volume
//! over-amplification) turns the overflow red past a separator at 100%,
//! like gnome-shell's BarLevel.
//!
//! Each monitor gets a transparent layer surface along its bottom edge,
//! sized to the monitor's full width (Mir doesn't stretch a surface anchored
//! to both sides) with an empty input region so it never takes clicks. The
//! pill is centred inside it. The surface is mapped when the OSD shows and
//! unmapped once it has faded out (keeping it mapped left the fade's last
//! frames on screen). Mir places a re-mapped layer surface wrongly, so the
//! next show uses a fresh surface.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use libadwaita::prelude::AnimationExt;

/// osdWindow.js HIDE_TIMEOUT, FADE_TIME and LEVEL_ANIMATION_TIME.
const HIDE_TIMEOUT: Duration = Duration::from_millis(1500);
const FADE_MS: u32 = 100;
const LEVEL_MS: u32 = 100;
/// Room for the pill and its 4em bottom margin. Fixed, see the module docs.
const SURFACE_HEIGHT: i32 = 200;
/// _osd.scss: -barlevel-height and -barlevel-overdrive-separator-width.
const BAR_HEIGHT: f64 = 6.0;
const SEPARATOR_WIDTH: f64 = 3.0;
/// BarLevel's overdrive-start: the separator sits at 100%.
const OVERDRIVE_START: f64 = 1.0;

/// What to show, as in gnome-shell's `ShowOSD`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    pub icon: String,
    pub label: Option<String>,
    /// 0..=max_level; `None` hides the bar.
    pub level: Option<f64>,
    pub max_level: f64,
    /// Show only on the monitor with this connector (e.g. "eDP-1").
    pub connector: Option<String>,
}

impl Request {
    pub fn icon(icon: &str) -> Self {
        Self {
            icon: icon.into(),
            max_level: 1.0,
            ..Default::default()
        }
    }

    pub fn level(mut self, level: f64, max_level: f64) -> Self {
        self.max_level = if max_level > 0.0 { max_level } else { 1.0 };
        self.level = Some(level.clamp(0.0, self.max_level));
        self
    }

    pub fn label(mut self, label: Option<String>) -> Self {
        self.label = label.filter(|l| !l.is_empty());
        self
    }

    pub fn connector(mut self, connector: Option<String>) -> Self {
        self.connector = connector;
        self
    }

    /// Parses `org.gnome.Shell.ShowOSD`'s dictionary: icon (a serialized
    /// GIcon), label, level, max_level and connector.
    pub fn from_dict(dict: &glib::VariantDict) -> Option<Self> {
        let icon: String = dict.lookup("icon").ok().flatten()?;
        let mut r = Self::icon(&icon);
        r.label = dict
            .lookup::<String>("label")
            .ok()
            .flatten()
            .filter(|l| !l.is_empty());
        r.connector = dict.lookup::<String>("connector").ok().flatten();
        let max = dict
            .lookup::<f64>("max_level")
            .ok()
            .flatten()
            .unwrap_or(1.0);
        // gnome-settings-daemon sends -1 for "no level".
        if let Some(level) = dict
            .lookup::<f64>("level")
            .ok()
            .flatten()
            .filter(|l| *l >= 0.0)
        {
            r = r.level(level, max);
        }
        Some(r)
    }
}

/// One monitor's OSD.
struct Window {
    monitor: gdk::Monitor,
    geometry: gdk::Rectangle,
    window: gtk::Window,
    pill: gtk::Box,
    icon: gtk::Image,
    label: gtk::Label,
    bar: gtk::DrawingArea,
    shown: Cell<bool>,
    hiding: Cell<bool>,
    /// Unmapped after fading out; replaced on the next show.
    retired: Cell<bool>,
    level: Rc<Cell<f64>>,
    max_level: Rc<Cell<f64>>,
    level_anim: RefCell<Option<adw::TimedAnimation>>,
    fade: RefCell<Option<adw::TimedAnimation>>,
    hide_timer: RefCell<Option<glib::SourceId>>,
}

impl Window {
    fn new(app: &gtk::Application, monitor: &gdk::Monitor) -> Rc<Self> {
        let window = gtk::Window::builder()
            .application(app)
            .css_classes(["umoja-osd"])
            .decorated(false)
            .resizable(false)
            .can_focus(false)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-osd");
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_monitor(monitor);
        window.set_anchor(Edge::Bottom, true);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, true);
        window.set_exclusive_zone(-1);
        // Mir doesn't stretch a surface anchored to both sides, so ask for
        // the monitor's width outright. A new geometry gets a new window.
        let width = monitor.geometry().width().max(1);
        window.set_default_size(width, SURFACE_HEIGHT);
        window.set_size_request(width, SURFACE_HEIGHT);

        let pill = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        pill.add_css_class("osd-window");
        pill.set_halign(gtk::Align::Center);
        pill.set_valign(gtk::Align::End);
        pill.set_opacity(0.0);
        pill.set_can_target(false);

        let icon = gtk::Image::new();
        icon.set_pixel_size(32);
        icon.set_valign(gtk::Align::Center);
        pill.append(&icon);

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
        vbox.set_valign(gtk::Align::Center);
        let label = gtk::Label::new(None);
        label.add_css_class("osd-label");
        label.set_justify(gtk::Justification::Center);
        label.set_visible(false);
        vbox.append(&label);

        let level = Rc::new(Cell::new(0.0));
        let max_level = Rc::new(Cell::new(1.0));
        let bar = gtk::DrawingArea::new();
        bar.add_css_class("osd-level");
        bar.set_content_width(160);
        bar.set_content_height(BAR_HEIGHT as i32);
        bar.set_visible(false);
        {
            let (level, max_level) = (level.clone(), max_level.clone());
            bar.set_draw_func(move |area, cr, w, h| {
                draw_bar(area, cr, w as f64, h as f64, level.get(), max_level.get());
            });
        }
        vbox.append(&bar);
        pill.append(&vbox);
        window.set_child(Some(&pill));

        // Never take input: the surface spans the monitor's bottom strip.
        window.connect_realize(|w| {
            if let Some(surface) = w.surface() {
                surface.set_input_region(&gtk::cairo::Region::create());
            }
        });
        // Mapped on first show, unmapped once faded out, then replaced.

        let this = Rc::new(Self {
            monitor: monitor.clone(),
            geometry: monitor.geometry(),
            window,
            pill,
            icon,
            label,
            bar,
            shown: Cell::new(false),
            hiding: Cell::new(false),
            retired: Cell::new(false),
            level,
            max_level,
            level_anim: RefCell::default(),
            fade: RefCell::default(),
            hide_timer: RefCell::default(),
        });
        // The input region has to be set again once the surface exists and
        // after every resize; GTK resets it.
        {
            let weak = Rc::downgrade(&this);
            this.window.connect_map(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.clear_input();
                }
            });
        }
        this
    }

    fn clear_input(&self) {
        if let Some(surface) = self.window.surface() {
            surface.set_input_region(&gtk::cairo::Region::create());
        }
    }

    fn show(self: &Rc<Self>, r: &Request) {
        let gicon = gio::Icon::for_string(&r.icon).ok();
        if gicon.is_none() && r.icon.is_empty() {
            return;
        }
        match gicon {
            Some(g) => self.icon.set_from_gicon(&g),
            None => self.icon.set_icon_name(Some(&r.icon)),
        }
        self.label.set_visible(r.label.is_some());
        self.label.set_text(r.label.as_deref().unwrap_or(""));
        self.max_level.set(r.max_level);
        self.bar.set_visible(r.level.is_some());
        if let Some(level) = r.level {
            self.set_level(level);
        }
        self.clear_input();

        // Fade in when hidden or on the way out.
        self.pill.set_visible(true);
        if !self.window.is_visible() {
            self.window.present();
        }
        let was_shown = self.shown.replace(true);
        if !was_shown | self.hiding.replace(false) {
            self.fade_to(1.0, None);
        }
        if let Some(id) = self.hide_timer.take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(HIDE_TIMEOUT, move || {
            if let Some(t) = weak.upgrade() {
                t.hide_timer.take();
                t.hide();
            }
        });
        self.hide_timer.replace(Some(id));
    }

    fn set_level(&self, level: f64) {
        if let Some(a) = self.level_anim.take() {
            a.pause();
        }
        if !self.shown.get() {
            self.level.set(level);
            self.bar.queue_draw();
            return;
        }
        // Ease the bar like gnome-shell (EASE_OUT_QUAD, 100ms).
        let (cell, bar) = (self.level.clone(), self.bar.clone());
        let target = adw::CallbackAnimationTarget::new(move |v| {
            cell.set(v);
            bar.queue_draw();
        });
        let anim = adw::TimedAnimation::new(&self.bar, self.level.get(), level, LEVEL_MS, target);
        anim.set_easing(adw::Easing::EaseOutQuad);
        anim.play();
        self.level_anim.replace(Some(anim));
    }

    /// Hide now (another monitor got the OSD).
    fn cancel(self: &Rc<Self>) {
        if let Some(id) = self.hide_timer.take() {
            id.remove();
            self.hide();
        }
    }

    fn hide(self: &Rc<Self>) {
        self.hiding.set(true);
        let weak = Rc::downgrade(self);
        self.fade_to(
            0.0,
            Some(Box::new(move || {
                if let Some(t) = weak.upgrade() {
                    t.hiding.set(false);
                    t.shown.set(false);
                    // Stop drawing it at all, so no faint last frame of
                    // the fade lingers.
                    t.pill.set_visible(false);
                    // And unmap the surface: on the laptop Mir stopped
                    // sending frame callbacks partway through the fade, so
                    // the last frame stayed on screen at ~15% opacity.
                    t.window.set_visible(false);
                    t.retired.set(true);
                    t.label.set_visible(false);
                    t.bar.set_visible(false);
                }
            })),
        );
    }

    fn fade_to(&self, to: f64, done: Option<Box<dyn Fn()>>) {
        if let Some(a) = self.fade.take() {
            a.pause();
        }
        let target = adw::PropertyAnimationTarget::new(&self.pill, "opacity");
        let anim = adw::TimedAnimation::new(&self.pill, self.pill.opacity(), to, FADE_MS, target);
        anim.set_easing(adw::Easing::EaseOutQuad);
        if let Some(done) = done {
            anim.connect_done(move |_| done());
        }
        anim.play();
        self.fade.replace(Some(anim));
    }
}

/// gnome-shell's BarLevel without a handle (LTR; RTL mirrors it).
fn draw_bar(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: f64,
    height: f64,
    value: f64,
    max: f64,
) {
    use std::f64::consts::TAU;
    let rtl = area.direction() == gtk::TextDirection::Rtl;
    if rtl {
        cr.translate(width, 0.0);
        cr.scale(-1.0, 1.0);
    }
    #[allow(deprecated)]
    let fg = area.color();
    let style = adw::StyleManager::default();
    let dark = style.is_dark();
    let track = gdk::RGBA::new(
        fg.red(),
        fg.green(),
        fg.blue(),
        if dark { 0.16 } else { 0.3 },
    );
    // The level follows the GNOME accent color, like the Quick Settings
    // sliders; the overdrive separator stays in the foreground color.
    let fill = style.accent_color_rgba();
    // $destructive_color: $red_4 (dark) / $red_3 (light).
    let overdrive = if dark {
        gdk::RGBA::new(
            0xc0 as f32 / 255.0,
            0x1c as f32 / 255.0,
            0x28 as f32 / 255.0,
            1.0,
        )
    } else {
        gdk::RGBA::new(
            0xe0 as f32 / 255.0,
            0x1b as f32 / 255.0,
            0x24 as f32 / 255.0,
            1.0,
        )
    };
    let set = |c: &gdk::RGBA| {
        cr.set_source_rgba(
            c.red() as f64,
            c.green() as f64,
            c.blue() as f64,
            c.alpha() as f64,
        )
    };

    let radius = width.min(BAR_HEIGHT) / 2.0;
    let top = (height - BAR_HEIGHT) / 2.0;
    let bottom = (height + BAR_HEIGHT) / 2.0;
    let mid = height / 2.0;
    let span = width - 2.0 * radius;
    let end_x = if max > 0.0 {
        radius + span * (value / max)
    } else {
        0.0
    };
    let sep_x = radius + span * (OVERDRIVE_START / max);
    let overdrive_active = max > OVERDRIVE_START;
    let sep_w = if overdrive_active {
        SEPARATOR_WIDTH
    } else {
        0.0
    };

    // Background, from the end of the fill to the right end cap.
    cr.arc(width - radius, mid, radius, TAU * 0.75, TAU * 0.25);
    cr.line_to(end_x, bottom);
    cr.line_to(end_x, top);
    cr.line_to(width - radius, top);
    set(&track);
    let _ = cr.fill();

    // Normal fill, up to the separator.
    let x = end_x.min(sep_x - sep_w / 2.0);
    cr.arc(radius, mid, radius, TAU * 0.25, TAU * 0.75);
    cr.line_to(x, top);
    cr.line_to(x, bottom);
    cr.line_to(radius, bottom);
    set(if value > 0.0 { &fill } else { &track });
    let _ = cr.fill();

    // Overdrive fill, past the separator.
    if value > OVERDRIVE_START {
        let x = end_x.min(sep_x) + sep_w / 2.0;
        cr.rectangle(x, top, end_x - x, BAR_HEIGHT);
        set(&overdrive);
        let _ = cr.fill();
    }

    // Rounded end of the fill.
    if value > 0.0 {
        set(if value <= OVERDRIVE_START {
            &fill
        } else {
            &overdrive
        });
        cr.arc(end_x, mid, radius, TAU * 0.75, TAU * 0.25);
        cr.line_to(end_x.floor(), bottom);
        cr.line_to(end_x.floor(), top);
        cr.line_to(end_x, top);
        let _ = cr.fill();
    }

    if overdrive_active {
        cr.rectangle(sep_x - sep_w / 2.0, top, sep_w, BAR_HEIGHT);
        set(if value <= OVERDRIVE_START {
            &fg
        } else {
            &track
        });
        let _ = cr.fill();
    }
}

/// The OSD on every monitor (gnome-shell's OsdWindowManager).
pub struct Osd {
    app: gtk::Application,
    windows: RefCell<Vec<Rc<Window>>>,
}

thread_local! {
    static OSD: RefCell<Option<Rc<Osd>>> = const { RefCell::new(None) };
}

pub fn init(app: &impl IsA<gtk::Application>) {
    let osd = Rc::new(Osd {
        app: app.as_ref().clone(),
        windows: RefCell::default(),
    });
    osd.sync_monitors();
    if let Some(display) = gdk::Display::default() {
        let weak = Rc::downgrade(&osd);
        display.monitors().connect_items_changed(move |_, _, _, _| {
            if let Some(o) = weak.upgrade() {
                o.sync_monitors();
            }
        });
    }
    OSD.with(|o| o.replace(Some(osd)));
}

/// Show the OSD. Does nothing before `init`.
pub fn show(r: Request) {
    tracing::debug!(?r, "osd");
    if let Some(osd) = OSD.with(|o| o.borrow().clone()) {
        osd.show(&r);
    }
}

impl Osd {
    fn sync_monitors(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let monitors: Vec<gdk::Monitor> = display
            .monitors()
            .iter::<gdk::Monitor>()
            .filter_map(Result::ok)
            .collect();
        let mut windows = self.windows.borrow_mut();
        windows.retain(|w| {
            let keep = monitors.contains(&w.monitor) && w.monitor.geometry() == w.geometry;
            if !keep {
                w.window.destroy();
            }
            keep
        });
        for m in &monitors {
            if !windows.iter().any(|w| &w.monitor == m) {
                windows.push(Window::new(&self.app, m));
                // Resolution or scale changes: rebuild at the new size.
                m.connect_geometry_notify(|_| {
                    glib::idle_add_local_once(|| {
                        if let Some(osd) = OSD.with(|o| o.borrow().clone()) {
                            osd.sync_monitors();
                        }
                    });
                });
            }
        }
    }

    fn show(&self, r: &Request) {
        // A surface that faded out was unmapped. Mir places a re-mapped
        // layer surface wrongly (this one came back ~280px too high), so
        // build a fresh one, which Mir places right on its first map.
        let shows_here = |w: &Window| match r.connector.as_deref() {
            Some(c) => w.monitor.connector().as_deref() == Some(c),
            None => true,
        };
        for w in self.windows.borrow_mut().iter_mut() {
            if w.retired.get() && shows_here(w) {
                w.window.destroy();
                let monitor = w.monitor.clone();
                *w = Window::new(&self.app, &monitor);
            }
        }
        let windows = self.windows.borrow().clone();
        let target = r.connector.as_deref().and_then(|c| {
            windows
                .iter()
                .position(|w| w.monitor.connector().as_deref() == Some(c))
        });
        for (i, w) in windows.iter().enumerate() {
            match target {
                Some(t) if t != i => w.cancel(),
                _ => w.show(r),
            }
        }
    }
}

/// gnome-settings-daemon's volume icons (get_icon_name_for_volume).
pub fn volume_icon(is_mic: bool, muted: bool, volume: f64) -> &'static str {
    const OUT: [&str; 5] = [
        "audio-volume-muted-symbolic",
        "audio-volume-low-symbolic",
        "audio-volume-medium-symbolic",
        "audio-volume-high-symbolic",
        "audio-volume-overamplified-symbolic",
    ];
    const MIC: [&str; 4] = [
        "microphone-sensitivity-muted-symbolic",
        "microphone-sensitivity-low-symbolic",
        "microphone-sensitivity-medium-symbolic",
        "microphone-sensitivity-high-symbolic",
    ];
    let n = if muted {
        0
    } else {
        let n = (3.0 * volume - 1e-9).ceil() as i64;
        if n < 1 {
            1
        } else if n > 3 && !is_mic {
            4
        } else {
            n.min(3)
        }
    } as usize;
    if is_mic { MIC[n] } else { OUT[n] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_icons_match_gsd() {
        assert_eq!(volume_icon(false, true, 0.5), "audio-volume-muted-symbolic");
        assert_eq!(volume_icon(false, false, 0.0), "audio-volume-low-symbolic");
        assert_eq!(volume_icon(false, false, 0.2), "audio-volume-low-symbolic");
        assert_eq!(
            volume_icon(false, false, 1.0 / 3.0),
            "audio-volume-low-symbolic"
        );
        assert_eq!(
            volume_icon(false, false, 0.5),
            "audio-volume-medium-symbolic"
        );
        assert_eq!(volume_icon(false, false, 1.0), "audio-volume-high-symbolic");
        assert_eq!(
            volume_icon(false, false, 1.2),
            "audio-volume-overamplified-symbolic"
        );
        assert_eq!(
            volume_icon(true, false, 1.2),
            "microphone-sensitivity-high-symbolic"
        );
        assert_eq!(
            volume_icon(true, true, 1.0),
            "microphone-sensitivity-muted-symbolic"
        );
    }

    #[test]
    fn request_clamps() {
        let r = Request::icon("x").level(2.0, 1.5);
        assert_eq!(r.level, Some(1.5));
        let r = Request::icon("x").level(0.5, -1.0);
        assert_eq!((r.level, r.max_level), (Some(0.5), 1.0));
    }
}
