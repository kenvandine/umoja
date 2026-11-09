//! notify-osd style banners: dark, translucent, rounded bubbles stacked in
//! the top center under the clock, as GNOME places banners, fading in and out.
//!
//! Behaviour follows GNOME: clicking a bubble runs its default action (or
//! raises the app), action buttons are shown, hovering keeps it up, and a
//! close button appears on hover. Critical bubbles stay until dismissed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Weak;

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use super::{Center, CloseReason, Icon, Note, Urgency, banner_timeout};

/// notify-osd showed one bubble at a time; GNOME one banner. A short stack
/// keeps bursts readable without covering the screen.
const MAX_BUBBLES: usize = 3;
const BUBBLE_WIDTH: i32 = 320;
const FADE_MS: u32 = 250;
/// Room for the bubbles plus their shadows. Fixed, see `Bubbles::new`.
const SURFACE_WIDTH: i32 = BUBBLE_WIDTH + 40;
const SURFACE_HEIGHT: i32 = 640;

struct Bubble {
    note: Note,
    revealer: gtk::Revealer,
    timer: Option<glib::SourceId>,
}

pub struct Bubbles {
    _window: gtk::Window,
    column: gtk::Box,
    items: RefCell<HashMap<u32, Bubble>>,
    /// Ids top to bottom, newest first.
    order: RefCell<Vec<u32>>,
    center: Weak<Center>,
}

impl Bubbles {
    pub fn new(app: &gtk::Application, center: Weak<Center>) -> Self {
        let window = gtk::Window::builder()
            .application(app)
            .css_classes(["umoja-notifications"])
            .decorated(false)
            .resizable(false)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-notifications");
        umoja_common::blur::blur_behind(&window);
        window.set_keyboard_mode(KeyboardMode::None);
        // A fixed-size surface at the top center, under the clock and the
        // notification list, as GNOME places its banners. Anchored to the
        // top edge only, so the compositor centers it horizontally. Mir
        // places a layer surface from its size when it maps, but doesn't
        // move it when it grows afterwards, so a surface that grew with each
        // new bubble drifted off center (or off the edge). The empty part
        // lets clicks through: the input region is just the bubbles (see
        // below).
        window.set_anchor(Edge::Top, true);
        window.set_size_request(SURFACE_WIDTH, SURFACE_HEIGHT);
        // Stay out of the exclusive-zone layout: with the default zone of 0,
        // Mir re-lays out the panel around this surface whenever it maps,
        // and the panel came back short. Clear the panel by hand instead.
        window.set_exclusive_zone(-1);
        window.set_margin(Edge::Top, crate::PANEL_HEIGHT + 8);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_valign(gtk::Align::Start);
        column.set_halign(gtk::Align::Center);
        window.set_child(Some(&column));

        // Keep the input region to the bubbles themselves, so the rest of
        // the transparent strip doesn't swallow clicks meant for windows.
        {
            let column = column.clone();
            window.connect_realize(move |w| {
                let Some(clock) = w.frame_clock() else { return };
                let w = w.clone();
                let column = column.clone();
                clock.connect_after_paint(move |_| {
                    let Some(surface) = w.surface() else { return };
                    let rect = column
                        .compute_bounds(&w)
                        .map(|b| {
                            gtk::cairo::RectangleInt::new(
                                b.x().floor() as i32,
                                b.y().floor() as i32,
                                b.width().ceil() as i32,
                                b.height().ceil() as i32,
                            )
                        })
                        .unwrap_or_else(|| gtk::cairo::RectangleInt::new(0, 0, 0, 0));
                    surface.set_input_region(&gtk::cairo::Region::create_rectangle(&rect));
                });
            });
        }
        // Mapped once and never hidden: Mir places a re-mapped surface in the
        // wrong place, and with no bubbles the input region is empty so
        // the transparent surface doesn't get in the way.
        window.present();
        Self {
            _window: window,
            column,
            items: RefCell::default(),
            order: RefCell::default(),
            center,
        }
    }

