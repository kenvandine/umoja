//! The app grid (Show Apps, and typing in the overview): a search entry,
//! GNOME's grid of app tiles and matching files. It has no assistant: that
//! is Ask Umoja's card (view.rs).

use adw::prelude::*;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use umoja_common::i18n;

/// Most apps shown for a search. With no search the grid lists every app
/// and scrolls, like GNOME's app grid.
const MAX_RESULTS: usize = 48;
/// Width of an app grid tile, icon plus label, as in GNOME (about 128px).
const TILE_WIDTH: i32 = 128;

pub struct AppGrid {
    window: adw::ApplicationWindow,
    entry: gtk::SearchEntry,
    grid: gtk::FlowBox,
    /// App tiles by desktop id, built once and reused: rebuilding hundreds
    /// of tiles on every open and keystroke made GTK render most icons again
    /// (its icon cache keeps only a few dozen unused ones).
    tiles: RefCell<HashMap<String, gtk::FlowBoxChild>>,
    files: Rc<crate::files::FilesSection>,
    apps: RefCell<Vec<gio::AppInfo>>,
    /// Held for the grid's lifetime: the monitor is a weak singleton, and
    /// once the last reference drops it stops emitting `changed`.
    app_monitor: gio::AppInfoMonitor,
    empty: gtk::Label,
    hide_serial: Rc<Cell<u32>>,
}

