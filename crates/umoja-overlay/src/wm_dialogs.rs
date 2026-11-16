//! Small window-manager dialogs gnome-shell has and the compositor asks
//! for: the Run a Command dialog (Alt+F2, runDialog.js), the display
//! switcher (Super+P, switchMonitor.js), the input source popup
//! (Super+Space, inputSourcePopup), and "App is not responding"
//! (closeDialog.js). Each is a layer surface on the primary monitor in the
//! shell's see-through card.

use gio::prelude::*;
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use umoja_common::{dbus, i18n};

fn compositor(method: &'static str, args: glib::Variant) {
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
                Some(&args),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await
        {
            tracing::warn!(method, error = %e, "compositor call failed");
        }
    });
}

/// A centred card on the primary monitor.
fn dialog_window(
    app: &adw::Application,
    namespace: &str,
    keyboard: KeyboardMode,
) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .css_classes(["umoja-chrome"])
        .build();
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(namespace);
    window.set_keyboard_mode(keyboard);
    window.set_exclusive_zone(-1);
    umoja_common::blur::blur_behind(&window);
    window
}

fn on_primary(window: &gtk::ApplicationWindow) {
    if let Some(m) = umoja_common::monitors::primary() {
        window.set_monitor(&m);
    }
}

// ---- Run a Command ----------------------------------------------------------

struct RunDialog {
    window: gtk::ApplicationWindow,
    entry: gtk::Entry,
    error: gtk::Label,
}

impl RunDialog {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = dialog_window(app, "umoja-run-dialog", KeyboardMode::Exclusive);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
        card.add_css_class("switcher-list");
        card.add_css_class("wm-dialog");
        let title = gtk::Label::new(Some(&i18n::shell("Run a Command")));
        title.add_css_class("title-3");
        card.append(&title);
        let entry = gtk::Entry::new();
        entry.set_width_chars(40);
        card.append(&entry);
        let error = gtk::Label::new(None);
        error.add_css_class("wm-dialog-error");
        error.set_wrap(true);
        error.set_visible(false);
        card.append(&error);
        let hint = gtk::Label::new(Some(&i18n::shell("Press ESC to close")));
        hint.add_css_class("dim-label");
        card.append(&hint);
        window.set_child(Some(&card));
        let dialog = Rc::new(Self {
            window,
            entry,
            error,
        });
        {
            let weak = Rc::downgrade(&dialog);
            dialog.entry.connect_activate(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.run();
                }
            });
        }
        let keys = gtk::EventControllerKey::new();
        {
            let weak = Rc::downgrade(&dialog);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if let Some(d) = weak.upgrade() {
                        d.window.set_visible(false);
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        dialog.window.add_controller(keys);
        dialog
    }

    fn show(&self) {
        self.entry.set_text("");
        self.error.set_visible(false);
        on_primary(&self.window);
        self.window.present();
        self.entry.grab_focus();
    }

    fn run(&self) {
        let command = self.entry.text().trim().to_string();
        if command.is_empty() {
            return;
        }
        let launched = gio::AppInfo::create_from_commandline(
            &command,
            None,
            gio::AppInfoCreateFlags::NONE,
        )
        .and_then(|info| {
            info.launch(
                &[],
                Some(&umoja_common::app_scope::launch_context()),
            )
        });
        match launched {
            Ok(()) => self.window.set_visible(false),
            Err(e) => {
                self.error.set_text(e.message());
                self.error.set_visible(true);
            }
        }
    }
}

// ---- Display switcher (Super+P) ---------------------------------------------

/// display_config::SWITCH_* in umoja-mir.
const MODES: [(&str, &str); 4] = [
    ("Mirror", "view-mirror-symbolic"),
    ("Join Displays", "video-joined-displays-symbolic"),
    ("External Only", "video-single-display-symbolic"),
    ("Built-in Only", "computer-symbolic"),
];

/// switchMonitor.js closes itself after a while without input.
const SWITCHER_IDLE_MS: u64 = 5000;

struct DisplaySwitcher {
    window: gtk::ApplicationWindow,
    items: Vec<gtk::Button>,
    selected: Cell<usize>,
    idle: RefCell<Option<glib::SourceId>>,
}