    pub fn contains(&self, id: u32) -> bool {
        self.items.borrow().contains_key(&id)
    }

    pub fn note(&self, id: u32) -> Option<Note> {
        self.items.borrow().get(&id).map(|b| b.note.clone())
    }

    pub fn show(&self, note: &Note) {
        let id = note.id;
        // Replacing an existing bubble updates it in place, no new fade.
        let replacing = self.items.borrow().contains_key(&id);
        let content = self.build(note);
        if replacing {
            let mut items = self.items.borrow_mut();
            let b = items.get_mut(&id).expect("present");
            b.revealer.set_child(Some(&content));
            b.note = note.clone();
            if let Some(t) = b.timer.take() {
                t.remove();
            }
        } else {
            let revealer = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::Crossfade)
                .transition_duration(FADE_MS)
                .child(&content)
                .build();
            self.column.prepend(&revealer);
            self.order.borrow_mut().insert(0, id);
            self.items.borrow_mut().insert(
                id,
                Bubble {
                    note: note.clone(),
                    revealer: revealer.clone(),
                    timer: None,
                },
            );
            glib::idle_add_local_once(move || revealer.set_reveal_child(true));
        }
        self.arm_timer(id, banner_timeout(note));

        // Too many: retire the oldest non-critical bubble (it stays in the
        // message list).
        let overflow: Vec<u32> = {
            let order = self.order.borrow();
            let items = self.items.borrow();
            order
                .iter()
                .rev()
                .filter(|i| {
                    items
                        .get(i)
                        .is_some_and(|b| b.note.urgency != Urgency::Critical)
                })
                .take(order.len().saturating_sub(MAX_BUBBLES))
                .copied()
                .collect()
        };
        for old in overflow {
            self.remove(old);
        }
    }

    fn arm_timer(&self, id: u32, after: Option<std::time::Duration>) {
        let Some(after) = after else { return };
        let center = self.center.clone();
        let src = glib::timeout_add_local_once(after, move || {
            if let Some(c) = center.upgrade() {
                if let Some(b) = c.bubbles.items.borrow_mut().get_mut(&id) {
                    b.timer = None;
                }
                c.bubbles.remove(id);
            }
        });
        if let Some(b) = self.items.borrow_mut().get_mut(&id) {
            if let Some(old) = b.timer.replace(src) {
                old.remove();
            }
        }
    }

    /// Fade a bubble out. Returns whether it was showing.
    pub fn remove(&self, id: u32) -> bool {
        let Some(mut b) = self.items.borrow_mut().remove(&id) else {
            return false;
        };
        self.order.borrow_mut().retain(|i| *i != id);
        if let Some(t) = b.timer.take() {
            t.remove();
        }
        let revealer = b.revealer;
        revealer.set_reveal_child(false);
        let column = self.column.clone();
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(FADE_MS as u64),
            move || {
                column.remove(&revealer);
            },
        );
        true
    }

    /// Do Not Disturb was switched on: only critical bubbles stay.
    pub fn hide_all_but_critical(&self) {
        let ids: Vec<u32> = self
            .items
            .borrow()
            .iter()
            .filter(|(_, b)| b.note.urgency != Urgency::Critical)
            .map(|(i, _)| *i)
            .collect();
        for id in ids {
            self.remove(id);
        }
    }

    fn build(&self, note: &Note) -> gtk::Widget {
        let id = note.id;
        let bubble = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        bubble.add_css_class("osd-bubble");
        if note.urgency == Urgency::Critical {
            bubble.add_css_class("critical");
        }
        bubble.set_size_request(BUBBLE_WIDTH, -1);

        if let Some(icon) = icon_widget(note.icon.as_ref(), 48) {
            icon.set_valign(gtk::Align::Start);
            icon.add_css_class("osd-icon");
            bubble.append(&icon);
        }

        let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        if !note.summary.is_empty() {
            let title = gtk::Label::builder()
                .label(&note.summary)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .lines(2)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(28)
                .css_classes(["osd-title"])
                .build();
            text.append(&title);
        }
        if !note.body.is_empty() {
            let body = gtk::Label::builder()
                .label(&note.body)
                .use_markup(true)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .lines(6)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(32)
                .css_classes(["osd-body"])
                .build();
            text.append(&body);
        }
        if let Some(v) = note.value {
            let gauge = gtk::ProgressBar::builder()
                .fraction(v as f64 / 100.0)
                .css_classes(["osd-gauge"])
                .build();
            gauge.set_valign(gtk::Align::Center);
            text.append(&gauge);
        }
        let buttons: Vec<_> = note.buttons().cloned().collect();
        // Quick reply: opens the message list at a reply field with replies
        // drafted by the assistant (keyboard input can't reach a banner).
        let reply = note.can_reply() && !buttons.iter().any(|(k, _)| k == "reply");
        if !buttons.is_empty() || reply {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_homogeneous(true);
            row.add_css_class("osd-actions");
            for (key, label) in buttons {
                let b = gtk::Button::with_label(&label);
                let center = self.center.clone();
                b.connect_clicked(move |_| {
                    if let Some(c) = center.upgrade() {
                        c.invoke(id, &key);
                    }
                });
                row.append(&b);
            }
            if reply {
                let b = gtk::Button::with_label(&umoja_common::i18n::shell("Reply"));
                b.add_css_class("osd-reply");
                let center = self.center.clone();
                b.connect_clicked(move |_| {
                    if let Some(c) = center.upgrade() {
                        c.request_reply(id);
                    }
                });
                row.append(&b);
            }
            text.append(&row);
        }
        bubble.append(&text);

        // Close button, shown on hover (GNOME); notify-osd had none.
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("osd-close");
        close.set_valign(gtk::Align::Start);
        close.set_halign(gtk::Align::End);
        close.set_tooltip_text(Some(&umoja_common::i18n::gtk("Close")));
        {
            let center = self.center.clone();
            close.connect_clicked(move |_| {
                if let Some(c) = center.upgrade() {
                    c.close(id, CloseReason::Dismissed);
                }
            });
        }
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&bubble));
        overlay.add_overlay(&close);

        // Clicking the bubble = default action / raise the app.
        let click = gtk::GestureClick::new();
        {
            let center = self.center.clone();
            click.connect_released(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(c) = center.upgrade() {
                    c.bubbles.remove(id);
                    c.activate(id);
                }
            });
        }
        bubble.add_controller(click);

        // Hovering keeps the bubble up, like GNOME banners.
        let motion = gtk::EventControllerMotion::new();
        {
            let center = self.center.clone();
            motion.connect_enter(move |_, _, _| {
                if let Some(c) = center.upgrade() {
                    if let Some(b) = c.bubbles.items.borrow_mut().get_mut(&id) {
                        if let Some(t) = b.timer.take() {
                            t.remove();
                        }
                    }
                }
            });
        }
        {
            let center = self.center.clone();
            motion.connect_leave(move |_| {
                if let Some(c) = center.upgrade() {
                    let crit = c
                        .bubbles
                        .items
                        .borrow()
                        .get(&id)
                        .is_none_or(|b| b.note.urgency == Urgency::Critical);
                    if !crit {
                        c.bubbles
                            .arm_timer(id, Some(std::time::Duration::from_millis(1500)));
                    }
                }
            });
        }
        overlay.add_controller(motion);
        overlay.upcast()
    }
}

pub fn icon_widget(icon: Option<&Icon>, size: i32) -> Option<gtk::Image> {
    let image = match icon? {
        Icon::Name(n) => gtk::Image::from_icon_name(n),
        Icon::File(p) => gtk::Image::from_file(p),
        Icon::Texture(t) => gtk::Image::from_paintable(Some(t)),
    };
    image.set_pixel_size(size);
    Some(image)
}