impl AppGrid {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .css_classes(["umoja-overlay"])
            .title(i18n::umoja("Apps"))
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-overlay");
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(e, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        umoja_common::blur::blur_behind(&window);

        let entry = gtk::SearchEntry::builder()
            .placeholder_text(i18n::umoja("Search apps and files…"))
            .width_request(560)
            .halign(gtk::Align::Center)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label(&i18n::umoja(
            "Search apps and files",
        ))]);

        // Like GNOME's app grid: a compact block of fixed-width tiles in the
        // middle of the screen. Centering the FlowBox gives it its natural
        // width (at most 8 tiles), so on a wide monitor the leftover space
        // goes to the sides instead of between the icons.
        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(8)
            .min_children_per_line(4)
            .homogeneous(true)
            .row_spacing(12)
            .column_spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .build();
        let empty = gtk::Label::builder()
            .label(i18n::umoja("No apps match."))
            .css_classes(["dim-label"])
            .visible(false)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 24);
        column.append(&grid);
        column.append(&empty);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&column)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 24);
        root.set_margin_top(64);
        root.set_margin_start(96);
        root.set_margin_end(96);
        root.set_margin_bottom(48);
        root.append(&entry);
        window.set_content(Some(&root));

        let this = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| Self {
            files: crate::files::FilesSection::new_plain({
                let weak = weak.clone();
                move || {
                    if let Some(g) = weak.upgrade() {
                        g.hide();
                    }
                }
            }),
            window,
            entry,
            grid,
            tiles: RefCell::default(),
            apps: RefCell::new(load_apps()),
            app_monitor: gio::AppInfoMonitor::get(),
            empty,
            hide_serial: Rc::new(Cell::new(0)),
        });
        root.append(&this.files.widget);
        root.append(&scroller);
        this.connect_signals();
        this.refresh();
        this
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.entry.connect_search_changed(move |_| {
            if let Some(g) = weak.upgrade() {
                g.refresh();
            }
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |e| {
            let Some(g) = weak.upgrade() else { return };
            let q = e.text().to_string();
            if crate::files::looks_like_file_search(&q) && g.files.activate_first() {
                return;
            }
            if let Some(app) = g.matches(&q).into_iter().next() {
                g.launch(&app);
            } else {
                g.files.activate_first();
            }
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_stop_search(move |_| {
            if let Some(g) = weak.upgrade() {
                g.hide();
            }
        });
        // Escape anywhere closes the grid.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(g) = weak.upgrade() {
                    g.hide();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);
        // Rescan apps when packages are installed or removed.
        let weak = Rc::downgrade(self);
        self.app_monitor.connect_changed(move |_| {
            if let Some(g) = weak.upgrade() {
                *g.apps.borrow_mut() = load_apps();
                g.tiles.borrow_mut().clear();
                g.refresh();
            }
        });
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn show(self: &Rc<Self>) {
        self.hide_serial.set(self.hide_serial.get().wrapping_add(1));
        self.entry.set_text("");
        self.refresh();
        self.files.clear();
        self.window.present();
        self.entry.grab_focus();
    }

    /// Open the grid with `text` already typed (from the overview).
    pub fn show_with_text(self: &Rc<Self>, text: &str) {
        self.show();
        self.entry.set_text(text);
        self.entry.set_position(-1);
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    pub fn hide(&self) {
        let serial = self.hide_serial.get().wrapping_add(1);
        self.hide_serial.set(serial);
        if !self.window.is_visible() {
            return;
        }
        self.entry.set_text("");
        hide_after_clean_frame(self.window.upcast_ref(), self.hide_serial.clone(), serial);
    }

    fn matches(&self, query: &str) -> Vec<gio::AppInfo> {
        let apps = self.apps.borrow();
        if query.trim().is_empty() {
            return apps.clone();
        }
        let mut scored: Vec<(u8, gio::AppInfo)> = apps
            .iter()
            .filter_map(|a| app_match(a, query).map(|s| (s, a.clone())))
            .collect();
        scored.sort_by_key(|(s, a)| (*s, a.display_name().to_lowercase()));
        scored.into_iter().map(|(_, a)| a).collect()
    }

    fn refresh(self: &Rc<Self>) {
        while let Some(c) = self.grid.first_child() {
            self.grid.remove(&c);
        }
        let q = self.entry.text().to_string();
        let found = self.matches(&q);
        let limit = if q.trim().is_empty() {
            usize::MAX
        } else {
            MAX_RESULTS
        };
        for app in found.iter().take(limit) {
            self.grid.append(&self.cached_tile(app));
        }
        self.empty.set_visible(found.is_empty());
        self.files.update(&q);
    }

    fn cached_tile(self: &Rc<Self>, app: &gio::AppInfo) -> gtk::FlowBoxChild {
        let key = app_key(app);
        if let Some(tile) = self.tiles.borrow().get(&key) {
            return tile.clone();
        }
        let child = gtk::FlowBoxChild::new();
        child.set_child(Some(&self.app_tile(app)));
        self.tiles.borrow_mut().insert(key, child.clone());
        child
    }

    fn app_tile(self: &Rc<Self>, app: &gio::AppInfo) -> gtk::Widget {
        let image = match app.icon() {
            Some(icon) => gtk::Image::from_gicon(&icon),
            None => gtk::Image::from_icon_name("application-x-executable"),
        };
        image.set_pixel_size(64);
        image.set_accessible_role(gtk::AccessibleRole::Presentation);
        let label = gtk::Label::builder()
            .label(app.display_name())
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(12)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&image);
        content.append(&label);
        let button = gtk::Button::builder()
            .child(&content)
            .width_request(TILE_WIDTH)
            .css_classes(["flat", "app-tile"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(&app.display_name())]);
        if let Some(d) = app.description() {
            button.update_property(&[gtk::accessible::Property::Description(&d)]);
        }
        let weak = Rc::downgrade(self);
        let app_c = app.clone();
        button.connect_clicked(move |_| {
            if let Some(g) = weak.upgrade() {
                g.launch(&app_c);
            }
        });

        // Right-click (or long-press) menu, as in GNOME's app grid.
        if let Ok(desktop) = app.clone().downcast::<gio::DesktopAppInfo>() {
            let menu = gtk::GestureClick::builder().button(3).build();
            let (weak, d) = (Rc::downgrade(self), desktop.clone());
            menu.connect_pressed(move |g, _, _, _| {
                if let (Some(o), Some(w)) = (weak.upgrade(), g.widget()) {
                    o.show_tile_menu(&w, &d);
                }
            });
            button.add_controller(menu);
            let press = gtk::GestureLongPress::builder().touch_only(true).build();
            let weak = Rc::downgrade(self);
            press.connect_pressed(move |g, _, _| {
                if let (Some(o), Some(w)) = (weak.upgrade(), g.widget()) {
                    o.show_tile_menu(&w, &desktop);
                }
            });
            button.add_controller(press);
        }
        button.upcast()
    }

    fn launch(&self, app: &gio::AppInfo) {
        launch_app(self.window.upcast_ref(), app);
        self.hide();
    }

    /// GNOME's app-grid menu: the app's desktop actions, Pin to Dock /
    /// Unpin (org.gnome.shell favorite-apps, which the dock follows) and
    /// App Details.
    fn show_tile_menu(self: &Rc<Self>, parent: &gtk::Widget, app: &gio::DesktopAppInfo) {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let popover = gtk::Popover::builder().child(&col).build();
        let weak_pop = popover.downgrade();
        let item = |label: &str, action: Box<dyn Fn()>| {
            let b = gtk::Button::builder()
                .child(&gtk::Label::builder().label(label).xalign(0.0).build())
                .css_classes(["flat"])
                .build();
            let pop = weak_pop.clone();
            b.connect_clicked(move |_| {
                if let Some(p) = pop.upgrade() {
                    p.popdown();
                }
                action();
            });
            col.append(&b);
        };

        let actions = app.list_actions();
        for action in &actions {
            let label = app.action_name(action).to_string();
            let (weak, a, action) = (Rc::downgrade(self), app.clone(), action.to_string());
            item(
                &label,
                Box::new(move || {
                    if let Some(g) = weak.upgrade() {
                        launch_action(g.window.upcast_ref(), &a, &action);
                        g.hide();
                    }
                }),
            );
        }
        if !actions.is_empty() {
            col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }

        let id = app.id().map(|s| s.to_string()).unwrap_or_default();
        let pinned = umoja_common::apps::is_favorite(&id);
        let label = if pinned {
            i18n::shell("Unpin")
        } else {
            i18n::dock("Pin to Dock")
        };
        let pin_id = id.clone();
        item(
            &label,
            Box::new(move || umoja_common::apps::set_favorite(&pin_id, !pinned)),
        );
        if let Some(details) = umoja_common::apps::details_for(app) {
            let weak = Rc::downgrade(self);
            item(
                &i18n::shell("App Details"),
                Box::new(move || {
                    umoja_common::apps::show_details(&details);
                    if let Some(g) = weak.upgrade() {
                        g.hide();
                    }
                }),
            );
        }

        popover.set_parent(parent);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }
}

/// Launch `app` in its own scope (so it isn't counted as the shell's).
pub fn launch_app(window: &gtk::Window, app: &gio::AppInfo) {
    let ctx = umoja_common::app_scope::scoped(WidgetExt::display(window).app_launch_context());
    if let Err(e) = app.launch(&[], Some(&ctx)) {
        tracing::warn!(error = %e, "launch failed");
    }
}

/// Run one of an app's launcher actions ("New Private Window").
pub fn launch_action(window: &gtk::Window, app: &gio::DesktopAppInfo, action: &str) {
    let ctx = umoja_common::app_scope::scoped(WidgetExt::display(window).app_launch_context());
    app.launch_action(action, Some(&ctx));
}

/// Stable key for an app: its desktop id.
pub fn app_key(app: &gio::AppInfo) -> String {
    app.id()
        .map(|id| id.to_string())
        .unwrap_or_else(|| format!("{}\0{}", app.display_name(), app.executable().display()))
}

/// 0: the name starts with the query, 1: contains it, 2: its description
/// or keywords do. None: no match.
pub fn app_match(app: &gio::AppInfo, query: &str) -> Option<u8> {
    let q = query.trim().to_lowercase();
    let name = app.display_name().to_lowercase();
    if name.starts_with(&q) {
        Some(0)
    } else if name.contains(&q) {
        Some(1)
    } else if haystack(app).contains(&q) {
        Some(2)
    } else {
        None
    }
}

/// An app's description and keywords, lowercased.
pub fn haystack(app: &gio::AppInfo) -> String {
    let mut s = app
        .description()
        .map(|d| d.to_lowercase())
        .unwrap_or_default();
    for k in keywords(app) {
        s.push(' ');
        s.push_str(&k);
    }
    s
}

/// An app's Keywords= and GenericName=, lowercased.
pub fn keywords(app: &gio::AppInfo) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(d) = app.downcast_ref::<gio::DesktopAppInfo>() {
        for k in d.keywords() {
            out.push(k.to_lowercase());
        }
        if let Some(g) = d.generic_name() {
            out.push(g.to_lowercase());
        }
    }
    out
}