impl DisplaySwitcher {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = dialog_window(app, "umoja-display-switcher", KeyboardMode::Exclusive);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("switcher-list");
        let mut items = Vec::new();
        for (label, icon) in MODES {
            let b = gtk::Button::new();
            b.add_css_class("item-box");
            b.add_css_class("flat");
            let v = gtk::Box::new(gtk::Orientation::Vertical, 8);
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(96);
            v.append(&img);
            v.append(&gtk::Label::new(Some(&i18n::shell(label))));
            b.set_child(Some(&v));
            row.append(&b);
            items.push(b);
        }
        window.set_child(Some(&row));
        let s = Rc::new(Self {
            window,
            items,
            selected: Cell::new(0),
            idle: RefCell::default(),
        });
        for (i, b) in s.items.iter().enumerate() {
            let weak = Rc::downgrade(&s);
            b.connect_clicked(move |_| {
                if let Some(s) = weak.upgrade() {
                    s.select(i);
                    s.apply();
                }
            });
        }
        let keys = gtk::EventControllerKey::new();
        {
            let weak = Rc::downgrade(&s);
            keys.connect_key_pressed(move |_, key, _, _| {
                let Some(s) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                match key {
                    gdk::Key::Escape => s.hide(),
                    gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => s.apply(),
                    gdk::Key::Right | gdk::Key::p | gdk::Key::P | gdk::Key::Tab => s.step(1),
                    gdk::Key::Left | gdk::Key::ISO_Left_Tab => s.step(-1),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            });
        }
        {
            // Like gnome-shell, letting go of Super picks the highlighted mode.
            let weak = Rc::downgrade(&s);
            keys.connect_key_released(move |_, key, _, _| {
                if matches!(key, gdk::Key::Super_L | gdk::Key::Super_R)
                    && let Some(s) = weak.upgrade()
                    && s.window.is_visible()
                {
                    s.apply();
                }
            });
        }
        s.window.add_controller(keys);
        s
    }

    fn select(&self, i: usize) {
        self.selected.set(i);
        for (n, b) in self.items.iter().enumerate() {
            if n == i {
                b.add_css_class("selected");
            } else {
                b.remove_css_class("selected");
            }
        }
    }

    fn step(self: &Rc<Self>, by: i32) {
        let n = self.items.len() as i32;
        let i = (self.selected.get() as i32 + by).rem_euclid(n);
        self.select(i as usize);
        self.rearm();
    }

    fn rearm(self: &Rc<Self>) {
        if let Some(id) = self.idle.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(SWITCHER_IDLE_MS), move || {
            if let Some(s) = weak.upgrade() {
                s.idle.borrow_mut().take();
                s.hide();
            }
        });
        *self.idle.borrow_mut() = Some(id);
    }

    /// Super+P: open on Join (or the next mode when already open).
    fn show(self: &Rc<Self>) {
        if self.window.is_visible() {
            self.step(1);
            return;
        }
        // Built-in Only is useless with one screen; gnome-shell still
        // lists every mode and the compositor refuses what can't apply.
        self.select(1);
        on_primary(&self.window);
        self.window.present();
        self.rearm();
    }

    fn apply(&self) {
        let mode = self.selected.get() as u32;
        self.hide();
        compositor("SwitchDisplayConfig", (mode,).to_variant());
    }

    fn hide(&self) {
        if let Some(id) = self.idle.borrow_mut().take() {
            id.remove();
        }
        self.window.set_visible(false);
    }
}

// ---- Input source popup -----------------------------------------------------

const INPUT_POPUP_MS: u64 = 900;

struct InputSourcePopup {
    window: gtk::ApplicationWindow,
    row: gtk::Box,
    hide_id: RefCell<Option<glib::SourceId>>,
}

impl InputSourcePopup {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = dialog_window(app, "umoja-input-source", KeyboardMode::None);
        // Click-through: it only shows what Super+Space picked.
        window.connect_map(|w| {
            if let Some(s) = w.surface() {
                s.set_input_region(&gtk::cairo::Region::create());
            }
        });
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("switcher-list");
        window.set_child(Some(&row));
        Rc::new(Self {
            window,
            row,
            hide_id: RefCell::default(),
        })
    }

    fn show(self: &Rc<Self>, labels: Vec<String>, index: u32) {
        while let Some(c) = self.row.first_child() {
            self.row.remove(&c);
        }
        for (i, l) in labels.iter().enumerate() {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
            b.add_css_class("item-box");
            b.add_css_class("input-source-item");
            if i as u32 == index {
                b.add_css_class("selected");
            }
            let label = gtk::Label::new(Some(l));
            b.append(&label);
            self.row.append(&b);
        }
        on_primary(&self.window);
        self.window.present();
        if let Some(id) = self.hide_id.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(INPUT_POPUP_MS), move || {
            if let Some(p) = weak.upgrade() {
                p.hide_id.borrow_mut().take();
                p.window.set_visible(false);
            }
        });
        *self.hide_id.borrow_mut() = Some(id);
    }
}

