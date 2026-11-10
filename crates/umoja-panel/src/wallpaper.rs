//! Desktop background: one layer-shell surface per monitor on the
//! background layer, showing the GNOME wallpaper
//! (`org.gnome.desktop.background`), as gnome-shell does.
//!
//! `picture-uri-dark` is used when `color-scheme` is `prefer-dark`, and
//! `picture-options` maps onto how the image is fitted. Changing the
//! wallpaper in Settings › Appearance, the colour scheme, or plugging in a
//! monitor updates the surfaces live.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::{gdk, gio, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use umoja_common::gsettings as gs;

pub struct Wallpaper {
    app: gtk::Application,
    windows: RefCell<Vec<gtk::Window>>,
    /// The first surfaces fade up from black after login; later rebuilds
    /// (wallpaper or monitor changes) swap in place.
    faded_in: std::cell::Cell<bool>,
    _watches: RefCell<Vec<gio::Settings>>,
}

impl Wallpaper {
    pub fn start(app: &impl IsA<gtk::Application>) -> Rc<Self> {
        let this = Rc::new(Self {
            app: app.clone().upcast(),
            windows: RefCell::default(),
            faded_in: std::cell::Cell::new(false),
            _watches: RefCell::default(),
        });

        let mut watches = Vec::new();
        for (schema, key) in [
            (gs::BACKGROUND, "picture-uri"),
            (gs::BACKGROUND, "picture-uri-dark"),
            (gs::BACKGROUND, "picture-options"),
            (gs::BACKGROUND, "primary-color"),
            (gs::DESKTOP_INTERFACE, "color-scheme"),
        ] {
            let weak = Rc::downgrade(&this);
            // `watch` fires immediately; the first real build happens below.
            if let Some(s) = gs::watch(schema, key, move |_, _| {
                if let Some(this) = weak.upgrade() {
                    if !this.windows.borrow().is_empty() {
                        this.rebuild();
                    }
                }
            }) {
                watches.push(s);
            }
        }
        *this._watches.borrow_mut() = watches;

        if let Some(display) = gdk::Display::default() {
            let weak = Rc::downgrade(&this);
            display.monitors().connect_items_changed(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.rebuild();
                }
            });
        }
        this.rebuild();
        this
    }

    fn rebuild(&self) {
        for w in self.windows.borrow_mut().drain(..) {
            w.destroy();
        }
        let Some(display) = gdk::Display::default() else {
            return;
        };
        // Decoded at the largest monitor's size, not the image's (often 4K+).
        let need = umoja_common::wallpaper::largest_monitor_pixels().unwrap_or((3840, 2160));
        let texture = gs::wallpaper_path(gs::prefers_dark())
            .and_then(|p| umoja_common::wallpaper::texture_cover(&p, need));
        if texture.is_none() {
            tracing::warn!("no usable wallpaper; showing primary-color");
        }
        let fit = match gs::get_string(gs::BACKGROUND, "picture-options", "zoom").as_str() {
            "scaled" | "centered" => gtk::ContentFit::Contain,
            "stretched" => gtk::ContentFit::Fill,
            _ => gtk::ContentFit::Cover, // zoom, spanned, wallpaper
        };
        let color = gs::get_string(gs::BACKGROUND, "primary-color", "#023c88");

        let monitors = display.monitors();
        let mut windows = Vec::new();
        for i in 0..monitors.n_items() {
            let Some(monitor) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
                continue;
            };
            let window = gtk::Window::builder()
                .application(&self.app)
                .css_classes(["umoja-wallpaper"])
                .decorated(false)
                .build();
            window.init_layer_shell();
            window.set_layer(Layer::Background);
            window.set_namespace("umoja-wallpaper");
            window.set_monitor(&monitor);
            window.set_keyboard_mode(KeyboardMode::None);
            window.set_exclusive_zone(-1);
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                window.set_anchor(edge, true);
            }

            let picture = gtk::Picture::new();
            picture.set_content_fit(fit);
            picture.set_can_shrink(true);
            picture.set_hexpand(true);
            picture.set_vexpand(true);
            if let Some(t) = &texture {
                picture.set_paintable(Some(t));
            }
            let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
            frame.append(&picture);
            set_background_color(&frame, &color);
            window.set_child(Some(&frame));
            if !self.faded_in.get() {
                crate::fade::fade_in_on_map(&window, crate::fade::SESSION_IN_MS);
            }
            window.present();
            windows.push(window);
        }
        tracing::info!(monitors = windows.len(), "wallpaper surfaces ready");
        self.faded_in.set(true);
        *self.windows.borrow_mut() = windows;
    }
}

fn set_background_color(widget: &gtk::Box, color: &str) {
    let rgba = gdk::RGBA::parse(color).unwrap_or(gdk::RGBA::BLACK);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&format!(
        ".umoja-wallpaper-frame {{ background-color: {rgba}; }}"
    ));
    widget.add_css_class("umoja-wallpaper-frame");
    #[allow(deprecated)]
    widget
        .style_context()
        .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
}