pub fn load_apps() -> Vec<gio::AppInfo> {
    let mut apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .collect();
    apps.sort_by_key(|a| a.display_name().to_lowercase());
    apps
}

/// Unmap `window` once the display has taken a frame of its cleared
/// contents.
///
/// Mir hands a compositor that hasn't used a surface's current frame that
/// frame, not the newer one, so a screen recording (its own compositor)
/// showed the previous contents for a frame or two when the window opened
/// again. Paint a clean frame and unmap only once the display has taken it
/// (the frame callback lets GTK paint a second time). `serial` is the
/// caller's show/hide counter: a show meanwhile cancels the hide.
pub fn hide_after_clean_frame(window: &gtk::Window, serial: Rc<Cell<u32>>, expect: u32) {
    let done = Rc::new(Cell::new(false));
    let finish = {
        let window = window.clone();
        let done = done.clone();
        Rc::new(move || {
            if !done.replace(true) && serial.get() == expect {
                window.set_visible(false);
            }
        })
    };
    if let Some(clock) = window.frame_clock() {
        let f = finish.clone();
        let paints = Rc::new(Cell::new(0u8));
        let id = Rc::new(RefCell::new(None::<glib::SignalHandlerId>));
        let id2 = id.clone();
        let c2 = clock.clone();
        let w = window.clone();
        *id.borrow_mut() = Some(clock.connect_after_paint(move |_| {
            paints.set(paints.get() + 1);
            if paints.get() < 2 {
                // Not from inside the paint itself.
                let w = w.clone();
                glib::idle_add_local_once(move || w.queue_draw());
                return;
            }
            if let Some(h) = id2.borrow_mut().take() {
                c2.disconnect(h);
            }
            f();
        }));
        window.queue_draw();
    }
    // In case no frame comes (the surface isn't being drawn).
    glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || finish());
}