// ---- App is not responding --------------------------------------------------

struct NotResponding {
    window: gtk::ApplicationWindow,
}

fn not_responding(app: &adw::Application, id: u64, app_id: &str, title: &str) -> NotResponding {
    let window = dialog_window(app, "umoja-close-dialog", KeyboardMode::OnDemand);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("switcher-list");
    card.add_css_class("wm-dialog");
    let name = app_name(app_id).unwrap_or_else(|| title.to_string());
    let heading = gtk::Label::new(Some(
        &i18n::shell("“%s” is not responding.").replacen("%s", &name, 1),
    ));
    heading.add_css_class("title-3");
    heading.set_wrap(true);
    card.append(&heading);
    let body = gtk::Label::new(Some(&i18n::shell(
        "You may choose to wait a short while for it to continue or force the app to quit entirely.",
    )));
    body.set_wrap(true);
    body.set_max_width_chars(48);
    card.append(&body);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_homogeneous(true);
    let quit = gtk::Button::with_label(&i18n::shell("Force Quit"));
    quit.add_css_class("destructive-action");
    quit.add_css_class("pill");
    let wait = gtk::Button::with_label(&i18n::shell("Wait"));
    wait.add_css_class("pill");
    buttons.append(&quit);
    buttons.append(&wait);
    card.append(&buttons);
    window.set_child(Some(&card));
    {
        let w = window.clone();
        quit.connect_clicked(move |_| {
            compositor("ForceQuitWindow", (id,).to_variant());
            w.close();
        });
    }
    {
        let w = window.clone();
        wait.connect_clicked(move |_| w.close());
    }
    on_primary(&window);
    window.present();
    wait.grab_focus();
    NotResponding { window }
}

fn app_name(app_id: &str) -> Option<String> {
    if app_id.is_empty() {
        return None;
    }
    let info = gio::DesktopAppInfo::new(&format!("{app_id}.desktop"))?;
    Some(info.display_name().to_string())
}

// ---- public face ------------------------------------------------------------

pub struct WmDialogs {
    app: adw::Application,
    run: Rc<RunDialog>,
    displays: Rc<DisplaySwitcher>,
    input: Rc<InputSourcePopup>,
    hung: RefCell<HashMap<u64, NotResponding>>,
}

impl WmDialogs {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        umoja_common::monitors::start();
        Rc::new(Self {
            app: app.clone(),
            run: RunDialog::new(app),
            displays: DisplaySwitcher::new(app),
            input: InputSourcePopup::new(app),
            hung: RefCell::default(),
        })
    }

    pub fn show_run_dialog(&self) {
        self.run.show();
    }

    pub fn show_display_switcher(&self) {
        self.displays.show();
    }

    pub fn input_source_switched(&self, labels: Vec<String>, index: u32) {
        self.input.show(labels, index);
    }

    pub fn app_not_responding(&self, id: u64, app_id: &str, title: &str) {
        let mut hung = self.hung.borrow_mut();
        if let Some(old) = hung.get(&id)
            && old.window.is_visible()
        {
            return;
        }
        hung.insert(id, not_responding(&self.app, id, app_id, title));
    }

    pub fn app_responding(&self, id: u64) {
        if let Some(d) = self.hung.borrow_mut().remove(&id) {
            d.window.close();
        }
    }

    /// For the test script: which of these are on screen.
    pub fn visible(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.run.window.is_visible() {
            v.push("run-dialog".to_string());
        }
        if self.displays.window.is_visible() {
            v.push(format!("display-switcher:{}", self.displays.selected.get()));
        }
        if self.input.window.is_visible() {
            v.push("input-source".to_string());
        }
        for (id, d) in self.hung.borrow().iter() {
            if d.window.is_visible() {
                v.push(format!("not-responding:{id}"));
            }
        }
        v
    }
}
