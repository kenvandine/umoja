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

use gtk::{gdk, gio, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use umoja_common::gsettings as gs;

pub struct Wallpaper {
    app: gtk::Application,
    surfaces: RefCell<Vec<Surface>>,
    /// The first surfaces fade up from black after login; later rebuilds
    /// (monitor changes) swap in place.
    faded_in: std::cell::Cell<bool>,
    /// Shared by every surface: `primary-color` behind the picture.
    color_css: gtk::CssProvider,
    _watches: RefCell<Vec<gio::Settings>>,
}

struct Surface {
    window: gtk::Window,
    picture: gtk::Picture,
}

/// What the settings ask the surfaces to show.
struct Look {
    texture: Option<gdk::Texture>,
    fit: gtk::ContentFit,
}

impl Wallpaper {
    pub fn start(app: &impl IsA<gtk::Application>) -> Rc<Self> {
        let this = Rc::new(Self {
            app: app.clone().upcast(),
            surfaces: RefCell::default(),
            faded_in: std::cell::Cell::new(false),
            color_css: gtk::CssProvider::new(),
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
                    if !this.surfaces.borrow().is_empty() {
                        this.refresh();
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

    /// Reads the settings and decodes the picture. The decode happens
    /// before anything on screen changes, so a swap never shows a gap.
    fn look(&self) -> Look {
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
        let rgba = gdk::RGBA::parse(color.as_str()).unwrap_or(gdk::RGBA::BLACK);
        self.color_css.load_from_string(&format!(
            ".umoja-wallpaper-frame {{ background-color: {rgba}; }}"
        ));
        Look { texture, fit }
    }

    /// A wallpaper or colour-scheme change: swap the picture inside the
    /// existing surfaces. Destroying and re-creating them showed a black
    /// frame between the old surface going and the new one mapping.
    fn refresh(&self) {
        let look = self.look();
        for s in self.surfaces.borrow().iter() {
            s.picture.set_content_fit(look.fit);
            s.picture.set_paintable(look.texture.as_ref());
        }
    }

    /// A monitor change: new surfaces for the new set. The old ones stay
    /// up until the new ones have drawn, then go.
    fn rebuild(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let look = self.look();
        let old: Vec<Surface> = self.surfaces.borrow_mut().drain(..).collect();

        let monitors = display.monitors();
        let mut surfaces = Vec::new();
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
            picture.set_content_fit(look.fit);
            picture.set_can_shrink(true);
            picture.set_hexpand(true);
            picture.set_vexpand(true);
            picture.set_paintable(look.texture.as_ref());
            let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
            frame.append(&picture);
            frame.add_css_class("umoja-wallpaper-frame");
            #[allow(deprecated)]
            frame
                .style_context()
                .add_provider(&self.color_css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            window.set_child(Some(&frame));
            if !self.faded_in.get() {
                crate::fade::fade_in_on_map(&window, crate::fade::SESSION_IN_MS);
            }
            window.present();
            surfaces.push(Surface { window, picture });
        }
        tracing::info!(monitors = surfaces.len(), "wallpaper surfaces ready");
        self.faded_in.set(true);
        *self.surfaces.borrow_mut() = surfaces;

        if !old.is_empty() {
            // Long enough for the new surfaces to map and draw a frame.
            glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                for s in old {
                    s.window.destroy();
                }
            });
        }
    }
}
